//! 字号记忆，移植自 C# 版 `SnugWin.Core.PhoneScreen.ZoomBook`。
//!
//! 只有明确支持 `Ctrl+=` / `Ctrl+-` 的程序才调字号；放大级数按进程名分别记住。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub const MIN_STEPS: i32 = -4;
pub const MAX_STEPS: i32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoomScope {
    /// 字号存在窗口里，窗口关掉即消失。
    LiveWindow,
    /// 恢复窗口时会带回字号。
    RestoredWindow,
    /// 按工作区保存（VS Code / Cursor）。
    Workspace,
    /// 整个 App 共用（Zed）。
    App,
}

/// 支持 Ctrl +/- 调字号的进程（小写匹配），数值含义见 [`ZoomScope`]。
pub const ZOOMABLE_APPS: &[(&str, ZoomScope)] = &[
    ("windowsterminal.exe", ZoomScope::LiveWindow),
    ("wt.exe", ZoomScope::LiveWindow),
    ("wezterm-gui.exe", ZoomScope::LiveWindow),
    ("alacritty.exe", ZoomScope::LiveWindow),
    ("mintty.exe", ZoomScope::LiveWindow),
    ("code.exe", ZoomScope::Workspace),
    ("code - insiders.exe", ZoomScope::Workspace),
    ("cursor.exe", ZoomScope::Workspace),
    ("windsurf.exe", ZoomScope::Workspace),
    ("zed.exe", ZoomScope::App),
];

pub fn scope_of(app_id: Option<&str>) -> Option<ZoomScope> {
    let id = app_id?;
    ZOOMABLE_APPS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(id))
        .map(|(_, scope)| *scope)
}

pub fn is_zoomable(app_id: Option<&str>) -> bool {
    scope_of(app_id).is_some()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZoomBook {
    #[serde(rename = "remembered", default)]
    pub remembered: HashMap<String, i32>,
}

impl ZoomBook {
    /// 该 App 记住的级数；不支持调字号的返回 0。
    pub fn steps_for(&self, app_id: Option<&str>, default_steps: i32) -> i32 {
        let Some(id) = app_id else {
            return 0;
        };
        if !is_zoomable(Some(id)) {
            return 0;
        }
        let raw = self
            .remembered
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(id))
            .map(|(_, v)| *v)
            .unwrap_or(default_steps);
        raw.clamp(MIN_STEPS, MAX_STEPS)
    }

    pub fn remember(&mut self, steps: i32, app_id: &str) {
        // 与 C# 的大小写不敏感字典一致：已有同名键时复用它的写法。
        if let Some(key) = self
            .remembered
            .keys()
            .find(|k| k.eq_ignore_ascii_case(app_id))
            .cloned()
        {
            self.remembered.insert(key, steps.clamp(MIN_STEPS, MAX_STEPS));
            return;
        }
        self.remembered.insert(app_id.to_string(), steps.clamp(MIN_STEPS, MAX_STEPS));
    }

    pub fn forget_all(&mut self) {
        self.remembered.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_apps_are_zoomable() {
        assert!(is_zoomable(Some("WindowsTerminal.exe")));
        assert!(is_zoomable(Some("code.exe")));
        assert!(!is_zoomable(Some("notepad.exe")));
        assert!(!is_zoomable(None));
    }

    #[test]
    fn scope_mapping_matches_csharp_table() {
        assert_eq!(scope_of(Some("wt.exe")), Some(ZoomScope::LiveWindow));
        assert_eq!(scope_of(Some("Cursor.exe")), Some(ZoomScope::Workspace));
        assert_eq!(scope_of(Some("Zed.exe")), Some(ZoomScope::App));
        assert_eq!(scope_of(Some("other.exe")), None);
    }

    #[test]
    fn steps_are_clamped_and_defaulted() {
        let mut book = ZoomBook::default();
        assert_eq!(book.steps_for(Some("Code.exe"), 2), 2);
        assert_eq!(book.steps_for(Some("notepad.exe"), 2), 0);
        assert_eq!(book.steps_for(None, 2), 0);

        book.remember(99, "Code.exe");
        assert_eq!(book.steps_for(Some("code.exe"), 0), MAX_STEPS);
        book.remember(-99, "code.EXE");
        assert_eq!(book.steps_for(Some("CODE.EXE"), 0), MIN_STEPS);
    }

    #[test]
    fn remembered_keys_are_case_insensitive() {
        let mut book = ZoomBook::default();
        book.remember(3, "Code.exe");
        book.remember(5, "code.exe");
        assert_eq!(book.remembered.len(), 1);
        assert_eq!(book.steps_for(Some("Code.Exe"), 0), 5);
    }

    #[test]
    fn json_shape_uses_lowercase_remembered() {
        let mut book = ZoomBook::default();
        book.remember(4, "Code.exe");
        let json = serde_json::to_string(&book).expect("serialize");
        assert!(json.contains("\"remembered\""), "json={json}");
        let back: ZoomBook = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.steps_for(Some("code.exe"), 0), 4);
    }
}
