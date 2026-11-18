//! 窗口还原记录，移植自 C# 版 `SnugWin.Core.Persistence.PhoneRecord`。
//!
//! 把窗口搬上手机屏之前先记下原位与状态，复原成功后才删除。

use serde::{Deserialize, Serialize};

use crate::geometry::{Rect, Size};
use crate::windows::WindowKey;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PhoneRecord {
    pub window: WindowKey,
    pub app: String,
    pub label: String,
    pub original: Rect,
    pub was_full_screen: bool,
    pub was_minimized: bool,
    pub zoom: i32,
    pub app_id: Option<String>,
    pub title: Option<String>,
    /// 铺满手机屏后的实际尺寸，用于「App 带着这个尺寸回来」的匹配。
    pub filled: Option<Rect>,
}

impl Default for PhoneRecord {
    fn default() -> Self {
        Self {
            window: WindowKey::default(),
            app: String::new(),
            label: String::new(),
            original: Rect::ZERO,
            was_full_screen: false,
            was_minimized: false,
            zoom: 0,
            app_id: None,
            title: None,
            filled: None,
        }
    }
}

impl PhoneRecord {
    /// 铺满后的尺寸（取宽度与高度，忽略位置）。
    pub fn filled_size(&self) -> Option<Size> {
        self.filled.map(|r| r.size())
    }

    /// 同一个 App 的窗口带着记录里的尺寸回来时，说明 App 原样存下又带回了。
    pub fn returned_as(&self, key: WindowKey) -> PhoneRecord {
        // 注意：字号是否仍然有效，要按**原来那条记录**判定（C# 里 CarriesZoom 用的是 this）。
        let keeps_zoom = self.carries_zoom(key.pid);
        let mut r = self.clone();
        r.window = key;
        if !keeps_zoom {
            r.zoom = 0;
        }
        r
    }

    fn carries_zoom(&self, pid: i32) -> bool {
        use crate::zoom::{ZoomScope, scope_of};
        match scope_of(self.app_id.as_deref()) {
            Some(ZoomScope::RestoredWindow) | Some(ZoomScope::Workspace) => true,
            Some(ZoomScope::LiveWindow) => pid == self.window.pid,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> PhoneRecord {
        PhoneRecord {
            window: WindowKey::new(10, 0xAB),
            app: "Code".into(),
            label: "Code「a.rs」".into(),
            original: Rect::new(1.0, 2.0, 300.0, 400.0),
            was_full_screen: false,
            was_minimized: true,
            zoom: 3,
            app_id: Some("Code.exe".into()),
            title: Some("a.rs".into()),
            filled: Some(Rect::new(100.0, 0.0, 540.0, 950.0)),
        }
    }

    #[test]
    fn json_uses_pascal_case_like_csharp() {
        let json = serde_json::to_string(&record()).expect("serialize");
        assert!(json.contains("\"WasFullScreen\""), "json={json}");
        assert!(json.contains("\"AppId\""));
        assert!(json.contains("\"Filled\""));
        assert!(json.contains("\"Pid\""));
        assert!(json.contains("\"Handle\""));
    }

    #[test]
    fn roundtrip_preserves_every_field() {
        let r = record();
        let json = serde_json::to_string(&r).expect("serialize");
        let back: PhoneRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, r);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let back: PhoneRecord = serde_json::from_str("{}").expect("deserialize");
        assert_eq!(back, PhoneRecord::default());
    }

    #[test]
    fn returned_as_keeps_workspace_zoom_and_drops_live_window_zoom() {
        let mut r = record(); // Code.exe → Workspace
        assert_eq!(r.returned_as(WindowKey::new(99, 1)).zoom, 3);

        r.app_id = Some("WindowsTerminal.exe".to_string()); // LiveWindow
        assert_eq!(r.returned_as(WindowKey::new(10, 1)).zoom, 3);
        assert_eq!(r.returned_as(WindowKey::new(99, 1)).zoom, 0);
    }

    #[test]
    fn filled_size_ignores_position() {
        assert_eq!(record().filled_size(), Some(Size::new(540.0, 950.0)));
        let mut r = record();
        r.filled = None;
        assert_eq!(r.filled_size(), None);
    }
}
