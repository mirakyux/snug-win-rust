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
pub mod driver_health;
pub mod touch;
pub mod uia;
pub mod vdd;
pub mod vdd_broker;
mod wide;
pub mod window;
pub mod apps;
pub mod com;
pub mod compression;
pub mod screenshot;
pub mod uu;
pub mod elevation;
pub mod placement;
pub mod startup;
pub mod system_paths;
pub mod update;
pub mod selection_action;

pub use apps::WinApps;
pub use com::{init_sta, uninit as uninit_com};
pub use screenshot::{WinScreenshot, WinScreens};
pub use uu::{UuProbe, UuSensor, UuSignalSet};
pub use display::{AdapterLuid, MonitorSnapshot, WinDisplay, WinPhoneDisplays};
pub use elevation::{is_elevated, restart_elevated};
pub use placement::{PlacementRequest, apply as apply_placement, matches as placement_matches};
pub use startup::{StartupError, StartupService};
pub use touch::{HookEngine, TouchHook, WinInput};
pub use uia::{Uia, UiaCloseButtons};
pub use update::{AppUpdateService, UpdateCheckResult, UpdateError, PreparedUpdate};
pub use vdd::{VddClient, VddError, VddMode, VddMonitor, VddProtocol};
pub use window::{find_own_window, frame_of, request_own_window_redraw, WinDesktop, WinWindows};
pub use selection_action::{WinKeyboard, WinSelectionActions};

/// 平台层已接通的模块数（用于界面显示进度）。
pub fn implemented_modules() -> &'static [&'static str] {
    &["vdd", "display", "window", "touch", "uia", "startup", "update", "apps", "screenshot", "uu", "selection"]
}

// ---------------------------------------------------------------------------
// 日志输出（告警 / 信息）
// ---------------------------------------------------------------------------

/// 应用层装的告警接收端（库本身不持有日志文件）。
type WarnSink = Box<dyn Fn(&str) + Send + Sync>;

static WARN_SINK: std::sync::OnceLock<WarnSink> = std::sync::OnceLock::new();

/// 应用层装的信息接收端（对应 C# 的 `Log.Info`）；库自己不开日志文件。
type InfoSink = Box<dyn Fn(&str) + Send + Sync>;

static INFO_SINK: std::sync::OnceLock<InfoSink> = std::sync::OnceLock::new();

/// 测试构建里额外记一份信息日志；生产构建不存在。
#[cfg(test)]
static TEST_INFOS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// 取测试构建里累计的信息日志（跨测试共享，断言请用 `iter().any(...)`）。
#[cfg(test)]
pub(crate) fn test_infos() -> Vec<String> {
    TEST_INFOS.lock().map(|guard| guard.clone()).unwrap_or_default()
}

/// 测试构建里额外记一份告警，方便断言“这条日志真的会出来”；生产构建不存在。
#[cfg(test)]
static TEST_WARNINGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// 取测试构建里累计的告警（跨测试共享，断言请用 `iter().any(...)`）。
#[cfg(test)]
pub(crate) fn test_warnings() -> Vec<String> {
    TEST_WARNINGS.lock().map(|guard| guard.clone()).unwrap_or_default()
}

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
    #[cfg(test)]
    {
        TEST_WARNINGS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(message.to_string());
    }
    match WARN_SINK.get() {
        Some(sink) => sink(message),
        None => eprintln!("[Warn] {message}"),
    }
}

/// 把平台层的信息日志接到应用自己的日志（通常是 `crate::logging::Info`）。
///
/// 和 [`set_warn_sink`] 同型：只认第一次安装，没装时 [`info`] 写 stderr。
pub fn set_info_sink(sink: impl Fn(&str) + Send + Sync + 'static) {
    let _ = INFO_SINK.set(Box::new(sink));
}

/// 平台层信息日志，对应 C# 的 `Log.Info`（如"已按归属记录清理残留手机屏"）。
///
/// 成功路径也要留痕：真机排障时"这次启动到底清没清屏"是最常看的一条。
pub fn info(message: &str) {
    #[cfg(test)]
    {
        TEST_INFOS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(message.to_string());
    }
    match INFO_SINK.get() {
        Some(sink) => sink(message),
        None => eprintln!("[Info] {message}"),
    }
}

// ---------------------------------------------------------------------------
// 系统明暗主题
// ---------------------------------------------------------------------------

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
    REG_VALUE_TYPE,
};
use windows::core::PCWSTR;

/// 系统「应用」主题的注册表位置（C# `AppController.IsSystemAppsDarkTheme`）。
const PERSONALIZE_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
const APPS_USE_LIGHT_THEME: &str = "AppsUseLightTheme";

/// 纯映射：注册表值 → 是否浅色。读不到/类型不对都按 Windows 默认浅色。
fn light_theme_from_value(value: Option<u32>) -> bool {
    value != Some(0)
}

/// 系统（应用）当前是否使用浅色主题。
///
/// 只读 `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme`
/// （DWORD：1=浅色、0=深色），不写任何东西；读不到时返回 `true`——Windows 默认
/// 浅色，也和界面默认一致（C# 读不到时按"非深色"处理）。
pub fn system_uses_light_theme() -> bool {
    let key_path = crate::wide::to_wide(PERSONALIZE_KEY);
    let value_name = crate::wide::to_wide(APPS_USE_LIGHT_THEME);
    let mut key = HKEY(std::ptr::null_mut());
    unsafe {
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
        .0 != 0
        {
            return true;
        }

        let mut kind = REG_VALUE_TYPE::default();
        let mut data = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = RegQueryValueExW(
            key,
            PCWSTR(value_name.as_ptr()),
            None,
            Some(&mut kind),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);

        let value = (status == ERROR_SUCCESS && kind == REG_DWORD).then_some(data);
        light_theme_from_value(value)
    }
}

#[cfg(test)]
mod theme_tests {
    use super::light_theme_from_value;

    #[test]
    fn missing_or_non_zero_registry_value_is_light() {
        assert!(light_theme_from_value(None), "读不到按 Windows 默认浅色");
        assert!(light_theme_from_value(Some(1)));
        assert!(!light_theme_from_value(Some(0)));
        // 其它值 C# 也只把 0 当深色。
        assert!(light_theme_from_value(Some(2)));
    }
}
