//! Optional read-only reader for the rizetngan/snug-for-windows v0.2.47 capture monitor.
//! Protocol: `Platforms/Windows/src/Snug.Windows.Core/UuCaptureFeed.cs` at that tag.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr::null_mut;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use snug_core::session::UuCaptureEvidence;
use windows::Win32::Foundation::{FILETIME, HLOCAL, LocalFree, STILL_ACTIVE};
use windows::Win32::Security::{
    IsWellKnownSid, PSID, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{FOLDERID_ProgramData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::core::GUID;

const SIZE: usize = 64;
const MAX_BATCH: u32 = (8 * SIZE) as u32;
const UNIX_EPOCH_TICKS: u64 = 621_355_968_000_000_000;
const FILETIME_EPOCH_TICKS: u64 = 504_911_232_000_000_000;
const TICKS_PER_SECOND: u64 = 10_000_000;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    schema_version: u32,
    feed_id: String,
}

pub(super) struct CaptureFeed {
    id: u128,
    path: String,
    pipe: Option<File>,
    sequence: u64,
    last_diagnosis: Option<&'static str>,
}

impl CaptureFeed {
    pub(super) fn configured() -> Option<Self> {
        if std::env::var_os("SNUG_UU_CAPTURE_FEED").as_deref() != Some(std::ffi::OsStr::new("1")) {
            return None;
        }
        let result = Self::load();
        if result.is_none() {
            crate::warn("uu: 捕获监视配置不可读或不兼容，使用常规重连等待");
        }
        result
    }

    fn load() -> Option<Self> {
        let directory = unsafe {
            let path = SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT, None).ok()?;
            let directory = path.to_string().ok();
            CoTaskMemFree(Some(path.0.cast()));
            directory?
        };
        let path = std::path::Path::new(&directory).join("Snug/uu-capture-monitor.json");
        let mut bytes = Vec::new();
        File::open(path)
            .ok()?
            .take(4097)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > 4096 {
            return None;
        }
        let json = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
        let configured: Config = serde_json::from_slice(json).ok()?;
        let id = GUID::try_from(configured.feed_id.as_str()).ok()?.to_u128();
        if configured.schema_version != 1 || id == 0 {
            return None;
        }
        Some(Self {
            id,
            path: format!(r"\\.\pipe\Snug-UU-Capture-Monitor-{id:032x}"),
            pipe: None,
            sequence: 0,
            last_diagnosis: None,
        })
    }

    pub(super) fn sample(&mut self, server_pid: Option<i32>) -> UuCaptureEvidence {
        let result = self.read_available(server_pid);
        let diagnosis = match result {
            Ok(UuCaptureEvidence::Unknown) => Some("等待完整且匹配当前 UU 服务的捕获清单"),
            Ok(_) => None,
            Err(reason) => {
                self.sequence = 0;
                self.pipe = None;
                Some(reason)
            }
        };
        if diagnosis != self.last_diagnosis {
            self.last_diagnosis = diagnosis;
            match diagnosis {
                Some(reason) => crate::info(&format!("uu: {reason}，使用常规重连等待")),
                None => crate::info("uu: 捕获监视证据可用"),
            }
        }
        result.unwrap_or_default()
    }

    fn read_available(
        &mut self,
        server_pid: Option<i32>,
    ) -> Result<UuCaptureEvidence, &'static str> {
        if self.pipe.is_none() {
            let pipe = OpenOptions::new()
                .read(true)
                .security_qos_flags(SECURITY_IDENTIFICATION.0)
                .open(&self.path)
                .map_err(|_| "捕获监视管道未就绪")?;
            if !privileged_owner(&pipe) {
                return Err("捕获监视管道的所有者未通过检查");
            }
            self.pipe = Some(pipe);
        }
        let pipe = self.pipe.as_mut().expect("pipe opened above");
        let available = bytes_available(pipe).map_err(|_| "捕获监视管道已断开")?;
        if available > MAX_BATCH {
            return Err("捕获监视积压过多");
        }
        if available < SIZE as u32 {
            return Ok(UuCaptureEvidence::Unknown);
        }
        let now = utc_ticks().ok_or("系统时钟无法读取")?;
        let server = server_pid.and_then(server_generation);
        let mut result = UuCaptureEvidence::Unknown;
        let mut can_confirm_stopped = true;
        for _ in 0..available / SIZE as u32 {
            let mut bytes = [0; SIZE];
            pipe.read_exact(&mut bytes)
                .map_err(|_| "未读完捕获监视数据")?;
            let frame = Frame::decode(bytes, self.id, self.sequence, now)
                .ok_or("捕获监视数据无效或过期")?;
            self.sequence = frame.sequence;
            let evidence = frame.evidence(server);
            if !matches!(evidence, UuCaptureEvidence::Stopped { .. }) {
                can_confirm_stopped = false;
                result = evidence;
            } else if can_confirm_stopped {
                result = evidence;
            }
        }
        Ok(result)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ServerGeneration {
    pid: u32,
    created: u64,
}

#[derive(Debug)]
struct Frame {
    sequence: u64,
    server: ServerGeneration,
    phase: u32,
    complete: bool,
}

impl Frame {
    fn decode(bytes: [u8; SIZE], expected: u128, previous: u64, now: u64) -> Option<Self> {
        let data1 = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        let data2 = u16::from_le_bytes(bytes[8..10].try_into().ok()?);
        let data3 = u16::from_le_bytes(bytes[10..12].try_into().ok()?);
        let id = GUID::from_values(data1, data2, data3, bytes[12..20].try_into().ok()?).to_u128();
        let sequence = u64::from_le_bytes(bytes[20..28].try_into().ok()?);
        let observed = u64::from_le_bytes(bytes[28..36].try_into().ok()?);
        let pid = u32::from_le_bytes(bytes[36..40].try_into().ok()?);
        let created = u64::from_le_bytes(bytes[40..48].try_into().ok()?);
        let phase = u32::from_le_bytes(bytes[48..52].try_into().ok()?);
        let contexts = u32::from_le_bytes(bytes[52..56].try_into().ok()?);
        let complete = bytes[56] == 1;
        let valid_inventory = match phase {
            0 => contexts == 0 && !complete,
            1 => (1..=1024).contains(&contexts),
            2 => contexts == 0,
            3 => contexts == 0 && complete,
            _ => false,
        };
        if &bytes[..4] != b"SCU1"
            || id != expected
            || sequence == 0
            || sequence > i64::MAX as u64
            || (previous != 0 && sequence != previous + 1)
            || pid == 0
            || pid > i32::MAX as u32
            || created == 0
            || observed < created
            || now.saturating_sub(observed) > 3 * TICKS_PER_SECOND
            || observed.saturating_sub(now) > TICKS_PER_SECOND
            || !valid_inventory
            || bytes[56] > 1
            || bytes[57..].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        Some(Self {
            sequence,
            server: ServerGeneration { pid, created },
            phase,
            complete,
        })
    }

    fn evidence(&self, actual: Option<ServerGeneration>) -> UuCaptureEvidence {
        if !self.complete || Some(self.server) != actual {
            return UuCaptureEvidence::Unknown;
        }
        let generation = self.server.created;
        match self.phase {
            1 => UuCaptureEvidence::Active { generation },
            2 => UuCaptureEvidence::Rebuilding { generation },
            3 => UuCaptureEvidence::Stopped { generation },
            _ => UuCaptureEvidence::Unknown,
        }
    }
}

fn server_generation(pid: i32) -> Option<ServerGeneration> {
    let path = super::process_image_path(pid)?;
    if !path
        .rsplit(['\\', '/'])
        .next()?
        .eq_ignore_ascii_case("GameViewerServer.exe")
    {
        return None;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32).ok()? };
    let _owned = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user).ok()? };
    let mut exit_code = 0;
    unsafe { GetExitCodeProcess(handle, &mut exit_code).ok()? };
    if exit_code != STILL_ACTIVE.0 as u32 {
        return None;
    }
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Some(ServerGeneration {
        pid: pid as u32,
        created: (ticks + FILETIME_EPOCH_TICKS) / 10 * 10,
    })
}

fn utc_ticks() -> Option<u64> {
    let ticks = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos()
        / 100;
    UNIX_EPOCH_TICKS.checked_add(u64::try_from(ticks).ok()?)
}

fn bytes_available(pipe: &File) -> io::Result<u32> {
    let mut available = 0;
    let ok = unsafe {
        PeekNamedPipe(
            pipe.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            &mut available,
            null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(available)
    }
}

fn privileged_owner(pipe: &File) -> bool {
    const SE_FILE_OBJECT: i32 = 1;
    const OWNER_SECURITY_INFORMATION: u32 = 1;
    let mut owner = null_mut();
    let mut descriptor = null_mut();
    unsafe {
        let status = GetSecurityInfo(
            pipe.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        );
        let accepted = status == 0
            && !owner.is_null()
            && (IsWellKnownSid(PSID(owner), WinLocalSystemSid).as_bool()
                || IsWellKnownSid(PSID(owner), WinBuiltinAdministratorsSid).as_bool());
        if !descriptor.is_null() {
            let _ = LocalFree(Some(HLOCAL(descriptor)));
        }
        accepted
    }
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn GetSecurityInfo(
        handle: RawHandle,
        object_type: i32,
        security_info: u32,
        owner: *mut *mut c_void,
        group: *mut *mut c_void,
        dacl: *mut *mut c_void,
        sacl: *mut *mut c_void,
        descriptor: *mut *mut c_void,
    ) -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: u128 = 0x1a2b3c4d_0102_0345_0607_112233445566;
    const NOW: u64 = UNIX_EPOCH_TICKS + 10 * TICKS_PER_SECOND;
    const SERVER: ServerGeneration = ServerGeneration {
        pid: 429,
        created: UNIX_EPOCH_TICKS,
    };

    // SCU1 uses Windows GUID field ordering, rather than serializing a u128.
    fn frame(phase: u32, contexts: u32, complete: u8) -> [u8; SIZE] {
        let mut bytes = [0; SIZE];
        bytes[..4].copy_from_slice(b"SCU1");
        let guid = GUID::from_u128(ID);
        bytes[4..8].copy_from_slice(&guid.data1.to_le_bytes());
        bytes[8..10].copy_from_slice(&guid.data2.to_le_bytes());
        bytes[10..12].copy_from_slice(&guid.data3.to_le_bytes());
        bytes[12..20].copy_from_slice(&guid.data4);
        bytes[20..28].copy_from_slice(&2_u64.to_le_bytes());
        bytes[28..36].copy_from_slice(&NOW.to_le_bytes());
        bytes[36..40].copy_from_slice(&SERVER.pid.to_le_bytes());
        bytes[40..48].copy_from_slice(&SERVER.created.to_le_bytes());
        bytes[48..52].copy_from_slice(&phase.to_le_bytes());
        bytes[52..56].copy_from_slice(&contexts.to_le_bytes());
        bytes[56] = complete;
        bytes
    }

    #[test]
    fn capture_frames_must_be_fresh_complete_and_from_the_same_generation() {
        let stopped = Frame::decode(frame(3, 0, 1), ID, 1, NOW).unwrap();
        assert_eq!(
            stopped.evidence(Some(SERVER)),
            UuCaptureEvidence::Stopped {
                generation: SERVER.created
            }
        );
        assert_eq!(
            stopped.evidence(Some(ServerGeneration {
                created: SERVER.created + 10,
                ..SERVER
            })),
            UuCaptureEvidence::Unknown
        );
        assert_eq!(stopped.evidence(None), UuCaptureEvidence::Unknown);
        let incomplete = Frame::decode(frame(1, 1, 0), ID, 1, NOW).unwrap();
        assert_eq!(
            incomplete.evidence(Some(SERVER)),
            UuCaptureEvidence::Unknown
        );
        assert!(Frame::decode(frame(3, 0, 0), ID, 1, NOW).is_none());
        assert!(Frame::decode(frame(3, 1, 1), ID, 1, NOW).is_none());
        assert!(Frame::decode(frame(3, 0, 1), ID, 1, NOW + 4 * TICKS_PER_SECOND).is_none());
        assert!(Frame::decode(frame(3, 0, 1), ID, 2, NOW).is_none());
        assert!(Frame::decode(frame(3, 0, 1), ID + 1, 1, NOW).is_none());
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn PeekNamedPipe(
        pipe: RawHandle,
        buffer: *mut c_void,
        size: u32,
        read: *mut u32,
        available: *mut u32,
        left: *mut u32,
    ) -> i32;
}
