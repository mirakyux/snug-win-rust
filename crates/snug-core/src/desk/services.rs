//! PhoneDesk 依赖的平台接口与时间线。
//!
//! 对应 C# 文件 `SnugWin.Core/PhoneScreen/PhoneServices.cs`：`IPhoneDisplays`、
//! `IWindowControl`、`IAppControl`、`ITimeline`、`WindowLook`、`PhoneScreenException`。
//!
//! 与 C# 的差别只在"异步"的表达：C# 每个方法返回 `Task`（参数还带 `CancellationToken`），
//! 这里改成**阻塞**方法 + [`CancelToken`]。原因是本移植不引入 async 运行时依赖：
//! PhoneDesk 把动作排进一个单线程串行队列执行（对应 C# 的 `SerialLane`），
//! 等待与轮询都发生在那个工作线程上，界面线程不受影响。
//! 语义（调用顺序、返回值、取消时机）与 C# 版一致。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::geometry::Rect;
use crate::screen_fit::ScreenFit;
use crate::windows::{AppInfo, DisplayModeInfo, OtherDisplay, WindowInfo, WindowKey};

/// 窗口当前的位置与状态（对应 C# `WindowLook`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowLook {
    pub frame: Rect,
    pub full_screen: bool,
    pub minimized: bool,
}

impl WindowLook {
    pub fn new(frame: Rect) -> Self {
        Self { frame, full_screen: false, minimized: false }
    }
}

/// 可取消的等待令牌，对应 C# 的 `CancellationTokenSource`/`CancellationToken`。
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// 手机屏创建失败的原因（对应 C# `PhoneScreenErrorKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhoneScreenErrorKind {
    CreateFailed,
    NeverOnline,
}

/// 加手机屏失败（对应 C# `PhoneScreenException`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneScreenException {
    pub kind: PhoneScreenErrorKind,
    pub detail: String,
}

impl PhoneScreenException {
    /// 默认文案与 C# 构造函数一致。
    pub fn new(kind: PhoneScreenErrorKind) -> Self {
        let detail = match kind {
            PhoneScreenErrorKind::CreateFailed => {
                "虚拟显示器创建失败，请检查 virtual-display-rs 驱动是否已安装并运行"
            }
            PhoneScreenErrorKind::NeverOnline => "手机屏已创建但系统未将其上线",
        };
        Self { kind, detail: detail.to_string() }
    }

    pub fn with_detail(kind: PhoneScreenErrorKind, detail: impl Into<String>) -> Self {
        Self { kind, detail: detail.into() }
    }
}

impl std::fmt::Display for PhoneScreenException {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for PhoneScreenException {}

/// 虚拟手机屏（对应 C# `IPhoneDisplays`）。
pub trait PhoneDisplays: Send + Sync {
    /// 建一块手机屏，返回显示器编号。
    fn create(&self, fit: ScreenFit) -> Result<u32, PhoneScreenException>;
    /// 把已有手机屏改成新的比例。
    fn reshape(&self, display_id: u32, fit: ScreenFit) -> bool;
    fn fit_of(&self, display_id: u32) -> Option<ScreenFit>;
    /// 撤掉手机屏，返回它是否已经下线。
    fn retire(&self, display_id: u32) -> bool;
    fn is_online(&self, display_id: u32) -> bool;
    fn width_of(&self, display_id: u32) -> Option<i32>;
    fn set_width(&self, display_id: u32, width: i32);
    fn bounds_of(&self, display_id: u32) -> Option<Rect>;
    /// 屏幕上能放窗口的区域（去掉底部 Home Bar）。
    fn window_area_of(&self, display_id: u32) -> Option<Rect>;
    fn main_area(&self) -> Option<Rect>;
    fn other_displays(&self) -> Vec<OtherDisplay>;
    fn set_mode(&self, display_id: u32, mode: DisplayModeInfo, permanently: bool);
}

/// 窗口控制（对应 C# `IWindowControl`）。
pub trait WindowControl: Send + Sync {
    fn exists(&self, window: WindowKey) -> bool;
    fn is_shown(&self, window: WindowKey) -> bool;
    fn look(&self, window: WindowKey) -> Option<WindowLook>;
    /// 把窗口挪到指定位置，返回系统实际给的位置。
    fn move_window(&self, window: WindowKey, frame: Rect) -> Option<Rect>;
    fn set_full_screen(&self, window: WindowKey, on: bool);
    fn set_minimized(&self, window: WindowKey, on: bool);
    fn focus(&self, window: WindowKey);
    fn zoom(&self, window: WindowKey, steps: i32) -> bool;
    fn shown_windows(&self, pid: i32) -> Vec<WindowInfo>;
    /// 正在弹"要不要保存"之类询问的窗口。
    fn asking_window(&self, pid: i32) -> Option<WindowInfo>;
    fn close(&self, window: WindowKey) -> bool;
    /// 窗口自带关闭按钮的位置（用于把点击翻译成"先归位再关"）。
    fn close_button(&self, window: WindowKey) -> Option<Rect>;
    fn frames(&self, pid: i32) -> HashMap<WindowKey, Rect>;
    fn snapshot(&self) -> HashMap<WindowKey, Rect>;
    /// 把快照里的窗口挪回去，`except` 是本次正在摆的窗口。
    fn restore(&self, snapshot: &HashMap<WindowKey, Rect>, except: Option<WindowKey>);
}

/// App 控制（对应 C# `IAppControl`）。
pub trait AppControl: Send + Sync {
    fn pid_of(&self, app: &AppInfo) -> Option<i32>;
    fn pids(&self, app_id: &str) -> Vec<i32>;
    fn open(&self, app: &AppInfo) -> Option<i32>;
    fn new_window(&self, pid: i32) -> bool;
    fn quit(&self, pid: i32) -> bool;
    fn force_quit(&self, pid: i32) -> bool;
    fn is_running(&self, pid: i32) -> bool;

    /// 找出该 App 已有的窗口；C# 接口此方法有默认空实现，这里照搬。
    fn app_windows(&self, _app: &AppInfo, _launched_pid: Option<i32>) -> Vec<WindowInfo> {
        Vec::new()
    }
}

/// 时间线与等待（对应 C# `ITimeline`）。
pub trait Timeline: Send + Sync {
    fn now(&self) -> f64;

    /// 睡一段时间；被取消时提前返回（对应 C# 吞掉 `OperationCanceledException` 的写法）。
    /// 调用方睡醒后自己检查 [`CancelToken::is_cancelled`]。
    fn sleep(&self, seconds: f64, cancel: &CancelToken);
}

/// 真实时间线（对应 C# `SystemTimeline`）：`Instant` + 分片睡眠以便响应取消。
#[derive(Debug, Default)]
pub struct SystemTimeline {
    start: Option<Instant>,
}

impl SystemTimeline {
    pub fn new() -> Self {
        Self { start: Some(Instant::now()) }
    }
}

impl Timeline for SystemTimeline {
    fn now(&self) -> f64 {
        // C# 用进程启动后的秒数；这里以本对象创建时刻为零点。
        self.start.unwrap_or_else(Instant::now).elapsed().as_secs_f64()
    }

    fn sleep(&self, seconds: f64, cancel: &CancelToken) {
        if seconds <= 0.0 {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        // 分片睡，取消能及时生效（C# 由 Task.Delay 的 token 承担同样职责）。
        while Instant::now() < deadline {
            if cancel.is_cancelled() {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(left.min(Duration::from_millis(20)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_token_flips_once_cancelled() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        let clone = token.clone();
        clone.cancel();
        assert!(token.is_cancelled(), "克隆共享同一状态");
    }

    #[test]
    fn phone_screen_exception_uses_csharp_default_messages() {
        let failed = PhoneScreenException::new(PhoneScreenErrorKind::CreateFailed);
        assert!(failed.detail.contains("virtual-display-rs"));
        let online = PhoneScreenException::new(PhoneScreenErrorKind::NeverOnline);
        assert!(online.detail.contains("未将其上线"));
        assert_eq!(online.to_string(), online.detail);
    }

    #[test]
    fn system_timeline_sleeps_but_stops_early_when_cancelled() {
        let timeline = SystemTimeline::new();
        let start = Instant::now();
        timeline.sleep(0.001, &CancelToken::new());
        assert!(start.elapsed() >= Duration::from_millis(1));

        let token = CancelToken::new();
        token.cancel();
        let start = Instant::now();
        timeline.sleep(5.0, &token);
        assert!(start.elapsed() < Duration::from_secs(1), "取消后应立刻返回");
        // 非正数时长直接返回。
        timeline.sleep(0.0, &CancelToken::new());
    }

    #[test]
    fn window_look_defaults_match_csharp_optional_parameters() {
        let look = WindowLook::new(Rect::new(1.0, 2.0, 3.0, 4.0));
        assert!(!look.full_screen);
        assert!(!look.minimized);
    }
}
