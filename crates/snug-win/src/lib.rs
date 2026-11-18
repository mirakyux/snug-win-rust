//! Windows 平台层（尚未实现）。
//!
//! 计划按依赖顺序做，每个模块都对应 C# 版 `SnugWin.Windows` 里的一个类：
//!
//! | 本 crate 模块 | 对应 C# 类 | 主要 Win32 面 |
//! |---|---|---|
//! | `display` | `DisplayConfigService` / `PhoneDisplayService` | `QueryDisplayConfig` / `SetDisplayConfig` / `ChangeDisplaySettingsEx` |
//! | `vdd` | `VirtualDisplayIpcClient` | 命名管道 `\\.\pipe\virtualdisplaydriver`（out 方向，带 `Add` 前缀） |
//! | `window` | `WindowService` | `EnumWindows` / `SetWindowPos` / `DwmGetWindowAttribute` |
//! | `touch` | `TouchHookService` | `SetWindowsHookExW(WH_MOUSE_LL)` + `SendInput` 重放 |
//! | `uia` | `UiaHelper` | UI Automation COM 接口 |
//! | `apps` | `AppService` | `IShellItemImageFactory` / `shell:AppsFolder` |
//! | `screenshot` | `ScreenshotService` | `PrintWindow` / `BitBlt` / `DwmGetWindowAttribute` |
//! | `session` | `UUSensor` | 覆盖窗口探测、UIA 控制端表、TCP peer、GameViewerServer 日志 |

/// 平台层是否已经可用（当前为占位实现）。
pub fn implemented() -> bool {
    false
}

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder_reports_not_implemented() {
        assert!(!super::implemented());
    }
}
