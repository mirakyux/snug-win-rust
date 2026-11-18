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
    file: std::fs::File,
}

/// 管道客户端：内部只保留一条连接，探测到协议后不再反复尝试。
pub struct VddClient {
    name: String,
    connection: Option<Connection>,
}

impl Default for VddClient {
    fn default() -> Self {
        Self::new(DEFAULT_PIPE_NAME)
    }
}

impl VddClient {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), connection: None }
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
            VddProtocol::Legacy => self.write_raw(&serialize_legacy_add(monitors)),
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
                self.write_raw(&serialize_remove(remove_ids))?;
                std::thread::sleep(Duration::from_millis(120));
                self.write_raw(&serialize_legacy_add(add_monitors))
            }
        }
    }

    pub fn remove(&mut self, ids: &[u32]) -> Result<(), VddError> {
        let json = serialize_remove(ids);
        match self.ensure()? {
            VddProtocol::V2 => self.write_framed(&json),
            VddProtocol::Legacy => self.write_raw(&json),
        }
    }

    pub fn remove_all(&mut self) -> Result<(), VddError> {
        let json = serialize_remove_all();
        match self.ensure()? {
            VddProtocol::V2 => self.write_framed(&json),
            VddProtocol::Legacy => self.write_raw(&json),
        }
    }

    /// 请求当前状态（仅 v2 支持应答）。
    pub fn state(&mut self) -> Result<Option<Vec<VddMonitor>>, VddError> {
        match self.ensure()? {
            VddProtocol::Legacy => Ok(None),
            VddProtocol::V2 => {
                self.write_framed(&serialize_state_request())?;
                let Some(connection) = self.connection.as_mut() else {
                    return Ok(None);
                };
                let mut buf = [0u8; 64 * 1024];
                let read = connection.file.read(&mut buf)?;
                if read == 0 {
                    return Ok(None);
                }
                let text = String::from_utf8_lossy(&buf[..read]);
                let text = text.trim_end_matches('\u{4}');
                Ok(parse_state_reply(text))
            }
        }
    }

    fn ensure(&mut self) -> Result<VddProtocol, VddError> {
        if let Some(connection) = &self.connection {
            return Ok(connection.protocol);
        }
        let connection = connect(&self.name)?;
        let protocol = connection.protocol;
        self.connection = Some(connection);
        Ok(protocol)
    }

    fn write_framed(&mut self, json: &str) -> Result<(), VddError> {
        let bytes = frame(json);
        if let Some(connection) = self.connection.as_mut() {
            connection.file.write_all(&bytes)?;
            connection.file.flush()?;
        }
        Ok(())
    }

    fn write_raw(&mut self, json: &str) -> Result<(), VddError> {
        if let Some(connection) = self.connection.as_mut() {
            connection.file.write_all(json.as_bytes())?;
            connection.file.flush()?;
        }
        Ok(())
    }
}

/// 先试双向（v2），"拒绝访问"则退回只写（v0.3.x）；管道忙时重试。
fn connect(name: &str) -> Result<Connection, VddError> {
    let path = pipe_path(name);
    let deadline = Instant::now() + Duration::from_millis(2500);
    let mut first_error: Option<std::io::Error> = None;

    loop {
        match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => return Ok(Connection { protocol: VddProtocol::V2, file }),
            Err(err) => {
                if err.kind() == std::io::ErrorKind::PermissionDenied {
                    // 驱动只开 INBOUND 时，双向打开会被拒；退回只写。
                    if let Ok(file) = OpenOptions::new().write(true).open(&path) {
                        return Ok(Connection { protocol: VddProtocol::Legacy, file });
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
}
