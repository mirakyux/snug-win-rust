//! virtual-display-rs 的命名管道客户端，移植自 C# 版 `SnugWin.Windows.Display.VirtualDisplayIpcClient`。
//!
//! 两套协议并存：
//! - **v2（master）**：双向管道，JSON 帧以 `0x04`(EOT) 结尾，指令为 `Notify` / `Remove` / `RemoveAll` / `State`；
//! - **v0.3.x**：驱动只开 `PIPE_ACCESS_INBOUND`，所以客户端只能写（`PipeDirection.Out`），
//!   指令为 `Add` / `Remove` / `RemoveAll`，并且不带帧尾。
//!
//! 判断顺序与 C# 版一致：先试双向，遇到"拒绝访问"退回只写。

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_PIPE_NAME: &str = "virtualdisplaydriver";
/// v2 帧结束符。
pub const EOT: u8 = 0x04;

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

/// 管道是否可打开（`\\.\pipe\<name>` 存在且能连接）。
pub fn is_driver_pipe_available(name: &str) -> bool {
    pipe_path(name).exists()
}

fn pipe_path(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!(r"\\.\pipe\{name}"))
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
    /// 内部不变量被破坏（公开方法都会先 `ensure()`，正常路径不会出现）。
    #[error("管道尚未连接")]
    NotConnected,
}

/// 协议探测结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VddProtocol {
    /// 双向 + EOT 帧（virtual-display-rs master）。
    V2,
    /// 只写、无帧尾（v0.3.x）。
    Legacy,
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
}

struct PipeFile(std::fs::File);

impl Read for PipeFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for PipeFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl PipeHandle for PipeFile {
    fn probe(&mut self) -> Liveness {
        peek_liveness(self.0.as_raw_handle())
    }
}

/// `PeekNamedPipe` 探测对端是否还在。驱动重启（`pnputil /restart-device`）之后，
/// 旧实例会立刻返回 `ERROR_BROKEN_PIPE`，缓存句柄就不能再用了。
///
/// 这里用一行 `extern` 声明，而不是 `windows` crate 的 `Win32_System_Pipes` feature：
/// 只为一个函数开一个 feature 不划算（Cargo.toml 由 Chief 统一改，能不惊动就不惊动）。
fn peek_liveness(handle: RawHandle) -> Liveness {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_BROKEN_PIPE: i32 = 109;
    const ERROR_PIPE_NOT_CONNECTED: i32 = 233;

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

    let ok = unsafe {
        PeekNamedPipe(
            handle as *mut core::ffi::c_void,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        return Liveness::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(ERROR_ACCESS_DENIED) => Liveness::Unknown,
        Some(ERROR_BROKEN_PIPE) | Some(ERROR_PIPE_NOT_CONNECTED) => Liveness::Dead,
        // 其它错误（无效句柄之类）也当断了，让下一次操作重新连。
        _ => Liveness::Dead,
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
}

impl Default for VddClient {
    fn default() -> Self {
        Self::new(DEFAULT_PIPE_NAME)
    }
}

impl VddClient {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), connection: None, connector: Box::new(connect) }
    }

    /// 单测入口：换掉真正去 `CreateFile` 的连接器。
    #[cfg(test)]
    fn with_connector(name: &str, connector: Connector) -> Self {
        Self { name: name.to_string(), connection: None, connector }
    }

    pub fn protocol(&self) -> Option<VddProtocol> {
        self.connection.as_ref().map(|c| c.protocol)
    }

    pub fn is_legacy(&self) -> bool {
        self.protocol() == Some(VddProtocol::Legacy)
    }

    /// 让驱动的显示器状态变成给定这一组。
    pub fn notify(&mut self, monitors: &[VddMonitor]) -> Result<(), VddError> {
        match self.ensure()? {
            VddProtocol::V2 => self.write_framed(&serialize_notify(monitors)),
            VddProtocol::Legacy => self.write_legacy_retry(&serialize_legacy_add(monitors)),
        }
    }

    /// 先删再加（v0.3.x 需要；v2 直接用 Notify 覆盖）。
    pub fn replace_legacy(
        &mut self,
        remove_ids: &[u32],
        add_monitors: &[VddMonitor],
    ) -> Result<(), VddError> {
        match self.ensure()? {
            VddProtocol::V2 => self.write_framed(&serialize_notify(add_monitors)),
            VddProtocol::Legacy => {
                self.write_legacy_retry(&serialize_remove(remove_ids))?;
                std::thread::sleep(Duration::from_millis(120));
                self.write_legacy_retry(&serialize_legacy_add(add_monitors))
            }
        }
    }

    pub fn remove(&mut self, ids: &[u32]) -> Result<(), VddError> {
        let json = serialize_remove(ids);
        match self.ensure()? {
            VddProtocol::V2 => self.write_framed(&json),
            VddProtocol::Legacy => self.write_legacy_retry(&json),
        }
    }

    pub fn remove_all(&mut self) -> Result<(), VddError> {
        let json = serialize_remove_all();
        match self.ensure()? {
            VddProtocol::V2 => self.write_framed(&json),
            VddProtocol::Legacy => self.write_legacy_retry(&json),
        }
    }

    /// 请求当前状态（仅 v2 支持应答）。
    pub fn state(&mut self) -> Result<Option<Vec<VddMonitor>>, VddError> {
        match self.ensure()? {
            VddProtocol::Legacy => Ok(None),
            VddProtocol::V2 => {
                self.write_framed(&serialize_state_request())?;
                let mut buf = [0u8; 64 * 1024];
                let read_result = {
                    let Some(connection) = self.connection.as_mut() else {
                        return Ok(None);
                    };
                    connection.pipe.read(&mut buf)
                };
                let read = match read_result {
                    Ok(read) => read,
                    Err(error) => {
                        // 读失败同样说明连接死了，丢掉，下次重连。
                        self.connection = None;
                        return Err(VddError::Io(error));
                    }
                };
                if read == 0 {
                    return Ok(None);
                }
                let text = String::from_utf8_lossy(&buf[..read]);
                let text = text.trim_end_matches('\u{4}');
                Ok(parse_state_reply(text))
            }
        }
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

    fn write_raw(&mut self, json: &str) -> Result<(), VddError> {
        self.write_bytes(json.as_bytes())
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

    /// legacy 通道：写失败 → 丢弃 → 重连 → 重写一次（C# `SendLegacyOneShotAsync`）。
    ///
    /// 重连会重新探测协议；真机上仍是 v0.3.x，所以还是 raw 写。若断线期间驱动换成了
    /// v2 实现，则改按 EOT 帧写，避免把裸 JSON 塞进双向协议。
    fn write_legacy_retry(&mut self, json: &str) -> Result<(), VddError> {
        match self.write_raw(json) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.connection = None;
                match self.ensure()? {
                    VddProtocol::Legacy => self.write_raw(json),
                    VddProtocol::V2 => self.write_framed(json),
                }
            }
        }
    }
}

/// 先试双向（v2），"拒绝访问"则退回只写（v0.3.x）；管道忙时重试。
fn connect(name: &str) -> Result<Connection, VddError> {
    let path = pipe_path(name);
    let deadline = Instant::now() + Duration::from_millis(2500);
    let mut first_error: Option<std::io::Error> = None;

    loop {
        match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => {
                return Ok(Connection {
                    protocol: VddProtocol::V2,
                    pipe: Box::new(PipeFile(file)),
                });
            }
            Err(err) => {
                if err.kind() == std::io::ErrorKind::PermissionDenied {
                    // 驱动只开 INBOUND 时，双向打开会被拒；退回只写。
                    if let Ok(file) = OpenOptions::new().write(true).open(&path) {
                        return Ok(Connection {
                            protocol: VddProtocol::Legacy,
                            pipe: Box::new(PipeFile(file)),
                        });
                    }
                    first_error.get_or_insert(err);
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

    // ----- 连接缓存与重连（假管道，绝不碰真驱动） -----

    #[derive(Debug, Default)]
    struct FakeWorld {
        protocol: Option<VddProtocol>,
        /// 对端是否还在；`false` 模拟驱动重启后旧管道实例消失。
        alive: bool,
        connects: usize,
        writes: Vec<Vec<u8>>,
        /// 接下来多少次写要失败（模拟 `ERROR_BROKEN_PIPE`）。
        fail_writes: usize,
    }

    struct FakePipe(Arc<Mutex<FakeWorld>>);

    impl Read for FakePipe {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
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
        Arc::new(Mutex::new(FakeWorld { protocol: Some(protocol), alive: true, ..Default::default() }))
    }

    fn fake_client(world: &Arc<Mutex<FakeWorld>>) -> VddClient {
        let shared = world.clone();
        VddClient::with_connector(
            "snug-rs-test-pipe",
            Box::new(move |_name| {
                let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
                state.connects += 1;
                let protocol = state.protocol.unwrap_or(VddProtocol::V2);
                Ok(Connection { protocol, pipe: Box::new(FakePipe(shared.clone())) })
            }),
        )
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
    fn legacy_write_failure_reconnects_and_rewrites_once() {
        // C# `SendLegacyOneShotAsync`：断线 → Dispose → 重连 → 重写一次。
        let world = fake_world(VddProtocol::Legacy);
        let mut client = fake_client(&world);
        world.lock().unwrap().fail_writes = 1;

        client.notify(&[monitor()]).expect("legacy 要断线→重连→重写一次");
        let state = world.lock().unwrap();
        assert_eq!(state.connects, 2, "写失败必须重连");
        assert_eq!(state.writes.len(), 1, "重试成功只留下一次成功写入");
        let text = String::from_utf8_lossy(&state.writes[0]);
        assert!(text.contains("\"Add\""), "legacy 应重写 Add 帧：{text}");
        assert_eq!(client.protocol(), Some(VddProtocol::Legacy));
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
}
