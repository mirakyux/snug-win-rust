//! Narrow elevated VDD broker. The normal GUI owns the server and authenticates the
//! exact elevated child; the child authenticates the server PID and executable before
//! accepting typed monitor operations. No paths, shell commands or arbitrary driver frames cross IPC.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::vdd::{DEFAULT_PIPE_NAME, VddClient, VddMonitor, VddProtocol};

const MAX_FRAME: usize = 128 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(15);
const DRIVER_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", content = "arguments", deny_unknown_fields)]
pub(crate) enum Request {
    Notify {
        monitors: Vec<VddMonitor>,
    },
    ReplaceLegacy {
        remove_ids: Vec<u32>,
        monitors: Vec<VddMonitor>,
    },
    Remove {
        ids: Vec<u32>,
    },
    State,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reply {
    pub legacy: Option<bool>,
    pub monitors: Option<Vec<VddMonitor>>,
    error: Option<String>,
}

impl Reply {
    fn failure(error: impl std::fmt::Display) -> Self {
        Self {
            legacy: None,
            monitors: None,
            error: Some(error.to_string().chars().take(1024).collect()),
        }
    }
}

fn validate_ids(ids: &[u32]) -> io::Result<()> {
    if ids.len() > 4 || ids.iter().any(|id| *id > 3) {
        return Err(invalid(
            "VDD operations only accept at most four IDs in 0..=3",
        ));
    }
    let mut unique = ids.to_vec();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() != ids.len() {
        return Err(invalid("duplicate monitor IDs"));
    }
    Ok(())
}

impl Request {
    fn validate(&self) -> io::Result<()> {
        let monitors = match self {
            Self::Notify { monitors } => monitors,
            Self::ReplaceLegacy {
                remove_ids,
                monitors,
            } => {
                validate_ids(remove_ids)?;
                monitors
            }
            Self::Remove { ids } => return validate_ids(ids),
            Self::State => return Ok(()),
        };
        validate_ids(
            &monitors
                .iter()
                .map(|monitor| monitor.id)
                .collect::<Vec<_>>(),
        )?;
        for monitor in monitors {
            if monitor
                .name
                .as_ref()
                .is_some_and(|name| name.len() > 256 || name.contains('\0'))
                || monitor.modes.is_empty()
                || monitor.modes.len() > 256
            {
                return Err(invalid("invalid monitor name or mode count"));
            }
            for mode in &monitor.modes {
                if !(1..=16384).contains(&mode.width)
                    || !(1..=16384).contains(&mode.height)
                    || mode.refresh_rates.is_empty()
                    || mode.refresh_rates.len() > 4
                    || mode
                        .refresh_rates
                        .iter()
                        .any(|rate| !(1..=240).contains(rate))
                {
                    return Err(invalid("invalid monitor mode"));
                }
            }
        }
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "VDD broker operation timed out")
}

/// A failed/cancelled launch is remembered for this GUI lifetime to avoid repeated UAC prompts.
enum Session {
    Unstarted,
    Connected(Broker),
    Failed(String),
}

static SESSION: OnceLock<Mutex<Session>> = OnceLock::new();

pub(crate) fn request(request: Request) -> io::Result<Reply> {
    request.validate()?;
    let mut session = SESSION
        .get_or_init(|| Mutex::new(Session::Unstarted))
        .lock()
        .map_err(|_| io::Error::other("VDD broker session lock poisoned"))?;
    if matches!(*session, Session::Unstarted) {
        *session = match Broker::start() {
            Ok(broker) => Session::Connected(broker),
            Err(error) => Session::Failed(error.to_string()),
        };
    }
    let result = match &mut *session {
        Session::Connected(broker) => broker.request(&request),
        Session::Failed(message) => return Err(io::Error::other(message.clone())),
        Session::Unstarted => unreachable!(),
    };
    if let Err(error) = &result {
        *session = Session::Failed(error.to_string());
    }
    let reply = result?;
    match &reply.error {
        Some(error) => Err(io::Error::other(error.clone())),
        None => Ok(reply),
    }
}

struct Broker {
    pipe: File,
    process: OwnedHandle,
}

impl Broker {
    fn start() -> io::Result<Self> {
        let parent = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let name = format!("snug-vdd-{parent}-{nonce:032x}");
        let mut pipe = create_server(&name)?;
        let process = crate::elevation::launch_elevated_current_exe(&[
            "--snug-helper-vdd".into(),
            parent.to_string(),
            name,
        ])?;
        let expected = process_id(&process)?;
        accept_verified(&mut pipe, expected, &process)?;
        Ok(Self { pipe, process })
    }

    fn request(&mut self, request: &Request) -> io::Result<Reply> {
        let deadline = Instant::now() + IO_TIMEOUT;
        let payload = serde_json::to_vec(request).map_err(io::Error::other)?;
        send_frame(&mut self.pipe, &payload, deadline, &self.process)?;
        let payload = receive_frame(&mut self.pipe, Some(deadline), &self.process)?;
        serde_json::from_slice(&payload).map_err(io::Error::other)
    }
}

fn execute(request: Request, driver: &mut VddClient) -> Reply {
    if let Err(error) = request.validate() {
        return Reply::failure(error);
    }
    let outcome = match request {
        Request::Notify { monitors } => driver.notify(&monitors).map(|()| None),
        Request::ReplaceLegacy {
            remove_ids,
            monitors,
        } => driver.replace_legacy(&remove_ids, &monitors).map(|()| None),
        Request::Remove { ids } => driver.remove(&ids).map(|()| None),
        Request::State => driver.state(),
    };
    match outcome {
        Ok(monitors) => Reply {
            legacy: driver
                .protocol()
                .map(|protocol| protocol == VddProtocol::Legacy),
            monitors,
            error: None,
        },
        Err(error) => Reply::failure(error),
    }
}

/// Run only from the fixed privileged dispatcher, before starting any GUI or installing drivers.
/// One driver worker is reused. A hung driver call cannot keep this process alive after its parent exits.
pub fn serve(parent_pid: u32, pipe_name: &str) -> io::Result<()> {
    match serve_inner(parent_pid, pipe_name) {
        // Closing the GUI or its authenticated channel is normal helper shutdown.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected
            ) =>
        {
            Ok(())
        }
        other => other,
    }
}

fn serve_inner(parent_pid: u32, pipe_name: &str) -> io::Result<()> {
    if !crate::elevation::is_elevated() || parent_pid == std::process::id() {
        return Err(denied(
            "VDD broker requires elevation and a separate GUI parent",
        ));
    }
    validate_pipe_name(pipe_name, parent_pid)?;
    let parent = open_process(parent_pid)?;
    if !crate::elevation::is_same_executable_process(parent_pid)? {
        return Err(denied("VDD broker parent is not this executable"));
    }
    let mut pipe = connect_client(pipe_name, parent_pid, &parent)?;
    let (requests_tx, requests_rx) = mpsc::sync_channel(1);
    let (replies_tx, replies_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("snug-vdd-driver".into())
        .spawn(move || {
            let mut driver = VddClient::direct(DEFAULT_PIPE_NAME);
            while let Ok(request) = requests_rx.recv() {
                if replies_tx.send(execute(request, &mut driver)).is_err() {
                    break;
                }
            }
        })?;
    loop {
        let payload = match receive_frame(&mut pipe, None, &parent) {
            Ok(payload) => payload,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let reply = match serde_json::from_slice::<Request>(&payload) {
            Ok(request) => match request.validate() {
                Err(error) => Reply::failure(error),
                Ok(()) => {
                    requests_tx.send(request).map_err(io::Error::other)?;
                    let deadline = Instant::now() + DRIVER_TIMEOUT;
                    loop {
                        check_wait(Some(deadline), &parent)?;
                        match replies_rx.try_recv() {
                            Ok(reply) => break reply,
                            Err(mpsc::TryRecvError::Empty) => std::thread::sleep(POLL),
                            Err(mpsc::TryRecvError::Disconnected) => {
                                return Err(io::Error::other("VDD worker stopped"));
                            }
                        }
                    }
                }
            },
            Err(error) => Reply::failure(error),
        };
        let payload = serde_json::to_vec(&reply).map_err(io::Error::other)?;
        send_frame(&mut pipe, &payload, Instant::now() + IO_TIMEOUT, &parent)?;
    }
}

fn validate_pipe_name(name: &str, parent: u32) -> io::Result<()> {
    let prefix = format!("snug-vdd-{parent}-");
    if !name.strip_prefix(&prefix).is_some_and(|nonce| {
        nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(invalid("invalid VDD broker pipe name"));
    }
    Ok(())
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn create_server(name: &str) -> io::Result<File> {
    validate_pipe_name(name, std::process::id())?;
    let mut descriptor = std::ptr::null_mut();
    let sddl = wide("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)");
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let attributes = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor,
        inherit: 0,
    };
    let path = wide(&format!(r"\\.\pipe\{name}"));
    // Duplex + first-instance; byte mode + nonblocking + reject remote clients. One local instance only.
    let raw = unsafe {
        CreateNamedPipeW(
            path.as_ptr(),
            3 | 0x0008_0000,
            1 | 8,
            1,
            MAX_FRAME as u32,
            MAX_FRAME as u32,
            0,
            (&attributes as *const SecurityAttributes).cast_mut().cast(),
        )
    };
    let error = io::Error::last_os_error();
    unsafe { LocalFree(descriptor) };
    if raw as isize == -1 {
        return Err(error);
    }
    Ok(unsafe { File::from_raw_handle(raw) })
}

fn accept_verified(pipe: &mut File, expected_pid: u32, process: &OwnedHandle) -> io::Result<()> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        check_wait(Some(deadline), process)?;
        let connected =
            unsafe { ConnectNamedPipe(pipe.as_raw_handle(), std::ptr::null_mut()) } != 0;
        let error = io::Error::last_os_error();
        if connected || error.raw_os_error() == Some(535) {
            let mut actual = 0;
            if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut actual) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if verify_peer(actual, expected_pid).is_ok() {
                return Ok(());
            }
            unsafe { DisconnectNamedPipe(pipe.as_raw_handle()) };
        } else if !matches!(error.raw_os_error(), Some(536) | Some(232)) {
            return Err(error);
        }
        std::thread::sleep(POLL);
    }
}

fn verify_peer(actual: u32, expected: u32) -> io::Result<()> {
    if actual != expected || !crate::elevation::is_same_executable_process(actual)? {
        return Err(denied("VDD broker peer identity does not match"));
    }
    Ok(())
}

fn connect_client(name: &str, expected_parent: u32, parent: &OwnedHandle) -> io::Result<File> {
    validate_pipe_name(name, expected_parent)?;
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let path = format!(r"\\.\pipe\{name}");
    loop {
        check_wait(Some(deadline), parent)?;
        // Identification, not impersonation: the medium server must not impersonate its high client.
        let opened = OpenOptions::new()
            .read(true)
            .write(true)
            .security_qos_flags(0x0001_0000)
            .open(&path);
        match opened {
            Ok(pipe) => {
                let mut actual = 0;
                if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut actual) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                verify_peer(actual, expected_parent)?;
                let mode = 1u32; // PIPE_NOWAIT: all IPC reads/writes remain bounded and parent-aware.
                if unsafe {
                    SetNamedPipeHandleState(
                        pipe.as_raw_handle(),
                        &mode,
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                } == 0
                {
                    return Err(io::Error::last_os_error());
                }
                return Ok(pipe);
            }
            Err(error) if matches!(error.raw_os_error(), Some(2) | Some(231)) => {
                std::thread::sleep(POLL)
            }
            Err(error) => return Err(error),
        }
    }
}

fn open_process(pid: u32) -> io::Result<OwnedHandle> {
    let raw = unsafe { OpenProcess(0x0010_1000, 0, pid) }; // SYNCHRONIZE | QUERY_LIMITED_INFORMATION
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

fn process_id(handle: &OwnedHandle) -> io::Result<u32> {
    let pid = unsafe { GetProcessId(handle.as_raw_handle()) };
    if pid == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(pid)
    }
}

fn check_wait(deadline: Option<Instant>, process: &OwnedHandle) -> io::Result<()> {
    match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
        258 => {}
        0 => {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "VDD broker peer exited",
            ));
        }
        _ => return Err(io::Error::last_os_error()),
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(timed_out());
    }
    Ok(())
}

fn send_frame(
    pipe: &mut File,
    payload: &[u8],
    deadline: Instant,
    peer: &OwnedHandle,
) -> io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(invalid("VDD broker frame is too large"));
    }
    let mut framed = (payload.len() as u32).to_le_bytes().to_vec();
    framed.extend_from_slice(payload);
    let mut offset = 0;
    while offset < framed.len() {
        check_wait(Some(deadline), peer)?;
        match pipe.write(&framed[offset..]) {
            Ok(0) => std::thread::sleep(POLL),
            Ok(written) => offset += written,
            Err(error)
                if error.raw_os_error() == Some(232)
                    || error.kind() == io::ErrorKind::WouldBlock =>
            {
                std::thread::sleep(POLL)
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn receive_frame(
    pipe: &mut File,
    deadline: Option<Instant>,
    peer: &OwnedHandle,
) -> io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    read_exact_poll(pipe, &mut header, deadline, peer)?;
    let length = u32::from_le_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(invalid("invalid VDD broker frame length"));
    }
    let mut payload = vec![0; length];
    // Once a frame starts, a stalled sender cannot hold the elevated helper forever.
    read_exact_poll(
        pipe,
        &mut payload,
        deadline.or_else(|| Some(Instant::now() + IO_TIMEOUT)),
        peer,
    )?;
    Ok(payload)
}

fn read_exact_poll(
    pipe: &mut File,
    bytes: &mut [u8],
    deadline: Option<Instant>,
    peer: &OwnedHandle,
) -> io::Result<()> {
    let mut offset = 0;
    let mut deadline = deadline;
    while offset < bytes.len() {
        check_wait(deadline, peer)?;
        let mut available = 0u32;
        if unsafe {
            PeekNamedPipe(
                pipe.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if available == 0 {
            std::thread::sleep(POLL);
            continue;
        }
        match pipe.read(&mut bytes[offset..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "VDD broker pipe closed",
                ));
            }
            Ok(read) => {
                deadline.get_or_insert_with(|| Instant::now() + IO_TIMEOUT);
                offset += read;
            }
            Err(error)
                if error.raw_os_error() == Some(232)
                    || error.kind() == io::ErrorKind::WouldBlock =>
            {
                std::thread::sleep(POLL)
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut core::ffi::c_void,
    inherit: i32,
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sddl: *const u16,
        revision: u32,
        descriptor: *mut *mut core::ffi::c_void,
        size: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        maximum: u32,
        out_size: u32,
        in_size: u32,
        timeout: u32,
        attributes: *mut core::ffi::c_void,
    ) -> RawHandle;
    fn ConnectNamedPipe(pipe: RawHandle, overlapped: *mut core::ffi::c_void) -> i32;
    fn DisconnectNamedPipe(pipe: RawHandle) -> i32;
    fn GetNamedPipeClientProcessId(pipe: RawHandle, pid: *mut u32) -> i32;
    fn GetNamedPipeServerProcessId(pipe: RawHandle, pid: *mut u32) -> i32;
    fn SetNamedPipeHandleState(
        pipe: RawHandle,
        mode: *const u32,
        maximum: *const u32,
        timeout: *const u32,
    ) -> i32;
    fn PeekNamedPipe(
        pipe: RawHandle,
        buffer: *mut core::ffi::c_void,
        size: u32,
        read: *mut u32,
        available: *mut u32,
        left: *mut u32,
    ) -> i32;
    fn GetProcessId(process: RawHandle) -> u32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> RawHandle;
    fn WaitForSingleObject(handle: RawHandle, milliseconds: u32) -> u32;
    fn LocalFree(memory: *mut core::ffi::c_void) -> RawHandle;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vdd::VddMode;

    fn monitor() -> VddMonitor {
        VddMonitor {
            id: 0,
            name: Some("Snug".into()),
            enabled: true,
            modes: vec![VddMode {
                width: 1080,
                height: 1920,
                refresh_rates: vec![60],
            }],
        }
    }

    #[test]
    fn broker_rejects_unbounded_requests_before_driver_access() {
        assert!(
            Request::Notify {
                monitors: vec![monitor()]
            }
            .validate()
            .is_ok()
        );
        let mut oversized = monitor();
        oversized.modes[0].height = 16385;
        assert!(
            Request::Notify {
                monitors: vec![oversized]
            }
            .validate()
            .is_err()
        );
        let mut invalid_rate = monitor();
        invalid_rate.modes[0].refresh_rates = vec![0];
        assert!(
            Request::Notify {
                monitors: vec![invalid_rate]
            }
            .validate()
            .is_err()
        );
        assert!(Request::Remove { ids: vec![0, 0] }.validate().is_err());
        assert!(Request::Remove { ids: vec![4] }.validate().is_err());
        assert!(
            Request::Notify {
                monitors: vec![monitor(); 5]
            }
            .validate()
            .is_err()
        );
        assert!(
            serde_json::from_str::<Request>(
                r#"{"command":"Shell","arguments":{"path":"anything"}}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Request>(r#"{"command":"State","unexpected":true}"#).is_err()
        );
    }

    #[test]
    fn broker_accepts_production_mode_tables_and_rejects_wrong_peer() {
        let mut monitor = monitor();
        monitor.modes = crate::display::build_modes(&snug_core::ScreenFit::standard());
        assert!(monitor.modes.len() > 4);
        assert!(
            Request::Notify {
                monitors: vec![monitor]
            }
            .validate()
            .is_ok()
        );
        assert!(verify_peer(std::process::id(), std::process::id()).is_ok());
        assert!(verify_peer(std::process::id(), std::process::id().wrapping_add(1)).is_err());
        assert!(validate_pipe_name("other-pipe", std::process::id()).is_err());
    }

    #[test]
    fn broker_waits_are_bounded_and_notice_peer_exit() {
        use std::os::windows::io::AsHandle;
        use std::os::windows::process::CommandExt;
        let running = open_process(std::process::id()).unwrap();
        assert_eq!(
            check_wait(Some(Instant::now()), &running)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        let command = crate::system_paths::system_tool("cmd.exe").unwrap();
        let mut child = std::process::Command::new(command)
            .args(["/D", "/C", "exit 0"])
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let exited = child.as_handle().try_clone_to_owned().unwrap();
        child.wait().unwrap();
        assert_eq!(
            check_wait(None, &exited).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn broker_transport_uses_a_single_authenticated_bounded_pipe_without_elevation() {
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("snug-vdd-{pid}-{nonce:032x}");
        let mut server = create_server(&name).unwrap();
        assert!(
            create_server(&name).is_err(),
            "FIRST_PIPE_INSTANCE must refuse name reuse"
        );
        let client = std::thread::spawn(move || {
            let parent = open_process(pid).unwrap();
            let mut pipe = connect_client(&name, pid, &parent).unwrap();
            let bytes =
                receive_frame(&mut pipe, Some(Instant::now() + IO_TIMEOUT), &parent).unwrap();
            let request: Request = serde_json::from_slice(&bytes).unwrap();
            assert!(matches!(request, Request::State));
            let reply = Reply {
                legacy: Some(true),
                monitors: None,
                error: None,
            };
            send_frame(
                &mut pipe,
                &serde_json::to_vec(&reply).unwrap(),
                Instant::now() + IO_TIMEOUT,
                &parent,
            )
            .unwrap();
        });
        let peer = open_process(pid).unwrap();
        accept_verified(&mut server, pid, &peer).unwrap();
        send_frame(
            &mut server,
            &serde_json::to_vec(&Request::State).unwrap(),
            Instant::now() + IO_TIMEOUT,
            &peer,
        )
        .unwrap();
        let bytes = receive_frame(&mut server, Some(Instant::now() + IO_TIMEOUT), &peer).unwrap();
        assert_eq!(
            serde_json::from_slice::<Reply>(&bytes).unwrap().legacy,
            Some(true)
        );
        client.join().unwrap();
    }
}
