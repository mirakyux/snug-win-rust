//! 「在 Snug 之外被关掉的窗口」记录，移植自 C# 版 `SnugWin.Core.PhoneScreen.LeftBehind`。
//!
//! 这类窗口 Snug 拦不住关闭，只能记录它被铺成了手机屏大小；等 App 带着同一尺寸
//! 的窗口回来时，再放回原位并缩回字号。记录保留 14 天，最多 20 条。

use serde::{Deserialize, Serialize};

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

/// 落盘形状与 C# `LeftBehindEntry` 一致：`Record` + `Since`。
/// C# 的 `Since` 是 `DateTimeOffset`，System.Text.Json 写成 ISO-8601 字符串
/// （例如 `2026-11-18T13:28:53.1234567+08:00`），这里照同一形状读写，
/// 两个版本的 left-behind.json 可以互读。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LeftBehindEntryWire {
    record: PhoneRecord,
    since: String,
}

impl Serialize for LeftBehindEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        LeftBehindEntryWire { record: self.record.clone(), since: format_iso8601(self.since_ms) }
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LeftBehindEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = LeftBehindEntryWire::deserialize(deserializer)?;
        let since_ms = parse_iso8601(&wire.since).ok_or_else(|| {
            serde::de::Error::custom(format!("Since 不是 ISO-8601 时间：{}", wire.since))
        })?;
        Ok(Self { record: wire.record, since_ms })
    }
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
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

// ---- 时间戳：与 C# `DateTimeOffset` 的 JSON 形状互转 ----
//
// 这段是为 left-behind.json 的兼容写的：C# 版用 System.Text.Json 序列化
// `DateTimeOffset`，写出来是 `2026-11-18T13:28:53.1234567+08:00` 这样的 ISO-8601。
// Rust 版内部只存 Unix 毫秒，于是自己格式化 / 解析，不引 chrono。

/// Unix 毫秒 → `YYYY-MM-DDTHH:MM:SS.mmmZ`（UTC）。
pub(crate) fn format_iso8601(ms: i64) -> String {
    let seconds = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        of_day / 3600,
        (of_day % 3600) / 60,
        of_day % 60
    )
}

/// ISO-8601 → Unix 毫秒。接受 `Z` 或 `±HH:MM` / `±HHMM` 偏移，小数秒最多 7 位。
pub(crate) fn parse_iso8601(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = text.split_once('T')?;
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // 时:分:秒[.小数]
    let (clock, offset_seconds) = split_offset(rest)?;
    let mut clock_parts = clock.split(':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let second_text = clock_parts.next()?;
    if clock_parts.next().is_some() {
        return None;
    }
    let (second_text, fraction) = match second_text.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (second_text, None),
    };
    let second: i64 = second_text.parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let mut millis = 0i64;
    if let Some(fraction) = fraction {
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let digits: String = fraction.chars().take(3).collect();
        let scale = 10i64.pow(3 - digits.len() as u32);
        millis = digits.parse::<i64>().ok()? * scale;
    }

    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_seconds;
    Some(seconds * 1000 + millis)
}

/// 把 `HH:MM:SS...` 与时间戳尾部拆开，返回（时钟部分，偏移秒数）。
fn split_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(clock) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        return Some((clock, 0));
    }
    // 从后往前找 + 或 -（日期部分已切掉，只剩时间与偏移）。
    for (index, char) in rest.char_indices().rev() {
        if char == '+' || char == '-' {
            let clock = &rest[..index];
            let offset = &rest[index + 1..];
            let (hours, minutes) = match offset.split_once(':') {
                Some((hours, minutes)) => (hours, minutes),
                None if offset.len() == 4 => (&offset[..2], &offset[2..]),
                None => return None,
            };
            let hours: i64 = hours.parse().ok()?;
            let minutes: i64 = minutes.parse().ok()?;
            if minutes > 59 {
                return None;
            }
            let sign = if char == '-' { -1 } else { 1 };
            return Some((clock, sign * (hours * 3600 + minutes * 60)));
        }
    }
    // 没写偏移：C# 会当成本地时间解析，这里不猜，直接判为无法识别。
    None
}

/// Howard Hinnant 的 civil-from-days 算法（1970-01-01 为零点）。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
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

    #[test]
    fn iso8601_parses_the_shapes_csharp_writes() {
        // C# 写的是带本地偏移的 DateTimeOffset，小数秒最多 7 位。
        let with_offset = parse_iso8601("2026-11-18T13:28:53.1234567+08:00").expect("offset");
        let utc = parse_iso8601("2026-11-18T05:28:53.123Z").expect("utc");
        assert_eq!(with_offset, utc);
        // 小数秒超过 3 位时截断到毫秒。
        assert_eq!(parse_iso8601("2026-11-18T05:28:53.1239Z"), Some(utc));
        // 没有小数秒、没有 T 后冒号偏移（+0800）。
        assert_eq!(parse_iso8601("2026-11-18T13:28:53+0800"), parse_iso8601("2026-11-18T05:28:53Z"));
        // 负偏移。
        assert_eq!(
            parse_iso8601("2026-11-18T00:28:53.123-05:00"),
            Some(utc),
            "05:28:53Z == 00:28:53-05:00"
        );
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn iso8601_rejects_garbage_instead_of_guessing() {
        assert_eq!(parse_iso8601("not a time"), None);
        assert_eq!(parse_iso8601("2026-11-18T05:28:53"), None, "没有偏移不猜");
        assert_eq!(parse_iso8601("2026-13-18T05:28:53Z"), None);
        assert_eq!(parse_iso8601("2026-11-18 05:28:53Z"), None, "C# 用 T 分隔");
        assert_eq!(parse_iso8601("2026-11-18T05:28:53.Z"), None);
    }

    #[test]
    fn iso8601_roundtrips_through_format() {
        for ms in [0i64, 1_700_000_000_123, 4_102_444_800_000, -86_400_000] {
            let text = format_iso8601(ms);
            assert_eq!(parse_iso8601(&text), Some(ms), "text={text}");
        }
    }

    #[test]
    fn json_shape_matches_csharp_left_behind_file() {
        let mut book = LeftBehindBook::default();
        book.add(record("Code.exe", 1, 2, "a", 1_700_000_000_000));
        let json = serde_json::to_string(&book).expect("serialize");
        assert!(json.contains("\"Entries\""), "json={json}");
        assert!(json.contains("\"Record\""), "json={json}");
        assert!(json.contains("\"Since\""), "json={json}");
        assert!(json.contains("T"), "Since 应是 ISO-8601 字符串：{json}");

        let back: LeftBehindBook = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].since_ms, 1_700_000_000_000);
        assert_eq!(back.entries[0].record.app_id.as_deref(), Some("Code.exe"));
    }
}
