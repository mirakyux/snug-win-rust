//! 「在 Snug 之外被关掉的窗口」记录，移植自 C# 版 `SnugWin.Core.PhoneScreen.LeftBehind`。
//!
//! 这类窗口 Snug 拦不住关闭，只能记录它被铺成了手机屏大小；等 App 带着同一尺寸
//! 的窗口回来时，再放回原位并缩回字号。记录保留 14 天，最多 20 条。

use crate::geometry::{Rect, Size};
use crate::record::PhoneRecord;
use crate::windows::{WindowInfo, WindowKey};
use crate::zoom::{ZoomScope, scope_of};

pub const KEEP_MILLIS: i64 = 14 * 24 * 60 * 60 * 1000;
pub const LIMIT: usize = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct LeftBehindEntry {
    pub record: PhoneRecord,
    /// 记录时间（Unix 毫秒）。
    pub since_ms: i64,
}

impl LeftBehindEntry {
    pub fn new(record: PhoneRecord, since_ms: i64) -> Self {
        Self { record, since_ms }
    }

    pub fn scope(&self) -> Option<ZoomScope> {
        scope_of(self.record.app_id.as_deref())
    }

    pub fn returned_as(&self, key: WindowKey) -> PhoneRecord {
        self.record.returned_as(key)
    }

    /// 该 App 的窗口带着字号回来时，字号是否还有效。
    pub fn carries_zoom(&self, pid: i32) -> bool {
        match self.scope() {
            Some(ZoomScope::RestoredWindow) | Some(ZoomScope::Workspace) => true,
            Some(ZoomScope::LiveWindow) => pid == self.record.window.pid,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LeftBehindBook {
    pub entries: Vec<LeftBehindEntry>,
}

impl LeftBehindBook {
    /// 没有 `filled` 尺寸的记录没有意义，直接忽略。
    pub fn add(&mut self, entry: LeftBehindEntry) {
        if entry.record.filled.is_none() {
            return;
        }
        self.entries.push(entry);
    }

    pub fn remove_at(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries.remove(index);
        }
    }

    /// 丢掉过期记录，按时间排序后只保留最近 [`LIMIT`] 条。
    pub fn prune(&mut self, now_ms: i64) {
        self.entries.retain(|e| now_ms - e.since_ms <= KEEP_MILLIS);
        self.entries.sort_by_key(|e| e.since_ms);
        if self.entries.len() > LIMIT {
            let drop = self.entries.len() - LIMIT;
            self.entries.drain(0..drop);
        }
    }

    /// 是否存在同 App、同尺寸的待还原记录（用于判断"这个窗口要不要归位"）。
    pub fn awaits(&self, app_id: &str, size: Size) -> bool {
        self.entries
            .iter()
            .any(|e| same_app(&e.record, app_id) && is_filled(e, size))
    }

    /// 找到与窗口匹配、且尺寸与铺满时一致的记录下标；优先窗口身份，其次标题。
    pub fn match_window(&self, window: &WindowInfo, frame: Rect) -> Option<usize> {
        let candidates: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.record
                    .app_id
                    .as_deref()
                    .map(|id| id.eq_ignore_ascii_case(window.app_id.as_deref().unwrap_or("")))
                    .unwrap_or(false)
                    && is_filled(e, frame.size())
            })
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return None;
        }

        let newest = |indices: &[usize]| -> Option<usize> {
            indices
                .iter()
                .copied()
                .max_by_key(|&i| self.entries[i].since_ms)
        };

        let by_key: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| self.entries[i].record.window == window.key)
            .collect();
        if let Some(hit) = newest(&by_key) {
            return Some(hit);
        }

        let by_title: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| {
                self.entries[i].record.title.as_deref() == Some(window.title.as_str())
            })
            .collect();
        if let Some(hit) = newest(&by_title) {
            return Some(hit);
        }

        newest(&candidates)
    }
}

fn same_app(record: &PhoneRecord, app_id: &str) -> bool {
    record
        .app_id
        .as_deref()
        .map(|id| id.eq_ignore_ascii_case(app_id))
        .unwrap_or(false)
}

fn is_filled(entry: &LeftBehindEntry, size: Size) -> bool {
    match entry.record.filled {
        Some(filled) => (filled.width - size.width).abs() < 2.0
            && (filled.height - size.height).abs() < 2.0,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::WindowKey;

    const FILLED: Rect = Rect { x: 100.0, y: 0.0, width: 540.0, height: 950.0 };

    fn record(app_id: &str, pid: i32, handle: u64, title: &str, since: i64) -> LeftBehindEntry {
        LeftBehindEntry::new(
            PhoneRecord {
                window: WindowKey::new(pid, handle),
                app: "App".into(),
                label: "App".into(),
                app_id: Some(app_id.into()),
                title: Some(title.into()),
                filled: Some(FILLED),
                ..PhoneRecord::default()
            },
            since,
        )
    }

    fn window(app_id: &str, pid: i32, handle: u64, title: &str) -> WindowInfo {
        WindowInfo {
            key: WindowKey::new(pid, handle),
            app_name: "App".into(),
            app_id: Some(app_id.into()),
            title: title.into(),
            minimized: false,
        }
    }

    #[test]
    fn entries_without_filled_size_are_ignored() {
        let mut book = LeftBehindBook::default();
        let mut e = record("Code.exe", 1, 2, "a.rs", 0);
        e.record.filled = None;
        book.add(e);
        assert!(book.entries.is_empty());
    }

    #[test]
    fn prune_drops_expired_and_keeps_newest_limit() {
        let mut book = LeftBehindBook::default();
        let now = KEEP_MILLIS * 3;
        book.add(record("Code.exe", 1, 1, "old", now - KEEP_MILLIS - 1));
        for i in 0..(LIMIT + 5) {
            book.add(record("Code.exe", 1, i as u64 + 10, "t", now - i as i64));
        }
        book.prune(now);
        assert_eq!(book.entries.len(), LIMIT);
        assert!(book.entries.windows(2).all(|w| w[0].since_ms <= w[1].since_ms));
        assert!(!book.awaits("Code.exe", FILLED.size()) || book.entries.iter().any(|e| e.since_ms >= now - 30));
    }

    #[test]
    fn awaits_matches_app_and_size() {
        let book = {
            let mut b = LeftBehindBook::default();
            b.add(record("Code.exe", 1, 2, "a", 0));
            b
        };
        assert!(book.awaits("code.exe", Size::new(540.0, 950.0)));
        assert!(!book.awaits("code.exe", Size::new(300.0, 200.0)));
        assert!(!book.awaits("Cursor.exe", Size::new(540.0, 950.0)));
    }

    #[test]
    fn match_prefers_window_identity_then_title_then_newest() {
        let mut book = LeftBehindBook::default();
        book.add(record("Code.exe", 7, 0xAA, "a.rs", 100));
        book.add(record("Code.exe", 7, 0xBB, "b.rs", 200));

        let same_window = window("Code.exe", 7, 0xBB, "other.rs");
        assert_eq!(book.match_window(&same_window, FILLED), Some(1));

        let same_title = window("Code.exe", 8, 0xCC, "a.rs");
        assert_eq!(book.match_window(&same_title, FILLED), Some(0));

        let unknown = window("Code.exe", 9, 0xDD, "c.rs");
        assert_eq!(book.match_window(&unknown, FILLED), Some(1), "无匹配时取最新一条");

        let other_app = window("Zed.exe", 9, 0xDD, "c.rs");
        assert_eq!(book.match_window(&other_app, FILLED), None);
    }

    #[test]
    fn remove_at_is_bounds_checked() {
        let mut book = LeftBehindBook::default();
        book.add(record("Code.exe", 1, 2, "a", 0));
        book.remove_at(5);
        assert_eq!(book.entries.len(), 1);
        book.remove_at(0);
        assert!(book.entries.is_empty());
    }
}
