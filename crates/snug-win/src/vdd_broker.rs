//! Narrow elevated VDD broker. The normal GUI owns the server and authenticates the
//! exact elevated child; the child authenticates the server PID and executable before
//! accepting typed monitor operations. No paths, shell commands or arbitrary driver frames cross IPC.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER, TokenElevation,
    TokenSessionId, TokenUser,
};
use windows::Win32::System::Threading::{
    OpenProcessToken, PROCESS_NAME_FORMAT, QueryFullProcessImageNameW,
};
use windows::core::{PCWSTR, PWSTR};

use crate::vdd::{DEFAULT_PIPE_NAME, RemoveDisposition, VddClient, VddError, VddMonitor, VddProtocol};

const MAX_REQUEST_FRAME: usize = 128 * 1024;
const MAX_RESPONSE_FRAME: usize = crate::vdd::MAX_STATE_RESPONSE_BYTES;
const IO_TIMEOUT: Duration = Duration::from_secs(15);
const DRIVER_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const TASK_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", content = "arguments", deny_unknown_fields)]
pub(crate) enum Request {
    #[serde(rename = "AtomicNotify")]
    Notify {
        monitors: Vec<VddMonitor>,
    },
    // These names also gate enrolled helpers that predate tokenless V2 guards.
    #[serde(rename = "LegacyOnlyReplace")]
    ReplaceLegacy {
        remove_ids: Vec<u32>,
        monitors: Vec<VddMonitor>,
    },
    #[serde(rename = "LegacyOnlyRemove")]
    Remove {
        ids: Vec<u32>,
    },
    #[serde(rename = "OwnedReserve")]
    ReserveOwned {
        id: u32,
        owner: String,
    },
    VerifyOwner {
        id: u32,
        owner: Option<String>,
    },
    #[serde(rename = "OwnedCreate")]
    CreateOwned {
        monitors: Vec<VddMonitor>,
        owner: String,
    },
    #[serde(rename = "OwnedReplace")]
    ReplaceOwned {
        monitors: Vec<VddMonitor>,
        owner: String,
    },
    #[serde(rename = "OwnedRelease")]
    RemoveOwned {
        ids: Vec<u32>,
        owner: String,
    },
    State,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reply {
    pub legacy: Option<bool>,
    pub monitors: Option<Vec<VddMonitor>>,
    pub removal: Option<RemoveDisposition>,
    error: Option<String>,
    #[serde(default)]
    unsupported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outcome_unknown: Option<BrokerErrorKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_kind: Option<BrokerErrorKind>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum BrokerErrorKind {
    AlreadyExists,
    NotFound,
    PermissionDenied,
    Unsupported,
    InvalidInput,
    NotConnected,
    BrokenPipe,
    TimedOut,
    InvalidData,
    Other,
}

impl BrokerErrorKind {
    fn from_io(kind: io::ErrorKind) -> Self {
        match kind {
            io::ErrorKind::AlreadyExists => Self::AlreadyExists,
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::Unsupported => Self::Unsupported,
            io::ErrorKind::InvalidInput => Self::InvalidInput,
            io::ErrorKind::NotConnected => Self::NotConnected,
            io::ErrorKind::BrokenPipe => Self::BrokenPipe,
            io::ErrorKind::TimedOut => Self::TimedOut,
            io::ErrorKind::InvalidData => Self::InvalidData,
            _ => Self::Other,
        }
    }

    fn into_io(self) -> io::ErrorKind {
        match self {
            Self::AlreadyExists => io::ErrorKind::AlreadyExists,
            Self::NotFound => io::ErrorKind::NotFound,
            Self::PermissionDenied => io::ErrorKind::PermissionDenied,
            Self::Unsupported => io::ErrorKind::Unsupported,
            Self::InvalidInput => io::ErrorKind::InvalidInput,
            Self::NotConnected => io::ErrorKind::NotConnected,
            Self::BrokenPipe => io::ErrorKind::BrokenPipe,
            Self::TimedOut => io::ErrorKind::TimedOut,
            Self::InvalidData => io::ErrorKind::InvalidData,
            Self::Other => io::ErrorKind::Other,
        }
    }
}

fn unknown_outcome(source: io::Error) -> io::Error {
    io::Error::new(source.kind(), VddError::OutcomeUnknown { source })
}

impl Reply {
    fn failure(error: impl std::fmt::Display) -> Self {
        Self {
            legacy: None,
            monitors: None,
            removal: None,
            error: Some(error.to_string().chars().take(1024).collect()),
            unsupported: false,
            outcome_unknown: None,
            error_kind: None,
        }
    }

    fn driver_failure(error: VddError) -> Self {
        let unsupported = matches!(&error, VddError::Io(error) if error.kind() == io::ErrorKind::Unsupported);
        let outcome_unknown = match &error {
            VddError::OutcomeUnknown { source } => Some(BrokerErrorKind::from_io(source.kind())),
            _ => None,
        };
        let error_kind = match &error {
            VddError::Io(source) | VddError::Connect { source, .. } => Some(BrokerErrorKind::from_io(source.kind())),
            VddError::NotConnected => Some(BrokerErrorKind::NotConnected),
            VddError::OutcomeUnknown { .. } => None,
        };
        Self { unsupported, outcome_unknown, error_kind, ..Self::failure(error) }
    }

    fn into_result(self) -> io::Result<Self> {
        match &self.error {
            Some(error) if self.outcome_unknown.is_some() => Err(unknown_outcome(io::Error::new(
                self.outcome_unknown.unwrap().into_io(), error.clone(),
            ))),
            Some(error) => Err(io::Error::new(
                self.error_kind.map(BrokerErrorKind::into_io).unwrap_or(
                    if self.unsupported { io::ErrorKind::Unsupported } else { io::ErrorKind::Other }
                ),
                error.clone(),
            )),
            None => Ok(self),
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
        let owner = match self {
            Self::ReserveOwned { owner, .. } | Self::CreateOwned { owner, .. } | Self::ReplaceOwned { owner, .. }
                | Self::RemoveOwned { owner, .. } => Some(owner),
            Self::VerifyOwner { owner, .. } => owner.as_ref(),
            _ => None,
        };
        if owner.is_some_and(|owner| owner.len() != 32 || !owner.bytes().all(|byte| byte.is_ascii_hexdigit())) {
            return Err(invalid("invalid monitor ownership token"));
        }
        let monitors = match self {
            Self::Notify { monitors } | Self::CreateOwned { monitors, .. } | Self::ReplaceOwned { monitors, .. } => monitors,
            Self::ReplaceLegacy {
                remove_ids,
                monitors,
            } => {
                validate_ids(remove_ids)?;
                monitors
            }
            Self::Remove { ids } | Self::RemoveOwned { ids, .. } => return validate_ids(ids),
            Self::ReserveOwned { id, .. } | Self::VerifyOwner { id, .. } => return validate_ids(&[*id]),
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

/// The connection can be opened by the startup or settings authorization path.
enum Session {
    Unstarted,
    Connected(Broker),
    Failed(BrokerFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerFailure {
    Cancelled,
    Launch(String),
    Disconnected(String),
}

impl std::fmt::Display for BrokerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("已取消驱动权限请求"),
            Self::Launch(error) => write!(f, "驱动权限代理启动失败：{error}"),
            Self::Disconnected(error) => write!(f, "驱动权限代理已断开：{error}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerStatus {
    Idle,
    Busy,
    Connected,
    Failed(BrokerFailure),
}

static SESSION: OnceLock<Mutex<Session>> = OnceLock::new();

pub fn status() -> BrokerStatus {
    let Some(session) = SESSION.get() else {
        return BrokerStatus::Idle;
    };
    match session.try_lock().as_deref() {
        Ok(Session::Unstarted) => BrokerStatus::Idle,
        Ok(Session::Connected(_)) => BrokerStatus::Connected,
        Ok(Session::Failed(failure)) => BrokerStatus::Failed(failure.clone()),
        Err(std::sync::TryLockError::WouldBlock) => BrokerStatus::Busy,
        Err(std::sync::TryLockError::Poisoned(_)) => {
            BrokerStatus::Failed(BrokerFailure::Disconnected("状态锁不可用".into()))
        }
    }
}

pub(crate) fn request(request: Request) -> io::Result<Reply> {
    request.validate()?;
    let session = SESSION.get().ok_or_else(authorization_required)?;
    let mut session = try_session(session)?;
    exchange_request(&mut session, &request)
}

/// Authorize or reuse the broker, then probe the driver without changing any displays.
pub fn retry_connection() -> io::Result<Option<VddProtocol>> {
    retry_connection_with(|| {
        launch_preferred(
            crate::vdd_task::is_enabled(),
            Broker::start_scheduled,
            Broker::start,
        )
    })
}

pub(crate) fn retry_connection_without_prompt() -> io::Result<Option<VddProtocol>> {
    retry_connection_with(|| launch_for_login(crate::vdd_task::is_enabled(), Broker::start_scheduled))
}

fn retry_connection_with(start: impl FnOnce() -> io::Result<Broker>) -> io::Result<Option<VddProtocol>> {
    let session = SESSION.get_or_init(|| Mutex::new(Session::Unstarted));
    let mut session = try_session(session)?;
    // Only an explicit connection attempt may replace a stale helper. Ordinary
    // requests still fail without relaunching or replaying a possible mutation.
    if matches!(&*session, Session::Connected(broker) if check_wait(None, &broker.process).is_err()) {
        *session = Session::Unstarted;
    }
    authorize(&mut session, start);
    let reply = exchange_request(&mut session, &Request::State)?;
    Ok(reply.legacy.map(|legacy| {
        if legacy {
            VddProtocol::Legacy
        } else {
            VddProtocol::V2
        }
    }))
}

fn launch_for_login<T>(
    task_enabled: io::Result<bool>,
    scheduled: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    launch_preferred(task_enabled, scheduled, || {
        Err(denied("登录自启尚未获得驱动授权，请在桌面上手动打开 Snug 完成授权"))
    })
}

fn launch_preferred<T>(
    task_enabled: io::Result<bool>,
    scheduled: impl FnOnce() -> io::Result<T>,
    interactive: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    match task_enabled {
        Ok(true) => scheduled().map_err(|error| {
            io::Error::new(error.kind(), format!("驱动授权计划任务连接失败：{error}"))
        }),
        Ok(false) => interactive(),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("读取驱动授权计划任务失败：{error}"),
        )),
    }
}

fn launch_failure(error: &io::Error) -> BrokerFailure {
    const ERROR_CANCELLED_HRESULT: i32 = 0x8007_04c7u32 as i32;
    if error.raw_os_error() == Some(1223)
        || error
            .get_ref()
            .and_then(|source| source.downcast_ref::<windows::core::Error>())
            .is_some_and(|source| source.code().0 == ERROR_CANCELLED_HRESULT)
    {
        BrokerFailure::Cancelled
    } else {
        BrokerFailure::Launch(error.to_string())
    }
}

fn authorization_required() -> io::Error {
    denied("驱动需要管理员授权，请在设置的「驱动与日志」中重新连接")
}

fn try_session(session: &Mutex<Session>) -> io::Result<std::sync::MutexGuard<'_, Session>> {
    session.try_lock().map_err(|error| match error {
        std::sync::TryLockError::WouldBlock => {
            io::Error::new(io::ErrorKind::WouldBlock, "驱动权限连接忙，请稍后重试")
        }
        std::sync::TryLockError::Poisoned(_) => io::Error::other("驱动权限连接状态不可用"),
    })
}

fn exchange_request(session: &mut Session, request: &Request) -> io::Result<Reply> {
    transact(session, |broker| broker.request(request))?.into_result()
}

fn authorize(session: &mut Session, start: impl FnOnce() -> io::Result<Broker>) {
    if !matches!(session, Session::Connected(_)) {
        *session = match start() {
            Ok(broker) => Session::Connected(broker),
            Err(error) => Session::Failed(launch_failure(&error)),
        };
    }
}

fn transact<T>(
    session: &mut Session,
    exchange: impl FnOnce(&mut Broker) -> io::Result<T>,
) -> io::Result<T> {
    let result = match session {
        Session::Connected(broker) => exchange(broker),
        Session::Failed(failure) => return Err(io::Error::other(failure.to_string())),
        Session::Unstarted => return Err(authorization_required()),
    };
    if let Err(error) = &result {
        *session = Session::Failed(BrokerFailure::Disconnected(error.to_string()));
    }
    result
}

struct Broker {
    pipe: File,
    process: OwnedHandle,
}

impl Broker {
    fn start() -> io::Result<Self> {
        let parent = std::process::id();
        let name = new_pipe_name(parent)?;
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

    fn start_scheduled() -> io::Result<Self> {
        let parent_pid = std::process::id();
        let parent = ProcessIdentity::open(parent_pid)?;
        let expected = crate::vdd_task::executable()?;
        let name = new_pipe_name(parent_pid)?;
        let pipe = create_server(&name)?;
        crate::vdd_task::launch(parent_pid, &name)?;
        let process = accept_peer(
            &pipe,
            TASK_CONNECT_TIMEOUT,
            |_| Ok(()),
            |pid| verify_scheduled_client(pid, &parent, &expected),
        )?;
        Ok(Self { pipe, process })
    }

    fn request(&mut self, request: &Request) -> io::Result<Reply> {
        let deadline = Instant::now() + IO_TIMEOUT;
        let payload = serde_json::to_vec(request).map_err(io::Error::other)?;
        send_frame(&mut self.pipe, &payload, deadline, &self.process, MAX_REQUEST_FRAME)?;
        let reply = receive_frame(&mut self.pipe, Some(deadline), &self.process, MAX_RESPONSE_FRAME)
            .and_then(|payload| serde_json::from_slice(&payload).map_err(io::Error::other));
        // The helper can mutate the driver once the full request was sent. A lost
        // response cannot establish success, rejection or ownership of any ID.
        reply.map_err(|error| match request {
            Request::State | Request::VerifyOwner { .. } => error,
            Request::ReserveOwned { .. } => io::Error::new(io::ErrorKind::Unsupported,
                format!("驱动权限代理未确认归属预留：{error}")),
            _ => unknown_outcome(error),
        })
    }
}

fn new_pipe_name(parent: u32) -> io::Result<String> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    Ok(format!("snug-vdd-{parent}-{nonce:032x}"))
}

fn verify_scheduled_client(
    pid: u32,
    parent: &ProcessIdentity,
    image: &Path,
) -> io::Result<OwnedHandle> {
    let client = ProcessIdentity::open(pid)?;
    if !client.elevated() || !client.same_user_session(parent) {
        return Err(denied(
            "VDD scheduled broker user, session or elevation does not match",
        ));
    }
    require_process_image(&client.process, image)?;
    check_wait(None, &client.process)?;
    Ok(client.process)
}

pub(crate) fn execute(request: Request, driver: &mut VddClient) -> Reply {
    if let Err(error) = request.validate() {
        return Reply::failure(error);
    }
    let outcome = match request {
        Request::Notify { monitors } => driver.notify(&monitors).map(|()| (None, None)),
        Request::ReplaceLegacy {
            remove_ids,
            monitors,
        } => driver
            .replace_legacy(&remove_ids, &monitors)
            .map(|()| (None, None)),
        Request::Remove { ids } => driver.remove(&ids).map(|removal| (None, Some(removal))),
        Request::ReserveOwned { id, owner } => driver.reserve_owned(id, &owner).map(|_| (None, None)),
        Request::VerifyOwner { id, owner } => driver.verify_owner(id, owner.as_deref()).map(|()| (None, None)),
        Request::CreateOwned { monitors, owner } => driver.create_owned(&monitors, &owner).map(|()| (None, None)),
        Request::ReplaceOwned { monitors, owner } => driver.replace_owned(&monitors, &owner).map(|()| (None, None)),
        Request::RemoveOwned { ids, owner } => driver.remove_owned(&ids, &owner).map(|removal| (None, Some(removal))),
        Request::State => driver.state().map(|monitors| (monitors, None)),
    };
    match outcome {
        Ok((monitors, removal)) => Reply {
            legacy: driver
                .protocol()
                .map(|protocol| protocol == VddProtocol::Legacy),
            monitors,
            removal,
            error: None,
            unsupported: false,
            outcome_unknown: None,
            error_kind: None,
        },
        Err(error) => Reply::driver_failure(error),
    }
}

/// Run only from the fixed privileged dispatcher, before starting any GUI or installing drivers.
/// One driver worker is reused. A hung driver call cannot keep this process alive after its parent exits.
pub fn serve(parent_pid: u32, pipe_name: &str) -> io::Result<()> {
    end_on_peer_close(serve_inner(parent_pid, pipe_name))
}

/// Serve the enrolled user's elevated VDD session from the protected scheduled executable.
pub fn serve_scheduled(parent_pid: u32, pipe_name: &str) -> io::Result<()> {
    if parent_pid == std::process::id() {
        return Err(denied(
            "VDD scheduled broker requires a separate GUI parent",
        ));
    }
    validate_pipe_name(pipe_name, parent_pid)?;
    let parent = ProcessIdentity::open(parent_pid)?;
    let self_identity = ProcessIdentity::open(std::process::id())?;
    if !self_identity.elevated() || !self_identity.same_user_session(&parent) {
        return Err(denied(
            "VDD scheduled broker parent user or session does not match",
        ));
    }
    let expected = crate::vdd_task::executable()?;
    require_process_image(&self_identity.process, &expected)?;
    end_on_peer_close(serve_connection(parent_pid, pipe_name, &parent.process))
}

fn end_on_peer_close(result: io::Result<()>) -> io::Result<()> {
    match result {
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
    require_same_executable(&parent)?;
    serve_connection(parent_pid, pipe_name, &parent)
}

fn serve_connection(parent_pid: u32, pipe_name: &str, parent: &OwnedHandle) -> io::Result<()> {
    let mut pipe = connect_client(pipe_name, parent_pid, parent)?;
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
        let payload = receive_frame(&mut pipe, None, parent, MAX_REQUEST_FRAME)?;
        let reply = match serde_json::from_slice::<Request>(&payload) {
            Ok(request) => match request.validate() {
                Err(error) => Reply::failure(error),
                Ok(()) => {
                    requests_tx.send(request).map_err(io::Error::other)?;
                    let deadline = Instant::now() + DRIVER_TIMEOUT;
                    loop {
                        check_wait(Some(deadline), parent)?;
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
        send_frame(&mut pipe, &payload, Instant::now() + IO_TIMEOUT, parent, MAX_RESPONSE_FRAME)?;
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
            MAX_REQUEST_FRAME as u32,
            MAX_REQUEST_FRAME as u32,
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
    require_same_executable(process)?;
    accept_peer(
        pipe,
        CONNECT_TIMEOUT,
        |deadline| check_wait(Some(deadline), process),
        |actual| verify_peer_pid(actual, expected_pid),
    )
}

fn accept_peer<T>(
    pipe: &File,
    timeout: Duration,
    wait: impl Fn(Instant) -> io::Result<()>,
    verify: impl Fn(u32) -> io::Result<T>,
) -> io::Result<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err(timed_out());
        }
        wait(deadline)?;
        let connected =
            unsafe { ConnectNamedPipe(pipe.as_raw_handle(), std::ptr::null_mut()) } != 0;
        let error = io::Error::last_os_error();
        if connected || error.raw_os_error() == Some(535) {
            let mut actual = 0;
            if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut actual) } != 0
                && let Ok(peer) = verify(actual)
            {
                return Ok(peer);
            }
            unsafe { DisconnectNamedPipe(pipe.as_raw_handle()) };
        } else {
            match error.raw_os_error() {
                Some(232) => {
                    unsafe { DisconnectNamedPipe(pipe.as_raw_handle()) };
                }
                Some(536) => {}
                _ => return Err(error),
            }
        }
        std::thread::sleep(POLL);
    }
}

fn verify_peer_pid(actual: u32, expected: u32) -> io::Result<()> {
    if actual == 0 || actual != expected {
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
                verify_peer_pid(actual, expected_parent)?;
                check_wait(Some(deadline), parent)?;
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

pub(crate) struct ProcessIdentity {
    process: OwnedHandle,
    sid: String,
    session_id: u32,
    elevated: bool,
}

impl ProcessIdentity {
    pub(crate) fn open(pid: u32) -> io::Result<Self> {
        let process = open_process(pid)?;
        check_wait(None, &process)?;
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(HANDLE(process.as_raw_handle()), TOKEN_QUERY, &mut token) }
            .map_err(io::Error::other)?;
        let _token = unsafe { OwnedHandle::from_raw_handle(token.0) };
        let sid = token_sid(token)?;
        let session_id = token_u32(token, TokenSessionId)?;
        // TOKEN_ELEVATION contains one DWORD, TokenIsElevated.
        let elevated = token_u32(token, TokenElevation)? != 0;
        check_wait(None, &process)?;
        Ok(Self {
            process,
            sid,
            session_id,
            elevated,
        })
    }

    pub(crate) fn sid(&self) -> &str {
        &self.sid
    }

    pub(crate) fn session_id(&self) -> u32 {
        self.session_id
    }

    pub(crate) fn same_user_session(&self, other: &Self) -> bool {
        self.session_id() != 0
            && self.session_id() == other.session_id()
            && self.sid() == other.sid()
    }

    pub(crate) fn elevated(&self) -> bool {
        self.elevated
    }

    pub(crate) fn is_alive(&self) -> bool {
        check_wait(None, &self.process).is_ok()
    }
}

fn token_sid(token: HANDLE) -> io::Result<String> {
    // TokenUser adds an inline Windows SID after the aligned TOKEN_USER header.
    const MAX_SID_BYTES: usize = 68;
    const WORDS: usize = (size_of::<TOKEN_USER>() + MAX_SID_BYTES).div_ceil(size_of::<usize>());
    let mut data = [0usize; WORDS];
    let mut size = 0;
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(data.as_mut_ptr().cast()),
            size_of::<[usize; WORDS]>() as u32,
            &mut size,
        )
    }
    .map_err(io::Error::other)?;
    if (size as usize) < size_of::<TOKEN_USER>() {
        return Err(invalid("Windows did not return the token user"));
    }
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid.0, &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let value = unsafe { PCWSTR(sid).to_string() }.map_err(io::Error::other);
    unsafe { LocalFree(sid.cast()) };
    value
}

fn token_u32(token: HANDLE, info: TOKEN_INFORMATION_CLASS) -> io::Result<u32> {
    let mut value = 0u32;
    let mut size = 0;
    unsafe {
        GetTokenInformation(
            token,
            info,
            Some((&mut value as *mut u32).cast()),
            size_of::<u32>() as u32,
            &mut size,
        )
    }
    .map_err(io::Error::other)?;
    if size as usize != size_of::<u32>() {
        return Err(invalid(
            "Windows did not return the expected token information",
        ));
    }
    Ok(value)
}

fn process_image(process: &OwnedHandle) -> io::Result<PathBuf> {
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            HANDLE(process.as_raw_handle()),
            PROCESS_NAME_FORMAT(0),
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )
    }
    .map_err(io::Error::other)?;
    Ok(PathBuf::from(OsString::from_wide(&path[..length as usize])))
}

fn require_process_image(process: &OwnedHandle, expected: &Path) -> io::Result<()> {
    // The enrolled helper's protected directory need not allow the GUI to open the file.
    require_image_path(&process_image(process)?, expected)
}

fn require_same_executable(process: &OwnedHandle) -> io::Result<()> {
    require_image_path(
        &std::fs::canonicalize(process_image(process)?)?,
        &std::fs::canonicalize(std::env::current_exe()?)?,
    )
}

fn require_image_path(actual: &Path, expected: &Path) -> io::Result<()> {
    if !actual
        .as_os_str()
        .eq_ignore_ascii_case(expected.as_os_str())
    {
        return Err(denied("VDD broker executable does not match"));
    }
    Ok(())
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
    maximum: usize,
) -> io::Result<()> {
    if payload.len() > maximum {
        return Err(invalid("VDD broker frame is too large"));
    }
    let mut framed = (payload.len() as u32).to_le_bytes().to_vec();
    framed.extend_from_slice(payload);
    let mut offset = 0;
    while offset < framed.len() {
        check_wait(Some(deadline), peer)?;
        // PIPE_NOWAIT can make no progress when a write exceeds the pipe's
        // kernel buffer. A legal full-state reply is larger than that buffer.
        let end = (offset + 4096).min(framed.len());
        match pipe.write(&framed[offset..end]) {
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
    maximum: usize,
) -> io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    read_exact_poll(pipe, &mut header, deadline, peer)?;
    let length = u32::from_le_bytes(header) as usize;
    if length == 0 || length > maximum {
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
    fn ConvertSidToStringSidW(sid: *mut core::ffi::c_void, text: *mut *mut u16) -> i32;
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
    fn configured_driver_task_failures_do_not_launch_an_elevation_prompt() {
        fn interactive<T>() -> io::Result<T> {
            panic!("configured or unreadable task must not open UAC")
        }
        assert_eq!(
            launch_preferred(Ok(true), || Ok(1), interactive).unwrap(),
            1
        );
        let failed =
            launch_preferred::<()>(Ok(true), || Err(timed_out()), interactive).unwrap_err();
        assert_eq!(failed.kind(), io::ErrorKind::TimedOut);
        assert!(failed.to_string().contains("计划任务连接失败"));
        let unreadable = launch_preferred::<()>(
            Err(denied("read task")),
            || panic!("unverified task cannot run"),
            interactive,
        )
        .unwrap_err();
        assert!(unreadable.to_string().contains("读取驱动授权计划任务失败"));
        assert_eq!(
            launch_preferred(Ok(false), || panic!("disabled task cannot run"), || Ok(2)).unwrap(),
            2,
        );
    }

    #[test]
    fn login_launch_requires_a_verified_enabled_task() {
        let unexpected_launch = || -> io::Result<()> {
            panic!("login cannot launch an absent, disabled or unreadable task")
        };
        let absent = launch_for_login(Ok(false), unexpected_launch).unwrap_err();
        assert_eq!(absent.kind(), io::ErrorKind::PermissionDenied);
        assert!(absent.to_string().contains("登录自启尚未获得驱动授权"));

        let unreadable = launch_for_login(Err(denied("read task")), unexpected_launch).unwrap_err();
        assert!(unreadable.to_string().contains("读取驱动授权计划任务失败"));

        assert_eq!(launch_for_login(Ok(true), || Ok(1)).unwrap(), 1);
        let removed = launch_for_login::<()>(Ok(true), || {
            Err(io::Error::new(io::ErrorKind::NotFound, "task removed"))
        })
        .unwrap_err();
        assert_eq!(removed.kind(), io::ErrorKind::NotFound);
        assert!(removed.to_string().contains("计划任务连接失败"));
    }

    #[test]
    fn scheduled_identity_is_tied_to_a_live_process_image_and_interactive_token() {
        let identity = ProcessIdentity::open(std::process::id()).unwrap();
        let mut other = ProcessIdentity::open(std::process::id()).unwrap();
        assert!(identity.is_alive());
        assert_eq!(process_id(&identity.process).unwrap(), std::process::id());
        assert!(identity.sid().starts_with("S-1-"));
        assert_eq!(
            identity.elevated(),
            crate::elevation::try_is_elevated().unwrap()
        );
        let exe = process_image(&identity.process).unwrap();
        assert!(require_process_image(&identity.process, &exe).is_ok());
        assert!(require_same_executable(&identity.process).is_ok());
        assert!(require_process_image(&identity.process, Path::new("other.exe")).is_err());

        assert_eq!(
            identity.same_user_session(&other),
            identity.session_id() != 0
        );
        other.session_id = identity.session_id.wrapping_add(1);
        assert!(!identity.same_user_session(&other));
        other.session_id = identity.session_id;
        other.sid = "S-1-5-18".to_owned();
        if identity.sid() != other.sid() {
            assert!(!identity.same_user_session(&other));
        }
    }

    #[test]
    fn requests_require_prior_authorization_and_do_not_restart_a_lost_broker() {
        let mut session = Session::Unstarted;
        let not_authorized = transact::<()>(&mut session, |_| panic!("unapproved driver access"));
        assert_eq!(
            not_authorized.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(matches!(session, Session::Unstarted));

        authorize(&mut session, || Err(io::Error::from_raw_os_error(1223)));
        assert!(matches!(session, Session::Failed(BrokerFailure::Cancelled)));
        let cancelled = transact::<()>(&mut session, |_| panic!("cancelled driver access"));
        assert!(cancelled.unwrap_err().to_string().contains("已取消"));

        let dummy_broker = || -> io::Result<Broker> {
            use std::os::windows::io::AsHandle;
            let name = format!("snug-vdd-{}-{:032x}", std::process::id(), 987654_u64);
            let pipe = create_server(&name)?;
            let process = pipe.as_handle().try_clone_to_owned()?;
            Ok(Broker { pipe, process })
        };
        authorize(&mut session, dummy_broker);
        authorize(&mut session, || panic!("connected broker launched twice"));
        transact(&mut session, |_| Ok(())).unwrap();
        assert!(matches!(session, Session::Connected(_)));
        let disconnected = transact::<()>(&mut session, |_| Err(timed_out()));
        assert!(disconnected.is_err());
        assert!(matches!(
            session,
            Session::Failed(BrokerFailure::Disconnected(_))
        ));
        assert!(transact::<()>(&mut session, |_| panic!("interrupted write replayed")).is_err());
        authorize(&mut session, dummy_broker);
        transact(&mut session, |_| Ok(())).unwrap();
    }

    #[test]
    fn explicit_retry_restarts_an_exited_broker_and_reuses_a_live_session() {
        use std::cell::Cell;
        use std::os::windows::io::AsHandle;
        use std::os::windows::process::CommandExt;

        let command = crate::system_paths::system_tool("cmd.exe").unwrap();
        let mut child = std::process::Command::new(command)
            .args(["/D", "/C", "exit 0"])
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let dead = child.as_handle().try_clone_to_owned().unwrap();
        child.wait().unwrap();
        let pid = std::process::id();
        let pipe = create_server(&new_pipe_name(pid).unwrap()).unwrap();
        let session = SESSION.get_or_init(|| Mutex::new(Session::Unstarted));
        *session.lock().unwrap() = Session::Connected(Broker {
            pipe,
            process: dead,
        });

        let name = new_pipe_name(pid).unwrap();
        let mut server = create_server(&name).unwrap();
        let peer = std::thread::spawn(move || {
            let process = open_process(pid).unwrap();
            let mut client = connect_client(&name, pid, &process).unwrap();
            for _ in 0..2 {
                let payload =
                    receive_frame(&mut client, Some(Instant::now() + IO_TIMEOUT), &process, MAX_REQUEST_FRAME)
                        .unwrap();
                assert!(matches!(
                    serde_json::from_slice::<Request>(&payload).unwrap(),
                    Request::State
                ));
                let reply = Reply {
                    legacy: Some(false),
                    monitors: Some(Vec::new()),
                    removal: None,
                    error: None,
                    unsupported: false,
                    outcome_unknown: None,
                    error_kind: None,
                };
                send_frame(
                    &mut client,
                    &serde_json::to_vec(&reply).unwrap(),
                    Instant::now() + IO_TIMEOUT,
                    &process,
                    MAX_RESPONSE_FRAME,
                )
                .unwrap();
            }
        });
        let process = open_process(pid).unwrap();
        accept_verified(&mut server, pid, &process).unwrap();
        let starts = Cell::new(0);
        let protocol = retry_connection_with(|| {
            starts.set(starts.get() + 1);
            Ok(Broker {
                pipe: server,
                process,
            })
        })
        .unwrap();
        assert_eq!(protocol, Some(VddProtocol::V2));
        assert_eq!(
            starts.get(),
            1,
            "the first explicit retry must replace the exited helper"
        );
        assert_eq!(
            retry_connection_with(|| panic!("a live broker must be reused")).unwrap(),
            Some(VddProtocol::V2)
        );
        *session.lock().unwrap() = Session::Unstarted;
        peer.join().unwrap();
    }

    #[test]
    fn requests_can_report_busy_without_waiting_for_authorization() {
        let session = Mutex::new(Session::Unstarted);
        let _pending_authorization = session.lock().unwrap();
        assert!(
            matches!(try_session(&session), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
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
    fn atomic_updates_are_rejected_by_old_brokers_before_driver_access() {
        // Scheduled helpers may still be an older enrolled executable. Their old
        // commands used State + Notify, so sharing those wire names would bypass
        // the new client's atomic-update guarantee.
        #[derive(Debug, Deserialize)]
        #[serde(tag = "command", content = "arguments", deny_unknown_fields)]
        #[allow(dead_code)]
        enum OldRequest {
            Notify { monitors: Vec<VddMonitor> },
            ReplaceLegacy { remove_ids: Vec<u32>, monitors: Vec<VddMonitor> },
            Remove { ids: Vec<u32> },
            State,
        }

        for (request, command, old_command) in [
            (Request::Notify { monitors: vec![monitor()] }, "AtomicNotify", "Notify"),
            (Request::ReplaceLegacy { remove_ids: vec![0], monitors: vec![monitor()] },
                "LegacyOnlyReplace", "ReplaceLegacy"),
        ] {
            let encoded = serde_json::to_value(&request).unwrap();
            assert_eq!(encoded["command"], command);
            assert!(serde_json::from_value::<Request>(encoded.clone()).is_ok());
            assert!(serde_json::from_value::<OldRequest>(encoded.clone()).is_err());
            let mut old_encoded = encoded;
            old_encoded["command"] = old_command.into();
            assert!(serde_json::from_value::<OldRequest>(old_encoded.clone()).is_ok());
            assert!(serde_json::from_value::<Request>(old_encoded).is_err());
        }
    }

    #[test]
    fn legacy_only_mutations_are_rejected_by_pre_guard_brokers_before_driver_access() {
        // An enrolled helper may understand ownership commands but still allow
        // tokenless V2 mutations. New names make it reject before driver access.
        #[derive(Debug, Deserialize)]
        #[serde(tag = "command", content = "arguments", deny_unknown_fields)]
        #[allow(dead_code)]
        enum PreGuardRequest {
            AtomicNotify { monitors: Vec<VddMonitor> },
            AtomicReplace { remove_ids: Vec<u32>, monitors: Vec<VddMonitor> },
            Remove { ids: Vec<u32> },
            OwnedReserve { id: u32, owner: String },
            OwnedCreate { monitors: Vec<VddMonitor>, owner: String },
            OwnedReplace { monitors: Vec<VddMonitor>, owner: String },
            OwnedRelease { ids: Vec<u32>, owner: String },
            State,
        }

        for (request, command, old_command) in [
            (Request::ReplaceLegacy { remove_ids: vec![0], monitors: vec![monitor()] },
                "LegacyOnlyReplace", "AtomicReplace"),
            (Request::Remove { ids: vec![0] }, "LegacyOnlyRemove", "Remove"),
        ] {
            let encoded = serde_json::to_value(&request).unwrap();
            assert_eq!(encoded["command"], command);
            assert!(serde_json::from_value::<Request>(encoded.clone()).is_ok());
            assert!(serde_json::from_value::<PreGuardRequest>(encoded.clone()).is_err());
            let mut old_encoded = encoded;
            old_encoded["command"] = old_command.into();
            assert!(serde_json::from_value::<PreGuardRequest>(old_encoded.clone()).is_ok());
            assert!(serde_json::from_value::<Request>(old_encoded).is_err());
        }
    }

    #[test]
    fn broker_reply_preserves_remove_confirmation_independently_of_protocol() {
        // A legacy write can reconnect to V2 without querying the new driver's state.
        // Protocol detection must not strengthen the deletion evidence in transit.
        for (legacy, removal) in [
            (false, "SubmittedOnly"),
            (false, "Confirmed"),
            (false, "NotOwned"),
            (true, "SubmittedOnly"),
        ] {
            let encoded = serde_json::json!({
                "legacy": legacy,
                "monitors": null,
                "removal": removal,
                "error": null,
                "unsupported": false,
            });
            let reply: Reply = serde_json::from_value(encoded.clone()).unwrap();
            assert_eq!(serde_json::to_value(&reply).unwrap(), encoded);
            let expected = serde_json::from_value::<RemoveDisposition>(encoded["removal"].clone()).unwrap();
            assert_eq!(reply.into_result().unwrap().removal, Some(expected));
        }
    }

    #[test]
    fn broker_preserves_explicit_driver_error_kinds_without_guessing_from_text() {
        for kind in [
            io::ErrorKind::AlreadyExists,
            io::ErrorKind::NotFound,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::TimedOut,
        ] {
            let reply = Reply::driver_failure(io::Error::new(kind, "driver rejected request").into());
            let round_trip: Reply = serde_json::from_slice(&serde_json::to_vec(&reply).unwrap()).unwrap();
            let VddError::Io(error) = VddError::from_broker(round_trip.into_result().unwrap_err()) else {
                panic!("a trusted rejection became an unknown mutation outcome");
            };
            assert_eq!(error.kind(), kind);
        }
        let ordinary = Reply::failure("AlreadyExists: NotFound: permission denied");
        assert_eq!(ordinary.into_result().unwrap_err().kind(), io::ErrorKind::Other);
    }

    #[test]
    fn broker_replies_without_remove_evidence_leave_confirmation_absent() {
        let old_reply: Reply = serde_json::from_value(serde_json::json!({
            "legacy": false,
            "monitors": null,
            "error": null,
        }))
        .unwrap();
        assert_eq!(old_reply.removal, None);
        let failed = Reply::failure("driver disconnected");
        assert_eq!(failed.removal, None);
        assert!(failed.error.is_some());
    }

    #[test]
    fn broker_understands_explicit_unsupported_errors() {
        let encoded = serde_json::json!({
            "legacy": null,
            "monitors": null,
            "removal": null,
            "error": "atomic update unavailable",
            "unsupported": true,
        });
        let reply: Reply = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(serde_json::to_value(&reply).unwrap(), encoded);
        assert_eq!(reply.into_result().unwrap_err().kind(), io::ErrorKind::Unsupported);

        let driver_error = Reply::driver_failure(io::Error::new(
            io::ErrorKind::Unsupported, "atomic update unavailable"
        ).into());
        let round_trip: Reply = serde_json::from_slice(&serde_json::to_vec(&driver_error).unwrap()).unwrap();
        assert_eq!(round_trip.into_result().unwrap_err().kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn broker_preserves_unknown_outcomes_and_their_causes() {
        for kind in [io::ErrorKind::BrokenPipe, io::ErrorKind::TimedOut, io::ErrorKind::InvalidData] {
            let reply = Reply::driver_failure(VddError::OutcomeUnknown {
                source: io::Error::new(kind, "write was applied; acknowledgement was lost"),
            });
            let encoded = serde_json::to_vec(&reply).unwrap();
            let round_trip: Reply = serde_json::from_slice(&encoded).unwrap();
            let error = VddError::from_broker(round_trip.into_result().unwrap_err());
            assert!(matches!(error, VddError::OutcomeUnknown { source } if source.kind() == kind));

            let transport = VddError::from_broker(unknown_outcome(io::Error::new(kind, "helper response lost")));
            assert!(matches!(transport, VddError::OutcomeUnknown { source } if source.kind() == kind));
        }
        assert!(matches!(VddError::from_broker(io::Error::other("not authorized")), VddError::Io(_)));
    }

    #[test]
    fn owned_commands_have_distinct_wire_names_and_validate_tokens() {
        let token = "0123456789abcdef0123456789abcdef";
        #[derive(Debug, Deserialize)]
        #[serde(tag = "command", content = "arguments", deny_unknown_fields)]
        #[allow(dead_code)]
        enum PreOwnedRequest {
            AtomicNotify { monitors: Vec<VddMonitor> },
            AtomicReplace { remove_ids: Vec<u32>, monitors: Vec<VddMonitor> },
            Remove { ids: Vec<u32> },
            State,
        }
        for (request, command) in [
            (Request::ReserveOwned { id: 0, owner: token.into() }, "OwnedReserve"),
            (Request::CreateOwned { monitors: vec![monitor()], owner: token.into() }, "OwnedCreate"),
            (Request::ReplaceOwned { monitors: vec![monitor()], owner: token.into() }, "OwnedReplace"),
            (Request::RemoveOwned { ids: vec![0], owner: token.into() }, "OwnedRelease"),
        ] {
            assert!(request.validate().is_ok());
            let encoded = serde_json::to_value(&request).unwrap();
            assert_eq!(encoded["command"], command);
            assert!(serde_json::from_value::<PreOwnedRequest>(encoded.clone()).is_err());
            assert!(serde_json::from_value::<Request>(encoded).is_ok());
        }
        assert!(Request::ReserveOwned { id: 4, owner: token.into() }.validate().is_err());
        for owner in ["", "0123456789abcdef0123456789abcdeg", "not-a-token"] {
            assert!(Request::ReserveOwned { id: 0, owner: owner.into() }.validate().is_err());
            assert!(Request::RemoveOwned { ids: vec![0], owner: owner.into() }.validate().is_err());
        }
    }

    #[test]
    fn owner_verification_is_rejected_by_older_helpers_before_driver_access() {
        #[derive(Debug, Deserialize)]
        #[serde(tag = "command", content = "arguments", deny_unknown_fields)]
        #[allow(dead_code)]
        enum PreCheckRequest {
            AtomicNotify { monitors: Vec<VddMonitor> },
            LegacyOnlyReplace { remove_ids: Vec<u32>, monitors: Vec<VddMonitor> },
            LegacyOnlyRemove { ids: Vec<u32> },
            OwnedReserve { id: u32, owner: String },
            OwnedCreate { monitors: Vec<VddMonitor>, owner: String },
            OwnedReplace { monitors: Vec<VddMonitor>, owner: String },
            OwnedRelease { ids: Vec<u32>, owner: String },
            State,
        }
        for owner in [None, Some("0123456789abcdef0123456789abcdef")] {
            let request = Request::VerifyOwner { id: 0, owner: owner.map(str::to_owned) };
            assert!(request.validate().is_ok());
            let encoded = serde_json::to_value(request).unwrap();
            assert_eq!(encoded["command"], "VerifyOwner");
            assert!(serde_json::from_value::<PreCheckRequest>(encoded.clone()).is_err());
            assert!(serde_json::from_value::<Request>(encoded).is_ok());
        }
        assert!(Request::VerifyOwner { id: 4, owner: None }.validate().is_err());
        for owner in ["", "0123456789abcdef0123456789abcdeg", "not-a-token"] {
            assert!(Request::VerifyOwner { id: 0, owner: Some(owner.into()) }.validate().is_err());
        }
    }

    #[test]
    fn owner_verification_broker_transport_failure_remains_a_read_only_error() {
        let pid = std::process::id();
        for malformed in [false, true] {
            let name = new_pipe_name(pid).unwrap();
            let mut server = create_server(&name).unwrap();
            let helper = std::thread::spawn(move || {
                let parent = open_process(pid).unwrap();
                let mut pipe = connect_client(&name, pid, &parent).unwrap();
                let payload = receive_frame(&mut pipe, Some(Instant::now() + IO_TIMEOUT),
                    &parent, MAX_REQUEST_FRAME).unwrap();
                assert!(matches!(serde_json::from_slice::<Request>(&payload).unwrap(),
                    Request::VerifyOwner { id: 0, owner: Some(_) }));
                if malformed {
                    send_frame(&mut pipe, b"invalid-json", Instant::now() + IO_TIMEOUT,
                        &parent, MAX_RESPONSE_FRAME).unwrap();
                }
            });
            let peer = open_process(pid).unwrap();
            accept_verified(&mut server, pid, &peer).unwrap();
            let mut session = Session::Connected(Broker { pipe: server, process: peer });
            let request = Request::VerifyOwner { id: 0,
                owner: Some("0123456789abcdef0123456789abcdef".into()) };
            let error = exchange_request(&mut session, &request).unwrap_err();
            assert!(matches!(VddError::from_broker(error), VddError::Io(_)),
                "read-only failures must not be classified as an unknown mutation outcome");
            assert!(matches!(session, Session::Failed(_)));
            assert!(exchange_request(&mut session, &request).is_err(),
                "a lost helper must not be restarted to replay verification");
            helper.join().unwrap();
        }
    }

    #[test]
    fn old_and_ordinary_broker_errors_remain_other_errors() {
        let old_reply: Reply = serde_json::from_str(
            r#"{"legacy":null,"monitors":null,"error":"ordinary failure"}"#
        ).unwrap();
        assert_eq!(old_reply.into_result().unwrap_err().kind(), io::ErrorKind::Other);
        let ordinary = Reply::driver_failure(io::Error::other("ordinary failure").into());
        assert_eq!(ordinary.into_result().unwrap_err().kind(), io::ErrorKind::Other);
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
        assert!(verify_peer_pid(std::process::id(), std::process::id()).is_ok());
        assert!(verify_peer_pid(std::process::id(), std::process::id().wrapping_add(1)).is_err());
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
                receive_frame(&mut pipe, Some(Instant::now() + IO_TIMEOUT), &parent, MAX_REQUEST_FRAME).unwrap();
            let request: Request = serde_json::from_slice(&bytes).unwrap();
            assert!(matches!(request, Request::State));
            let reply = Reply {
                legacy: Some(true),
                monitors: None,
                removal: None,
                error: None,
                unsupported: false,
                outcome_unknown: None,
                error_kind: None,
            };
            send_frame(
                &mut pipe,
                &serde_json::to_vec(&reply).unwrap(),
                Instant::now() + IO_TIMEOUT,
                &parent,
                MAX_RESPONSE_FRAME,
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
            MAX_REQUEST_FRAME,
        )
        .unwrap();
        let bytes = receive_frame(&mut server, Some(Instant::now() + IO_TIMEOUT), &peer, MAX_RESPONSE_FRAME).unwrap();
        assert_eq!(
            serde_json::from_slice::<Reply>(&bytes).unwrap().legacy,
            Some(true)
        );
        client.join().unwrap();
    }

    #[test]
    fn response_size_broker_transports_maximum_legal_state() {
        let pid = std::process::id();
        let name = new_pipe_name(pid).unwrap();
        let mut server = create_server(&name).unwrap();
        let monitors = crate::vdd::maximum_test_state();
        let expected = monitors.clone();
        let client = std::thread::spawn(move || {
            let parent = open_process(pid).unwrap();
            let mut pipe = connect_client(&name, pid, &parent).unwrap();
            let request = receive_frame(&mut pipe, Some(Instant::now() + IO_TIMEOUT), &parent, MAX_REQUEST_FRAME).unwrap();
            assert!(matches!(serde_json::from_slice::<Request>(&request).unwrap(), Request::State));
            let reply = Reply {
                legacy: Some(false), monitors: Some(monitors), removal: None,
                error: None, unsupported: false, outcome_unknown: None, error_kind: None,
            };
            let payload = serde_json::to_vec(&reply).unwrap();
            assert!(payload.len() > 128 * 1024);
            assert!(payload.len() <= MAX_RESPONSE_FRAME);
            println!("maximum legal broker Reply: {} bytes; request/response caps: {MAX_REQUEST_FRAME}/{MAX_RESPONSE_FRAME}", payload.len());
            send_frame(&mut pipe, &payload, Instant::now() + IO_TIMEOUT, &parent, MAX_RESPONSE_FRAME)
        });
        let process = open_process(pid).unwrap();
        accept_verified(&mut server, pid, &process).unwrap();
        let result = Broker { pipe: server, process }.request(&Request::State);
        let sent = client.join().unwrap();
        assert!(sent.is_ok(), "broker must send a legal large State: sent={sent:?}, reply={result:?}");
        assert_eq!(result.unwrap().monitors, Some(expected));
    }

    #[test]
    fn response_size_broker_preserves_request_limit_and_rejects_oversized_sends() {
        assert_eq!(MAX_REQUEST_FRAME, 128 * 1024);
        let name = new_pipe_name(std::process::id()).unwrap();
        let mut pipe = create_server(&name).unwrap();
        let process = open_process(std::process::id()).unwrap();
        for maximum in [MAX_REQUEST_FRAME, MAX_RESPONSE_FRAME] {
            let error = send_frame(&mut pipe, &vec![b' '; maximum + 1],
                Instant::now() + IO_TIMEOUT, &process, maximum).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn response_size_broker_rejects_invalid_headers_before_reading_payload() {
        for (maximum, length) in [
            (MAX_REQUEST_FRAME, MAX_REQUEST_FRAME as u32 + 1),
            (MAX_RESPONSE_FRAME, MAX_RESPONSE_FRAME as u32 + 1),
            (MAX_RESPONSE_FRAME, u32::MAX),
            (MAX_RESPONSE_FRAME, 0),
        ] {
            let pid = std::process::id();
            let name = new_pipe_name(pid).unwrap();
            let mut server = create_server(&name).unwrap();
            let (done_tx, done_rx) = mpsc::channel();
            let client = std::thread::spawn(move || {
                let parent = open_process(pid).unwrap();
                let mut pipe = connect_client(&name, pid, &parent).unwrap();
                pipe.write_all(&length.to_le_bytes()).unwrap();
                // Keep the pipe open but never send a body. Rejecting the header
                // must finish before a normal body read could time out.
                done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            });
            let peer = open_process(pid).unwrap();
            accept_verified(&mut server, pid, &peer).unwrap();
            let error = receive_frame(&mut server, Some(Instant::now() + IO_TIMEOUT), &peer, maximum)
                .unwrap_err();
            done_tx.send(()).unwrap();
            client.join().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn pipe_accept_rejects_an_unverified_client_and_waits_for_the_next() {
        let pid = std::process::id();
        let name = new_pipe_name(pid).unwrap();
        let mut server = create_server(&name).unwrap();
        let timeout = Duration::from_secs(2);
        let client = std::thread::spawn(move || {
            let process = open_process(pid).unwrap();
            let mut rejected = connect_client(&name, pid, &process).unwrap();
            let error =
                receive_frame(&mut rejected, Some(Instant::now() + timeout), &process, MAX_REQUEST_FRAME).unwrap_err();
            assert_ne!(error.kind(), io::ErrorKind::TimedOut);
            drop(rejected);

            let mut accepted = connect_client(&name, pid, &process).unwrap();
            send_frame(
                &mut accepted,
                b"accepted",
                Instant::now() + timeout,
                &process,
                MAX_REQUEST_FRAME,
            )
            .unwrap();
            receive_frame(&mut accepted, Some(Instant::now() + timeout), &process, MAX_RESPONSE_FRAME).unwrap();
        });
        let attempts = std::cell::Cell::new(0);
        accept_peer(
            &server,
            timeout,
            |_| Ok(()),
            |actual| {
                verify_peer_pid(actual, pid)?;
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    return Err(denied("unverified test client"));
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(attempts.get(), 2);
        let process = open_process(pid).unwrap();
        let received =
            receive_frame(&mut server, Some(Instant::now() + timeout), &process, MAX_REQUEST_FRAME).unwrap();
        assert_eq!(received, b"accepted");
        send_frame(&mut server, b"ack", Instant::now() + timeout, &process, MAX_RESPONSE_FRAME).unwrap();
        client.join().unwrap();
    }
}
