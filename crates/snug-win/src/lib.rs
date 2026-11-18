//! Windows 平台层。
//!
//! 每个模块对应 C# 版 `SnugWin.Windows` 里的一个类：
//!
//! | 本 crate 模块 | 对应 C# 类 | 主要 Win32 面 |
//! |---|---|---|
//! | `vdd` | `VirtualDisplayIpcClient` | 命名管道 `\\.\pipe\virtualdisplaydriver` |
//! | `display` | `DisplayConfigService` / `PhoneDisplayService` | `QueryDisplayConfig` / `SetDisplayConfig` / `ChangeDisplaySettingsEx` |
//! | `window` | `WindowService` | `EnumWindows` / `SetWindowPos` / `DwmGetWindowAttribute` |
//! | `touch` | `TouchHookService` | `SetWindowsHookExW(WH_MOUSE_LL)` + `SendInput` 重放 |
//! | `uia` | `UiaHelper` | UI Automation COM 接口 |
//! | `apps` | `AppService` | `IShellItemImageFactory` / `shell:AppsFolder` |
//! | `screenshot` | `ScreenshotService` | `PrintWindow` / `BitBlt` |
//! | `uu` | `UUSensor` | 覆盖窗口、UIA 控制端表、TCP peer、GameViewerServer 日志 |

pub mod display;
pub mod touch;
pub mod uia;
pub mod vdd;
mod wide;
pub mod window;
pub mod apps;
pub mod screenshot;
pub mod elevation;
pub mod placement;
pub mod startup;
pub mod update;

pub use apps::WinApps;
pub use screenshot::{WinScreenshot, WinScreens};
pub use display::{AdapterLuid, MonitorSnapshot, WinDisplay, WinPhoneDisplays};
pub use elevation::{is_elevated, restart_elevated};
pub use placement::{PlacementRequest, apply as apply_placement};
pub use startup::{StartupError, StartupService};
pub use touch::{HookEngine, TouchHook, WinInput};
pub use uia::{Uia, UiaCloseButtons};
pub use update::{AppUpdateService, UpdateCheckResult, UpdateError, PreparedUpdate};
pub use vdd::{VddClient, VddError, VddMode, VddMonitor, VddProtocol};
pub use window::{find_own_window, WinDesktop, WinWindows};

/// 平台层已接通的模块数（用于界面显示进度）。
pub fn implemented_modules() -> &'static [&'static str] {
    &["vdd", "display", "window", "touch", "uia", "startup", "update", "apps", "screenshot"]
}

// ---------------------------------------------------------------------------
// 告警输出
// ---------------------------------------------------------------------------

/// 应用层装的告警接收端（库本身不持有日志文件）。
type WarnSink = Box<dyn Fn(&str) + Send + Sync>;

static WARN_SINK: std::sync::OnceLock<WarnSink> = std::sync::OnceLock::new();

/// 把平台层的告警接到应用自己的日志（通常是 `crate::logging::Warn`）。
///
/// 只认第一次安装：后装的调用会被忽略。没装时 [`warn`] 写 stderr——GUI 程序没有
/// 控制台，所以正式运行前应当装一个，否则告警只进调试器。
pub fn set_warn_sink(sink: impl Fn(&str) + Send + Sync + 'static) {
    let _ = WARN_SINK.set(Box::new(sink));
}

/// 平台层告警，对应 C# 的 `Log.Warn`。库里没有日志后端，交给 [`set_warn_sink`] 装的
/// 接收端；没装就写 stderr。
pub fn warn(message: &str) {
    match WARN_SINK.get() {
        Some(sink) => sink(message),
        None => eprintln!("[Warn] {message}"),
    }
}
