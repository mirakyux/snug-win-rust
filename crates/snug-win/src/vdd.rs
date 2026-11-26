//! virtual-display-rs 的命名管道客户端，移植自 C# 版 `SnugWin.Windows.Display.VirtualDisplayIpcClient`。
//!
//! 两套协议并存：
//! - **v2**：双向管道，JSON 帧以 `0x04`(EOT) 结尾；手机屏以 owner token
//!   预留、创建、更新和删除；没有 token 的旧版替换/删除拒绝发送，查询使用 `State`；
//! - **v0.3.x**：驱动只开 `PIPE_ACCESS_INBOUND`，所以客户端只能写（`PipeDirection.Out`），
//!   指令为 `Add` / `Remove` / `RemoveAll`，并且不带帧尾。
//!
//! 判断顺序与 C# 版一致：先试双向，遇到"拒绝访问"退回只写。

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use windows::Win32::Foundation::{ERROR_IO_PENDING, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, ReadFile, WriteFile};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

pub const DEFAULT_PIPE_NAME: &str = "virtualdisplaydriver";
/// v2 帧结束符。
pub const EOT: u8 = 0x04;
const PIPE_IO_TIMEOUT: Duration = Duration::from_secs(3);
const V2_OWNER_REQUIRED: &str =
    "V2 虚拟屏的修改和移除需要归属令牌；旧版归属记录需手动核对处理";

// Driver contract: 16 monitors, 256 modes each, dimensions <= 16384, four
// refresh rates <= 240, and 256 UTF-8 bytes per name. JSON may escape each name
// byte as six bytes (\u0001). Include commas plus 1 KiB for the State/Changed
// or broker Reply envelope; this bounds each response, not all queued events.
const MAX_MODE_JSON_BYTES: usize =
    br#"{"width":16384,"height":16384,"refresh_rates":[120,144,165,240]}"#.len();
const MAX_MONITOR_JSON_BYTES: usize = br#"{"id":15,"name":"","enabled":false,"modes":[]}"#.len()
    + 256 * 6 + 256 * MAX_MODE_JSON_BYTES + 255;
pub(crate) const MAX_STATE_RESPONSE_BYTES: usize = 16 * (MAX_MONITOR_JSON_BYTES + 1) + 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VddMode {
    pub width: u32,
    pub height: u32,
    pub refresh_rates: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VddMonitor {
    pub id: u32,
    #[serde(default)]
    pub name: Option<String>,
    pub enabled: bool,
    #[serde(default)]
    pub modes: Vec<VddMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LegacyMode {
    width: u32,
    height: u32,
    refresh_rate: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LegacyMonitor {
    id: u32,
    modes: Vec<LegacyMode>,
}

/// `{"Notify":[...]}`（v2 协议："把这些显示器状态设为当前状态"）。
pub fn serialize_notify(monitors: &[VddMonitor]) -> String {
    #[derive(Serialize)]
    struct Frame<'a> {
        #[serde(rename = "Notify")]
        notify: &'a [VddMonitor],
    }
    serde_json::to_string(&Frame { notify: monitors }).unwrap_or_else(|_| "{\"Notify\":[]}".into())
}

/// Atomic, acknowledged V2 extension. Never fall back to the destructive Notify command.
fn serialize_update(monitors: &[VddMonitor], replacing: bool) -> String {
    serde_json::json!({ "SnugUpdate": { "monitors": monitors, "replacing": replacing } }).to_string()
}

/// `{"Add":[...]}`（v0.3.x 协议，刷新率只取第一个，缺省 60）。
pub fn serialize_legacy_add(monitors: &[VddMonitor]) -> String {
    let legacy: Vec<LegacyMonitor> = monitors
        .iter()
        .map(|m| LegacyMonitor {
            id: m.id,
            modes: m
                .modes
                .iter()
                .map(|mode| LegacyMode {
                    width: mode.width,
                    height: mode.height,
                    refresh_rate: mode.refresh_rates.first().copied().unwrap_or(60),
                })
                .collect(),
        })
        .collect();
    #[derive(Serialize)]
    struct Frame {
        #[serde(rename = "Add")]
        add: Vec<LegacyMonitor>,
    }
    serde_json::to_string(&Frame { add: legacy }).unwrap_or_else(|_| "{\"Add\":[]}".into())
}

/// `{"Remove":[id,...]}`。
pub fn serialize_remove(ids: &[u32]) -> String {
    #[derive(Serialize)]
    struct Frame<'a> {
        #[serde(rename = "Remove")]
        remove: &'a [u32],
    }
    serde_json::to_string(&Frame { remove: ids }).unwrap_or_else(|_| "{\"Remove\":[]}".into())
}

/// `"RemoveAll"`。
pub fn serialize_remove_all() -> String {
    "\"RemoveAll\"".to_string()
}

/// `"State"`。
pub fn serialize_state_request() -> String {
    "\"State\"".to_string()
}

/// 解析驱动回报的状态：`{"State":[...]}` 或 `{"Changed":[...]}`。
pub fn parse_state_reply(json: &str) -> Option<Vec<VddMonitor>> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let object = value.as_object()?;
    let key = if object.contains_key("State") {
        "State"
    } else if object.contains_key("Changed") {
        "Changed"
    } else {
        return None;
    };
    serde_json::from_value(object.get(key)?.clone()).ok()
}

/// 给 v2 帧补上 EOT 结尾。
pub fn frame(json: &str) -> Vec<u8> {
    let mut bytes = json.as_bytes().to_vec();
    bytes.push(EOT);
    bytes
}

/// 驱动管道在不在（`\\.\pipe\<name>` 存在，也就是"驱动装好且服务起着"）。
///
/// **不能只问 `Path::exists()`。** 命名管道的元数据查询依赖"有可用实例"，客户端连接
/// 占满实例时会直接失败；而且同一个 exe 在不同进程上下文里的结果还不一样——真机上
/// 从计划任务（开机自启）启动时报"未检测到"，手动双击却报"就绪"，同一台机器同一次连接。
/// 这道判断题一旦答错，代价是**误判成没装驱动、白跑一遍 msiexec 安装**。
///
/// 所以先走 `Path::exists()` 快路，失败再用 `WaitNamedPipe(0)` 复查：它只是问
/// "有没有可用实例"，不占用实例、不建立连接。只有 `ERROR_FILE_NOT_FOUND` /
/// `ERROR_PATH_NOT_FOUND` 才算真不存在；`ERROR_SEM_TIMEOUT`（实例全忙，管道在）、
/// 拒绝访问（管道在但不给我们连）都算存在。
pub fn is_driver_pipe_available(name: &str) -> bool {
    if pipe_path(name).exists() {
        return true;
    }
    wait_named_pipe_exists(name)
}

/// [`is_driver_pipe_available`] 的兜底判定：见那里的注释。
fn wait_named_pipe_exists(name: &str) -> bool {
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PATH_NOT_FOUND: i32 = 3;

    unsafe extern "system" {
        fn WaitNamedPipeW(lp_named_pipe_name: *const u16, n_time_out: u32) -> i32;
    }

    let mut wide: Vec<u16> = format!(r"\\.\pipe\{name}").encode_utf16().collect();
    wide.push(0);
    // 超时 0 = 只问一句，不排队等。
    let ok = unsafe { WaitNamedPipeW(wide.as_ptr(), 0) };
    if ok != 0 {
        return true;
    }
    !matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PATH_NOT_FOUND)
    )
}

fn pipe_path(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!(r"\\.\pipe\{name}"))
}

/// Check the caller's actual access without issuing a driver command.
pub(crate) fn probe_access(name: &str) -> crate::driver_health::PipeAccess {
    use crate::driver_health::PipeAccess;
    let path = pipe_path(name);
    let open = |duplex| {
        OpenOptions::new()
            .read(duplex)
            .write(true)
            .security_qos_flags(0x0001_0000)
            .open(&path)
    };
    match open(true) {
        Ok(_) => PipeAccess::Direct(VddProtocol::V2),
        Err(error) => match error.raw_os_error() {
            Some(5) => match open(false) {
                Ok(_) => PipeAccess::Direct(VddProtocol::Legacy),
                Err(error) if error.raw_os_error() == Some(5) => PipeAccess::RequiresElevation,
                Err(error) if error.raw_os_error() == Some(231) => PipeAccess::Busy,
                Err(error) if matches!(error.raw_os_error(), Some(2 | 3)) => PipeAccess::Missing,
                _ => PipeAccess::Unavailable,
            },
            Some(2 | 3) => PipeAccess::Missing,
            Some(231) => PipeAccess::Busy,
            _ => PipeAccess::Unavailable,
        },
    }
}

#[derive(Debug, Error)]
pub enum VddError {
    #[error("连不上虚拟屏管道 {name}：{source}")]
    Connect {
        name: String,
        #[source]
        source: std::io::Error,
    },
    #[error("管道读写失败：{0}")]
    Io(#[from] std::io::Error),
    /// A mutation was sent, but its acknowledgement could not establish the result.
    /// This is neither confirmation of ownership nor permission to replay the write.
    #[error("驱动写入结果未知，未收到可信确认：{source}")]
    OutcomeUnknown {
        #[source]
        source: std::io::Error,
    },
    /// 内部不变量被破坏（公开方法都会先 `ensure()`，正常路径不会出现）。
    #[error("管道尚未连接")]
    NotConnected,
}

impl VddError {
    pub(crate) fn from_broker(error: std::io::Error) -> Self {
        if error.get_ref().is_some_and(|source| {
            matches!(source.downcast_ref::<Self>(), Some(Self::OutcomeUnknown { .. }))
        }) {
            // Only the broker's explicit marker survives this conversion as an unknown outcome.
            *error.into_inner().unwrap().downcast::<Self>().unwrap()
        } else {
            Self::Io(error)
        }
    }
}

/// 协议探测结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VddProtocol {
    /// 双向 + EOT 帧（virtual-display-rs master）。
    V2,
    /// 只写、无帧尾（v0.3.x）。
    Legacy,
}

/// How much the driver protocol can confirm after a Remove request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoveDisposition {
    /// The driver confirmed the requested removal; desktop detachment can lag.
    Confirmed,
    /// The supplied token owns none of the requested IDs; foreign screens stay intact.
    NotOwned,
    /// The legacy write succeeded, but driver state cannot be queried.
    SubmittedOnly,
}

struct Connection {
    protocol: VddProtocol,
    pipe: Box<dyn PipeHandle>,
}

/// 连接存活探测结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    Alive,
    /// 对端已经走了（`ERROR_BROKEN_PIPE` / `ERROR_PIPE_NOT_CONNECTED`）。
    Dead,
    /// 探测不了（例如 v0.3.x 的只写句柄不允许 `PeekNamedPipe`），按"还活着"处理，
    /// 真正断了会由写失败兜住。
    Unknown,
}

/// 一条管道句柄。真机是 [`PipeFile`]，单测换成假实现。
trait PipeHandle: Read + Write + Send {
    fn probe(&mut self) -> Liveness;

    fn read_ready(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        self.read(buf).map(Some)
    }
}

struct PipeFile(std::fs::File);

impl PipeFile {
    fn transfer(
        &self,
        deadline: Instant,
        start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> std::io::Result<usize> {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "驱动管道 I/O 超时"));
        }
        let event = unsafe { CreateEventW(None, true, false, None) }.map_err(pipe_io_error)?;
        let _event = unsafe { OwnedHandle::from_raw_handle(event.0) };
        let mut overlapped = OVERLAPPED { hEvent: event, ..Default::default() };
        let pipe = HANDLE(self.0.as_raw_handle());
        match start(pipe, &mut overlapped) {
            Ok(()) => {}
            Err(error) if error.code() == windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let milliseconds = remaining.as_millis().max(1).min(u128::from(u32::MAX - 1)) as u32;
                let wait = unsafe { WaitForSingleObject(event, milliseconds) };
                if wait != WAIT_OBJECT_0 {
                    let error = if wait == WAIT_TIMEOUT {
                        std::io::Error::new(std::io::ErrorKind::TimedOut, "驱动管道 I/O 超时")
                    } else {
                        std::io::Error::last_os_error()
                    };
                    // Cancellation is asynchronous. Drain even an ERROR_NOT_FOUND race before
                    // the buffer, OVERLAPPED or event can leave this scope.
                    unsafe {
                        let _ = CancelIoEx(pipe, Some(&overlapped));
                        let mut transferred = 0;
                        let _ = GetOverlappedResult(pipe, &overlapped, &mut transferred, true);
                    }
                    return Err(error);
                }
            }
            Err(error) => return Err(pipe_io_error(error)),
        }
        let mut transferred = 0;
        unsafe { GetOverlappedResult(pipe, &overlapped, &mut transferred, false) }
            .map_err(pipe_io_error)?;
        Ok(transferred as usize)
    }

    fn write_until(&self, buf: &[u8], deadline: Instant) -> std::io::Result<usize> {
        let length = buf.len().min(u32::MAX as usize);
        self.transfer(deadline, |pipe, overlapped| unsafe {
            WriteFile(pipe, Some(&buf[..length]), None, Some(overlapped))
        })
    }
}

fn pipe_io_error(error: windows::core::Error) -> std::io::Error {
    std::io::Error::from_raw_os_error((error.code().0 as u32 & 0xffff) as i32)
}

impl Read for PipeFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() { return Ok(0); }
        let length = buf.len().min(u32::MAX as usize);
        self.transfer(Instant::now() + PIPE_IO_TIMEOUT, |pipe, overlapped| unsafe {
            ReadFile(pipe, Some(&mut buf[..length]), None, Some(overlapped))
        })
    }
}

impl Write for PipeFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() { return Ok(0); }
        self.write_until(buf, Instant::now() + PIPE_IO_TIMEOUT)
    }

    fn write_all(&mut self, mut buf: &[u8]) -> std::io::Result<()> {
        let deadline = Instant::now() + PIPE_IO_TIMEOUT;
        while !buf.is_empty() {
            let written = self.write_until(buf, deadline)?;
            if written == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "驱动管道未写入数据"));
            }
            buf = &buf[written..];
        }
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // Named pipes are unbuffered here. FlushFileBuffers could wait for the peer forever.
        Ok(())
    }
}

impl PipeHandle for PipeFile {
    fn probe(&mut self) -> Liveness {
        peek_liveness(self.0.as_raw_handle())
    }

    fn read_ready(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        let available = available_bytes(self.0.as_raw_handle())? as usize;
        if available == 0 {
            return Ok(None);
        }
        let length = available.min(buf.len());
        self.read(&mut buf[..length]).map(Some)
    }
}

/// `PeekNamedPipe` 探测对端是否还在。驱动重启（`pnputil /restart-device`）之后，
/// 旧实例会立刻返回 `ERROR_BROKEN_PIPE`，缓存句柄就不能再用了。
///
/// 这里用一行 `extern` 声明，而不是 `windows` crate 的 `Win32_System_Pipes` feature：
/// 只为一个函数开一个 feature 不划算（Cargo.toml 由 Chief 统一改，能不惊动就不惊动）。
fn peek_liveness(handle: RawHandle) -> Liveness {
    match available_bytes(handle) {
        Ok(_) => Liveness::Alive,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Liveness::Unknown,
        Err(_) => Liveness::Dead,
    }
}

fn available_bytes(handle: RawHandle) -> std::io::Result<u32> {
    unsafe extern "system" {
        fn PeekNamedPipe(
            h_named_pipe: *mut core::ffi::c_void,
            lp_buffer: *mut core::ffi::c_void,
            n_buffer_size: u32,
            lp_bytes_read: *mut u32,
            lp_total_bytes_avail: *mut u32,
            lp_bytes_left_this_message: *mut u32,
        ) -> i32;
    }

    let mut available = 0;
    let ok = unsafe {
        PeekNamedPipe(
            handle as *mut core::ffi::c_void,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        Ok(available)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

type Connector = Box<dyn Fn(&str) -> Result<Connection, VddError> + Send + Sync>;

/// 管道客户端。
///
/// **重连触发条件**（与 C# `EnsureProtocolAsync` 对齐，修的是 Rust 特有的"缓存句柄永不重置"）：
/// 1. 每次操作前 `PeekNamedPipe` 探测到断线（`ERROR_BROKEN_PIPE` / `ERROR_PIPE_NOT_CONNECTED`）；
/// 2. 任何一次写失败（写失败即丢弃连接）；
/// 3. v0.3.x 的 legacy 通道在写失败后还会**立刻**重连并重写一次（C# `SendLegacyOneShotAsync`）。
/// 丢弃之后下一次操作会重新 `CreateFile` 并重新探测协议，调用方感知不到多了一步。
pub struct VddClient {
    name: String,
    connection: Option<Connection>,
    connector: Connector,
    use_broker: bool,
    broker_protocol: Option<VddProtocol>,
}

impl Default for VddClient {
    fn default() -> Self {
        Self::new(DEFAULT_PIPE_NAME)
    }
}

impl VddClient {
    pub fn new(name: impl Into<String>) -> Self {
        let mut client = Self::direct(name);
        client.use_broker = client.name == DEFAULT_PIPE_NAME && !crate::elevation::is_elevated();
        client
    }

    pub(crate) fn direct(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            connection: None,
            connector: Box::new(connect),
            use_broker: false,
            broker_protocol: None,
        }
    }

    /// 单测入口：换掉真正去 `CreateFile` 的连接器。
    #[cfg(test)]
    fn with_connector(name: &str, connector: Connector) -> Self {
        Self {
            name: name.to_string(),
            connection: None,
            connector,
            use_broker: false,
            broker_protocol: None,
        }
    }

    pub fn protocol(&self) -> Option<VddProtocol> {
        self.connection
            .as_ref()
            .map(|c| c.protocol)
            .or(self.broker_protocol)
    }

    pub fn is_legacy(&self) -> bool {
        self.protocol() == Some(VddProtocol::Legacy)
    }

    pub(crate) fn probe_connection(&mut self) -> Result<VddProtocol, VddError> {
        self.state()?;
        self.protocol().ok_or(VddError::NotConnected)
    }

    /// Create monitors atomically without replacing existing V2 entries, including disabled ones.
    pub fn notify(&mut self, monitors: &[VddMonitor]) -> Result<(), VddError> {
        if self.needs_broker() {
            return self
                .broker(crate::vdd_broker::Request::Notify {
                    monitors: monitors.to_vec(),
                })
                .map(|_| ());
        }
        match self.ensure()? {
            VddProtocol::V2 => self.update_v2_monitors(monitors, false),
            VddProtocol::Legacy => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Legacy driver mutations are disabled without an ownership token",
            ).into()),
        }
    }

    /// Reserve a vacant V2 ID for a persisted owner token without changing topology.
    /// Legacy drivers return false without sending a reservation command.
    pub fn reserve_owned(&mut self, id: u32, owner: &str) -> Result<bool, VddError> {
        if self.needs_broker() {
            let reply = self.broker(crate::vdd_broker::Request::ReserveOwned {
                id, owner: owner.into(),
            })?;
            return reply.legacy.map(|legacy| !legacy).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "VDD broker 未返回预留协议").into()
            });
        }
        if self.ensure()? == VddProtocol::Legacy {
            return Ok(false);
        }
        let json = serde_json::json!({ "SnugReserve": { "owner": owner, "id": id } }).to_string();
        self.owned_command("SnugReserve", owner, &json, false)?;
        Ok(true)
    }

    /// Check current ownership without renewing a reservation or changing topology.
    /// Tokenless bindings are only compatible with the legacy protocol.
    pub fn verify_owner(&mut self, id: u32, owner: Option<&str>) -> Result<(), VddError> {
        if owner.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Tokenless display ownership cannot authorize mutations",
            ).into());
        }
        if self.needs_broker() {
            let reply = self.broker(crate::vdd_broker::Request::VerifyOwner {
                id, owner: owner.map(str::to_owned),
            })?;
            if reply.legacy == Some(owner.is_none()) {
                return Ok(());
            }
            self.broker_protocol = None;
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported,
                "驱动权限代理未确认绑定对应的归属协议").into());
        }
        let owner = owner.expect("tokenless ownership is rejected above");
        self.require_owned_protocol()?;
        let json = serde_json::json!({ "SnugCheck": { "owner": owner, "id": id } }).to_string();
        self.owned_command("SnugCheck", owner, &json, false)
    }

    /// Create only through a matching V2 reservation; never downgrade to an unguarded Add.
    pub fn create_owned(&mut self, monitors: &[VddMonitor], owner: &str) -> Result<(), VddError> {
        if self.needs_broker() {
            return self.broker(crate::vdd_broker::Request::CreateOwned {
                monitors: monitors.to_vec(), owner: owner.into(),
            }).map(|_| ());
        }
        self.require_owned_protocol()?;
        let json = serde_json::json!({ "SnugCreate": { "owner": owner, "monitors": monitors } }).to_string();
        self.owned_command("SnugCreate", owner, &json, true)
    }

    /// Update modes without dropping the V2 ownership reservation.
    pub fn replace_owned(&mut self, monitors: &[VddMonitor], owner: &str) -> Result<(), VddError> {
        if self.needs_broker() {
            return self.broker(crate::vdd_broker::Request::ReplaceOwned {
                monitors: monitors.to_vec(), owner: owner.into(),
            }).map(|_| ());
        }
        self.require_owned_protocol()?;
        let json = serde_json::json!({ "SnugReplace": { "owner": owner, "monitors": monitors } }).to_string();
        self.owned_command("SnugReplace", owner, &json, true)
    }

    /// Idempotently release only displays and reservations matching the persisted token.
    pub fn remove_owned(&mut self, ids: &[u32], owner: &str) -> Result<RemoveDisposition, VddError> {
        if self.needs_broker() {
            let reply = self.broker(crate::vdd_broker::Request::RemoveOwned {
                ids: ids.to_vec(), owner: owner.into(),
            })?;
            return remove_disposition(reply);
        }
        self.require_owned_protocol()?;
        let json = serde_json::json!({ "SnugRelease": { "owner": owner, "ids": ids } }).to_string();
        match self.owned_command("SnugRelease", owner, &json, true) {
            Ok(()) => Ok(RemoveDisposition::Confirmed),
            // Transport and untrusted replies are OutcomeUnknown; only a matching
            // owner acknowledgement can produce this ordinary NotFound error.
            Err(VddError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(RemoveDisposition::NotOwned)
            }
            Err(error) => Err(error),
        }
    }

    fn require_owned_protocol(&mut self) -> Result<(), VddError> {
        if self.ensure()? == VddProtocol::V2 { Ok(()) } else {
            Err(std::io::Error::new(std::io::ErrorKind::Unsupported,
                "驱动协议已变化，无法安全操作带归属凭据的虚拟屏").into())
        }
    }

    fn owned_command(&mut self, command: &str, owner: &str, json: &str, mutating: bool) -> Result<(), VddError> {
        use std::io::{Error, ErrorKind};
        let reply = self.write_framed(json).and_then(|()| self.read_v2_reply(command));
        let result = match reply {
            Ok(reply) if reply.get("owner").and_then(serde_json::Value::as_str) == Some(owner) => {
                match reply.get("result").and_then(serde_json::Value::as_str) {
                    Some("Applied") => return Ok(()),
                    Some("AlreadyExists") => return Err(Error::new(ErrorKind::AlreadyExists, "虚拟屏编号已被占用").into()),
                    Some("NotFound") => return Err(Error::new(ErrorKind::NotFound, "虚拟屏预留凭据不存在").into()),
                    Some("InvalidData") => return Err(Error::new(ErrorKind::InvalidInput, "驱动拒绝了无效的归属操作").into()),
                    Some("ApplyFailed") => return Err(Error::other("驱动未能应用虚拟屏操作").into()),
                    _ => Error::new(ErrorKind::InvalidData, "无效的驱动归属操作应答"),
                }
            }
            Ok(_) => Error::new(ErrorKind::InvalidData, "驱动应答的归属凭据不匹配"),
            Err(VddError::Io(source)) => source,
            Err(error) => Error::other(error),
        };
        self.connection = None;
        if mutating {
            Err(VddError::OutcomeUnknown { source: result })
        } else {
            Err(Error::new(ErrorKind::Unsupported,
                format!("驱动未确认归属操作，需要支持 {command} 的 V2 驱动：{result}")).into())
        }
    }

    /// Legacy 先删再加；V2 必须通过 `replace_owned` 验证归属，不能沿用旧编号。
    pub fn replace_legacy(
        &mut self,
        remove_ids: &[u32],
        add_monitors: &[VddMonitor],
    ) -> Result<(), VddError> {
        if self.needs_broker() {
            return self
                .broker(crate::vdd_broker::Request::ReplaceLegacy {
                    remove_ids: remove_ids.to_vec(),
                    monitors: add_monitors.to_vec(),
                })
                .map(|_| ());
        }
        match self.ensure()? {
            VddProtocol::V2 => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported, V2_OWNER_REQUIRED,
            ).into()),
            VddProtocol::Legacy => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Legacy replacement is disabled without an ownership token",
            ).into()),
        }
    }

    /// 无令牌的旧版删除仅适用于 Legacy；V2 必须使用 `remove_owned`。
    pub fn remove(&mut self, ids: &[u32]) -> Result<RemoveDisposition, VddError> {
        if self.needs_broker() {
            let reply = self
                .broker(crate::vdd_broker::Request::Remove { ids: ids.to_vec() })?;
            return remove_disposition(reply);
        }
        match self.ensure()? {
            VddProtocol::V2 => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported, V2_OWNER_REQUIRED,
            ).into()),
            VddProtocol::Legacy => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Legacy removal is disabled without an ownership token",
            ).into()),
        }
    }

    pub fn remove_all(&mut self) -> Result<(), VddError> {
        Err(VddError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Removing all displays without explicit ownership is disabled",
        )))
    }

    /// 请求当前状态（仅 v2 支持应答）。
    pub fn state(&mut self) -> Result<Option<Vec<VddMonitor>>, VddError> {
        if self.needs_broker() {
            return self
                .broker(crate::vdd_broker::Request::State)
                .map(|reply| reply.monitors);
        }
        match self.ensure()? {
            VddProtocol::Legacy => Ok(None),
            VddProtocol::V2 => {
                self.write_framed(&serialize_state_request())?;
                let result = self.read_v2_state();
                if result.is_err() {
                    self.connection = None;
                }
                result.map(Some)
            }
        }
    }

    fn needs_broker(&mut self) -> bool {
        if !self.use_broker {
            return false;
        }
        if let Some(connection) = self.connection.as_mut() {
            if connection.pipe.probe() != Liveness::Dead {
                return false;
            }
            self.connection = None;
        }
        match (self.connector)(&self.name) {
            Ok(connection) => {
                self.connection = Some(connection);
                self.broker_protocol = None;
                false
            }
            Err(VddError::Connect { source, .. }) => {
                source.kind() == std::io::ErrorKind::PermissionDenied
            }
            Err(_) => false,
        }
    }

    fn update_v2_monitors(
        &mut self,
        updates: &[VddMonitor],
        replacing: bool,
    ) -> Result<(), VddError> {
        use std::io::{Error, ErrorKind};
        // A State snapshot followed by Notify can delete a concurrent client's display.
        // The driver must check IDs and merge under its own state lock, then acknowledge.
        self.write_framed(&serialize_update(updates, replacing))?;
        let reply = self.read_v2_reply("SnugUpdate");
        if reply.is_err() {
            self.connection = None;
        }
        match reply {
            Ok(serde_json::Value::String(result)) => match result.as_str() {
                "Applied" => Ok(()),
                "AlreadyExists" => Err(Error::new(ErrorKind::AlreadyExists, "虚拟屏编号已被占用").into()),
                "NotFound" => Err(Error::new(ErrorKind::NotFound, "待更新的虚拟屏已不存在").into()),
                "InvalidData" => Err(Error::new(ErrorKind::InvalidInput, "驱动拒绝了无效的虚拟屏更新").into()),
                "ApplyFailed" => Err(Error::other("驱动未能应用虚拟屏模式；保留实际状态供恢复").into()),
                _ => {
                    self.connection = None;
                    Err(VddError::OutcomeUnknown {
                        source: Error::new(ErrorKind::InvalidData, "无法识别驱动的原子更新应答"),
                    })
                },
            },
            Ok(_) => {
                self.connection = None;
                Err(VddError::OutcomeUnknown {
                    source: Error::new(ErrorKind::InvalidData, "无效的驱动原子更新应答"),
                })
            },
            Err(VddError::Io(source)) => Err(VddError::OutcomeUnknown { source }),
            Err(error) => Err(VddError::OutcomeUnknown { source: Error::other(error) }),
        }
    }

    fn read_v2_state(&mut self) -> Result<Vec<VddMonitor>, VddError> {
        serde_json::from_value(self.read_v2_reply("State")?)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error).into())
    }

    fn read_v2_reply(&mut self, key: &str) -> Result<serde_json::Value, VddError> {
        use std::io::{Error, ErrorKind};
        let deadline = Instant::now() + Duration::from_secs(3);
        let pipe = &mut self.connection.as_mut().ok_or(VddError::NotConnected)?.pipe;
        let mut frame = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if Instant::now() >= deadline {
                return Err(Error::new(ErrorKind::TimedOut, "读取驱动状态超时").into());
            }
            let Some(read) = pipe.read_ready(&mut chunk)? else {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            if read == 0 {
                return Err(Error::new(ErrorKind::BrokenPipe, "驱动未返回完整状态").into());
            }
            for &byte in &chunk[..read] {
                if byte != EOT {
                    if frame.len() >= MAX_STATE_RESPONSE_BYTES {
                        return Err(Error::new(ErrorKind::InvalidData, "驱动状态超过大小限制").into());
                    }
                    frame.push(byte);
                    continue;
                }
                if let Ok(text) = std::str::from_utf8(&frame)
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(text)
                    && let Some(reply) = value.get(key)
                {
                    return Ok(reply.clone());
                }
                frame.clear();
            }
        }
    }

    fn broker(
        &mut self,
        request: crate::vdd_broker::Request,
    ) -> Result<crate::vdd_broker::Reply, VddError> {
        let reply = match crate::vdd_broker::request(request) {
            Ok(reply) => reply,
            Err(error) => {
                self.broker_protocol = None;
                return Err(VddError::from_broker(error));
            }
        };
        self.broker_protocol = reply.legacy.map(|legacy| {
            if legacy {
                VddProtocol::Legacy
            } else {
                VddProtocol::V2
            }
        });
        Ok(reply)
    }

    /// 确认连接可用；不能用（探测到断线）就丢弃，重新 `CreateFile` 并重新探测协议。
    fn ensure(&mut self) -> Result<VddProtocol, VddError> {
        if let Some(connection) = self.connection.as_mut() {
            if connection.pipe.probe() != Liveness::Dead {
                return Ok(connection.protocol);
            }
            // 驱动重启后旧管道实例没了：这里丢弃缓存句柄，下面重连。
            self.connection = None;
        }
        let connection = (self.connector)(&self.name)?;
        let protocol = connection.protocol;
        self.connection = Some(connection);
        Ok(protocol)
    }

    fn write_framed(&mut self, json: &str) -> Result<(), VddError> {
        self.write_bytes(&frame(json))
    }

    /// 写一段字节；写失败就把连接丢掉（下次 `ensure()` 重新连）。
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), VddError> {
        let Some(connection) = self.connection.as_mut() else {
            return Err(VddError::NotConnected);
        };
        let result = connection.pipe.write_all(bytes).and_then(|()| connection.pipe.flush());
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                self.connection = None;
                Err(VddError::Io(error))
            }
        }
    }

}

fn remove_disposition(reply: crate::vdd_broker::Reply) -> Result<RemoveDisposition, VddError> {
    reply.removal.ok_or_else(|| std::io::Error::other("VDD broker 未返回移除确认状态").into())
}

/// 先试双向（v2），"拒绝访问"则退回只写（v0.3.x）；管道忙时重试。
fn connect(name: &str) -> Result<Connection, VddError> {
    let path = pipe_path(name);
    let deadline = Instant::now() + Duration::from_millis(2500);
    let mut first_error: Option<std::io::Error> = None;

    loop {
        // A pipe-name squatter must not impersonate the elevated VDD helper.
        match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OVERLAPPED.0)
            .security_qos_flags(0x0001_0000)
            .open(&path)
        {
            Ok(file) => {
                return Ok(Connection {
                    protocol: VddProtocol::V2,
                    pipe: Box::new(PipeFile(file)),
                });
            }
            Err(err) => {
                if err.kind() == std::io::ErrorKind::PermissionDenied {
                    // 驱动只开 INBOUND 时，双向打开会被拒；退回只写。
                    match OpenOptions::new()
                        .write(true)
                        .custom_flags(FILE_FLAG_OVERLAPPED.0)
                        .security_qos_flags(0x0001_0000)
                        .open(&path)
                    {
                        Ok(file) => {
                            return Ok(Connection {
                                protocol: VddProtocol::Legacy,
                                pipe: Box::new(PipeFile(file)),
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                            first_error = Some(error);
                            break;
                        }
                        Err(error) => {
                            first_error.get_or_insert(error);
                        }
                    }
                } else if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    Err(VddError::Connect {
        name: name.to_string(),
        source: first_error.unwrap_or_else(|| std::io::Error::other("管道不可用")),
    })
}

#[cfg(test)]
pub(crate) fn maximum_test_state() -> Vec<VddMonitor> {
    (0..16).map(|id| VddMonitor {
        id,
        name: Some("\u{1}".repeat(256)),
        enabled: false,
        modes: (0..256).map(|index| VddMode {
            width: 10000 + index,
            height: 16384,
            refresh_rates: vec![120, 144, 165, 240],
        }).collect(),
    }).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    fn monitor() -> VddMonitor {
        VddMonitor {
            id: 0,
            name: Some("Snug".into()),
            enabled: true,
            modes: vec![VddMode { width: 1080, height: 1900, refresh_rates: vec![60, 75] }],
        }
    }

    #[test]
    fn notify_frame_shape_matches_driver() {
        let json = serialize_notify(&[monitor()]);
        assert_eq!(
            json,
            r#"{"Notify":[{"id":0,"name":"Snug","enabled":true,"modes":[{"width":1080,"height":1900,"refresh_rates":[60,75]}]}]}"#
        );
    }

    #[test]
    fn notify_can_create_multiple_displays_in_one_frame() {
        // v1.3.0 多屏：drivers 的 id 是参数，一帧里可以带多块屏；
        // 建屏流程仍是一块一块发，这里只确认协议层不会把第二块丢掉。
        let mut second = monitor();
        second.id = 1;
        second.name = Some("Snug 手机屏 2".into());
        let json = serialize_notify(&[monitor(), second.clone()]);

        let value: serde_json::Value = serde_json::from_str(&json).expect("合法 JSON");
        let monitors = value["Notify"].as_array().expect("Notify 数组");
        assert_eq!(monitors.len(), 2, "两块屏都要在同一帧里：{json}");
        assert_eq!(monitors[0]["id"], 0);
        assert_eq!(monitors[1]["id"], 1);
        assert_eq!(monitors[1]["name"], "Snug 手机屏 2");

        // legacy（v0.3.x）同理，Add 也是数组。
        let legacy: serde_json::Value =
            serde_json::from_str(&serialize_legacy_add(&[monitor(), second])).expect("合法 JSON");
        assert_eq!(legacy["Add"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn legacy_add_uses_first_refresh_rate_and_defaults_to_60() {
        let mut m = monitor();
        m.modes[0].refresh_rates.clear();
        let json = serialize_legacy_add(&[m]);
        assert_eq!(
            json,
            r#"{"Add":[{"id":0,"modes":[{"width":1080,"height":1900,"refresh_rate":60}]}]}"#
        );
    }

    #[test]
    fn legacy_protocol_cannot_carry_a_display_name() {
        // v0.3.x 的 VddLegacyMonitor 只有 id/modes，没有 name 字段；带名字建屏
        // 在 legacy 通道上会退化成驱动的默认名（v1.3 C# 的协议定义同样如此）。
        let json = serialize_legacy_add(&[monitor()]);
        assert!(!json.contains("\"name\""), "legacy Add 里不该出现 name：{json}");
        let value: serde_json::Value = serde_json::from_str(&json).expect("合法 JSON");
        assert_eq!(value["Add"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn remove_and_control_frames() {
        assert_eq!(serialize_remove(&[3, 7]), r#"{"Remove":[3,7]}"#);
        assert_eq!(serialize_remove_all(), "\"RemoveAll\"");
        assert_eq!(serialize_state_request(), "\"State\"");
    }

    #[test]
    fn framing_appends_eot() {
        let bytes = frame("{}");
        assert_eq!(bytes, vec![b'{', b'}', EOT]);
        assert_eq!(EOT, 0x04);
    }

    #[test]
    fn parse_state_accepts_state_and_changed() {
        let json = r#"{"State":[{"id":1,"name":"Snug","enabled":true,"modes":[{"width":540,"height":950,"refresh_rates":[60]}]}]}"#;
        let monitors = parse_state_reply(json).expect("state");
        assert_eq!(monitors.len(), 1);
        assert_eq!(monitors[0].id, 1);
        assert_eq!(monitors[0].modes[0].width, 540);

        let changed = json.replace("State", "Changed");
        assert!(parse_state_reply(&changed).is_some());
    }

    #[test]
    fn parse_state_tolerates_other_replies() {
        assert!(parse_state_reply("\"ok\"").is_none());
        assert!(parse_state_reply("not json").is_none());
        assert!(parse_state_reply(r#"{"Other":[]}"#).is_none());
    }

    #[test]
    fn missing_optional_fields_deserialize() {
        let monitors = parse_state_reply(r#"{"State":[{"id":2,"enabled":false}]}"#).expect("state");
        assert_eq!(monitors[0].name, None);
        assert!(monitors[0].modes.is_empty());
    }

    #[test]
    fn pipe_availability_is_false_without_driver() {
        // 真机上没装驱动时必须安全返回 false，不能 panic。
        assert!(!is_driver_pipe_available("snug-rs-test-pipe-that-does-not-exist"));
    }

    /// 真机 bug 的回归：**已连接（实例全忙）的管道必须算"存在"**。
    ///
    /// 只问 `Path::exists()` 的老实现在这里就会答错——元数据查询依赖可用实例，
    /// 而计划任务上下文里它就是这么失败的。答错的代价是白跑一遍驱动安装。
    #[test]
    fn busy_pipe_still_counts_as_available() {
        let name = format!("snug-rs-test-busy-{}", std::process::id());
        let _server = PipeServer::create(&name);
        // 连上一个客户端 → 这个只有 1 个实例的管道就"忙"了。
        let _client = OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe_path(&name))
            .expect("客户端应当能连上测试管道");
        assert!(
            is_driver_pipe_available(&name),
            "管道存在且正忙时不能说驱动没装"
        );
    }

    /// 一个只有 1 个实例的命名管道服务端，只用于测试探测逻辑。
    struct PipeServer(*mut core::ffi::c_void);

    impl PipeServer {
        fn create(name: &str) -> Self {
            Self::create_with_access(name, 3)
        }

        fn create_with_access(name: &str, access: u32) -> Self {
            unsafe extern "system" {
                fn CreateNamedPipeW(
                    lp_name: *const u16,
                    dw_open_mode: u32,
                    dw_pipe_mode: u32,
                    n_max_instances: u32,
                    n_out_buffer_size: u32,
                    n_in_buffer_size: u32,
                    n_default_time_out: u32,
                    lp_security_attributes: *mut core::ffi::c_void,
                ) -> *mut core::ffi::c_void;
            }
            let mut wide: Vec<u16> = format!(r"\\.\pipe\{name}").encode_utf16().collect();
            wide.push(0);
            let handle = unsafe {
                CreateNamedPipeW(wide.as_ptr(), access, 0, 1, 4096, 4096, 0, std::ptr::null_mut())
            };
            assert_ne!(
                handle as isize, -1,
                "CreateNamedPipeW 失败：{}",
                std::io::Error::last_os_error()
            );
            Self(handle)
        }

        fn accept(self) -> std::fs::File {
            use std::os::windows::io::FromRawHandle;
            unsafe extern "system" {
                fn ConnectNamedPipe(pipe: RawHandle, overlapped: *mut core::ffi::c_void) -> i32;
            }
            let connected = unsafe { ConnectNamedPipe(self.0, std::ptr::null_mut()) };
            if connected == 0 {
                // A client can connect and close before this test thread calls
                // ConnectNamedPipe. Windows reports ERROR_NO_DATA (232) for
                // that race, while ERROR_PIPE_CONNECTED (535) means it is
                // still connected and ready for the test read.
                let code = std::io::Error::last_os_error().raw_os_error();
                assert!(
                    matches!(code, Some(232 | 535)),
                    "unexpected pipe connect error: {code:?}"
                );
            }
            let server = std::mem::ManuallyDrop::new(self);
            unsafe { std::fs::File::from_raw_handle(server.0) }
        }
    }

    impl Drop for PipeServer {
        fn drop(&mut self) {
            unsafe extern "system" {
                fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
            }
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn read_test_frame(pipe: &mut std::fs::File) -> Vec<u8> {
        let mut frame = Vec::new();
        loop {
            let mut byte = [0];
            pipe.read_exact(&mut byte).unwrap();
            frame.push(byte[0]);
            if byte[0] == EOT { return frame; }
        }
    }

    #[test]
    fn direct_v2_write_times_out_and_discards_the_connection() {
        use std::sync::mpsc;
        let name = format!("snug-rs-test-stalled-write-{}", std::process::id());
        let server_name = name.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let server = PipeServer::create(&server_name);
            ready_tx.send(()).unwrap();
            let _pipe = server.accept();
            // The fallback close bounds this regression even against the old blocking writer.
            let _ = release_rx.recv_timeout(Duration::from_secs(8));
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut client = VddClient::new(name);
        let mut spec = monitor();
        spec.name = Some("stalled-test".repeat(8192));
        let started = Instant::now();
        let result = client.notify(&[spec]);
        let elapsed = started.elapsed();
        let _ = release_tx.send(());
        server.join().unwrap();
        assert!(matches!(result, Err(VddError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut));
        assert!(elapsed < Duration::from_secs(5), "write exceeded its deadline: {elapsed:?}");
        assert!(client.protocol().is_none(), "a partial timed-out frame must not be reused");
    }

    #[test]
    fn direct_v2_write_preserves_a_frame_larger_than_the_pipe_buffer() {
        use std::sync::mpsc;
        let name = format!("snug-rs-test-large-write-{}", std::process::id());
        let server_name = name.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let server = PipeServer::create(&server_name);
            ready_tx.send(()).unwrap();
            let mut pipe = server.accept();
            let received = read_test_frame(&mut pipe);
            pipe.write_all(&frame(r#"{"SnugUpdate":"Applied"}"#)).unwrap();
            received
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut spec = monitor();
        spec.name = Some("large-test".repeat(1024));
        let expected = frame(&serialize_update(&[spec.clone()], false));
        VddClient::new(name).notify(&[spec]).unwrap();
        assert_eq!(server.join().unwrap(), expected);
    }

    #[test]
    fn old_v2_driver_is_rejected_without_sending_a_destructive_notify() {
        use std::sync::mpsc;
        let name = format!("snug-rs-test-old-v2-{}", std::process::id());
        let server_name = name.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let server = PipeServer::create(&server_name);
            ready_tx.send(()).unwrap();
            let mut pipe = server.accept();
            let request = read_test_frame(&mut pipe);
            // Unextended V2 silently ignores unknown commands. Other-client events
            // and State replies must not be mistaken for the missing update ACK.
            pipe.write_all(&frame(r#"{"Changed":[]}"#)).unwrap();
            pipe.write_all(&frame(r#"{"State":[]}"#)).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(8));
            request
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut client = VddClient::new(name);
        let result = client.notify(&[monitor()]);
        release_tx.send(()).unwrap();
        assert_eq!(server.join().unwrap(), frame(&serialize_update(&[monitor()], false)));
        assert!(matches!(result, Err(VddError::OutcomeUnknown { source }) if source.kind() == std::io::ErrorKind::TimedOut));
        assert!(client.protocol().is_none(), "a missing ACK must invalidate the connection");
    }

    #[test]
    fn legacy_connection_rejects_mutations_without_writing_to_the_pipe() {
        use std::io::Read;
        use std::sync::mpsc;
        let name = format!("snug-rs-test-legacy-write-{}", std::process::id());
        let server_name = name.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let server = PipeServer::create_with_access(&server_name, 1);
            ready_tx.send(()).unwrap();
            let mut pipe = server.accept();
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).unwrap();
            bytes
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut client = VddClient::new(name);
        assert!(matches!(client.notify(&[monitor()]), Err(VddError::Io(error))
            if error.kind() == std::io::ErrorKind::Unsupported));
        assert_eq!(client.protocol(), Some(VddProtocol::Legacy));
        drop(client);
        assert!(server.join().unwrap().is_empty(), "Legacy mutation wrote unguarded bytes");
    }

    #[test]
    fn legacy_remove_is_rejected_without_writing_to_the_pipe() {
        use std::sync::mpsc;
        let name = format!("snug-rs-test-legacy-remove-{}", std::process::id());
        let server_name = name.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let server = PipeServer::create_with_access(&server_name, 1);
            ready_tx.send(()).unwrap();
            let mut pipe = server.accept();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut pipe, &mut bytes).unwrap();
            bytes
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut client = VddClient::new(name);
        let outcome = client.remove(&[0]);
        assert!(matches!(outcome, Err(VddError::Io(error))
            if error.kind() == std::io::ErrorKind::Unsupported));
        drop(client);
        assert!(server.join().unwrap().is_empty(), "Legacy Remove wrote unguarded bytes");
    }

    #[test]
    fn direct_v2_incomplete_reply_times_out_and_discards_the_connection() {
        use std::sync::mpsc;
        let name = format!("snug-rs-test-stalled-reply-{}", std::process::id());
        let server_name = name.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let server = PipeServer::create(&server_name);
            ready_tx.send(()).unwrap();
            let mut pipe = server.accept();
            assert_eq!(read_test_frame(&mut pipe), frame("\"State\""));
            pipe.write_all(b"{\"State\":[]}").unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(8));
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut client = VddClient::new(name);
        let started = Instant::now();
        let result = client.state();
        let elapsed = started.elapsed();
        let _ = release_tx.send(());
        server.join().unwrap();
        assert!(matches!(result, Err(VddError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut));
        assert!(elapsed < Duration::from_secs(5));
        assert!(client.protocol().is_none());
    }

    #[test]
    fn direct_v2_tokenless_operations_write_no_bytes_to_a_real_pipe() {
        use std::sync::mpsc;
        let name = format!("snug-rs-test-tokenless-v2-{}", std::process::id());
        let server_name = name.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let server = PipeServer::create(&server_name);
            ready_tx.send(()).unwrap();
            let mut pipe = server.accept();
            let mut byte = [0];
            match pipe.read(&mut byte) {
                Ok(0) => (),
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => (),
                result => panic!("unguarded V2 operation wrote bytes: {result:?}, {byte:?}"),
            }
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut client = VddClient::new(name);
        let replacing = client.replace_legacy(&[0], &[monitor()]);
        let removing = client.remove(&[0]);
        drop(client);
        server.join().unwrap();
        for result in [replacing, removing.map(|_| ())] {
            assert!(matches!(result, Err(VddError::Io(error))
                if error.kind() == std::io::ErrorKind::Unsupported
                    && error.to_string().contains("归属令牌")));
        }
    }

    #[test]
    fn direct_v2_owned_release_waits_for_a_matching_driver_acknowledgement() {
        use std::sync::mpsc;
        let name = format!("snug-rs-test-owned-release-{}", std::process::id());
        let server_name = name.clone();
        let owner = "0123456789abcdef0123456789abcdef";
        let (ready_tx, ready_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let server = PipeServer::create(&server_name);
            ready_tx.send(()).unwrap();
            let mut pipe = server.accept();
            let request = frame(&serde_json::json!({ "SnugRelease": {"owner": owner, "ids": [0]} }).to_string());
            for result in ["ApplyFailed", "Applied"] {
                assert_eq!(read_test_frame(&mut pipe), request);
                pipe.write_all(&frame(&serde_json::json!({ "SnugRelease": {"owner": owner, "result": result} }).to_string())).unwrap();
            }
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut client = VddClient::new(name);
        assert!(matches!(client.remove_owned(&[0], owner), Err(VddError::Io(error))
            if error.kind() == std::io::ErrorKind::Other));
        assert_eq!(client.remove_owned(&[0], owner).unwrap(), RemoveDisposition::Confirmed);
        server.join().unwrap();
    }

    // ----- 连接缓存与重连（假管道，绝不碰真驱动） -----

    #[derive(Debug, Default)]
    struct FakeWorld {
        protocol: Option<VddProtocol>,
        /// 对端是否还在；`false` 模拟驱动重启后旧管道实例消失。
        alive: bool,
        connects: usize,
        writes: Vec<Vec<u8>>,
        replies: std::collections::VecDeque<Vec<u8>>,
        read_error: Option<std::io::ErrorKind>,
        /// 接下来多少次写要失败（模拟 `ERROR_BROKEN_PIPE`）。
        fail_writes: usize,
    }

    struct FakePipe(Arc<Mutex<FakeWorld>>);

    impl Read for FakePipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut world = self.0.lock().unwrap();
            if let Some(error) = world.read_error {
                return Err(error.into());
            }
            let Some(mut reply) = world.replies.pop_front() else { return Ok(0) };
            let length = reply.len().min(buf.len());
            buf[..length].copy_from_slice(&reply[..length]);
            if length < reply.len() {
                world.replies.push_front(reply.split_off(length));
            }
            Ok(length)
        }
    }

    impl Write for FakePipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut world = self.0.lock().unwrap_or_else(|error| error.into_inner());
            if world.fail_writes > 0 {
                world.fail_writes -= 1;
                return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "假管道断线"));
            }
            world.writes.push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl PipeHandle for FakePipe {
        fn probe(&mut self) -> Liveness {
            if self.0.lock().unwrap_or_else(|error| error.into_inner()).alive {
                Liveness::Alive
            } else {
                Liveness::Dead
            }
        }
    }

    fn fake_world(protocol: VddProtocol) -> Arc<Mutex<FakeWorld>> {
        let mut world = FakeWorld { protocol: Some(protocol), alive: true, ..Default::default() };
        if protocol == VddProtocol::V2 {
            world.replies.extend((0..2).map(|_| frame(r#"{"SnugUpdate":"Applied"}"#)));
        }
        Arc::new(Mutex::new(world))
    }

    fn fake_client(world: &Arc<Mutex<FakeWorld>>) -> VddClient {
        let shared = world.clone();
        VddClient::with_connector(
            "snug-rs-test-pipe",
            Box::new(move |_name| {
                let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
                state.connects += 1;
                state.alive = true;
                let protocol = state.protocol.unwrap_or(VddProtocol::V2);
                Ok(Connection { protocol, pipe: Box::new(FakePipe(shared.clone())) })
            }),
        )
    }

    #[test]
    fn response_size_accepts_maximum_legal_disabled_state() {
        let monitors = maximum_test_state();
        let reply = frame(&serde_json::json!({"State": monitors}).to_string());
        assert!(reply.len() > 128 * 1024);
        assert!(reply.len() <= MAX_STATE_RESPONSE_BYTES);
        println!("maximum legal State: {} bytes; response cap: {}", reply.len(), MAX_STATE_RESPONSE_BYTES);
        let world = fake_world(VddProtocol::V2);
        world.lock().unwrap().replies = [reply].into();
        assert_eq!(fake_client(&world).state().unwrap(), Some(monitors));
    }

    #[test]
    fn response_size_accepts_exact_frame_limit_without_counting_eot() {
        let mut reply = br#"{"State":[]}"#.to_vec();
        reply.resize(MAX_STATE_RESPONSE_BYTES, b' ');
        reply.push(EOT);
        let world = fake_world(VddProtocol::V2);
        world.lock().unwrap().replies = [reply].into();
        assert_eq!(fake_client(&world).state().unwrap(), Some(Vec::new()));
    }

    #[test]
    fn response_size_rejects_oversized_state_and_changed_frames() {
        for key in ["State", "Changed"] {
            let mut reply = serde_json::to_vec(&serde_json::json!({key: []})).unwrap();
            reply.resize(MAX_STATE_RESPONSE_BYTES + 1, b' ');
            reply.push(EOT);
            let world = fake_world(VddProtocol::V2);
            world.lock().unwrap().replies = [reply, frame(r#"{"State":[]}"#)].into();
            let mut client = fake_client(&world);
            assert!(matches!(client.state(), Err(VddError::Io(error))
                if error.kind() == std::io::ErrorKind::InvalidData));
            assert!(client.protocol().is_none());
        }
    }

    #[test]
    fn response_size_counts_changed_notifications_per_frame() {
        let monitors = maximum_test_state()[..4].to_vec();
        let changed = frame(&serde_json::json!({"Changed": monitors}).to_string());
        assert!(changed.len() < 128 * 1024);
        assert!(changed.len() * 5 > 300_000);
        let mut replies = vec![changed; 5];
        replies.push(frame(&serde_json::json!({"State": monitors}).to_string()));
        let world = fake_world(VddProtocol::V2);
        world.lock().unwrap().replies = replies.into();
        assert_eq!(fake_client(&world).state().unwrap(), Some(monitors));
    }

    #[test]
    fn write_failure_drops_the_connection_and_the_next_call_reconnects() {
        // Rust 特有 bug 的回归：旧实现只 `is_some()`，写失败也不重置，
        // 同进程内重试永远复用死连接。
        let world = fake_world(VddProtocol::V2);
        let mut client = fake_client(&world);
        client.notify(&[monitor()]).expect("第一次写成功");
        assert_eq!(world.lock().unwrap().connects, 1);

        world.lock().unwrap().fail_writes = 1;
        assert!(client.notify(&[monitor()]).is_err(), "v2 写失败把错误交给调用方");
        assert_eq!(world.lock().unwrap().connects, 1, "v2 不立刻重写（C# 同）");
        assert!(client.protocol().is_none(), "写失败后必须丢掉缓存连接");

        client.notify(&[monitor()]).expect("下一次调用应重新连接并成功");
        assert_eq!(world.lock().unwrap().connects, 2);
        assert_eq!(world.lock().unwrap().writes.len(), 2);
    }

    #[test]
    fn legacy_notify_rejects_before_write_or_retry() {
        let world = fake_world(VddProtocol::Legacy);
        let mut client = fake_client(&world);
        world.lock().unwrap().fail_writes = 1;

        assert!(matches!(client.notify(&[monitor()]), Err(VddError::Io(error))
            if error.kind() == std::io::ErrorKind::Unsupported));
        let state = world.lock().unwrap();
        assert_eq!(state.connects, 1, "拒绝 Legacy 写入前不得重连");
        assert!(state.writes.is_empty());
        assert_eq!(client.protocol(), Some(VddProtocol::Legacy));
    }

    #[test]
    fn legacy_replacement_stops_before_any_mutation_when_remove_reconnects_as_v2() {
        let world = fake_world(VddProtocol::Legacy);
        let mut client = fake_client(&world);
        assert_eq!(client.ensure().unwrap(), VddProtocol::Legacy);
        {
            let mut state = world.lock().unwrap();
            state.protocol = Some(VddProtocol::V2);
            state.fail_writes = 1;
        }

        assert!(client.replace_legacy(&[0], &[monitor()]).is_err());
        assert_eq!(client.protocol(), Some(VddProtocol::Legacy));
        let state = world.lock().unwrap();
        assert_eq!(state.connects, 1, "unsupported Legacy mutation must not reconnect");
        assert!(state.writes.is_empty(), "an unguarded Remove must not be replayed");
    }

    #[test]
    fn owned_protocol_reserves_creates_replaces_and_releases_with_the_same_token() {
        let token = "0123456789abcdef0123456789abcdef";
        let world = fake_world(VddProtocol::V2);
        world.lock().unwrap().replies = ["SnugReserve", "SnugCreate", "SnugReplace", "SnugRelease"]
            .map(|command| frame(&serde_json::json!({command: {"owner": token, "result": "Applied"}}).to_string())).into();
        let mut client = fake_client(&world);
        assert!(client.reserve_owned(0, token).unwrap());
        client.create_owned(&[monitor()], token).unwrap();
        client.replace_owned(&[monitor()], token).unwrap();
        assert_eq!(client.remove_owned(&[0], token).unwrap(), RemoveDisposition::Confirmed);
        assert_eq!(world.lock().unwrap().writes, [
            serde_json::json!({"SnugReserve": {"owner": token, "id": 0}}),
            serde_json::json!({"SnugCreate": {"owner": token, "monitors": [monitor()]}}),
            serde_json::json!({"SnugReplace": {"owner": token, "monitors": [monitor()]}}),
            serde_json::json!({"SnugRelease": {"owner": token, "ids": [0]}}),
        ].map(|request| frame(&request.to_string())));
    }

    #[test]
    fn owned_release_distinguishes_a_foreign_id_from_a_detaching_owned_screen() {
        let token = "0123456789abcdef0123456789abcdef";
        let world = fake_world(VddProtocol::V2);
        world.lock().unwrap().replies = [frame(&serde_json::json!({"SnugRelease": {
            "owner": token, "result": "NotFound",
        }}).to_string())].into();
        let mut client = fake_client(&world);
        assert_eq!(client.remove_owned(&[0], token).unwrap(), RemoveDisposition::NotOwned);
        assert_eq!(client.protocol(), Some(VddProtocol::V2));
        let state = world.lock().unwrap();
        assert_eq!(state.connects, 1);
        assert_eq!(state.writes, [frame(&serde_json::json!({"SnugRelease": {
            "owner": token, "ids": [0],
        }}).to_string())]);
    }

    #[test]
    fn only_a_matching_release_not_found_reply_can_confirm_absent_ownership() {
        let token = "0123456789abcdef0123456789abcdef";
        for command in ["SnugReserve", "SnugCreate", "SnugReplace"] {
            let world = fake_world(VddProtocol::V2);
            world.lock().unwrap().replies = [frame(&serde_json::json!({command: {
                "owner": token, "result": "NotFound",
            }}).to_string())].into();
            let mut client = fake_client(&world);
            let result = match command {
                "SnugReserve" => client.reserve_owned(0, token).map(|_| ()),
                "SnugCreate" => client.create_owned(&[monitor()], token),
                _ => client.replace_owned(&[monitor()], token),
            };
            assert!(matches!(result, Err(VddError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound));
        }
        let world = fake_world(VddProtocol::V2);
        world.lock().unwrap().replies = [frame(&serde_json::json!({"SnugRelease": {
            "owner": "fedcba9876543210fedcba9876543210", "result": "NotFound",
        }}).to_string())].into();
        let mut client = fake_client(&world);
        assert!(matches!(client.remove_owned(&[0], token),
            Err(VddError::OutcomeUnknown { source }) if source.kind() == std::io::ErrorKind::InvalidData));
        assert!(client.protocol().is_none());
    }

    #[test]
    fn owned_protocol_never_downgrades_token_guarded_operations_to_legacy() {
        let token = "0123456789abcdef0123456789abcdef";
        let world = fake_world(VddProtocol::Legacy);
        let mut client = fake_client(&world);
        assert!(!client.reserve_owned(0, token).unwrap());
        for result in [client.create_owned(&[monitor()], token), client.replace_owned(&[monitor()], token),
            client.remove_owned(&[0], token).map(|_| ())] {
            assert!(matches!(result, Err(VddError::Io(error)) if error.kind() == std::io::ErrorKind::Unsupported));
        }
        assert!(world.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn owner_verification_accepts_only_a_matching_read_only_driver_acknowledgement() {
        use std::sync::mpsc;
        let owner = "0123456789abcdef0123456789abcdef";
        let other = "fedcba9876543210fedcba9876543210";
        let acknowledgement = |command: &str, token: &str, result: &str| {
            frame(&serde_json::json!({command: {"owner": token, "result": result}}).to_string())
        };
        for (index, reply, expected) in [
            (0, Some(acknowledgement("SnugCheck", owner, "Applied")), true),
            // Both a missing display and a display held by another owner are NotFound.
            (1, Some(acknowledgement("SnugCheck", owner, "NotFound")), false),
            (2, Some(acknowledgement("SnugCheck", owner, "AlreadyExists")), false),
            (3, Some(acknowledgement("SnugCheck", other, "Applied")), false),
            (4, Some(acknowledgement("SnugReserve", owner, "Applied")), false),
            (5, Some(acknowledgement("SnugCheck", owner, "Unknown")), false),
            (6, None, false),
        ] {
            let name = format!("snug-rs-test-owner-check-{}-{index}", std::process::id());
            let server_name = name.clone();
            let (ready_tx, ready_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let server = std::thread::spawn(move || {
                let server = PipeServer::create(&server_name);
                ready_tx.send(()).unwrap();
                let mut pipe = server.accept();
                let request = read_test_frame(&mut pipe);
                if let Some(reply) = reply { pipe.write_all(&reply).unwrap(); }
                // Keep the pipe open so an absent or unrelated ACK exercises the deadline.
                let _ = release_rx.recv_timeout(Duration::from_secs(8));
                assert_eq!(available_bytes(pipe.as_raw_handle()).unwrap_or(0), 0,
                    "ownership verification must send exactly one read-only request");
                request
            });
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let mut client = VddClient::direct(name);
            let started = Instant::now();
            let result = client.verify_owner(0, Some(owner));
            let elapsed = started.elapsed();
            let _ = release_tx.send(());
            assert_eq!(server.join().unwrap(), frame(&serde_json::json!({
                "SnugCheck": {"owner": owner, "id": 0},
            }).to_string()));
            assert_eq!(result.is_ok(), expected, "case {index}: {result:?}");
            assert!(elapsed < Duration::from_secs(5), "unbounded ownership check: {elapsed:?}");
            if index >= 3 { assert!(client.protocol().is_none()); }
        }
    }

    #[test]
    fn owner_verification_is_protocol_guarded_without_legacy_writes() {
        for (protocol, owner, expected) in [
            (VddProtocol::Legacy, None, false),
            (VddProtocol::Legacy, Some("0123456789abcdef0123456789abcdef"), false),
            (VddProtocol::V2, None, false),
        ] {
            let world = fake_world(protocol);
            let mut client = fake_client(&world);
            let result = client.verify_owner(0, owner);
            assert_eq!(result.is_ok(), expected);
            if !expected {
                assert!(matches!(result, Err(VddError::Io(error))
                    if error.kind() == std::io::ErrorKind::Unsupported));
            }
            assert!(world.lock().unwrap().writes.is_empty());
        }
    }

    #[test]
    fn owner_verification_transport_errors_never_retry_the_request() {
        let owner = "0123456789abcdef0123456789abcdef";
        for write_failure in [false, true] {
            let world = fake_world(VddProtocol::V2);
            {
                let mut state = world.lock().unwrap();
                state.replies.clear();
                state.read_error = Some(std::io::ErrorKind::BrokenPipe);
                state.fail_writes = usize::from(write_failure);
            }
            let mut client = fake_client(&world);
            assert!(matches!(client.verify_owner(0, Some(owner)), Err(VddError::Io(error))
                if error.kind() == std::io::ErrorKind::Unsupported));
            assert!(client.protocol().is_none());
            let state = world.lock().unwrap();
            assert_eq!(state.connects, 1);
            assert_eq!(state.writes.len(), usize::from(!write_failure));
            if !write_failure {
                assert_eq!(state.writes[0], frame(&serde_json::json!({
                    "SnugCheck": {"owner": owner, "id": 0},
                }).to_string()));
            }
        }
    }

    #[test]
    fn owner_verification_broker_execution_preserves_protocol_and_rejections() {
        let owner = "0123456789abcdef0123456789abcdef";
        for (protocol, token, result, expected) in [
            (VddProtocol::V2, Some(owner), "Applied", true),
            (VddProtocol::V2, Some(owner), "NotFound", false),
            (VddProtocol::V2, None, "Applied", false),
            (VddProtocol::Legacy, None, "Applied", false),
            (VddProtocol::Legacy, Some(owner), "Applied", false),
        ] {
            let world = fake_world(protocol);
            world.lock().unwrap().replies = [frame(&serde_json::json!({
                "SnugCheck": {"owner": owner, "result": result},
            }).to_string())].into();
            let request = crate::vdd_broker::Request::VerifyOwner { id: 0, owner: token.map(str::to_owned) };
            let encoded = serde_json::to_vec(&request).unwrap();
            let request = serde_json::from_slice(&encoded).unwrap();
            let reply = serde_json::to_value(crate::vdd_broker::execute(request, &mut fake_client(&world))).unwrap();
            assert_eq!(reply["error"].is_null(), expected);
            if expected { assert_eq!(reply["legacy"], protocol == VddProtocol::Legacy); }
            let state = world.lock().unwrap();
            if protocol == VddProtocol::V2 && token.is_some() {
                assert_eq!(state.writes, [frame(&serde_json::json!({
                    "SnugCheck": {"owner": owner, "id": 0},
                }).to_string())]);
            } else {
                assert!(state.writes.is_empty());
            }
        }
    }

    #[test]
    fn owned_protocol_rejects_missing_or_mismatched_reservation_acknowledgements() {
        let token = "0123456789abcdef0123456789abcdef";
        for reply in [None, Some(serde_json::json!({"SnugReserve": {"owner": "other-owner", "result": "Applied"}}))] {
            let world = fake_world(VddProtocol::V2);
            world.lock().unwrap().replies = reply.into_iter().map(|value| frame(&value.to_string())).collect();
            let mut client = fake_client(&world);
            assert!(matches!(client.reserve_owned(0, token),
                Err(VddError::Io(error)) if error.kind() == std::io::ErrorKind::Unsupported));
            assert!(client.protocol().is_none());
            assert_eq!(world.lock().unwrap().writes.len(), 1);
        }
    }

    #[test]
    fn owned_protocol_missing_mutation_acknowledgements_never_confirm_ownership() {
        let token = "0123456789abcdef0123456789abcdef";
        for failure in [None, Some(std::io::ErrorKind::TimedOut)] {
            for command in ["SnugCreate", "SnugReplace", "SnugRelease"] {
                let world = fake_world(VddProtocol::V2);
                {
                    let mut state = world.lock().unwrap();
                    state.replies.clear();
                    state.read_error = failure;
                }
                let mut client = fake_client(&world);
                let result = match command {
                    "SnugCreate" => client.create_owned(&[monitor()], token),
                    "SnugReplace" => client.replace_owned(&[monitor()], token),
                    _ => client.remove_owned(&[0], token).map(|_| ()),
                };
                assert!(matches!(result, Err(VddError::OutcomeUnknown { source })
                    if source.kind() == failure.unwrap_or(std::io::ErrorKind::BrokenPipe)));
                assert!(client.protocol().is_none());
                let state = world.lock().unwrap();
                assert_eq!(state.connects, 1);
                assert_eq!(state.writes.len(), 1, "must not replay an unknown mutation");
            }
        }
    }

    #[test]
    fn owned_protocol_only_accepts_a_mutation_acknowledgement_for_its_token() {
        let token = "0123456789abcdef0123456789abcdef";
        for result in ["Applied", "AlreadyExists"] {
            let world = fake_world(VddProtocol::V2);
            world.lock().unwrap().replies = [frame(&serde_json::json!({"SnugCreate": {
                "owner": "fedcba9876543210fedcba9876543210", "result": result,
            }}).to_string())].into();
            let mut client = fake_client(&world);
            assert!(matches!(client.create_owned(&[monitor()], token),
                Err(VddError::OutcomeUnknown { source }) if source.kind() == std::io::ErrorKind::InvalidData));
            assert!(client.protocol().is_none());
        }
        let world = fake_world(VddProtocol::V2);
        world.lock().unwrap().replies = [frame(&serde_json::json!({"SnugCreate": {
            "owner": token, "result": "AlreadyExists",
        }}).to_string())].into();
        let mut client = fake_client(&world);
        assert!(matches!(client.create_owned(&[monitor()], token),
            Err(VddError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists));
        assert_eq!(client.protocol(), Some(VddProtocol::V2));
    }

    #[test]
    fn dead_connection_is_probed_and_replaced_before_writing() {
        // 模拟 pnputil /restart-device：驱动重启，旧管道实例没了。
        let world = fake_world(VddProtocol::V2);
        let mut client = fake_client(&world);
        client.notify(&[monitor()]).expect("先建立连接");

        world.lock().unwrap().alive = false;
        client
            .notify(&[monitor()])
            .expect("探测到断线后应静默重连，而不是把错误抛给调用方");

        let state = world.lock().unwrap();
        assert_eq!(state.connects, 2, "探测到断线必须重新 CreateFile");
        assert_eq!(state.writes.len(), 2);
    }

    #[test]
    fn v2_update_sends_only_target_ids_and_waits_for_its_acknowledgement() {
        let world = fake_world(VddProtocol::V2);
        let changed = frame("{\"Changed\":[]}");
        let reply = frame(r#"{"SnugUpdate":"Applied"}"#);
        world.lock().unwrap().replies = [changed, reply[..7].to_vec(), reply[7..].to_vec()].into();
        fake_client(&world).notify(&[monitor()]).unwrap();
        assert_eq!(world.lock().unwrap().writes, vec![frame(&serialize_update(&[monitor()], false))]);
    }

    #[test]
    fn v2_never_reports_success_when_the_atomic_reply_is_incomplete() {
        let world = fake_world(VddProtocol::V2);
        world.lock().unwrap().replies = [br#"{"SnugUpdate":"Applied"}"#.to_vec()].into();
        let mut client = fake_client(&world);
        assert!(client.notify(&[monitor()]).is_err());
        assert!(client.protocol().is_none());
        assert_eq!(world.lock().unwrap().writes, vec![frame(&serialize_update(&[monitor()], false))]);
    }

    #[test]
    fn v2_applied_update_with_lost_acknowledgement_has_an_unknown_outcome() {
        struct AppliedPipe {
            applied: Arc<Mutex<usize>>,
            failure: std::io::ErrorKind,
        }
        impl Read for AppliedPipe {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                if self.failure == std::io::ErrorKind::BrokenPipe {
                    Ok(0)
                } else {
                    Err(self.failure.into())
                }
            }
        }
        impl Write for AppliedPipe {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                assert_eq!(bytes.last(), Some(&EOT));
                let request = serde_json::from_slice::<serde_json::Value>(&bytes[..bytes.len() - 1]).unwrap();
                assert!(request.get("SnugUpdate").is_some() || request.get("SnugReplace").is_some());
                *self.applied.lock().unwrap() += 1;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        impl PipeHandle for AppliedPipe {
            fn probe(&mut self) -> Liveness { Liveness::Alive }
        }

        for failure in [std::io::ErrorKind::BrokenPipe, std::io::ErrorKind::TimedOut] {
            for replacing in [false, true] {
                let applied = Arc::new(Mutex::new(0));
                let mut client = VddClient::with_connector("fake-applied-update", Box::new(|_| {
                    panic!("an unknown mutation must not reconnect or replay itself")
                }));
                client.connection = Some(Connection {
                    protocol: VddProtocol::V2,
                    pipe: Box::new(AppliedPipe { applied: applied.clone(), failure }),
                });
                let result = if replacing {
                    client.replace_owned(&[monitor()], "0123456789abcdef0123456789abcdef")
                } else {
                    client.notify(&[monitor()])
                };
                assert!(matches!(result,
                    Err(VddError::OutcomeUnknown { source }) if source.kind() == failure));
                assert_eq!(*applied.lock().unwrap(), 1);
                assert!(client.protocol().is_none());
            }
        }
    }

    #[test]
    fn v2_creation_propagates_an_atomic_id_conflict() {
        let world = fake_world(VddProtocol::V2);
        world.lock().unwrap().replies = [frame(r#"{"SnugUpdate":"AlreadyExists"}"#)].into();
        assert!(matches!(fake_client(&world).notify(&[monitor()]),
            Err(VddError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists));
        assert_eq!(world.lock().unwrap().writes, vec![frame(&serialize_update(&[monitor()], false))]);
    }

    #[test]
    fn v2_owned_replacement_requires_an_acknowledgement_for_its_token() {
        let owner = "0123456789abcdef0123456789abcdef";
        let world = fake_world(VddProtocol::V2);
        let reply = |result| frame(&serde_json::json!({"SnugReplace": {"owner": owner, "result": result}}).to_string());
        world.lock().unwrap().replies = [reply("Applied"), reply("NotFound")].into();
        let mut client = fake_client(&world);
        client.replace_owned(&[monitor()], owner).unwrap();
        let request = frame(&serde_json::json!({"SnugReplace": {"owner": owner, "monitors": [monitor()]}}).to_string());
        assert_eq!(world.lock().unwrap().writes, vec![request]);
        assert!(matches!(client.replace_owned(&[monitor()], owner),
            Err(VddError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound));
    }

    #[test]
    fn v2_owned_replacement_reports_native_failure_without_replaying_or_disconnecting() {
        let owner = "0123456789abcdef0123456789abcdef";
        let world = fake_world(VddProtocol::V2);
        world.lock().unwrap().replies = [frame(&serde_json::json!({"SnugReplace": {"owner": owner, "result": "ApplyFailed"}}).to_string())].into();
        let mut client = fake_client(&world);
        assert!(matches!(client.replace_owned(&[monitor()], owner),
            Err(VddError::Io(error)) if error.kind() == std::io::ErrorKind::Other));
        assert_eq!(client.protocol(), Some(VddProtocol::V2));
        assert_eq!(world.lock().unwrap().writes, vec![frame(&serde_json::json!({"SnugReplace": {"owner": owner, "monitors": [monitor()]}}).to_string())]);
    }

    #[test]
    fn v2_unknown_or_invalid_acknowledgements_never_mean_success() {
        for ack in [r#""Unknown""#, "true", "null"] {
            let world = fake_world(VddProtocol::V2);
            world.lock().unwrap().replies = [frame(&format!("{{\"SnugUpdate\":{ack}}}"))].into();
            let mut client = fake_client(&world);
            assert!(matches!(client.notify(&[monitor()]),
                Err(VddError::OutcomeUnknown { source }) if source.kind() == std::io::ErrorKind::InvalidData));
            assert!(client.protocol().is_none(), "an invalid ACK must invalidate the connection");
        }
    }

    #[test]
    fn concurrent_v2_clients_preserve_displays_created_after_a_stale_snapshot() {
        use std::collections::VecDeque;
        use std::sync::mpsc;

        struct StatePipe {
            state: Arc<Mutex<Vec<VddMonitor>>>,
            reply: VecDeque<u8>,
            pause_after_state: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
        }
        impl Read for StatePipe {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let length = buf.len().min(self.reply.len());
                for byte in &mut buf[..length] {
                    *byte = self.reply.pop_front().unwrap();
                }
                Ok(length)
            }
        }
        impl Write for StatePipe {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                assert_eq!(bytes.last(), Some(&EOT));
                let command: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
                if command == "State" {
                    let snapshot = self.state.lock().unwrap().clone();
                    self.reply.extend(frame(&serde_json::json!({"State": snapshot}).to_string()));
                    if let Some((ready, resume)) = self.pause_after_state.take() {
                        ready.send(()).unwrap();
                        resume.recv_timeout(Duration::from_secs(3)).unwrap();
                    }
                } else if let Some(update) = command.get("SnugUpdate") {
                    if let Some((ready, resume)) = self.pause_after_state.take() {
                        ready.send(()).unwrap();
                        resume.recv_timeout(Duration::from_secs(3)).unwrap();
                    }
                    let updates: Vec<VddMonitor> = serde_json::from_value(update["monitors"].clone()).unwrap();
                    let mut state = self.state.lock().unwrap();
                    assert!(!updates.iter().any(|u| state.iter().any(|m| m.id == u.id)));
                    state.extend(updates);
                    self.reply.extend(frame(r#"{"SnugUpdate":"Applied"}"#));
                } else if let Some(notify) = command.get("Notify") {
                    // Driver ipc.rs::notify removes every monitor absent from the full list.
                    *self.state.lock().unwrap() = serde_json::from_value(notify.clone()).unwrap();
                } else {
                    panic!("unexpected command: {command}");
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        impl PipeHandle for StatePipe {
            fn probe(&mut self) -> Liveness { Liveness::Alive }
        }
        fn client(pipe: StatePipe) -> VddClient {
            VddClient {
                name: "isolated-state-model".into(),
                connection: Some(Connection { protocol: VddProtocol::V2, pipe: Box::new(pipe) }),
                connector: Box::new(|_| panic!("unexpected reconnect")),
                use_broker: false,
                broker_protocol: None,
            }
        }

        let state = Arc::new(Mutex::new(vec![monitor()]));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let mut a = client(StatePipe {
            state: state.clone(), reply: VecDeque::new(),
            pause_after_state: Some((ready_tx, resume_rx)),
        });
        let a_thread = std::thread::spawn(move || {
            a.notify(&[VddMonitor { id: 2, ..monitor() }]).unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut b = client(StatePipe {
            state: state.clone(), reply: VecDeque::new(), pause_after_state: None,
        });
        b.notify(&[VddMonitor { id: 1, ..monitor() }]).unwrap();
        assert_eq!(state.lock().unwrap().iter().map(|m| m.id).collect::<Vec<_>>(), vec![0, 1]);
        resume_tx.send(()).unwrap();
        a_thread.join().unwrap();
        let final_ids = state.lock().unwrap().iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(final_ids, vec![0, 1, 2]);
    }

    #[test]
    fn direct_and_broker_removal_reject_legacy_without_reconnecting_or_writing() {
        fn transitioning_client() -> (VddClient, Arc<Mutex<FakeWorld>>) {
            let world = fake_world(VddProtocol::Legacy);
            let mut client = fake_client(&world);
            assert_eq!(client.ensure().unwrap(), VddProtocol::Legacy);
            {
                let mut state = world.lock().unwrap();
                state.protocol = Some(VddProtocol::V2);
                state.fail_writes = 1;
            }
            (client, world)
        }

        let (mut direct, direct_world) = transitioning_client();
        assert!(matches!(direct.remove(&[0]), Err(VddError::Io(error))
            if error.kind() == std::io::ErrorKind::Unsupported));
        assert_eq!(direct.protocol(), Some(VddProtocol::Legacy));
        assert_eq!(direct_world.lock().unwrap().connects, 1);
        assert!(direct_world.lock().unwrap().writes.is_empty());

        let (mut broker_driver, broker_world) = transitioning_client();
        let reply = crate::vdd_broker::execute(
            crate::vdd_broker::Request::Remove { ids: vec![0] },
            &mut broker_driver,
        );
        let reply = serde_json::to_value(reply).unwrap();
        assert_eq!(reply["error_kind"], "Unsupported");
        assert!(reply["removal"].is_null());
        assert_eq!(broker_driver.protocol(), Some(VddProtocol::Legacy));
        assert_eq!(broker_world.lock().unwrap().connects, 1);
        assert!(broker_world.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn broker_reply_without_explicit_removal_never_confirms_deletion() {
        let reply = serde_json::from_str(r#"{"legacy":false,"monitors":null,"error":null}"#).unwrap();
        assert!(remove_disposition(reply).is_err());
    }

    #[test]
    fn tokenless_v2_replace_and_remove_never_write_or_query_the_driver() {
        for replacing in [false, true] {
            let world = fake_world(VddProtocol::V2);
            world.lock().unwrap().replies.push_back(frame(r#"{"State":[]}"#));
            let mut client = fake_client(&world);
            let result = if replacing {
                client.replace_legacy(&[0], &[monitor()])
            } else {
                client.remove(&[0]).map(|_| ())
            };
            assert!(matches!(result, Err(VddError::Io(error))
                if error.kind() == std::io::ErrorKind::Unsupported));
            assert_eq!(client.protocol(), Some(VddProtocol::V2));
            assert!(world.lock().unwrap().writes.is_empty(), "State cannot prove ownership, and no unguarded mutation may be sent");
        }
    }

    #[test]
    fn tokenless_v2_broker_operations_preserve_the_unsupported_error_without_writes() {
        for request in [
            crate::vdd_broker::Request::ReplaceLegacy { remove_ids: vec![0], monitors: vec![monitor()] },
            crate::vdd_broker::Request::Remove { ids: vec![0] },
        ] {
            let world = fake_world(VddProtocol::V2);
            world.lock().unwrap().replies.push_back(frame(r#"{"State":[]}"#));
            let mut client = fake_client(&world);
            let reply = crate::vdd_broker::execute(request, &mut client);
            let reply = serde_json::to_value(reply).unwrap();
            assert_eq!(reply["error_kind"], "Unsupported");
            assert_eq!(reply["unsupported"], true);
            assert!(reply["removal"].is_null());
            assert!(world.lock().unwrap().writes.is_empty());
        }
    }

    #[test]
    fn legacy_replace_and_remove_are_unsupported_without_owner_tokens() {
        let world = fake_world(VddProtocol::Legacy);
        let mut client = fake_client(&world);
        for result in [
            client.replace_legacy(&[0], &[monitor()]),
            client.remove(&[0]).map(|_| ()),
        ] {
            assert!(matches!(result, Err(VddError::Io(error))
                if error.kind() == std::io::ErrorKind::Unsupported));
        }
        assert!(world.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn legacy_broker_mutations_preserve_unsupported_without_writes() {
        let world = fake_world(VddProtocol::Legacy);
        let mut client = fake_client(&world);
        for request in [
            crate::vdd_broker::Request::ReplaceLegacy {
                remove_ids: vec![0], monitors: vec![monitor()],
            },
            crate::vdd_broker::Request::Remove { ids: vec![0] },
        ] {
            let encoded = serde_json::to_vec(&request).unwrap();
            let decoded = serde_json::from_slice(&encoded).unwrap();
            let reply = serde_json::to_value(crate::vdd_broker::execute(decoded, &mut client)).unwrap();
            assert_eq!(reply["error_kind"], "Unsupported");
            assert_eq!(reply["unsupported"], true);
            assert!(reply["error"].is_string());
            assert!(reply["removal"].is_null());
        }
        assert!(world.lock().unwrap().writes.is_empty());
    }

}
