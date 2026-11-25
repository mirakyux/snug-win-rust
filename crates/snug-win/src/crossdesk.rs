//! Observe CrossDesk desktop sharing without changing the remote application.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, SYSTEMTIME};
use windows::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::core::PWSTR;

const LOG_FILES: [&str; 4] = [
    "crossdesk.log",
    "crossdesk.1.log",
    "crossdesk.2.log",
    "crossdesk.3.log",
];
const LOG_TAIL_BYTES: u64 = 128 * 1024;
const SHARING_STARTED: &str =
    "Requested display and system idle-sleep inhibition for screen sharing";
const SHARING_STOPPED: &str = "Released screen sharing power inhibitors";

/// Experimental observation of the desktop host in Snug's Windows session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossDeskActivity {
    Inactive,
    Active,
    Unknown,
}

#[derive(Default)]
pub struct CrossDeskSensor {
    observation: Mutex<Observation>,
}

#[derive(Default)]
struct Observation {
    process: Option<(u32, String)>,
    logs: Vec<LogTail>,
}

impl CrossDeskSensor {
    pub fn new() -> Self {
        Self::default()
    }

    /// CrossDesk v1.7.7 records its sharing transitions in the desktop log. The Windows
    /// power-request list requires elevation, whereas the desktop log is read-only and
    /// available in the interactive session. Missing or ambiguous logs yield Unknown.
    pub fn sample(&self) -> CrossDeskActivity {
        let Ok(mut observation) = self.observation.lock() else {
            return CrossDeskActivity::Unknown;
        };
        let process = match crossdesk_process() {
            Ok(Some(process)) => process,
            Ok(None) => {
                *observation = Observation::default();
                return CrossDeskActivity::Inactive;
            }
            Err(_) => return CrossDeskActivity::Unknown,
        };
        let Some((executable, started_at)) = process_identity(process) else {
            return CrossDeskActivity::Unknown;
        };

        let generation = (process, started_at);
        if observation.process.as_ref() != Some(&generation) {
            let installed = std::env::var_os("LOCALAPPDATA")
                .map(|path| PathBuf::from(path).join("CrossDesk").join("logs"));
            let portable = executable.parent().map(|path| path.join("logs"));
            observation.logs = installed
                .into_iter()
                .chain(portable)
                .map(LogTail::new)
                .collect();
            observation.process = Some(generation.clone());
        }
        let mut readings = observation
            .logs
            .iter_mut()
            .filter_map(|log| log.activity(&generation.1));
        let Some(mut latest) = readings.next() else {
            return CrossDeskActivity::Unknown;
        };
        for reading in readings {
            if reading.stamp == latest.stamp && reading.activity != latest.activity {
                return CrossDeskActivity::Unknown;
            }
            if reading.stamp > latest.stamp {
                latest = reading;
            }
        }
        latest.activity
    }
}

fn crossdesk_process() -> windows::core::Result<Option<u32>> {
    let mut our_session = 0;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut our_session) }?;
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }?;
    let result = (|| {
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut candidate = None;
        unsafe { Process32FirstW(snapshot, &mut entry) }?;
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
            if name.eq_ignore_ascii_case("CrossDesk.exe") {
                let mut session = 0;
                unsafe { ProcessIdToSessionId(entry.th32ProcessID, &mut session) }?;
                if session == our_session && candidate.replace(entry.th32ProcessID).is_some() {
                    return Err(windows::core::Error::from_hresult(
                        windows::Win32::Foundation::E_FAIL,
                    ));
                }
            }
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                return Ok(candidate);
            }
        }
    })();
    let _ = unsafe { CloseHandle(snapshot) };
    result
}

fn process_identity(process_id: u32) -> Option<(PathBuf, String)> {
    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;
    let result = (|| {
        let mut image = vec![0u16; 32_768];
        let mut len = image.len() as u32;
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(image.as_mut_ptr()),
                &mut len,
            )
            .ok()?;
            GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user).ok()?;
        }
        if exited.dwHighDateTime != 0 || exited.dwLowDateTime != 0 {
            return None;
        }
        let mut utc = SYSTEMTIME::default();
        let mut local = SYSTEMTIME::default();
        unsafe {
            FileTimeToSystemTime(&created, &mut utc).ok()?;
            SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
        }
        use std::os::windows::ffi::OsStringExt;
        let executable = PathBuf::from(std::ffi::OsString::from_wide(&image[..len as usize]));
        let stamp = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            local.wYear,
            local.wMonth,
            local.wDay,
            local.wHour,
            local.wMinute,
            local.wSecond,
            local.wMilliseconds
        );
        Some((executable, stamp))
    })();
    let _ = unsafe { CloseHandle(process) };
    result
}

#[derive(Clone)]
struct LogReport {
    stamp: String,
    activity: CrossDeskActivity,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    volume: u32,
    index: u64,
}

struct LogFile {
    file: File,
    id: FileIdentity,
    len: u64,
}

impl LogFile {
    fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }?;
        Ok(Self {
            file,
            id: FileIdentity {
                volume: info.dwVolumeSerialNumber,
                index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            },
            len: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        })
    }

    fn scan(
        &mut self,
        start: u64,
        skip_partial: bool,
        started_at: &str,
    ) -> Option<(u64, Option<LogReport>)> {
        let len = usize::try_from(self.len.checked_sub(start)?).ok()?;
        if len > LOG_TAIL_BYTES as usize {
            return None;
        }
        let mut bytes = vec![0; len];
        self.file.seek(SeekFrom::Start(start)).ok()?;
        self.file.read_exact(&mut bytes).ok()?;
        // A writer can finish an incomplete last line before the next sample.
        let complete = bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |index| index + 1);
        let mut lines = bytes[..complete].split_inclusive(|&byte| byte == b'\n');
        if skip_partial {
            lines.next();
        }
        let latest = lines
            .filter_map(|line| report_line(line, started_at))
            .next_back()
            .map(|(stamp, activity)| LogReport {
                stamp: stamp.to_string(),
                activity,
            });
        Some((start + complete as u64, latest))
    }

    fn checkpoint(&mut self, offset: u64) -> Option<Vec<u8>> {
        if offset > self.len {
            return None;
        }
        let start = offset.saturating_sub(64);
        let mut bytes = vec![0; (offset - start) as usize];
        self.file.seek(SeekFrom::Start(start)).ok()?;
        self.file.read_exact(&mut bytes).ok()?;
        Some(bytes)
    }

    fn continue_from(
        &mut self,
        cursor: &LogCursor,
        started_at: &str,
    ) -> Option<(u64, Option<LogReport>)> {
        if self.id != cursor.id || self.checkpoint(cursor.offset)? != cursor.checkpoint {
            return None;
        }
        self.scan(cursor.offset, false, started_at)
    }
}

struct LogCursor {
    id: FileIdentity,
    offset: u64,
    checkpoint: Vec<u8>,
    report: Option<LogReport>,
}

impl LogCursor {
    fn record(log: &mut LogFile, offset: u64, report: Option<LogReport>) -> Option<Self> {
        Some(Self {
            id: log.id,
            offset,
            checkpoint: log.checkpoint(offset)?,
            report,
        })
    }
}

struct LogTail {
    directory: PathBuf,
    cursor: Option<LogCursor>,
}

impl LogTail {
    fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            cursor: None,
        }
    }

    fn activity(&mut self, started_at: &str) -> Option<LogReport> {
        let mut log = LogFile::open(&self.directory.join(LOG_FILES[0])).ok()?;
        if let Some(cursor) = &mut self.cursor {
            if log.id == cursor.id {
                if let Some((offset, report)) = log.continue_from(cursor, started_at)
                    && let Some(updated) = LogCursor::record(
                        &mut log,
                        offset,
                        report.or_else(|| cursor.report.clone()),
                    )
                {
                    *cursor = updated;
                    return cursor.report.clone();
                }
            } else if let Ok(mut rotated) = LogFile::open(&self.directory.join(LOG_FILES[1]))
                && let Some((_, previous)) = rotated.continue_from(cursor, started_at)
                && let Some((offset, latest)) = log.scan(0, false, started_at)
                && let Some(updated) = LogCursor::record(
                    &mut log,
                    offset,
                    latest.or(previous).or_else(|| cursor.report.clone()),
                )
            {
                *cursor = updated;
                return cursor.report.clone();
            }
        }

        self.cursor = None;
        let start = log.len.saturating_sub(LOG_TAIL_BYTES);
        let (offset, mut report) = log.scan(start, start != 0, started_at)?;
        if report.is_none() && start == 0 {
            for name in &LOG_FILES[1..] {
                let Ok(mut rotated) = LogFile::open(&self.directory.join(name)) else {
                    break;
                };
                let skip = rotated.len.saturating_sub(LOG_TAIL_BYTES);
                let Some((_, reading)) = rotated.scan(skip, skip != 0, started_at) else {
                    break;
                };
                report = reading;
                // A skipped prefix could contain a newer transition than the older files.
                if report.is_some() || skip != 0 {
                    break;
                }
            }
        }
        self.cursor = LogCursor::record(&mut log, offset, report.clone());
        report
    }
}

fn report_line<'a>(line: &'a [u8], started_at: &str) -> Option<(&'a str, CrossDeskActivity)> {
    let line = std::str::from_utf8(line.strip_suffix(b"\n")?)
        .ok()?
        .trim_end_matches('\r');
    let (stamp, message) = line
        .strip_prefix('[')?
        .split_once("] [crossdesk] [info] ")?;
    if stamp.len() != 23 || stamp < started_at {
        return None;
    }
    let activity = match message {
        SHARING_STARTED => CrossDeskActivity::Active,
        SHARING_STOPPED | "Logger initialized" => CrossDeskActivity::Inactive,
        _ => return None,
    };
    Some((stamp, activity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn preserves_log_evidence_across_append_and_reads_stop_in_rotated_file() {
        static ID: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "snug-crossdesk-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&dir).unwrap();
        let active = dir.join(LOG_FILES[0]);
        let rotated = dir.join(LOG_FILES[1]);
        let start = "2026-11-24 13:00:02.033";
        let log = |stamp: &str, message: &str| format!("[{stamp}] [crossdesk] [info] {message}\n");
        fs::write(&active, log(start, SHARING_STARTED)).unwrap();
        let mut tail = LogTail::new(dir.clone());
        let activity = |reader: &mut LogTail| reader.activity(start).map(|report| report.activity);
        assert_eq!(activity(&mut tail), Some(CrossDeskActivity::Active));
        for _ in 0..3 {
            OpenOptions::new()
                .append(true)
                .open(&active)
                .unwrap()
                .write_all(b"another log message\n".repeat(3200).as_slice())
                .unwrap();
            assert_eq!(activity(&mut tail), Some(CrossDeskActivity::Active));
        }
        assert!(fs::metadata(&active).unwrap().len() > LOG_TAIL_BYTES);
        OpenOptions::new()
            .append(true)
            .open(&active)
            .unwrap()
            .write_all(log("2026-11-24 13:02:02.038", SHARING_STOPPED).as_bytes())
            .unwrap();
        fs::rename(&active, &rotated).unwrap();
        fs::write(&active, "new log file\n").unwrap();
        assert_eq!(activity(&mut tail), Some(CrossDeskActivity::Inactive));

        fs::remove_file(active).unwrap();
        fs::remove_file(rotated).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn recognizes_only_complete_lifecycle_lines_in_this_process_generation() {
        let start = "2026-11-24 13:00:02.033";
        let report = |stamp: &str, message: &str| {
            let line = format!("[{stamp}] [crossdesk] [info] {message}\r\n");
            report_line(line.as_bytes(), start).map(|(_, activity)| activity)
        };
        assert_eq!(report("2026-11-24 13:00:02.032", SHARING_STARTED), None);
        assert_eq!(
            report(start, "Logger initialized"),
            Some(CrossDeskActivity::Inactive)
        );
        assert_eq!(
            report(start, SHARING_STARTED),
            Some(CrossDeskActivity::Active)
        );
        assert_eq!(
            report(start, SHARING_STOPPED),
            Some(CrossDeskActivity::Inactive)
        );
        assert_eq!(report(start, "Start screen capturer"), None);
        let partial = format!("[{start}] [crossdesk] [info] {SHARING_STARTED}");
        assert_eq!(report_line(partial.as_bytes(), start), None);
    }
}
