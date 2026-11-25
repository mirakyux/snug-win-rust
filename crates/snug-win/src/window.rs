//! 窗口搬运与查询，移植自 C# 版 `SnugWin.Windows.Windows.WindowService`。
//!
//! C# 里那些 `async` 方法在这里是阻塞的（等待由 `snug-core` 的串行队列承担），
//! 返回值与判定顺序不变。有两处与 C# 不同、需要特别说明：
//!
//! - **边框补偿**：Win10/11 的窗口有不可见的阴影/圆角外扩。C# 用
//!   `DWMWA_EXTENDED_FRAME_BOUNDS` 量出"看得见的边框"，再反推出 `SetWindowPos`
//!   该用的原始矩形。这里原样移植（[`shadow_margins`] / [`window_rect_for_target`]）。
//! - **关闭按钮位置**依赖 UIA（C# `UiaHelper.FindCloseButtonRect`）。[`WinWindows`] 默认
//!   就接上了 [`crate::uia::UiaCloseButtons`]，所以 [`WindowControl::close_button`] 会真的
//!   去问 UIA；[`CloseButtonFinder`] 这个插口留着给测试换假实现。
//!
//! 选择复制（C# `CopySelectionAtAsync` / `SelectionBubbleAction`）依赖
//! `SnugWin.Core.Touch.SelectionShortcutResolver`，等 core 移植过来再在 `touch` 模块里接。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use snug_core::desk::{WindowControl, WindowLook};
use snug_core::{Rect, WindowInfo, WindowKey};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VIRTUAL_KEY,
    VK_CONTROL, VK_OEM_MINUS, VK_OEM_PLUS, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetForegroundWindow, GetWindow,
    GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed, PostMessageW,
    SetForegroundWindow, SetWindowPos, ShowWindow, WindowFromPoint, GA_ROOT, GWL_EXSTYLE,
    GWL_STYLE, GW_OWNER, SWP_NOACTIVATE, SWP_NOZORDER, SW_MINIMIZE, SW_RESTORE, WM_CLOSE,
    WM_PAINT,
};
use windows::core::{w, BOOL, PCWSTR, PWSTR};

use crate::wide::{from_wide, to_wide};

/// 窗口样式：有标题栏（C# `WS_CAPTION`）。
pub const WS_CAPTION: u32 = 0x00C0_0000;
/// 窗口扩展样式：工具窗口（不进任务栏、也不该被当应用窗口）。
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
/// 窗口扩展样式：强制出现在任务栏。
pub const WS_EX_APPWINDOW: u32 = 0x0004_0000;

/// 挪窗口最多试几次（C# `MoveRetries`）。
pub const MOVE_RETRIES: usize = 6;
/// 判定"已经摆到位"的容差（C# `got.Matches(targetFrame, tolerance: 4.0)`）。
pub const MOVE_TOLERANCE: f64 = 4.0;
/// 判定"系统已经把窗口放稳了"的容差（C# 1.5）。
pub const SETTLE_TOLERANCE: f64 = 1.5;
/// 恢复窗口时判定"没被动过"的容差（C# `RectD.Matches` 的默认值 2.0）。
pub const RESTORE_TOLERANCE: f64 = 2.0;

/// 四边预留量（对应 C# `GetShadowMargins` 的元组）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Margins {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl Margins {
    pub const ZERO: Margins = Margins { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
}

/// 一个顶层窗口的完整状态：C# 在 `EnumWindows` 回调里逐个 API 现查的东西，一次拿全。
#[derive(Debug, Clone, PartialEq)]
pub struct WindowProbe {
    pub handle: u64,
    pub pid: i32,
    pub visible: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub cloaked: bool,
    /// `GWL_EXSTYLE`
    pub ex_style: u32,
    /// `GWL_STYLE`
    pub style: u32,
    /// `GetWindow(hwnd, GW_OWNER)` 是否非空。
    pub has_owner: bool,
    /// `GetWindowRect`（含不可见边框）。
    pub rect: Rect,
    pub class: String,
    pub title: String,
}

impl WindowProbe {
    /// 测试与文档用：最小可用的探针。
    pub fn new(handle: u64, pid: i32, class: &str, title: &str, rect: Rect) -> Self {
        Self {
            handle,
            pid,
            visible: true,
            minimized: false,
            maximized: false,
            cloaked: false,
            ex_style: 0,
            style: 0,
            has_owner: false,
            rect,
            class: class.to_string(),
            title: title.to_string(),
        }
    }
}

/// 进程的显示信息（C# `GetProcessAppMeta` 的返回）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessMeta {
    pub name: String,
    /// 可执行文件名（C# 里的 `AppId`）。
    pub app_id: Option<String>,
}

// ---------------------------------------------------------------------------
// 纯逻辑
// ---------------------------------------------------------------------------

/// 由「原始窗口矩形」与「DWM 可见边框」算出四边各被不可见边框吃掉多少
/// （C# `GetShadowMargins`）。
pub fn shadow_margins(raw: Option<Rect>, extended: Option<Rect>) -> Margins {
    let (Some(raw), Some(ext)) = (raw, extended) else {
        return Margins::ZERO;
    };
    Margins {
        left: f64::max(0.0, ext.min_x() - raw.min_x()),
        top: f64::max(0.0, ext.min_y() - raw.min_y()),
        right: f64::max(0.0, raw.max_x() - ext.max_x()),
        bottom: f64::max(0.0, raw.max_y() - ext.max_y()),
    }
}

/// 想让它**看起来**在 `target`，`SetWindowPos` 该给的原始矩形（C# `MoveAsync` 里的算式）。
pub fn window_rect_for_target(target: Rect, margins: Margins) -> (i32, i32, i32, i32) {
    (
        (target.x - margins.left).round() as i32,
        (target.y - margins.top).round() as i32,
        (target.width + margins.left + margins.right).round() as i32,
        (target.height + margins.top + margins.bottom).round() as i32,
    )
}

/// 「看得见的边框」：DWM 扩展边框优先，退回 `GetWindowRect`（C# `TryGetVisibleBounds`）。
pub fn visible_bounds(raw: Option<Rect>, extended: Option<Rect>) -> Option<Rect> {
    if let Some(ext) = extended
        && ext.width > 0.0
        && ext.height > 0.0
    {
        return Some(ext);
    }
    raw
}

/// 是否像"全屏"：正好铺满所在显示器、而且没有标题栏（C# `IsLikelyFullScreen`）。
pub fn is_likely_full_screen(bounds: Rect, monitor: Rect, has_caption: bool) -> bool {
    bounds.matches(monitor, MOVE_TOLERANCE) && !has_caption
}

/// 挪窗口时这一轮的结果能不能收工（C# `MoveAsync` 里两个 `return` 条件）。
///
/// - 目标容差 4 像素内：成了；
/// - 第二轮起，若和上一轮几乎一样（1.5 像素内）：说明系统已经摆稳，别再折腾。
pub fn accept_moved_frame(attempt: usize, target: Rect, got: Rect, last: Option<Rect>) -> bool {
    if got.matches(target, MOVE_TOLERANCE) {
        return true;
    }
    attempt >= 1 && last.is_some_and(|previous| got.matches(previous, SETTLE_TOLERANCE))
}

/// 是不是"标准应用窗口"（C# `IsStandardAppWindow`）。
pub fn is_standard_app_window(window: &WindowProbe) -> bool {
    if window.ex_style & WS_EX_TOOLWINDOW != 0 {
        return false;
    }
    if window.has_owner && window.ex_style & WS_EX_APPWINDOW == 0 {
        return false;
    }
    if window.rect.width <= 40.0 || window.rect.height <= 40.0 {
        return false;
    }
    !is_shell_class(&window.class)
}

/// 桌面/任务栏/系统壳的窗口类名（C# 里那张黑名单）。
///
/// 注意 C# 先把 `ApplicationFrameWindow` 也列进黑名单，随后又特意放过 UWP 应用，
/// 所以它最终是**允许**的——这里直接不列它。
fn is_shell_class(class: &str) -> bool {
    matches!(
        class,
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Windows.UI.Core.CoreWindow"
    )
}

/// SnugWin 自己和游戏查看器不该出现在窗口列表里（C# `IsExcludedApp`）。
pub fn is_excluded_app(app_id: Option<&str>) -> bool {
    let Some(app_id) = app_id else {
        return false;
    };
    app_id.eq_ignore_ascii_case("SnugWin.App.exe") || app_id.to_lowercase().starts_with("gameviewer")
}

/// 枚举"标准应用窗口"（C# `EnumerateStandardWindows`）。
///
/// `meta_of` 用来补进程名（真机要读 PE 版本信息，测试直接给表）。
pub fn enumerate_standard(
    probes: &[WindowProbe],
    self_pid: i32,
    pid_filter: Option<i32>,
    include_minimized: bool,
    meta_of: impl Fn(i32) -> ProcessMeta,
) -> Vec<WindowInfo> {
    let mut results = Vec::new();
    let mut cache: HashMap<i32, ProcessMeta> = HashMap::new();

    for probe in probes {
        if !probe.visible || probe.cloaked {
            continue;
        }
        if probe.pid == self_pid {
            continue;
        }
        if let Some(filter) = pid_filter
            && probe.pid != filter
        {
            continue;
        }
        if !include_minimized && probe.minimized {
            continue;
        }
        if !is_standard_app_window(probe) {
            continue;
        }
        let title = WindowInfo::visible_title(&probe.title);
        if title.is_empty() {
            continue;
        }

        let meta = cache.entry(probe.pid).or_insert_with(|| meta_of(probe.pid));
        if is_excluded_app(meta.app_id.as_deref()) {
            continue;
        }
        results.push(WindowInfo {
            key: WindowKey::new(probe.pid, probe.handle),
            app_name: meta.name.clone(),
            app_id: meta.app_id.clone(),
            title,
            minimized: probe.minimized,
        });
    }
    results
}

/// 正在弹询问框的窗口（C# `AskingWindowAsync`）：有 owner 或是对话框类 `#32770`，
/// 且标题不是空的。这里**不**套用"标准应用窗口"那套过滤。
pub fn asking_window(
    probes: &[WindowProbe],
    pid: i32,
    meta_of: impl Fn(i32) -> ProcessMeta,
) -> Option<WindowInfo> {
    let probe = probes.iter().find(|probe| {
        probe.visible
            && probe.pid == pid
            && (probe.has_owner || probe.class == "#32770")
            && !WindowInfo::visible_title(&probe.title).is_empty()
    })?;
    let meta = meta_of(pid);
    Some(WindowInfo {
        key: WindowKey::new(pid, probe.handle),
        app_name: meta.name,
        app_id: meta.app_id,
        title: WindowInfo::visible_title(&probe.title),
        minimized: false,
    })
}

/// 屏幕上可见窗口的层级顺序（C# `GetOnScreenWindowZOrder`）：探测顺序即 Z 序。
pub fn on_screen_z_order(probes: &[WindowProbe]) -> Vec<(u64, Rect)> {
    probes
        .iter()
        .filter(|probe| probe.visible && !probe.minimized)
        .filter(|probe| probe.rect.width > 1.0 && probe.rect.height > 1.0)
        .map(|probe| (probe.handle, probe.rect))
        .collect()
}

/// 缩放用哪几个键：正数放大（Ctrl+=），负数缩小（Ctrl+-），C# `ZoomAsync`。
pub fn zoom_step_keys(steps: i32) -> Vec<u16> {
    let key = if steps > 0 { VK_OEM_PLUS.0 } else { VK_OEM_MINUS.0 };
    vec![key; steps.unsigned_abs() as usize]
}

// ---------------------------------------------------------------------------
// 平台接口
// ---------------------------------------------------------------------------

/// 窗口相关的 Win32 能力。真机实现是 [`WinDesktop`]。
pub trait WindowApi: Send + Sync {
    /// 所有顶层窗口的状态（`EnumWindows` 的批量版本）。
    fn probes(&self) -> Vec<WindowProbe>;
    /// 只探测某个进程的顶层窗口。
    ///
    /// 默认实现是"全量探测再过滤"，语义一样但白扫一遍：真机上 708 个顶层窗口，
    /// 每多扫一次就是几毫秒到一秒半（见 [`WinDesktop::probes`]）。真机实现改成在
    /// `EnumWindows` 回调里先比 pid，只对命中的窗口做完整探测。
    fn probes_of(&self, pid: i32) -> Vec<WindowProbe> {
        self.probes().into_iter().filter(|probe| probe.pid == pid).collect()
    }
    /// 单个窗口的状态；窗口已经没了（或不是窗口）返回 `None`。
    fn probe(&self, handle: u64) -> Option<WindowProbe>;
    fn process_meta(&self, pid: i32) -> ProcessMeta;
    /// 看得见的边框（DWM 扩展边框优先）。
    fn visible_bounds(&self, handle: u64) -> Option<Rect>;
    /// `(GetWindowRect, DWM 扩展边框)`，任一项取不到就是 `None`。
    fn raw_and_extended(&self, handle: u64) -> (Option<Rect>, Option<Rect>);
    fn show(&self, handle: u64, command: ShowCommand);
    fn set_window_rect(&self, handle: u64, x: i32, y: i32, width: i32, height: i32);
    /// 请求前台焦点；系统拒绝时不合并其它程序的输入队列来强抢焦点。
    fn focus(&self, handle: u64);
    /// 只读确认目标仍在前台；无法确认的平台默认拒绝全局按键注入。
    fn is_foreground(&self, _handle: u64) -> bool {
        false
    }
    fn post_close(&self, handle: u64) -> bool;
    /// 发一次 Ctrl(+Shift)+某键（C# `SendKeystroke`）。
    fn send_ctrl_key(&self, virtual_key: u16, with_shift: bool);
    fn window_at_point(&self, point: snug_core::Point) -> Option<u64>;
    fn root_of(&self, handle: u64) -> Option<u64>;
    /// 窗口所在显示器的矩形（C# `IsLikelyFullScreen` 里的 `GetMonitorInfoW`）。
    fn monitor_rect_of(&self, handle: u64) -> Option<Rect>;
}

/// `ShowWindow` 的三个用途，对应 C# 的 `SW_*`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowCommand {
    Restore,
    Minimize,
    Show,
}

/// 关窗按钮位置来自 UIA（C# `UiaHelper.FindCloseButtonRect`）。
pub trait CloseButtonFinder: Send + Sync {
    fn find_close_button(&self, handle: u64) -> Option<Rect>;
}

/// 等待节奏，对应 C# 里的几个 `Task.Delay`。测试里全设 0。
#[derive(Debug, Clone, Copy)]
pub struct WindowTimings {
    /// 每次挪完等系统落位（C# `RetryDelayMs`，120ms）。
    pub move_retry: Duration,
    /// 从最大化恢复后等它真的缩回来（C# 200ms）。
    pub restore_settle: Duration,
    /// 抢前台之后等焦点稳一下（C# 60ms）。
    pub focus_settle: Duration,
    /// 两下缩放之间的间隔（C# 70ms）。
    pub zoom_step: Duration,
}

impl Default for WindowTimings {
    fn default() -> Self {
        Self {
            move_retry: Duration::from_millis(120),
            restore_settle: Duration::from_millis(200),
            focus_settle: Duration::from_millis(60),
            zoom_step: Duration::from_millis(70),
        }
    }
}

fn sleep_for(duration: Duration) {
    if !duration.is_zero() {
        std::thread::sleep(duration);
    }
}

// ---------------------------------------------------------------------------
// 窗口控制
// ---------------------------------------------------------------------------

/// 窗口控制本体：`snug_core::desk::WindowControl` 的实现（C# `WindowService`）。
pub struct Windows<A: WindowApi> {
    api: A,
    timings: WindowTimings,
    close_buttons: Option<Arc<dyn CloseButtonFinder>>,
}

/// 真机窗口控制。
pub type WinWindows = Windows<WinDesktop>;

impl WinWindows {
    /// 真机窗口控制：默认已经接上 UIA 的关闭按钮查询（C# 里 `CloseButtonAsync` 走 `UiaHelper`）。
    pub fn new() -> Self {
        Self {
            api: WinDesktop,
            timings: WindowTimings::default(),
            close_buttons: Some(Arc::new(crate::uia::UiaCloseButtons)),
        }
    }
}

impl Default for WinWindows {
    fn default() -> Self {
        Self::new()
    }
}

impl<A: WindowApi> Windows<A> {
    pub fn with_api(api: A, timings: WindowTimings) -> Self {
        Self { api, timings, close_buttons: None }
    }

    /// 接上 UIA 的关闭按钮查找器（`uia` 模块完成后调用）。
    pub fn with_close_buttons(mut self, finder: Arc<dyn CloseButtonFinder>) -> Self {
        self.close_buttons = Some(finder);
        self
    }

    /// 这批窗口的可见边框（C# `FramesAsync` / `SnapshotAsync` 的公共部分）。
    fn frames_of(&self, pid_filter: Option<i32>) -> HashMap<WindowKey, Rect> {
        let self_pid = std::process::id() as i32;
        // 按 pid 查时走 `probes_of`：真机上一个 pid 通常只有 1–2 个窗口，
        // 全量探测 708 个再过滤纯属白扫（标题缓存那条路径曾是 1.5 秒）。
        let probes = match pid_filter {
            Some(pid) if pid != self_pid => self.api.probes_of(pid),
            _ => self.api.probes(),
        };
        let windows = enumerate_standard(&probes, self_pid, pid_filter, false, |pid| {
            self.api.process_meta(pid)
        });
        let mut map = HashMap::new();
        for window in windows {
            if let Some(bounds) = self.api.visible_bounds(window.key.handle) {
                map.insert(window.key, bounds);
            }
        }
        map
    }

    /// 屏幕上可见窗口的层级（C# `GetOnScreenWindowZOrder`）。
    pub fn on_screen_windows(&self) -> Vec<(u64, Rect)> {
        on_screen_z_order(&self.api.probes())
    }
}

impl<A: WindowApi> WindowControl for Windows<A> {
    fn exists(&self, window: WindowKey) -> bool {
        // C#：先 IsWindow，再核对 PID，防止句柄被系统复用。
        self.api
            .probe(window.handle)
            .is_some_and(|probe| probe.pid == window.pid)
    }

    fn is_shown(&self, window: WindowKey) -> bool {
        self.api.probe(window.handle).is_some_and(|probe| {
            probe.pid == window.pid && probe.visible && !probe.minimized && !probe.cloaked
        })
    }

    fn look(&self, window: WindowKey) -> Option<WindowLook> {
        let probe = self.api.probe(window.handle)?;
        if probe.pid != window.pid {
            return None;
        }
        let frame = self.api.visible_bounds(window.handle)?;
        let full_screen = self.api.monitor_rect_of(window.handle).is_some_and(|monitor| {
            is_likely_full_screen(frame, monitor, probe.style & WS_CAPTION == WS_CAPTION)
        });
        Some(WindowLook { frame, full_screen, minimized: probe.minimized })
    }

    fn move_window(&self, window: WindowKey, target: Rect) -> Option<Rect> {
        let probe = self.api.probe(window.handle)?;
        if probe.minimized || probe.maximized {
            self.api.show(window.handle, ShowCommand::Restore);
        }

        let mut last: Option<Rect> = None;
        for attempt in 0..MOVE_RETRIES {
            let (raw, extended) = self.api.raw_and_extended(window.handle);
            let margins = shadow_margins(raw, extended);
            let (x, y, width, height) = window_rect_for_target(target, margins);
            self.api.set_window_rect(window.handle, x, y, width, height);
            sleep_for(self.timings.move_retry);

            if let Some(got) = self.api.visible_bounds(window.handle) {
                if accept_moved_frame(attempt, target, got, last) {
                    return Some(got);
                }
                last = Some(got);
            }
        }
        last
    }

    fn set_full_screen(&self, window: WindowKey, on: bool) {
        if on {
            // C# 只负责"取消全屏"，进全屏是应用自己的事。
            return;
        }
        let Some(probe) = self.api.probe(window.handle) else {
            return;
        };
        if probe.maximized {
            self.api.show(window.handle, ShowCommand::Restore);
            sleep_for(self.timings.restore_settle);
        }
    }

    fn set_minimized(&self, window: WindowKey, on: bool) {
        if self.api.probe(window.handle).is_none() {
            return;
        }
        self.api.show(
            window.handle,
            if on { ShowCommand::Minimize } else { ShowCommand::Restore },
        );
    }

    fn focus(&self, window: WindowKey) {
        let Some(probe) = self.api.probe(window.handle) else {
            return;
        };
        if probe.minimized {
            self.api.show(window.handle, ShowCommand::Restore);
        }
        self.api.focus(window.handle);
    }

    fn zoom(&self, window: WindowKey, steps: i32) -> bool {
        if steps == 0 {
            return true;
        }
        if !self.exists(window) {
            return false;
        }
        self.focus(window);
        sleep_for(self.timings.focus_settle);
        for key in zoom_step_keys(steps) {
            // focus 是尽力请求；被系统拒绝或中途切换应用时，不能向其它窗口发 Ctrl 键。
            if !self.api.is_foreground(window.handle) {
                return false;
            }
            self.api.send_ctrl_key(key, false);
            sleep_for(self.timings.zoom_step);
        }
        true
    }

    fn shown_windows(&self, pid: i32) -> Vec<WindowInfo> {
        let self_pid = std::process::id() as i32;
        let probes = if pid == self_pid { self.api.probes() } else { self.api.probes_of(pid) };
        enumerate_standard(&probes, self_pid, Some(pid), false, |pid| {
            self.api.process_meta(pid)
        })
    }

    fn asking_window(&self, pid: i32) -> Option<WindowInfo> {
        let self_pid = std::process::id() as i32;
        let probes = if pid == self_pid { self.api.probes() } else { self.api.probes_of(pid) };
        asking_window(&probes, pid, |pid| self.api.process_meta(pid))
    }

    fn close(&self, window: WindowKey) -> bool {
        if self.api.probe(window.handle).is_none() {
            return false;
        }
        self.api.post_close(window.handle)
    }

    fn close_button(&self, window: WindowKey) -> Option<Rect> {
        let finder = self.close_buttons.as_ref()?;
        if self.api.probe(window.handle).is_none() {
            return None;
        }
        finder.find_close_button(window.handle)
    }

    fn frames(&self, pid: i32) -> HashMap<WindowKey, Rect> {
        self.frames_of(Some(pid))
    }

    fn snapshot(&self) -> HashMap<WindowKey, Rect> {
        self.frames_of(None)
    }

    fn restore(&self, snapshot: &HashMap<WindowKey, Rect>, except: Option<WindowKey>) {
        for (key, saved) in snapshot {
            if except == Some(*key) {
                continue;
            }
            let Some(probe) = self.api.probe(key.handle) else {
                continue;
            };
            if probe.minimized {
                continue;
            }
            let Some(now) = self.api.visible_bounds(key.handle) else {
                continue;
            };
            if now.matches(*saved, RESTORE_TOLERANCE) {
                continue;
            }
            let _ = self.move_window(*key, *saved);
        }
    }
}

// ---------------------------------------------------------------------------
// 真机实现
// ---------------------------------------------------------------------------

/// 真机窗口子系统。
#[derive(Debug, Default, Clone, Copy)]
pub struct WinDesktop;

fn hwnd(handle: u64) -> HWND {
    HWND(handle as usize as *mut core::ffi::c_void)
}

fn own_hwnd(handle: u64) -> Option<HWND> {
    let window = hwnd(handle);
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    (thread != 0 && pid == std::process::id()).then_some(window)
}

fn set_window_bounds(window: HWND, x: i32, y: i32, width: i32, height: i32) -> bool {
    unsafe { SetWindowPos(window, None, x, y, width, height, SWP_NOZORDER | SWP_NOACTIVATE) }
        .is_ok()
}

fn rect_of(rect: RECT) -> Rect {
    Rect::new(
        rect.left as f64,
        rect.top as f64,
        (rect.right - rect.left) as f64,
        (rect.bottom - rect.top) as f64,
    )
}

impl WinDesktop {
    fn window_rect(window: HWND) -> Option<Rect> {
        let mut rect = RECT::default();
        unsafe { GetWindowRect(window, &mut rect) }.ok()?;
        Some(rect_of(rect))
    }

    fn dwm_bounds(window: HWND) -> Option<Rect> {
        let mut rect = RECT::default();
        unsafe {
            DwmGetWindowAttribute(
                window,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut rect as *mut RECT as *mut core::ffi::c_void,
                std::mem::size_of::<RECT>() as u32,
            )
        }
        .ok()?;
        Some(rect_of(rect))
    }

    fn is_cloaked(window: HWND) -> bool {
        let mut cloaked = 0i32;
        let status = unsafe {
            DwmGetWindowAttribute(
                window,
                DWMWA_CLOAKED,
                &mut cloaked as *mut i32 as *mut core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            )
        };
        status.is_ok() && cloaked != 0
    }

    fn class_name(window: HWND) -> String {
        let mut buffer = [0u16; 256];
        let len = unsafe { GetClassNameW(window, &mut buffer) };
        if len <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..len as usize])
    }

    /// 读窗口标题（**只用于别的进程的窗口**）。
    ///
    /// 真机实测（708 个顶层窗口）：用 `SendMessageTimeoutW(WM_GETTEXT, 150ms)` 逐个同步问，
    /// 累计 **1499ms**；用 `GetWindowTextW` 只读窗口标题缓存，累计 **0.8ms**，
    /// 而且 438 个非空标题**一个字符都不差**。这一秒半原先全花在共享 lane 上：
    /// `check_returns` 每 5 秒要枚举两次窗口，于是点 App 的动作排在它后面等 3 秒，
    /// 表现为「点了半天才反应」。
    ///
    /// 这里**不能**用 `GetWindowTextW` 读自己进程的窗口：那会同步投递 `WM_GETTEXT`，
    /// 自己的界面线程若正阻塞（收尾时在等 desk 线程撤屏）就会自己等自己——真机上
    /// 「退出卡死、虚拟屏残留」正是这条路径。所以调用方（[`Self::probe_window`]）对
    /// 自己的进程直接跳过标题。跨进程的 `GetWindowTextW` 只读缓存、不发消息，
    /// 对端卡死也不会拖住我们（MSDN 明确说明这正是它的设计意图）。
    fn window_title(window: HWND) -> String {
        let mut buffer = [0u16; 512];
        let len = unsafe { GetWindowTextW(window, &mut buffer) };
        if len <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..(len as usize).min(buffer.len())])
    }

    /// 一次把这个窗口的样式、状态、类名、标题全查出来。
    fn probe_window(handle: u64) -> Option<WindowProbe> {
        if handle == 0 {
            return None;
        }
        let window = hwnd(handle);
        let self_pid = std::process::id();
        unsafe {
            if !IsWindow(Some(window)).as_bool() {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(window, Some(&mut pid));
            let has_owner = GetWindow(window, GW_OWNER)
                .map(|owner| !owner.0.is_null())
                .unwrap_or(false);
            // 自己的窗口不读标题：快照（`probes`）只为恢复别人的窗口，而读自己的标题
            // 要跨线程发 `WM_GETTEXT`，收尾时 UI 线程正阻塞等 desk 线程，白白多等一个超时。
            let title = if pid == self_pid { String::new() } else { Self::window_title(window) };
            Some(WindowProbe {
                handle,
                pid: pid as i32,
                visible: IsWindowVisible(window).as_bool(),
                minimized: IsIconic(window).as_bool(),
                maximized: IsZoomed(window).as_bool(),
                cloaked: Self::is_cloaked(window),
                ex_style: GetWindowLongPtrW(window, GWL_EXSTYLE) as u32,
                style: GetWindowLongPtrW(window, GWL_STYLE) as u32,
                has_owner,
                rect: Self::window_rect(window).unwrap_or(Rect::ZERO),
                class: Self::class_name(window),
                title,
            })
        }
    }

    fn key_input(virtual_key: u16, down: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(virtual_key),
                    wScan: 0,
                    dwFlags: if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP },
                    time: 0,
                    // C# 用的就是 0x534E5547（ASCII "SNUG"），方便认自己注入的按键。
                    dwExtraInfo: 0x534E_5547,
                },
            },
        }
    }
}

unsafe extern "system" fn collect_window(window: HWND, data: LPARAM) -> BOOL {
    if data.0 != 0 {
        let handles = unsafe { &mut *(data.0 as *mut Vec<u64>) };
        handles.push(window.0 as usize as u64);
    }
    BOOL(1)
}

/// `probes_of` 的枚举状态：只要命中 pid 的句柄。
struct CollectOfPid<'a> {
    pid: i32,
    handles: &'a mut Vec<u64>,
}

/// 与 [`collect_window`] 相同，但先在 `EnumWindows` 回调里按 pid 过滤，
/// 避免为无关窗口做完整探测（真机上那是 700 多个窗口）。
unsafe extern "system" fn collect_window_of_pid(window: HWND, data: LPARAM) -> BOOL {
    if data.0 != 0 {
        let state = unsafe { &mut *(data.0 as *mut CollectOfPid) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        if pid as i32 == state.pid {
            state.handles.push(window.0 as usize as u64);
        }
    }
    BOOL(1)
}

/// 进程的可执行文件全路径（C# `GetProcessImagePath`）。
fn process_image_path(pid: i32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32).ok()?;
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let status = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        status.ok()?;
        Some(String::from_utf16_lossy(&buffer[..size as usize]))
    }
}

/// 读可执行文件的版本信息里的 `FileDescription`（C# `FileVersionInfo.FileDescription`）。
///
/// 拿不到（没有版本资源、文件被删）就返回 `None`，调用方退回文件名。
fn file_description(path: &str) -> Option<String> {
    let wide_path = to_wide(path);
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide_path.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut block = vec![0u8; size as usize];
        GetFileVersionInfoW(
            PCWSTR(wide_path.as_ptr()),
            None,
            size,
            block.as_mut_ptr() as *mut core::ffi::c_void,
        )
        .ok()?;

        let mut translation: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut translation_len = 0u32;
        if !VerQueryValueW(
            block.as_ptr() as *const core::ffi::c_void,
            w!("\\VarFileInfo\\Translation"),
            &mut translation,
            &mut translation_len,
        )
        .as_bool()
            || translation_len < 4
            || translation.is_null()
        {
            return None;
        }
        let language = *(translation as *const u16);
        let codepage = *((translation as *const u16).add(1));

        let query = format!("\\StringFileInfo\\{language:04x}{codepage:04x}\\FileDescription");
        let wide_query = to_wide(&query);
        let mut value: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut value_len = 0u32;
        if !VerQueryValueW(
            block.as_ptr() as *const core::ffi::c_void,
            PCWSTR(wide_query.as_ptr()),
            &mut value,
            &mut value_len,
        )
        .as_bool()
            || value.is_null()
            || value_len == 0
        {
            return None;
        }
        let units = std::slice::from_raw_parts(value as *const u16, value_len as usize);
        let text = from_wide(units).trim().to_string();
        if text.is_empty() { None } else { Some(text) }
    }
}

impl WindowApi for WinDesktop {
    fn probes(&self) -> Vec<WindowProbe> {
        let mut handles: Vec<u64> = Vec::new();
        unsafe {
            let _ = EnumWindows(
                Some(collect_window),
                LPARAM(&mut handles as *mut Vec<u64> as isize),
            );
        }
        let mut out = Vec::with_capacity(handles.len());
        for handle in handles {
            if let Some(probe) = Self::probe_window(handle) {
                out.push(probe);
            }
        }
        out
    }

    fn probes_of(&self, pid: i32) -> Vec<WindowProbe> {
        let mut handles: Vec<u64> = Vec::new();
        unsafe {
            let _ = EnumWindows(
                Some(collect_window_of_pid),
                LPARAM(&mut CollectOfPid { pid, handles: &mut handles } as *mut _ as isize),
            );
        }
        handles
            .into_iter()
            .filter_map(Self::probe_window)
            .collect()
    }

    fn probe(&self, handle: u64) -> Option<WindowProbe> {
        Self::probe_window(handle)
    }

    fn process_meta(&self, pid: i32) -> ProcessMeta {
        let Some(path) = process_image_path(pid) else {
            return ProcessMeta { name: format!("PID {pid}"), app_id: None };
        };
        let exe = std::path::Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().to_string());
        let stem = std::path::Path::new(&path)
            .file_stem()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("PID {pid}"));
        // C# 优先用文件描述（"记事本" 而不是 "notepad"）。
        ProcessMeta { name: file_description(&path).unwrap_or(stem), app_id: exe }
    }

    fn visible_bounds(&self, handle: u64) -> Option<Rect> {
        let window = hwnd(handle);
        visible_bounds(Self::window_rect(window), Self::dwm_bounds(window))
    }

    fn raw_and_extended(&self, handle: u64) -> (Option<Rect>, Option<Rect>) {
        let window = hwnd(handle);
        (Self::window_rect(window), Self::dwm_bounds(window))
    }

    fn show(&self, handle: u64, command: ShowCommand) {
        let command = match command {
            ShowCommand::Restore => SW_RESTORE,
            ShowCommand::Minimize => SW_MINIMIZE,
            ShowCommand::Show => windows::Win32::UI::WindowsAndMessaging::SW_SHOW,
        };
        unsafe {
            let _ = ShowWindow(hwnd(handle), command);
        }
    }

    fn set_window_rect(&self, handle: u64, x: i32, y: i32, width: i32, height: i32) {
        set_window_bounds(hwnd(handle), x, y, width, height);
    }

    fn focus(&self, handle: u64) {
        let window = hwnd(handle);
        unsafe {
            // 不用 AttachThreadInput：合并后台、前台与目标应用的输入队列后，
            // 目标应用响应慢可能连带拖住用户正在操作的前台程序。
            // 焦点请求只尽力一次，系统的前台限制仍由系统决定。
            let _ = SetForegroundWindow(window);
        }
    }

    fn is_foreground(&self, handle: u64) -> bool {
        handle != 0 && unsafe { GetForegroundWindow() == hwnd(handle) }
    }

    fn post_close(&self, handle: u64) -> bool {
        unsafe { PostMessageW(Some(hwnd(handle)), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok() }
    }

    fn send_ctrl_key(&self, virtual_key: u16, with_shift: bool) {
        let mut inputs = Vec::with_capacity(6);
        inputs.push(Self::key_input(VK_CONTROL.0, true));
        if with_shift {
            inputs.push(Self::key_input(VK_SHIFT.0, true));
        }
        inputs.push(Self::key_input(virtual_key, true));
        inputs.push(Self::key_input(virtual_key, false));
        if with_shift {
            inputs.push(Self::key_input(VK_SHIFT.0, false));
        }
        inputs.push(Self::key_input(VK_CONTROL.0, false));
        unsafe {
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }

    fn window_at_point(&self, point: snug_core::Point) -> Option<u64> {
        let window = unsafe {
            WindowFromPoint(POINT { x: point.x.round() as i32, y: point.y.round() as i32 })
        };
        if window.0.is_null() { None } else { Some(window.0 as usize as u64) }
    }

    fn root_of(&self, handle: u64) -> Option<u64> {
        let window = unsafe { GetAncestor(hwnd(handle), GA_ROOT) };
        if window.0.is_null() { None } else { Some(window.0 as usize as u64) }
    }

    fn monitor_rect_of(&self, handle: u64) -> Option<Rect> {
        unsafe {
            let monitor = MonitorFromWindow(hwnd(handle), MONITOR_DEFAULTTONEAREST);
            if monitor.0.is_null() {
                return None;
            }
            let mut info = MONITORINFO::default();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return None;
            }
            Some(rect_of(info.rcMonitor))
        }
    }
}

/// 便于别处判断某窗口是不是我们自己的进程。
pub fn is_own_process(pid: i32) -> bool {
    pid == std::process::id() as i32
}

/// 读取本进程窗口在调用线程 DPI 坐标系里的原生矩形，包括隐藏窗口。
pub fn own_window_rect(handle: u64) -> Option<Rect> {
    WinDesktop::window_rect(own_hwnd(handle)?)
}

/// 在调用线程 DPI 坐标系里移动和调整本进程窗口，保持窗口状态与层级。
pub fn position_own_window(handle: u64, bounds: Rect) -> bool {
    let Some(window) = own_hwnd(handle) else {
        return false;
    };
    let rounded = [bounds.x, bounds.y, bounds.width, bounds.height].map(f64::round);
    if !rounded
        .iter()
        .all(|value| (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(value))
    {
        return false;
    }
    let [x, y, width, height] = rounded.map(|value| value as i32);
    width > 0 && height > 0 && set_window_bounds(window, x, y, width, height)
}

/// 异步唤醒本进程窗口的原生重绘，不依赖 egui 合并后的 repaint 请求。
///
/// winit 在 `WM_PAINT` 中派发 `RedrawRequested`；投递消息不等待窗口线程，
/// 也不改变窗口的可见性、位置或焦点。失效或其它进程的句柄一律拒绝。
pub fn request_own_window_redraw(handle: u64) -> bool {
    own_hwnd(handle)
        .is_some_and(|window| unsafe { PostMessageW(Some(window), WM_PAINT, WPARAM(0), LPARAM(0)) }.is_ok())
}

/// 在本进程里找标题匹配的顶层可见窗口，返回它的句柄。
///
/// 应用层要用它找自己的窗口：eframe/winit 不对外暴露 `HWND`，而"把手机屏界面
/// 搬到虚拟屏上"必须走 `SetWindowPos`（[`crate::placement::apply`]）。
/// 只认本进程自己的窗口，不会碰别的程序。
pub fn find_own_window(title: &str) -> Option<u64> {
    struct Search {
        pid: i32,
        title: Vec<u16>,
        handle: Option<u64>,
    }

    unsafe extern "system" fn find(window: HWND, data: LPARAM) -> BOOL {
        let search = unsafe { &mut *(data.0 as *mut Search) };
        // 注意：**不能**过滤 IsWindowVisible。应用会把自己那个窗口藏起来
        // （手机屏没就绪时只留托盘），藏起来之后还得能再被找到并搬回屏上，
        // 否则就是一去不返（真机阶段 B 撞到过）。
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        if pid as i32 != search.pid {
            return BOOL(1);
        }
        let mut buffer = [0u16; 256];
        let len = unsafe { GetWindowTextW(window, &mut buffer) };
        if len <= 0 {
            return BOOL(1);
        }
        if buffer[..len as usize] == search.title[..] {
            search.handle = Some(window.0 as usize as u64);
        }
        BOOL(1)
    }

    let mut search = Search {
        pid: std::process::id() as i32,
        title: title.encode_utf16().collect(),
        handle: None,
    };
    unsafe {
        let _ = EnumWindows(Some(find), LPARAM(&mut search as *mut Search as isize));
    }
    search.handle
}

// ---------------------------------------------------------------------------
// 单测（纯逻辑、假实现与独占隐藏测试窗口，不操作用户窗口）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn native_helpers_manage_only_our_window_and_preserve_hidden_state() {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GetDesktopWindow, MSG, PM_REMOVE, PeekMessageW,
            WINDOW_EX_STYLE, WINDOW_STYLE,
        };

        struct TestWindow(HWND);
        impl Drop for TestWindow {
            fn drop(&mut self) {
                let _ = unsafe { DestroyWindow(self.0) };
            }
        }

        let window = TestWindow(unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(), w!("STATIC"), w!("Snug native redraw test"),
                WINDOW_STYLE::default(), 0, 0, 1, 1, None, None, None, None,
            ).expect("create an exclusive hidden test window")
        });
        let handle = window.0.0 as usize as u64;
        let mut message = MSG::default();
        assert!(!unsafe { IsWindowVisible(window.0) }.as_bool());
        while unsafe { PeekMessageW(&mut message, Some(window.0), WM_PAINT, WM_PAINT, PM_REMOVE) }.as_bool() {}

        // Neither dispatch nor render the messages: every tick must independently wake the queue.
        for _ in 0..2 {
            assert!(request_own_window_redraw(handle));
            assert!(unsafe {
                PeekMessageW(&mut message, Some(window.0), WM_PAINT, WM_PAINT, PM_REMOVE)
            }.as_bool());
            assert_eq!(message.hwnd, window.0);
            assert_eq!(message.message, WM_PAINT);
        }
        assert!(!unsafe { IsWindowVisible(window.0) }.as_bool());
        let bounds = Rect::new(120.0, 160.0, 480.0, 360.0);
        let style = unsafe { GetWindowLongPtrW(window.0, GWL_STYLE) };
        assert!(position_own_window(handle, bounds));
        assert_eq!(own_window_rect(handle), Some(bounds));
        assert_eq!(unsafe { GetWindowLongPtrW(window.0, GWL_STYLE) }, style);
        assert_ne!(unsafe { GetForegroundWindow() }, window.0);
        assert!(!unsafe { IsWindowVisible(window.0) }.as_bool());
        assert!(!request_own_window_redraw(0));
        let desktop = unsafe { GetDesktopWindow() };
        assert!(unsafe { IsWindow(Some(desktop)) }.as_bool());
        let foreign_handle = desktop.0 as usize as u64;
        assert!(!request_own_window_redraw(foreign_handle));
        assert!(own_window_rect(foreign_handle).is_none());
        assert!(!position_own_window(foreign_handle, bounds));
        drop(window);
        assert!(!request_own_window_redraw(handle));
        assert!(own_window_rect(handle).is_none());
        assert!(!position_own_window(handle, bounds));
    }

    // ----- 纯函数 -----

    #[test]
    fn shadow_margins_measure_hidden_border() {
        let raw = Rect::new(100.0, 100.0, 1100.0, 600.0);
        let extended = Rect::new(108.0, 100.0, 1084.0, 592.0);
        let margins = shadow_margins(Some(raw), Some(extended));
        assert_eq!(margins.left, 8.0);
        assert_eq!(margins.top, 0.0);
        assert_eq!(margins.right, 8.0);
        assert_eq!(margins.bottom, 8.0);

        // 没有 DWM 数据（Win7 风格或窗口已销毁）就当作没有边距。
        assert_eq!(shadow_margins(Some(raw), None), Margins::ZERO);
        assert_eq!(shadow_margins(None, Some(extended)), Margins::ZERO);
        // DWM 边框比原始矩形还大时不产生负边距。
        let bigger = shadow_margins(Some(raw), Some(Rect::new(90.0, 90.0, 1120.0, 620.0)));
        assert_eq!(bigger, Margins::ZERO);
    }

    #[test]
    fn window_rect_for_target_compensates_margins() {
        let target = Rect::new(100.0, 50.0, 540.0, 960.0);
        let margins = Margins { left: 8.0, top: 0.0, right: 8.0, bottom: 8.0 };
        assert_eq!(window_rect_for_target(target, margins), (92, 50, 556, 968));
        assert_eq!(
            window_rect_for_target(target, Margins::ZERO),
            (100, 50, 540, 960)
        );
    }

    #[test]
    fn visible_bounds_prefers_dwm_then_falls_back() {
        let raw = Rect::new(0.0, 0.0, 100.0, 100.0);
        let extended = Rect::new(8.0, 0.0, 84.0, 92.0);
        assert_eq!(visible_bounds(Some(raw), Some(extended)), Some(extended));
        // DWM 给的是空矩形（窗口还没合成完）→ 退回 GetWindowRect。
        assert_eq!(visible_bounds(Some(raw), Some(Rect::ZERO)), Some(raw));
        assert_eq!(visible_bounds(Some(raw), None), Some(raw));
        assert_eq!(visible_bounds(None, Some(Rect::ZERO)), None);
    }

    #[test]
    fn full_screen_needs_matching_monitor_and_no_caption() {
        let monitor = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        assert!(is_likely_full_screen(monitor, monitor, false));
        // 有标题栏就不算全屏。
        assert!(!is_likely_full_screen(monitor, monitor, true));
        // 差得太远也不算。
        assert!(!is_likely_full_screen(Rect::new(0.0, 0.0, 1600.0, 900.0), monitor, false));
        // 4 像素容差内仍算铺满。
        assert!(is_likely_full_screen(Rect::new(2.0, 0.0, 1920.0, 1080.0), monitor, false));
    }

    #[test]
    fn accept_moved_frame_uses_two_tolerances() {
        let target = Rect::new(100.0, 100.0, 540.0, 960.0);
        // 第一轮就到位。
        assert!(accept_moved_frame(0, target, Rect::new(101.0, 99.0, 540.0, 960.0), None));
        // 差得远，且没有上一轮可以参考 → 继续试。
        let off = Rect::new(300.0, 300.0, 540.0, 960.0);
        assert!(!accept_moved_frame(0, target, off, None));
        // 第二轮和上一轮几乎一样 → 系统就这水平了，收。
        assert!(accept_moved_frame(1, target, off, Some(Rect::new(301.0, 300.0, 540.0, 960.0))));
        // 第一轮永不以"稳定"为理由收工（C# 的 attempt >= 1）。
        assert!(!accept_moved_frame(0, target, off, Some(off)));
        // 第二轮但位置变了 → 继续。
        assert!(!accept_moved_frame(1, target, off, Some(Rect::new(200.0, 200.0, 540.0, 960.0))));
    }

    #[test]
    fn standard_window_filters_tool_and_shell_windows() {
        let base = WindowProbe::new(1, 100, "Notepad", "无标题 - 记事本", Rect::new(0.0, 0.0, 800.0, 600.0));
        assert!(is_standard_app_window(&base));

        let mut tool = base.clone();
        tool.ex_style = WS_EX_TOOLWINDOW;
        assert!(!is_standard_app_window(&tool));

        let mut owned = base.clone();
        owned.has_owner = true;
        assert!(!is_standard_app_window(&owned));
        owned.ex_style = WS_EX_APPWINDOW;
        assert!(is_standard_app_window(&owned), "有 WS_EX_APPWINDOW 的对话框要保留");

        let mut tiny = base.clone();
        tiny.rect = Rect::new(0.0, 0.0, 40.0, 600.0);
        assert!(!is_standard_app_window(&tiny));

        for class in ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd", "Windows.UI.Core.CoreWindow"] {
            let mut shell = base.clone();
            shell.class = class.to_string();
            assert!(!is_standard_app_window(&shell), "{class} 是系统壳");
        }
        let mut frame = base.clone();
        frame.class = "ApplicationFrameWindow".to_string();
        assert!(is_standard_app_window(&frame), "UWP 应用要保留");
    }

    #[test]
    fn excluded_apps_are_snugwin_and_gameviewer() {
        assert!(is_excluded_app(Some("SnugWin.App.exe")));
        assert!(is_excluded_app(Some("snugwin.app.exe")));
        assert!(is_excluded_app(Some("GameViewer.exe")));
        assert!(is_excluded_app(Some("gameviewerserver.exe")));
        assert!(!is_excluded_app(Some("notepad.exe")));
        assert!(!is_excluded_app(None));
    }

    #[test]
    fn enumerate_standard_keeps_only_real_app_windows() {
        let mut normal = WindowProbe::new(0x10, 100, "Notepad", "  \u{200B}记事本  ", Rect::new(0.0, 0.0, 800.0, 600.0));
        normal.title = "记事本".to_string();

        let mut hidden = normal.clone();
        hidden.handle = 0x11;
        hidden.visible = false;

        let mut cloaked = normal.clone();
        cloaked.handle = 0x12;
        cloaked.cloaked = true;

        let mut minimized = normal.clone();
        minimized.handle = 0x13;
        minimized.minimized = true;

        let mut self_window = normal.clone();
        self_window.handle = 0x14;
        self_window.pid = 999; // self_pid

        let mut other_pid = normal.clone();
        other_pid.handle = 0x15;
        other_pid.pid = 200;

        let mut excluded = normal.clone();
        excluded.handle = 0x16;
        excluded.pid = 300;

        let mut no_title = normal.clone();
        no_title.handle = 0x17;
        no_title.title = "   ".to_string();

        let probes = vec![
            normal.clone(),
            hidden,
            cloaked,
            minimized,
            self_window,
            other_pid,
            excluded,
            no_title,
        ];
        let meta = |pid: i32| ProcessMeta {
            name: format!("app{pid}"),
            app_id: Some(if pid == 300 { "GameViewer.exe".to_string() } else { format!("app{pid}.exe") }),
        };

        let all = enumerate_standard(&probes, 999, None, false, meta);
        assert_eq!(all.len(), 2, "普通窗口 + 另一个进程的普通窗口：{all:?}");
        assert_eq!(all[0].key, WindowKey::new(100, 0x10));
        assert_eq!(all[0].app_name, "app100");
        assert_eq!(all[0].title, "记事本");
        assert!(!all[0].minimized);

        // 限定 PID 200。
        let filtered = enumerate_standard(&probes, 999, Some(200), false, meta);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].key.pid, 200);

        // 允许最小化时，最小化的那条也要进来。
        let with_minimized = enumerate_standard(&probes, 999, None, true, meta);
        assert_eq!(with_minimized.len(), 3);
        assert!(with_minimized.iter().any(|info| info.minimized));
    }

    #[test]
    fn asking_window_finds_owned_dialog_first() {
        let normal = WindowProbe::new(0x10, 100, "Notepad", "记事本", Rect::new(0.0, 0.0, 800.0, 600.0));
        let dialog = WindowProbe {
            handle: 0x20,
            has_owner: true,
            class: "#32770".to_string(),
            title: "要保存吗？".to_string(),
            ..normal.clone()
        };
        let empty_dialog = WindowProbe {
            handle: 0x21,
            class: "#32770".to_string(),
            title: String::new(),
            ..normal.clone()
        };
        let meta = |pid: i32| ProcessMeta { name: format!("app{pid}"), app_id: Some("app.exe".to_string()) };

        let found = asking_window(&[normal.clone(), dialog.clone(), empty_dialog], 100, meta)
            .expect("应该找到询问框");
        assert_eq!(found.key, WindowKey::new(100, 0x20));
        assert_eq!(found.title, "要保存吗？");
        assert_eq!(found.app_name, "app100");

        assert!(asking_window(&[normal], 100, meta).is_none());
    }

    #[test]
    fn on_screen_z_order_skips_minimized_and_tiny() {
        let a = WindowProbe::new(1, 10, "A", "a", Rect::new(0.0, 0.0, 100.0, 100.0));
        let mut minimized = WindowProbe::new(2, 10, "B", "b", Rect::new(0.0, 0.0, 100.0, 100.0));
        minimized.minimized = true;
        let tiny = WindowProbe::new(3, 10, "C", "c", Rect::new(0.0, 0.0, 1.0, 1.0));

        let ordered = on_screen_z_order(&[a.clone(), minimized, tiny]);
        assert_eq!(ordered, vec![(1, a.rect)]);
    }

    #[test]
    fn zoom_keys_follow_direction() {
        assert_eq!(zoom_step_keys(0), Vec::<u16>::new());
        assert_eq!(zoom_step_keys(2), vec![VK_OEM_PLUS.0, VK_OEM_PLUS.0]);
        assert_eq!(zoom_step_keys(-3), vec![VK_OEM_MINUS.0; 3]);
    }

    // ----- 假实现的整条流程 -----

    #[derive(Clone)]
    struct FakeWindow {
        probe: WindowProbe,
        bounds: Rect,
        applied: Option<Rect>,
    }

    #[derive(Default)]
    struct FakeState {
        windows: Vec<FakeWindow>,
        set_calls: Vec<(u64, i32, i32, i32, i32)>,
        shows: Vec<(u64, ShowCommand)>,
        focuses: Vec<u64>,
        closes: Vec<u64>,
        keys: Vec<(u16, bool)>,
        foreground: Option<u64>,
        deny_focus: bool,
        lose_focus_after_keys: Option<usize>,
        /// 前 N 次挪窗故意摆偏一点，逼出重试。
        honor_after: usize,
        /// 每个窗口的"当前可见边框"查询要不要返回 None。
        blind: bool,
    }

    struct FakeApi(Arc<Mutex<FakeState>>);

    impl WindowApi for FakeApi {
        fn probes(&self) -> Vec<WindowProbe> {
            self.0.lock().unwrap().windows.iter().map(|w| w.probe.clone()).collect()
        }

        fn probe(&self, handle: u64) -> Option<WindowProbe> {
            self.0
                .lock()
                .unwrap()
                .windows
                .iter()
                .find(|w| w.probe.handle == handle)
                .map(|w| w.probe.clone())
        }

        fn process_meta(&self, pid: i32) -> ProcessMeta {
            ProcessMeta { name: format!("app{pid}"), app_id: Some(format!("app{pid}.exe")) }
        }

        fn visible_bounds(&self, handle: u64) -> Option<Rect> {
            let state = self.0.lock().unwrap();
            if state.blind {
                return None;
            }
            state
                .windows
                .iter()
                .find(|w| w.probe.handle == handle)
                .map(|w| w.applied.unwrap_or(w.bounds))
        }

        fn raw_and_extended(&self, handle: u64) -> (Option<Rect>, Option<Rect>) {
            let state = self.0.lock().unwrap();
            state
                .windows
                .iter()
                .find(|w| w.probe.handle == handle)
                .map(|w| (Some(w.bounds), None))
                .unwrap_or((None, None))
        }

        fn show(&self, handle: u64, command: ShowCommand) {
            self.0.lock().unwrap().shows.push((handle, command));
        }

        fn set_window_rect(&self, handle: u64, x: i32, y: i32, width: i32, height: i32) {
            let mut state = self.0.lock().unwrap();
            let index = state.set_calls.len();
            state.set_calls.push((handle, x, y, width, height));
            let offset = if index < state.honor_after {
                ((index + 1) * 10) as f64
            } else {
                0.0
            };
            for window in state.windows.iter_mut() {
                if window.probe.handle == handle {
                    window.applied = Some(Rect::new(
                        x as f64 + offset,
                        y as f64,
                        width as f64,
                        height as f64,
                    ));
                    window.probe.rect = Rect::new(x as f64, y as f64, width as f64, height as f64);
                    window.bounds = window.applied.unwrap();
                }
            }
        }

        fn focus(&self, handle: u64) {
            let mut state = self.0.lock().unwrap();
            state.focuses.push(handle);
            if !state.deny_focus {
                state.foreground = Some(handle);
            }
        }

        fn is_foreground(&self, handle: u64) -> bool {
            self.0.lock().unwrap().foreground == Some(handle)
        }

        fn post_close(&self, handle: u64) -> bool {
            self.0.lock().unwrap().closes.push(handle);
            true
        }

        fn send_ctrl_key(&self, virtual_key: u16, with_shift: bool) {
            let mut state = self.0.lock().unwrap();
            state.keys.push((virtual_key, with_shift));
            if state.lose_focus_after_keys.is_some_and(|count| state.keys.len() >= count) {
                state.foreground = None;
            }
        }

        fn window_at_point(&self, _point: snug_core::Point) -> Option<u64> {
            None
        }

        fn root_of(&self, handle: u64) -> Option<u64> {
            Some(handle)
        }

        fn monitor_rect_of(&self, _handle: u64) -> Option<Rect> {
            Some(Rect::new(0.0, 0.0, 1920.0, 1080.0))
        }
    }

    fn fast_timings() -> WindowTimings {
        WindowTimings {
            move_retry: Duration::ZERO,
            restore_settle: Duration::ZERO,
            focus_settle: Duration::ZERO,
            zoom_step: Duration::ZERO,
        }
    }

    fn fake_window(handle: u64, pid: i32, bounds: Rect) -> FakeWindow {
        let mut probe = WindowProbe::new(handle, pid, "Notepad", "记事本", bounds);
        probe.style = WS_CAPTION;
        FakeWindow { probe, bounds, applied: None }
    }

    fn make(windows: Vec<FakeWindow>) -> (Windows<FakeApi>, Arc<Mutex<FakeState>>) {
        let state = Arc::new(Mutex::new(FakeState { windows, ..FakeState::default() }));
        let service = Windows::with_api(FakeApi(state.clone()), fast_timings());
        (service, state)
    }

    #[test]
    fn look_reports_full_screen_and_minimized() {
        let (service, state) = make(vec![fake_window(0x10, 100, Rect::new(0.0, 0.0, 1920.0, 1080.0))]);
        {
            let mut state = state.lock().unwrap();
            // 去掉标题栏 → 满屏且无 caption，算全屏。
            state.windows[0].probe.style = 0;
        }
        let look = service.look(WindowKey::new(100, 0x10)).expect("窗口在");
        assert!(look.full_screen);
        assert!(!look.minimized);
        assert_eq!(look.frame, Rect::new(0.0, 0.0, 1920.0, 1080.0));

        // PID 对不上就当窗口不存在（句柄复用）。
        assert!(service.look(WindowKey::new(101, 0x10)).is_none());

        {
            let mut state = state.lock().unwrap();
            state.windows[0].probe.minimized = true;
            state.windows[0].probe.style = WS_CAPTION;
        }
        let look = service.look(WindowKey::new(100, 0x10)).expect("窗口在");
        assert!(look.minimized);
        assert!(!look.full_screen);
    }

    #[test]
    fn move_window_walks_until_system_settles() {
        let target = Rect::new(100.0, 50.0, 540.0, 960.0);
        let (service, state) = make(vec![fake_window(0x10, 100, Rect::new(0.0, 0.0, 540.0, 960.0))]);
        {
            let mut state = state.lock().unwrap();
            state.honor_after = 2; // 前两次故意摆偏
        }

        let got = service.move_window(WindowKey::new(100, 0x10), target).expect("挪成功");
        assert_eq!(got, target);
        let state = state.lock().unwrap();
        assert_eq!(state.set_calls.len(), 3, "两次摆偏 + 一次到位");
        assert_eq!(state.set_calls[0], (0x10, 100, 50, 540, 960));
    }

    #[test]
    fn move_window_restores_minimized_and_maximized_first() {
        let mut window = fake_window(0x10, 100, Rect::new(0.0, 0.0, 540.0, 960.0));
        window.probe.minimized = true;
        window.probe.maximized = true;
        let (service, state) = make(vec![window]);

        let target = Rect::new(100.0, 50.0, 540.0, 960.0);
        assert_eq!(service.move_window(WindowKey::new(100, 0x10), target), Some(target));
        let state = state.lock().unwrap();
        assert_eq!(state.shows, vec![(0x10, ShowCommand::Restore)]);
    }

    #[test]
    fn move_window_returns_last_seen_when_it_never_fits() {
        let target = Rect::new(100.0, 50.0, 540.0, 960.0);
        let (service, state) = make(vec![fake_window(0x10, 100, Rect::new(0.0, 0.0, 540.0, 960.0))]);
        {
            let mut state = state.lock().unwrap();
            // 每轮都偏，且越偏越远 → 永远稳不下来，跑满 6 次后返回最后看到的位置。
            state.honor_after = 99;
            state.blind = false;
        }
        let got = service.move_window(WindowKey::new(100, 0x10), target);
        assert!(got.is_some());
        assert_eq!(state.lock().unwrap().set_calls.len(), MOVE_RETRIES);
    }

    #[test]
    fn set_minimized_close_and_focus_route_to_the_os() {
        let mut window = fake_window(0x10, 100, Rect::new(0.0, 0.0, 540.0, 960.0));
        window.probe.minimized = true;
        let (service, state) = make(vec![window]);

        service.set_minimized(WindowKey::new(100, 0x10), true);
        service.set_minimized(WindowKey::new(100, 0x10), false);
        assert!(service.close(WindowKey::new(100, 0x10)));
        service.focus(WindowKey::new(100, 0x10));

        let state = state.lock().unwrap();
        assert_eq!(
            state.shows,
            vec![
                (0x10, ShowCommand::Minimize),
                (0x10, ShowCommand::Restore),
                // focus 前发现最小化，会先恢复
                (0x10, ShowCommand::Restore),
            ]
        );
        assert_eq!(state.closes, vec![0x10]);
        assert_eq!(state.focuses, vec![0x10]);
    }

    #[test]
    fn set_full_screen_only_restores_when_turning_off() {
        let mut window = fake_window(0x10, 100, Rect::new(0.0, 0.0, 1920.0, 1080.0));
        window.probe.maximized = true;
        let (service, state) = make(vec![window]);

        service.set_full_screen(WindowKey::new(100, 0x10), true);
        assert!(state.lock().unwrap().shows.is_empty(), "开全屏不归我们管");
        service.set_full_screen(WindowKey::new(100, 0x10), false);
        assert_eq!(state.lock().unwrap().shows, vec![(0x10, ShowCommand::Restore)]);
    }

    #[test]
    fn zoom_focuses_then_sends_ctrl_keys() {
        let (service, state) = make(vec![fake_window(0x10, 100, Rect::new(0.0, 0.0, 540.0, 960.0))]);
        assert!(service.zoom(WindowKey::new(100, 0x10), 2));
        {
            let state = state.lock().unwrap();
            assert_eq!(state.focuses, vec![0x10]);
            assert_eq!(state.keys, vec![(VK_OEM_PLUS.0, false), (VK_OEM_PLUS.0, false)]);
        }
        assert!(!service.zoom(WindowKey::new(999, 0x99), 1), "窗口不在就别按键");
    }

    #[test]
    fn zoom_does_not_send_keys_when_focus_is_denied() {
        let (service, state) = make(vec![fake_window(0x10, 100, Rect::new(0.0, 0.0, 540.0, 960.0))]);
        {
            let mut state = state.lock().unwrap();
            state.foreground = Some(0x20);
            state.deny_focus = true;
        }
        assert!(!service.zoom(WindowKey::new(100, 0x10), 2));
        let state = state.lock().unwrap();
        assert_eq!(state.foreground, Some(0x20));
        assert_eq!(state.focuses, vec![0x10]);
        assert!(state.keys.is_empty(), "focus refusal must not send Ctrl keys to another app");
    }

    #[test]
    fn zoom_stops_sending_keys_if_focus_changes_after_the_first_step() {
        let (service, state) = make(vec![fake_window(0x10, 100, Rect::new(0.0, 0.0, 540.0, 960.0))]);
        state.lock().unwrap().lose_focus_after_keys = Some(1);
        assert!(!service.zoom(WindowKey::new(100, 0x10), 3));
        let state = state.lock().unwrap();
        assert_eq!(state.focuses, vec![0x10], "do not keep forcing focus back");
        assert_eq!(state.keys, vec![(VK_OEM_PLUS.0, false)]);
    }

    #[test]
    fn restore_skips_except_and_windows_that_did_not_move() {
        let moved = fake_window(0x10, 100, Rect::new(0.0, 0.0, 540.0, 960.0));
        let still = fake_window(0x11, 100, Rect::new(600.0, 0.0, 540.0, 960.0));
        let excepted = fake_window(0x12, 100, Rect::new(1000.0, 0.0, 540.0, 960.0));
        let (service, state) = make(vec![moved, still.clone(), excepted]);

        let mut snapshot = HashMap::new();
        snapshot.insert(WindowKey::new(100, 0x10), Rect::new(0.0, 0.0, 540.0, 960.0));
        snapshot.insert(WindowKey::new(100, 0x11), still.bounds);
        snapshot.insert(WindowKey::new(100, 0x12), Rect::new(0.0, 0.0, 540.0, 960.0));

        service.restore(&snapshot, Some(WindowKey::new(100, 0x12)));

        let state = state.lock().unwrap();
        // 0x10 位置已经和快照一致（fake 里 bounds == snapshot）→ 不该动。
        // 0x11 也没动过；0x12 被排除。
        assert!(state.set_calls.is_empty(), "{:?}", state.set_calls);
    }

    #[test]
    fn frames_and_snapshot_only_cover_standard_windows() {
        let visible = fake_window(0x10, 100, Rect::new(0.0, 0.0, 540.0, 960.0));
        let mut minimized = fake_window(0x11, 100, Rect::new(0.0, 0.0, 540.0, 960.0));
        minimized.probe.minimized = true;
        let (service, _state) = make(vec![visible.clone(), minimized]);

        let frames = service.frames(100);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames.get(&WindowKey::new(100, 0x10)), Some(&visible.bounds));
        assert_eq!(service.snapshot().len(), 1);
    }

    #[test]
    fn close_button_is_none_without_a_finder() {
        let (service, _state) = make(vec![fake_window(0x10, 100, Rect::new(0.0, 0.0, 540.0, 960.0))]);
        assert_eq!(service.close_button(WindowKey::new(100, 0x10)), None);
    }

    #[test]
    fn close_button_uses_the_wired_uia_finder() {
        let (service, _state) = make(vec![fake_window(0x10, 100, Rect::new(0.0, 0.0, 540.0, 960.0))]);
        let button = Rect::new(8.0, 12.0, 24.0, 24.0);
        let service = service.with_close_buttons(Arc::new(StaticFinder(button)));
        assert_eq!(service.close_button(WindowKey::new(100, 0x10)), Some(button));
        // 窗口不在时不去问 UIA。
        assert_eq!(service.close_button(WindowKey::new(100, 0x99)), None);
    }

    /// 假的 UIA 查找器：固定返回一个矩形。
    struct StaticFinder(Rect);

    impl CloseButtonFinder for StaticFinder {
        fn find_close_button(&self, _handle: u64) -> Option<Rect> {
            Some(self.0)
        }
    }

    #[test]
    fn shown_windows_and_asking_window_use_process_meta() {
        let mut normal = WindowProbe::new(0x10, 100, "Notepad", "记事本", Rect::new(0.0, 0.0, 800.0, 600.0));
        normal.visible = true;
        let dialog = WindowProbe {
            handle: 0x11,
            has_owner: true,
            class: "#32770".to_string(),
            title: "保存吗".to_string(),
            ..normal.clone()
        };
        let (service, _state) = make(vec![
            FakeWindow { probe: normal, bounds: Rect::new(0.0, 0.0, 800.0, 600.0), applied: None },
            FakeWindow { probe: dialog, bounds: Rect::new(10.0, 10.0, 300.0, 200.0), applied: None },
        ]);

        // 有 owner 的对话框不算"标准应用窗口"，但算"询问框"。
        assert_eq!(service.shown_windows(100).len(), 1);
        let asking = service.asking_window(100).expect("找到询问框");
        assert_eq!(asking.key, WindowKey::new(100, 0x11));
    }
}

/// 读窗口当前的物理矩形（`GetWindowRect`，不含 DWM 阴影扩展）。
///
/// 用来核对"我们让它待的位置"和"它实际在的位置"是否一致：显示器 DPI 变化时
/// winit 会按新比例改窗口尺寸，绕开 `SetWindowPos`。
pub fn frame_of(handle: u64) -> Option<Rect> {
    if handle == 0 {
        return None;
    }
    WinDesktop::window_rect(hwnd(handle))
}
