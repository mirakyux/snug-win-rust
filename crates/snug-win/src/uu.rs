//! UU 远程（网易 GameViewer）的本地信号探测，移植自 C# `SnugWin.Windows.Sensing.UUSensor`
//! （`UUSensor.cs`）。
//!
//! 四路只读信号，各自独立 try，一路失败不影响其他三路：
//! 1. `GameViewer.exe` / `GameViewerServer.exe` 进程，以及 server 的可见窗口
//!    （悬浮球/覆盖层是否在）；
//! 2. UIA 读 `GameViewer.exe` 窗口里的"本机被控中"控制端表；
//! 3. `GetExtendedTcpTable(TCP_TABLE_OWNER_PID_ALL)` 里 server 的 ESTABLISHED 对端
//!    （排除回环、80/443）；
//! 4. `streamer_log_controlled_*_<pid>.slog` 的持续增长（相邻增长最多 15 秒，
//!    最近一次增长也最多保持 15 秒）。
//!
//! [`UuSensor::sample`] 只出原始信号，不替上层做判断；合成规则在 core 的
//! `ConnectionTracker` / `RemoteSession` 里。`capture_active` 是辅助日志心跳，确定停止需要
//! 独立的 `capture_evidence`。兼容已有捕获监视器的只读入口由 `SNUG_UU_CAPTURE_FEED=1` 启用。
//!
//! 推流 `.slog` 增长不是远程连接的充分证据：本地运行 GameViewer/游戏时日志也可能增长，
//! 已经造成无远程时虚拟屏反复弹出（历史复现见 `docs/upstream-watch.md`）。因此当前策略是：
//! UIA 读到远程控制行时可开始或恢复连接。控制表不可读时，可由完整捕获监视确认，
//! 或通过当前服务进程的持续硬件视频编码判断。
//! `.slog` 只能在最近该证据后的 15 秒内维持既有连接。
//! `client_visible` 仍只是诊断字段：“服务进程有可见且大于 10×10
//! 的顶层窗口”启发式，尚未确认窗口身份一定是远控覆盖层，因此不参与准入。TCP 对端可否决提前撤屏。
//!
//! UU 隐藏在托盘时，控制端表可能读不到。可选捕获监视需要确认同一 UU 进程的捕获已开始。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use snug_core::session::{ControllerTable, UUSignals, UuCaptureEvidence};
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, TCP_TABLE_OWNER_PID_ALL,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_ProgramFiles, KF_FLAG_DEFAULT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
};

use crate::wide::from_wide;

#[path = "uu_capture_feed.rs"]
mod capture_feed;
use capture_feed::CaptureFeed;
#[path = "uu_gpu_encoding.rs"]
mod gpu_encoding;
use gpu_encoding::{EncodingActivity, sample_video_encode};

// ---------------------------------------------------------------------------
// 信号集
// ---------------------------------------------------------------------------

/// 一次采样（对应 C# `UUSignals`，外加几项诊断字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UuSignalSet {
    /// `GameViewer.exe`（远程客户端）在跑。
    pub client_running: bool,
    /// `GameViewerServer.exe` 有可见窗候选；当前只按可见性和尺寸筛选，未验证覆盖层身份。
    pub client_visible: bool,
    /// UIA 读到的控制端表；`readable == false` 表示读不到。
    pub controllers: ControllerTable,
    /// `None` 表示日志不可读，`Some(false)` 表示当前未观察到日志增长。
    ///
    /// 取值就是推流日志的"持续增长"（C# `CaptureActive`）。它只能短暂维持最近有远端证据的
    /// 会话，不能独自开启新连接或从断开中恢复。
    pub capture_active: Option<bool>,
    /// 本机捕获监视器或持续的硬件编码活动。
    pub capture_evidence: UuCaptureEvidence,
    /// 有活动 TCP 对端（C# `HasActivePeerTcpConnection`），可否决提前撤屏。
    pub peer_active: bool,
    /// 推流日志的"持续增长"（与 `capture_active` 同值，保留原名供诊断/校准读）。
    ///
    /// `None` = 读不到。
    pub log_growth: Option<bool>,
    /// 控制端表读不到的原因（可读、或客户端没跑时为 `None`）。
    pub controller_diagnosis: Option<ControllerDiagnosis>,
    /// GameViewerServer 日志尾部（诊断用），读不到就是 `None`。
    pub log_tail: Option<String>,
}

impl UuSignalSet {
    /// 转成 core 的 `UUSignals`（C# `Sample` 的返回值）。
    pub fn to_signals(&self) -> UUSignals {
        UUSignals {
            overlay_visible: self.client_visible,
            controllers: self.controllers.clone(),
            capture_active: self.capture_active,
            capture_evidence: self.capture_evidence,
        }
    }

    /// 什么都不在（进程不在、表读不到、没有对端、日志读不到）。
    pub fn empty() -> Self {
        Self {
            client_running: false,
            client_visible: false,
            controllers: ControllerTable::unreadable(),
            capture_active: Some(false),
            capture_evidence: UuCaptureEvidence::Unknown,
            peer_active: false,
            log_growth: Some(false),
            controller_diagnosis: None,
            log_tail: None,
        }
    }
}

/// 控制端表读不到的原因（只用于真机排障日志与诊断字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerDiagnosis {
    /// 客户端进程在，但没有可见顶层窗口（在托盘里）——C# 同样是直接"读不到"。
    NoVisibleWindow,
    /// UIA COM 起不来（`Uia::new` 失败）。
    ComFailed,
    /// 有窗口，但 UIA 没读出内容（含遍历超预算、窗口刚好关掉）。
    UiaUnreadable,
}

impl ControllerDiagnosis {
    fn describe(self) -> &'static str {
        match self {
            Self::NoVisibleWindow => "GameViewer 没有可见顶层窗口（在托盘时属正常）",
            Self::ComFailed => "UIA COM 初始化失败",
            Self::UiaUnreadable => "UIA 未读到可识别的被控端列表（界面未匹配、窗口不可读或读取超限）",
        }
    }
}

// ---------------------------------------------------------------------------
// TCP 表（纯逻辑）
// ---------------------------------------------------------------------------

/// `MIB_TCPROW_OWNER_PID` 一行。字段都是网络字节序的原始值，解释时再转换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpRow {
    pub state: u32,
    pub local_addr: u32,
    pub local_port: u32,
    pub remote_addr: u32,
    pub remote_port: u32,
    pub pid: i32,
}

/// `MIB_TCP_STATE_ESTAB`（C# 里写的是 5；2 是 LISTEN，别混）。
pub const MIB_TCP_STATE_ESTAB: u32 = 5;
/// `MIB_TCPROW_OWNER_PID` 的字节数。
const TCP_ROW_BYTES: usize = 24;

/// 解析 `GetExtendedTcpTable` 返回的缓冲：前 4 字节行数，随后每行 24 字节。
pub fn parse_tcp_rows(bytes: &[u8]) -> Vec<TcpRow> {
    let Some(count) = read_u32_le(bytes, 0) else {
        return Vec::new();
    };
    let mut rows = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        let start = 4 + index * TCP_ROW_BYTES;
        let Some(pid) = read_i32_le(bytes, start + 20) else {
            break;
        };
        rows.push(TcpRow {
            state: read_u32_le(bytes, start).unwrap_or(0),
            local_addr: read_u32_le(bytes, start + 4).unwrap_or(0),
            local_port: read_u32_le(bytes, start + 8).unwrap_or(0),
            remote_addr: read_u32_le(bytes, start + 12).unwrap_or(0),
            remote_port: read_u32_le(bytes, start + 16).unwrap_or(0),
            pid,
        });
    }
    rows
}

/// 网络字节序的端口（`dwRemotePort` 低 16 位）→ 主机序。
pub fn host_port(raw: u32) -> u32 {
    ((raw & 0xFF) << 8) | ((raw >> 8) & 0xFF)
}

/// 主机序端口 → `MIB_TCPROW` 里网络字节序的低 16 位（与 [`host_port`] 互逆）。
pub fn wire_port(port: u32) -> u32 {
    ((port >> 8) & 0xFF) | ((port & 0xFF) << 8)
}

/// 该 pid 有没有"活动对端"（C# `HasActivePeerTcpConnection` 的筛选）。
pub fn has_active_peer(rows: &[TcpRow], pid: i32) -> bool {
    rows.iter().any(|row| {
        row.pid == pid
            && row.state == MIB_TCP_STATE_ESTAB
            && host_port(row.remote_port) != 443
            && host_port(row.remote_port) != 80
            && host_port(row.remote_port) != 0
            && (row.remote_addr & 0xFF) != 127
    })
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_i32_le(bytes: &[u8], offset: usize) -> Option<i32> {
    read_u32_le(bytes, offset).map(|value| value as i32)
}

// ---------------------------------------------------------------------------
// 日志增长（纯逻辑）
// ---------------------------------------------------------------------------

/// 文件名是否匹配 `streamer_log_controlled_*_<pid>.slog`
/// （`Directory.EnumerateFiles` 的通配符语义）。
///
/// Windows 上通配符匹配**忽略大小写**，这里同样转小写比较；区分大小写会漏掉
/// `Streamer_Log_Controlled_...SLOG` 这类变体。
pub fn is_streamer_log_name(name: &str, server_pid: i32) -> bool {
    let name = name.to_lowercase();
    name.starts_with("streamer_log_controlled_")
        && name.ends_with(&format!("_{server_pid}.slog"))
}

/// 两次"增长"之间最多隔多久才算连续推流。
///
/// C# 是 3 秒。**我们放宽到 15 秒**：真机实测 UU 的推流心跳在空闲（人没操作手机）
/// 时会从 2–3 秒拉到 **8–10 秒**一次，C# 的 3 秒判据在空闲时恒为 false，于是每 15 秒
/// 判一次"断开"→ 撤屏，用户看到手机屏反复消失（2026-11-19 真机日志：
/// `11:01:10 推流=false → 11:01:27 本次会话结束`）。15 秒能覆盖空闲心跳，
/// 又短到"真的断开、日志停止增长"时能在 15 秒内收敛。
const ADVANCE_PAIR_MAX_GAP: f64 = 15.0;
/// 最近一次增长之后还保持"在推流"多久。C# 是 2 秒，放宽理由同上。
const ACTIVE_HOLD_WINDOW: f64 = 15.0;

/// 推流日志的"持续增长"判定（C# `SampleCaptureActive` 里的 Signal B）。
#[derive(Debug, Default)]
struct LogGrowth {
    pid: i32,
    /// 上次观察到的总字节数；`None` 表示还没基线。
    last_bytes: Option<u64>,
    /// 上一次"增长"发生的时刻。
    previous_advance: Option<f64>,
    /// 最近一次"增长"发生的时刻。
    last_advance: Option<f64>,
}

impl LogGrowth {
    fn reset(&mut self, pid: i32) {
        *self = Self { pid, ..Self::default() };
    }

    /// 喂一次总字节数，返回"是否处于持续推流"。
    fn observe(&mut self, pid: i32, total: u64, now: f64) -> bool {
        if self.pid != pid {
            self.reset(pid);
        }
        match self.last_bytes {
            None => self.last_bytes = Some(total),
            Some(previous) if total > previous => {
                self.last_bytes = Some(total);
                self.previous_advance = self.last_advance;
                self.last_advance = Some(now);
            }
            Some(_) => {}
        }
        let continuous = matches!(
            (self.previous_advance, self.last_advance),
            (Some(previous), Some(last)) if last - previous <= ADVANCE_PAIR_MAX_GAP
        );
        continuous && self.last_advance.is_some_and(|last| now - last < ACTIVE_HOLD_WINDOW)
    }
}

// ---------------------------------------------------------------------------
// 平台接口
// ---------------------------------------------------------------------------

/// UU 探测需要的只读系统能力；真机实现是 [`WinProbe`]。
///
/// 每个探测各自失败：返回 `None`/`false`/空表即可，不抛错、不 panic。
pub trait UuProbe: Send + Sync {
    /// `Process.GetProcessesByName(name)` 取第一个 pid（名字不带扩展名）。
    fn process_pid(&self, name: &str) -> Option<i32>;
    /// `HasVisibleWindow` 启发式：pid 有可见且大于 10×10 的顶层窗口；不证明它就是远控覆盖层。
    fn has_visible_window(&self, pid: i32) -> bool;
    /// pid 的所有可见顶层窗口句柄（UIA 用）。
    fn visible_windows(&self, pid: i32) -> Vec<u64>;
    /// `UiaHelper.ReadGameViewerControllers`。
    ///
    /// 第二个返回值是"读不到的原因"，只用于诊断日志；可读时给 `None`。
    fn controllers(
        &self,
        handles: &[u64],
    ) -> (ControllerTable, Option<ControllerDiagnosis>);
    /// TCP 表；`None` = 读不到（不要当成"没有连接"）。
    fn tcp_rows(&self) -> Option<Vec<TcpRow>>;
    /// 是否观察到当前服务进程的 GPU 视频编码。
    fn video_encoding(&self, _pid: i32) -> bool {
        false
    }
    /// `ResolveServerLogDir`：按候选目录找存在的那个。
    fn server_log_dir(&self, server_pid: i32) -> Option<String>;
    /// `Directory.EnumerateFiles(dir, "streamer_log_controlled_*_<pid>.slog")`
    /// 的 `(路径, 字节数)`；`None` = 目录/枚举失败（读不到）。
    fn log_files(&self, dir: &str, server_pid: i32) -> Option<Vec<(String, u64)>>;
    /// 读日志尾部（最多 `max` 字节）；失败返回 `None`。
    fn read_tail(&self, path: &str, max: usize) -> Option<String>;
}

// ---------------------------------------------------------------------------
// 传感器
// ---------------------------------------------------------------------------

/// UU 传感器（C# `UUSensor`）。采样由调用方按节奏驱动，内部不起线程。
pub struct UuSensor {
    probe: Box<dyn UuProbe>,
    manual: AtomicBool,
    log: Mutex<LogGrowth>,
    encoding: Mutex<EncodingActivity>,
    capture_feed: Option<Mutex<CaptureFeed>>,
    /// 上次记过的控制端诊断（用来做"状态变化才记一条"）。
    last_diagnosis: Mutex<Option<ControllerDiagnosis>>,
}

impl UuSensor {
    /// 真机传感器。
    pub fn new() -> Self {
        let mut sensor = Self::with_probe(Box::new(WinProbe));
        sensor.capture_feed = CaptureFeed::configured().map(Mutex::new);
        sensor
    }

    /// 换一套探测实现（测试用）。
    pub fn with_probe(probe: Box<dyn UuProbe>) -> Self {
        Self {
            probe,
            manual: AtomicBool::new(false),
            log: Mutex::new(LogGrowth::default()),
            encoding: Mutex::new(EncodingActivity::default()),
            capture_feed: None,
            last_diagnosis: Mutex::new(None),
        }
    }

    /// C# `ManualOverride`：不管 UU 连没连都当作"在远程"。
    pub fn manual_override(&self) -> bool {
        self.manual.load(Ordering::Relaxed)
    }

    /// 设置手动强制（设置界面用）。
    pub fn set_manual_override(&self, on: bool) {
        self.manual.store(on, Ordering::Relaxed);
    }

    /// 采样一次（C# `Sample`）。`now` 是单调秒数，只用于日志增长窗口。
    pub fn sample(&self, now: f64) -> UuSignalSet {
        if self.manual_override() {
            // C# 手动模式直接给"在远程"的三项信号。
            return UuSignalSet {
                client_running: false,
                client_visible: true,
                controllers: ControllerTable::from_names(["手动开启 (Phone)"]),
                capture_active: Some(true),
                capture_evidence: UuCaptureEvidence::Unknown,
                peer_active: false,
                log_growth: None,
                controller_diagnosis: None,
                log_tail: None,
            };
        }

        let server_pid = self.probe.process_pid("GameViewerServer");
        let client_pid = self.probe.process_pid("GameViewer");

        let client_visible = server_pid.is_some_and(|pid| self.probe.has_visible_window(pid));

        let (controllers, controller_diagnosis) = match client_pid {
            Some(pid) => {
                let handles = self.probe.visible_windows(pid);
                if handles.is_empty() {
                    // C# 同样：客户端没有可见窗口就直接 Unreadable，不去读 UIA。
                    (ControllerTable::unreadable(), Some(ControllerDiagnosis::NoVisibleWindow))
                } else {
                    self.probe.controllers(&handles)
                }
            }
            // 客户端没跑不算"读不到"，不记诊断。
            None => (ControllerTable::unreadable(), None),
        };
        self.note_controller_diagnosis(controllers.readable, controller_diagnosis);

        let peer_active = server_pid
            .and_then(|pid| self.probe.tcp_rows().map(|rows| has_active_peer(&rows, pid)))
            .unwrap_or(false);

        // 日志：目录与文件列表各失败一次都算"读不到"。
        let log_files = server_pid.and_then(|pid| {
            self.probe
                .server_log_dir(pid)
                .and_then(|dir| self.probe.log_files(&dir, pid))
        });
        let log_tail = log_files.as_deref().and_then(|files| {
            let (path, _) = files.iter().max_by_key(|(_, bytes)| *bytes)?;
            self.probe.read_tail(path, LOG_TAIL_BYTES)
        });

        let log_growth = self.log_signal(server_pid, log_files.as_deref(), now);

        // SLOG 单独会在本地使用时增长；它只作为已有会话的心跳。
        let capture_active = log_growth;
        let capture_evidence = self.capture_feed.as_ref()
            .and_then(|feed| feed.lock().ok().map(|mut feed| feed.sample(server_pid)))
            .unwrap_or_default();
        let encoding = server_pid.is_some_and(|pid| self.probe.video_encoding(pid));
        let encoding_active = self.encoding.lock().unwrap().observe(server_pid, encoding, now);
        // A positive peer signal vetoes early retirement. UDP sessions need not have a TCP peer.
        let capture_evidence = match capture_evidence {
            UuCaptureEvidence::Stopped { .. } if peer_active => UuCaptureEvidence::Unknown,
            UuCaptureEvidence::Unknown if encoding_active => UuCaptureEvidence::HardwareEncoding,
            evidence => evidence,
        };

        UuSignalSet {
            client_running: client_pid.is_some(),
            client_visible,
            controllers,
            capture_active,
            capture_evidence,
            peer_active,
            log_growth,
            controller_diagnosis,
            log_tail,
        }
    }

    /// 控制端诊断只在**状态变化**时记一条，避免一秒一条刷爆日志。
    fn note_controller_diagnosis(
        &self,
        readable: bool,
        diagnosis: Option<ControllerDiagnosis>,
    ) {
        let mut last = self.last_diagnosis.lock().unwrap();
        if *last == diagnosis {
            return;
        }
        let previous = std::mem::replace(&mut *last, diagnosis);
        match diagnosis {
            Some(reason) => crate::warn(&format!(
                "uu: 控制端表读不到：{}",
                reason.describe()
            )),
            // 从"读不到"恢复成可读才值得记；客户端退出导致的 None 不记。
            None if readable && previous.is_some() => {
                crate::warn("uu: 控制端表恢复可读")
            }
            None => {}
        }
    }

    /// 日志增长信号（C# `SampleCaptureActive` 的 Signal B）。
    ///
    /// 与 C# 的差异：日志读不到时 C# 给 `false`，这里给 `None`（"不可读"）。
    /// 注意它现在**只是诊断/校准字段**，不再单独决定 `capture_active`。
    fn log_signal(
        &self,
        server_pid: Option<i32>,
        log_files: Option<&[(String, u64)]>,
        now: f64,
    ) -> Option<bool> {
        let Some(pid) = server_pid else {
            // server 不在：确定没有推流（C# 也是 false），顺手清状态。
            *self.log.lock().unwrap() = LogGrowth::default();
            return Some(false);
        };
        let Some(files) = log_files else {
            return None;
        };
        if files.is_empty() {
            return Some(false);
        }
        let total: u64 = files.iter().map(|(_, bytes)| *bytes).sum();
        let mut growth = self.log.lock().unwrap();
        Some(growth.observe(pid, total, now))
    }
}

impl Default for UuSensor {
    fn default() -> Self {
        Self::new()
    }
}

/// 日志尾部最多读这么多字节（诊断用，别把大文件全读进来）。
pub const LOG_TAIL_BYTES: usize = 4096;

// ---------------------------------------------------------------------------
// 真机实现
// ---------------------------------------------------------------------------

/// 真机只读探测。
pub struct WinProbe;

fn process_image_path(pid: i32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32).ok()?;
        let mut buffer = [0u16; 1024];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        Some(from_wide(&buffer[..length as usize]))
    }
}

fn process_infos() -> Vec<(i32, String)> {
    let mut result = Vec::new();
    unsafe {
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return result;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                result.push((entry.th32ProcessID as i32, from_wide(&entry.szExeFile)));
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    result
}

/// `EnumWindows` 回调收集某个 pid 的可见窗口；`large_only` 时要求大于 10×10。
unsafe extern "system" fn collect_windows(window: HWND, lparam: LPARAM) -> BOOL {
    let context = unsafe { &mut *(lparam.0 as *mut WindowSearch) };
    if !unsafe { IsWindowVisible(window).as_bool() } {
        return BOOL(1);
    }
    let mut owner = 0u32;
    unsafe { GetWindowThreadProcessId(window, Some(&mut owner)) };
    if owner as i32 != context.pid {
        return BOOL(1);
    }
    if context.large_only {
        let mut rect = windows::Win32::Foundation::RECT::default();
        let ok = unsafe { GetWindowRect(window, &mut rect) }.is_ok();
        if !ok || rect.right - rect.left <= 10 || rect.bottom - rect.top <= 10 {
            return BOOL(1);
        }
    }
    context.handles.push(window.0 as usize as u64);
    BOOL(1)
}

struct WindowSearch {
    pid: i32,
    large_only: bool,
    handles: Vec<u64>,
}

fn search_windows(pid: i32, large_only: bool) -> Vec<u64> {
    let mut context = WindowSearch { pid, large_only, handles: Vec::new() };
    unsafe {
        let _ = EnumWindows(
            Some(collect_windows),
            LPARAM(&mut context as *mut WindowSearch as isize),
        );
    }
    context.handles
}

/// 已知文件夹（Program Files）。
fn known_program_files() -> Option<String> {
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, None).ok()?;
        if path.is_null() {
            return None;
        }
        let mut length = 0usize;
        while *path.0.add(length) != 0 {
            length += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(path.0, length));
        windows::Win32::System::Com::CoTaskMemFree(Some(path.0 as *const core::ffi::c_void));
        Some(text)
    }
}

impl UuProbe for WinProbe {
    fn process_pid(&self, name: &str) -> Option<i32> {
        let wanted = name.to_lowercase();
        process_infos()
            .into_iter()
            .find(|(_, exe)| {
                let stem = crate::apps::exe_stem(exe).to_lowercase();
                stem == wanted
            })
            .map(|(pid, _)| pid)
    }

    fn has_visible_window(&self, pid: i32) -> bool {
        !search_windows(pid, true).is_empty()
    }

    fn video_encoding(&self, pid: i32) -> bool {
        sample_video_encode(pid)
    }

    fn visible_windows(&self, pid: i32) -> Vec<u64> {
        search_windows(pid, false)
    }

    fn controllers(&self, handles: &[u64]) -> (ControllerTable, Option<ControllerDiagnosis>) {
        match crate::uia::with_thread_uia(|uia| uia.read_controllers(handles)) {
            Some(table) => {
                if table.readable {
                    (table, None)
                } else {
                    (table, Some(ControllerDiagnosis::UiaUnreadable))
                }
            }
            None => (
                ControllerTable::unreadable(),
                Some(ControllerDiagnosis::ComFailed),
            ),
        }
    }

    fn tcp_rows(&self) -> Option<Vec<TcpRow>> {
        const AF_INET: u32 = 2;
        const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
        let mut size = 0u32;
        unsafe {
            GetExtendedTcpTable(None, &mut size, false, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0);
            if size == 0 {
                return None;
            }
            for _ in 0..3 {
                let mut buffer = vec![0u8; size as usize];
                let status = GetExtendedTcpTable(
                    Some(buffer.as_mut_ptr() as *mut core::ffi::c_void),
                    &mut size,
                    false,
                    AF_INET,
                    TCP_TABLE_OWNER_PID_ALL,
                    0,
                );
                if status == 0 {
                    return Some(parse_tcp_rows(&buffer));
                }
                if status != ERROR_INSUFFICIENT_BUFFER {
                    return None;
                }
            }
        }
        None
    }

    fn server_log_dir(&self, server_pid: i32) -> Option<String> {
        let mut candidates = Vec::new();
        if let Some(exe) = process_image_path(server_pid)
            && let Some(dir) = std::path::Path::new(&exe).parent()
        {
            candidates.push(dir.join("log"));
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join("log").join("server").join("log"));
                candidates.push(parent.join("log"));
            }
        }
        if let Some(program_files) = known_program_files() {
            candidates.push(
                std::path::Path::new(&program_files)
                    .join("Netease")
                    .join("GameViewer")
                    .join("log")
                    .join("server")
                    .join("log"),
            );
        }
        // C# 里三条硬编码兜底（机器上常见安装位置）。
        candidates.push(r"C:\Program Files\Netease\GameViewer\log\server\log".into());
        candidates.push(r"D:\Program Files\Netease\GameViewer\log\server\log".into());
        candidates.push(r"D:\GameViewer\log\server\log".into());

        candidates
            .into_iter()
            .find(|candidate| candidate.is_dir())
            .and_then(|candidate| candidate.to_str().map(str::to_string))
    }

    fn log_files(&self, dir: &str, server_pid: i32) -> Option<Vec<(String, u64)>> {
        let entries = std::fs::read_dir(dir).ok()?;
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !is_streamer_log_name(&name.to_string_lossy(), server_pid) {
                continue;
            }
            let path = entry.path();
            if let Ok(metadata) = entry.metadata()
                && let Some(text) = path.to_str()
            {
                files.push((text.to_string(), metadata.len()));
            }
        }
        Some(files)
    }

    fn read_tail(&self, path: &str, max: usize) -> Option<String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(path).ok()?;
        let length = file.metadata().ok()?.len();
        if length > max as u64 {
            file.seek(SeekFrom::Start(length - max as u64)).ok()?;
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).ok()?;
        Some(String::from_utf8_lossy(&bytes).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn row(state: u32, remote_addr: u32, remote_port: u32, pid: i32) -> TcpRow {
        TcpRow { state, local_addr: 0, local_port: 0, remote_addr, remote_port, pid }
    }

    /// 127.0.0.1 在网络字节序下低字节是 0x7F。
    const LOOPBACK: u32 = 0x0100_007F;

    #[test]
    fn tcp_rows_parse_count_and_rows() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2u32.to_le_bytes());
        for pid in [10i32, 11] {
            bytes.extend_from_slice(&MIB_TCP_STATE_ESTAB.to_le_bytes());
            bytes.extend_from_slice(&0x0100_A8C0u32.to_le_bytes());
            bytes.extend_from_slice(&0x0A1Fu32.to_le_bytes());
            bytes.extend_from_slice(&0x0500_000Au32.to_le_bytes());
            bytes.extend_from_slice(&0xE90Bu32.to_le_bytes());
            bytes.extend_from_slice(&(pid as u32).to_le_bytes());
        }
        let rows = parse_tcp_rows(&bytes);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].pid, 11);
        assert_eq!(host_port(rows[0].remote_port), 0x0BE9);

        // 截断的缓冲不会 panic，能解析几行算几行。
        assert_eq!(parse_tcp_rows(&bytes[..40]).len(), 1);
        assert!(parse_tcp_rows(&[]).is_empty());
    }

    #[test]
    fn active_peer_filters_like_csharp() {
        assert!(has_active_peer(&[row(MIB_TCP_STATE_ESTAB, 0x0100_000A, wire_port(2533), 7)], 7));
        // 443 / 80 / 0 不算。
        assert!(!has_active_peer(&[row(MIB_TCP_STATE_ESTAB, 0x0100_000A, wire_port(443), 7)], 7));
        assert!(!has_active_peer(&[row(MIB_TCP_STATE_ESTAB, 0x0100_000A, wire_port(80), 7)], 7));
        assert!(!has_active_peer(&[row(MIB_TCP_STATE_ESTAB, 0x0100_000A, 0, 7)], 7));
        // 回环不算。
        assert!(!has_active_peer(
            &[row(MIB_TCP_STATE_ESTAB, LOOPBACK, wire_port(2533), 7)],
            7
        ));
        // 别的进程不算；LISTEN（2）也不算。
        assert!(!has_active_peer(
            &[row(MIB_TCP_STATE_ESTAB, 0x0100_000A, wire_port(2533), 8)],
            7
        ));
        assert!(!has_active_peer(&[row(2, 0x0100_000A, wire_port(2533), 7)], 7));
    }

    #[test]
    fn log_growth_keeps_presence_through_idle_heartbeat() {
        let mut growth = LogGrowth::default();
        assert!(!growth.observe(7, 100, 0.0), "第一次只建基线");
        assert!(!growth.observe(7, 200, 1.0), "第一次增长还不够");
        assert!(growth.observe(7, 300, 2.0), "两次增长之间成立");
        // 空闲心跳：8–10 秒才增长一次也不能掉（C# 的 2/3 秒在这里会误判断开）。
        assert!(growth.observe(7, 400, 12.0), "10 秒后心跳再次增长，仍在场");
        assert!(growth.observe(7, 500, 22.0), "再隔 10 秒，继续保持");
        assert!(!growth.observe(7, 500, 40.0), "超过保持窗口就不再算在场");
        // 换 pid 会清基线。
        assert!(!growth.observe(8, 1000, 41.0));
    }

    // ---- 假探测 ----

    #[derive(Default)]
    struct FakeProbe {
        pids: HashMap<String, i32>,
        visible: Vec<i32>,
        windows: HashMap<i32, Vec<u64>>,
        controllers: Option<ControllerTable>,
        /// `controllers` 读不到时报告的原因；`None` 时按 `UiaUnreadable` 算。
        controller_diag: Option<ControllerDiagnosis>,
        tcp: Option<Vec<TcpRow>>,
        dir: Option<String>,
        files: Option<Vec<(String, u64)>>,
        tails: HashMap<String, String>,
    }

    impl UuProbe for FakeProbe {
        fn process_pid(&self, name: &str) -> Option<i32> {
            self.pids.get(name).copied()
        }

        fn has_visible_window(&self, pid: i32) -> bool {
            self.visible.contains(&pid)
        }

        fn visible_windows(&self, pid: i32) -> Vec<u64> {
            self.windows.get(&pid).cloned().unwrap_or_default()
        }

        fn controllers(
            &self,
            _handles: &[u64],
        ) -> (ControllerTable, Option<ControllerDiagnosis>) {
            match &self.controllers {
                Some(table) if table.readable => (table.clone(), None),
                Some(table) => (
                    table.clone(),
                    Some(self.controller_diag.unwrap_or(ControllerDiagnosis::UiaUnreadable)),
                ),
                None => (
                    ControllerTable::unreadable(),
                    Some(self.controller_diag.unwrap_or(ControllerDiagnosis::UiaUnreadable)),
                ),
            }
        }

        fn tcp_rows(&self) -> Option<Vec<TcpRow>> {
            self.tcp.clone()
        }

        fn server_log_dir(&self, _server_pid: i32) -> Option<String> {
            self.dir.clone()
        }

        fn log_files(&self, _dir: &str, _server_pid: i32) -> Option<Vec<(String, u64)>> {
            self.files.clone()
        }

        fn read_tail(&self, path: &str, _max: usize) -> Option<String> {
            self.tails.get(path).cloned()
        }
    }

    #[test]
    fn sample_returns_all_empty_when_nothing_is_readable() {
        let sensor = UuSensor::with_probe(Box::new(FakeProbe::default()));
        let signals = sensor.sample(0.0);
        assert!(!signals.client_running);
        assert!(!signals.client_visible);
        assert!(!signals.controllers.readable);
        assert!(signals.controllers.names.is_empty());
        assert_eq!(signals.capture_active, Some(false), "server 不在 = 确定没推流");
        assert!(!signals.peer_active);
        assert_eq!(signals.log_tail, None);
        // 转成 core 信号也不会假装在线。
        assert!(!signals.to_signals().is_present());
    }

    #[test]
    fn each_signal_fails_independently() {
        let port = 2533u32;
        let mut probe = FakeProbe::default();
        probe.pids.insert("GameViewerServer".into(), 7);
        probe.pids.insert("GameViewer".into(), 8);
        probe.visible.push(7);
        // UIA 读不到、TCP 读得到。
        probe.tcp = Some(vec![row(MIB_TCP_STATE_ESTAB, 0x0100_000A, (port >> 8) | ((port & 0xFF) << 8), 7)]);
        probe.dir = Some(r"C:\gv\log".into());
        probe.files = None; // 日志读不到
        let sensor = UuSensor::with_probe(Box::new(probe));

        let signals = sensor.sample(0.0);
        assert!(signals.client_running);
        assert!(signals.client_visible);
        assert!(!signals.controllers.readable, "没有窗口时就是不可读");
        assert!(signals.peer_active);
        // 日志读不到 → `None`（"不知道"），不是 `false`（"确定没推流"）。
        // 对端只在诊断字段里，按 C# 不参与 capture 判定。
        assert_eq!(signals.capture_active, None);
        assert_eq!(signals.log_tail, None);
    }

    #[test]
    fn controllers_come_from_visible_client_windows() {
        let mut probe = FakeProbe::default();
        probe.pids.insert("GameViewer".into(), 8);
        probe.windows.insert(8, vec![100, 101]);
        probe.controllers = Some(ControllerTable::from_names(["iPhone 15 (Phone)"]));
        let sensor = UuSensor::with_probe(Box::new(probe));
        let signals = sensor.sample(0.0);
        assert_eq!(signals.controllers.names, vec!["iPhone 15 (Phone)"]);
        assert!(signals.controllers.readable);
        assert_eq!(signals.controller_diagnosis, None);

        // 没有可见窗口 → 不去读 UIA，直接不可读，并给出"在托盘"这一诊断。
        let mut probe = FakeProbe::default();
        probe.pids.insert("GameViewer".into(), 8);
        probe.controllers = Some(ControllerTable::from_names(["不应该被用到"]));
        let sensor = UuSensor::with_probe(Box::new(probe));
        let signals = sensor.sample(0.0);
        assert!(!signals.controllers.readable);
        assert_eq!(
            signals.controller_diagnosis,
            Some(ControllerDiagnosis::NoVisibleWindow)
        );

        // 有窗口但 COM 起不来 → 诊断要能区分开。
        let mut probe = FakeProbe::default();
        probe.pids.insert("GameViewer".into(), 8);
        probe.windows.insert(8, vec![100]);
        probe.controller_diag = Some(ControllerDiagnosis::ComFailed);
        let sensor = UuSensor::with_probe(Box::new(probe));
        let signals = sensor.sample(0.0);
        assert!(!signals.controllers.readable);
        assert_eq!(
            signals.controller_diagnosis,
            Some(ControllerDiagnosis::ComFailed)
        );
    }

    #[test]
    fn capture_is_none_when_log_is_unreadable_but_false_when_empty() {
        let mut probe = FakeProbe::default();
        probe.pids.insert("GameViewerServer".into(), 7);
        // 目录可读、但没有日志文件：确定没在推流。
        probe.dir = Some(r"C:\gv\log".into());
        probe.files = Some(Vec::new());
        let sensor = UuSensor::with_probe(Box::new(probe));
        assert_eq!(sensor.sample(0.0).capture_active, Some(false));

        // 目录都找不到：不可读。
        let mut probe = FakeProbe::default();
        probe.pids.insert("GameViewerServer".into(), 7);
        let sensor = UuSensor::with_probe(Box::new(probe));
        assert_eq!(sensor.sample(0.0).capture_active, None);
    }

    #[test]
    fn log_growth_is_diagnostic_and_not_new_session_evidence() {
        let bytes = std::sync::Arc::new(Mutex::new(100u64));
        let sensor = UuSensor::with_probe(Box::new(MutableProbe { bytes: bytes.clone() }));

        assert_eq!(sensor.sample(0.0).log_growth, Some(false), "第一次只建基线");
        *bytes.lock().unwrap() = 200;
        assert_eq!(sensor.sample(1.0).log_growth, Some(false), "第一次增长还不够");
        *bytes.lock().unwrap() = 300;
        let signals = sensor.sample(2.0);
        assert_eq!(signals.log_growth, Some(true), "日志确实在持续增长");
        // 日志增长可维持已确认会话，但不能单独开启会话。
        assert_eq!(signals.capture_active, Some(true));
        assert!(
            signals.to_signals().is_present(),
            "增长可以维持已确认的会话"
        );
        assert!(
            !signals.to_signals().has_remote_evidence(),
            "本地 GameViewer/游戏产生的心跳不能单独开启手机屏"
        );
        assert_eq!(
            sensor.sample(2.0).log_tail.as_deref(),
            Some("last line\n"),
            "日志尾部原样给上层"
        );
        // 心跳间隔 2.1 秒仍在保持窗口内 → 可维持既有会话。
        assert_eq!(sensor.sample(4.1).log_growth, Some(true));
        // 彻底停止增长 15 秒后才掉。
        assert_eq!(sensor.sample(20.0).log_growth, Some(false));
    }

    #[test]
    fn three_second_cadence_is_not_continuous_by_the_csharp_rule() {
        let bytes = std::sync::Arc::new(Mutex::new(1000u64));
        let sensor = UuSensor::with_probe(Box::new(MutableProbe { bytes: bytes.clone() }));

        // 3.05 秒一次的心跳（真机空闲节奏）可维持会话，但不能成为新会话证据。
        let mut t = 0.0;
        let mut advances = 0;
        for delta in [0u64, 148, 933, 500, 819] {
            *bytes.lock().unwrap() += delta;
            let signals = sensor.sample(t);
            // 前两次增长还凑不成"一对"（第一次建基线、第二次才记下前一个点）。
            if advances < 2 {
                assert_eq!(signals.capture_active, Some(false), "t={t} 还在建基线");
            } else {
                assert_eq!(
                    signals.capture_active,
                    Some(true),
                    "t={t} 3.05 秒心跳仍在保持窗口内"
                );
                assert!(signals.to_signals().is_present(), "t={t} 心跳维持");
                assert!(
                    !signals.to_signals().has_remote_evidence(),
                    "t={t} 不应凭心跳连入"
                );
            }
            advances += 1;
            t += 3.05;
        }
        assert_eq!(sensor.sample(t).log_growth, Some(true), "心跳停止后 3 秒仍在保持窗口");
        assert_eq!(
            sensor.sample(t + 16.0).log_growth,
            Some(false),
            "彻底停止增长超过保持窗口才掉"
        );
    }

    #[test]
    fn peer_alone_is_diagnostic_only_like_csharp() {
        let mut probe = FakeProbe::default();
        probe.pids.insert("GameViewerServer".into(), 7);
        probe.dir = Some(r"C:\gv\log".into());
        probe.files = Some(Vec::new()); // 日志没增长
        probe.tcp = Some(vec![row(
            MIB_TCP_STATE_ESTAB,
            0x0100_000A,
            wire_port(2533),
            7,
        )]);
        let sensor = UuSensor::with_probe(Box::new(probe));

        let signals = sensor.sample(0.0);
        assert!(signals.peer_active);
        assert_eq!(signals.log_growth, Some(false));
        // C# 从不让 TCP 对端单独判在场（断开后残留的 keepalive socket 会骗人），
        // 它只是 `LastPeerTcpObserved` 诊断字段。
        assert_eq!(signals.capture_active, Some(false));
        assert!(!signals.to_signals().is_present());
    }

    #[test]
    fn streamer_log_name_matching_ignores_case() {
        // C# 的 Directory.EnumerateFiles 在 Windows 上忽略大小写。
        assert!(is_streamer_log_name(
            "streamer_log_controlled_20261115185258370_5924.slog",
            5924
        ));
        assert!(is_streamer_log_name(
            "STREAMER_LOG_CONTROLLED_20261115185258370_5924.SLOG",
            5924
        ));
        assert!(!is_streamer_log_name("streamer_log_controlled_x_999.slog", 5924));
        assert!(!is_streamer_log_name("connection_log_controlled_x_5924.slog", 5924));
        assert!(!is_streamer_log_name("streamer_log_controlled_x_5924.slog.bak", 5924));
    }

    // 可改文件大小的假探测（用 Arc<Mutex<u64>> 共享）。
    struct MutableProbe {
        bytes: std::sync::Arc<Mutex<u64>>,
    }

    impl UuProbe for MutableProbe {
        fn process_pid(&self, name: &str) -> Option<i32> {
            (name == "GameViewerServer").then_some(7)
        }
        fn has_visible_window(&self, _pid: i32) -> bool {
            false
        }
        fn visible_windows(&self, _pid: i32) -> Vec<u64> {
            Vec::new()
        }
        fn controllers(
            &self,
            _handles: &[u64],
        ) -> (ControllerTable, Option<ControllerDiagnosis>) {
            (
                ControllerTable::unreadable(),
                Some(ControllerDiagnosis::UiaUnreadable),
            )
        }
        fn tcp_rows(&self) -> Option<Vec<TcpRow>> {
            Some(Vec::new())
        }
        fn server_log_dir(&self, _server_pid: i32) -> Option<String> {
            Some(r"C:\gv\log".into())
        }
        fn log_files(&self, _dir: &str, _server_pid: i32) -> Option<Vec<(String, u64)>> {
            Some(vec![(r"C:\gv\log\a.slog".into(), *self.bytes.lock().unwrap())])
        }
        fn read_tail(&self, _path: &str, _max: usize) -> Option<String> {
            Some("last line\n".into())
        }
    }

    #[test]
    fn manual_override_behaves_like_csharp() {
        let sensor = UuSensor::with_probe(Box::new(FakeProbe::default()));
        assert!(!sensor.manual_override());
        sensor.set_manual_override(true);
        assert!(sensor.manual_override());
        let signals = sensor.sample(0.0);
        assert!(signals.client_visible);
        assert_eq!(signals.controllers.names, vec!["手动开启 (Phone)"]);
        assert!(signals.controllers.readable);
        assert_eq!(signals.capture_active, Some(true));
        assert!(signals.to_signals().is_present());
        sensor.set_manual_override(false);
        assert!(!sensor.sample(0.0).client_visible);
    }
}
