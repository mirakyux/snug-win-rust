//! 窗口与应用的基本类型，移植自 C# 版 `SnugWin.Core.PhoneScreen.Windows`。

use serde::{Deserialize, Serialize};

/// 会话内稳定的窗口身份：进程号 + 系统窗口句柄。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct WindowKey {
    pub pid: i32,
    pub handle: u64,
}

impl WindowKey {
    pub const fn new(pid: i32, handle: u64) -> Self {
        Self { pid, handle }
    }
}

impl std::fmt::Display for WindowKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{:X}", self.pid, self.handle)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub key: WindowKey,
    pub app_name: String,
    pub app_id: Option<String>,
    pub title: String,
    pub minimized: bool,
}

impl WindowInfo {
    /// 列表里显示的文案：`App「标题前 30 字」`。
    pub fn label(&self) -> String {
        if self.title.is_empty() {
            return self.app_name.clone();
        }
        let truncated: String = self.title.chars().take(30).collect();
        format!("{}「{}」", self.app_name, truncated)
    }

    /// 去掉标题里的零宽字符等格式控制符，但保留 ZWJ（emoji 组合需要）。
    pub fn visible_title(raw: &str) -> String {
        raw.chars()
            .filter(|c| *c == '\u{200D}' || !is_format_char(*c))
            .collect::<String>()
            .trim()
            .to_string()
    }
}

/// `UnicodeCategory.Format`（Cf）的近似判定：截断这些字符与 C# 版一致。
fn is_format_char(c: char) -> bool {
    let v = c as u32;
    matches!(v,
        0x00AD | 0x061C | 0x06DD | 0x070F | 0x08E2 | 0x180E | 0xFEFF
        | 0x110BD | 0x110CD | 0xE0001)
        || (0x0600..=0x0605).contains(&v)
        || (0x0890..=0x0891).contains(&v)
        || (0x200B..=0x200F).contains(&v)
        || (0x202A..=0x202E).contains(&v)
        || (0x2060..=0x2064).contains(&v)
        || (0x2066..=0x206F).contains(&v)
        || (0xFFF9..=0xFFFB).contains(&v)
        || (0x13430..=0x1343F).contains(&v)
        || (0x1BCA0..=0x1BCA3).contains(&v)
        || (0x1D173..=0x1D17A).contains(&v)
        || (0xE0020..=0xE007F).contains(&v)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    pub app_id: String,
    pub name: String,
    pub executable_path: Option<String>,
    pub launch_target: Option<String>,
    pub arguments: Option<String>,
    pub working_directory: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunningApp {
    pub app: AppInfo,
    pub pid: i32,
    pub windows: Vec<WindowInfo>,
}

/// 某块屏幕的显示模式（分辨率 / 刷新率 / 缩放）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayModeInfo {
    pub width: i32,
    pub height: i32,
    pub refresh_rate: f64,
    pub dpi_scale_percent: i32,
}

impl DisplayModeInfo {
    pub fn is_same_as(&self, other: &Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && (self.refresh_rate - other.refresh_rate).abs() < 0.5
            && self.dpi_scale_percent == other.dpi_scale_percent
    }
}

impl std::fmt::Display for DisplayModeInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}x{}@{:.1}Hz ({}%)",
            self.width, self.height, self.refresh_rate, self.dpi_scale_percent
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OtherDisplay {
    pub display_id: u32,
    pub device_name: String,
    pub friendly_name: String,
    pub mode: DisplayModeInfo,
    pub asleep: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_key_formats_like_csharp() {
        assert_eq!(WindowKey::new(1234, 0xFF).to_string(), "1234/FF");
    }

    #[test]
    fn label_truncates_long_titles() {
        let info = WindowInfo {
            key: WindowKey::new(1, 2),
            app_name: "任务管理器".into(),
            app_id: Some("Taskmgr.exe".into()),
            title: "任务管理器".into(),
            minimized: false,
        };
        assert_eq!(info.label(), "任务管理器「任务管理器」");

        let long = WindowInfo {
            key: WindowKey::new(1, 2),
            app_name: "App".into(),
            app_id: None,
            title: "一".repeat(40),
            minimized: false,
        };
        assert_eq!(long.label().chars().filter(|c| *c == '一').count(), 30);

        let empty = WindowInfo {
            key: WindowKey::new(1, 2),
            app_name: "Finder".into(),
            app_id: None,
            title: String::new(),
            minimized: false,
        };
        assert_eq!(empty.label(), "Finder");
    }

    #[test]
    fn visible_title_strips_format_chars_but_keeps_zwj() {
        assert_eq!(WindowInfo::visible_title("\u{200B}标题\u{200E}"), "标题");
        assert_eq!(WindowInfo::visible_title("a\u{200D}b"), "a\u{200D}b");
        assert_eq!(WindowInfo::visible_title("  "), "");
        assert_eq!(WindowInfo::visible_title("  名字  "), "名字");
    }

    #[test]
    fn display_mode_equality_allows_tiny_refresh_drift() {
        let a = DisplayModeInfo { width: 1080, height: 1900, refresh_rate: 60.0, dpi_scale_percent: 175 };
        let b = DisplayModeInfo { refresh_rate: 60.4, ..a };
        assert!(a.is_same_as(&b));
        assert_eq!(a.to_string(), "1080x1900@60.0Hz (175%)");
    }
}
