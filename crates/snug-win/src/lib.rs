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
pub mod vdd;
mod wide;
pub mod window;

pub use display::{AdapterLuid, MonitorSnapshot, WinDisplay, WinPhoneDisplays};
pub use vdd::{VddClient, VddError, VddMode, VddMonitor, VddProtocol};
pub use window::{WinDesktop, WinWindows};

/// 平台层已接通的模块数（用于界面显示进度）。
pub fn implemented_modules() -> &'static [&'static str] {
    &["vdd", "display", "window"]
}
