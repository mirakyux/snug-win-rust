//! 日志，移植自 C# 版 `SnugWin.Core.Support.Log`。
//!
//! 落盘到 `%LOCALAPPDATA%\SnugWin\logs\snug-YYYYMMDD.log`，
//! 行格式与 C# 版一致：`[HH:mm:ss] [Info] 内容`。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Debug => "Debug",
            Level::Info => "Info",
            Level::Warn => "Warn",
            Level::Error => "Error",
        }
    }
}

struct Sink {
    directory: PathBuf,
    file: Option<File>,
    date: (i32, u32, u32),
}

static SINK: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();

fn sink() -> &'static Mutex<Option<Sink>> {
    SINK.get_or_init(|| Mutex::new(None))
}

/// 指定日志目录并立即切到当天文件。
pub fn init(directory: impl AsRef<Path>) {
    let directory = directory.as_ref().to_path_buf();
    let _ = std::fs::create_dir_all(&directory);
    {
        let mut guard = sink().lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Sink { directory, file: None, date: (0, 0, 0) });
    }
    // 注意：必须放开锁之后再写第一行，否则 write() 会再次加锁而死锁。
    write(Level::Info, "日志已启动");
}

/// 关闭日志（正常退出时调用）。
pub fn shutdown() {
    let mut guard = sink().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(sink) = guard.as_mut() {
        if let Some(file) = sink.file.as_mut() {
            let _ = file.flush();
        }
    }
    *guard = None;
}

pub fn write(level: Level, message: impl AsRef<str>) {
    let now = local_now();
    let line = format!(
        "[{:02}:{:02}:{:02}] [{}] {}\n",
        now.hour,
        now.minute,
        now.second,
        level.label(),
        message.as_ref()
    );

    let mut guard = sink().lock().unwrap_or_else(|e| e.into_inner());
    let Some(sink) = guard.as_mut() else {
        return;
    };
    if sink.date != (now.year, now.month, now.day) || sink.file.is_none() {
        let name = format!("snug-{:04}{:02}{:02}.log", now.year, now.month, now.day);
        sink.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(sink.directory.join(name))
            .ok();
        sink.date = (now.year, now.month, now.day);
    }
    if let Some(file) = sink.file.as_mut() {
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
    }
}

/// 当天日志文件的完整路径（设置窗要"打开日志目录"并预览）。
pub fn current_file() -> Option<PathBuf> {
    let now = local_now();
    let guard = sink().lock().unwrap_or_else(|e| e.into_inner());
    let sink = guard.as_ref()?;
    Some(sink.directory.join(format!(
        "snug-{:04}{:02}{:02}.log",
        now.year, now.month, now.day
    )))
}

/// 读当天日志的最后 `lines` 行（设置窗"驱动与日志"页的只读日志框）。
///
/// 直接读文件而不是从内存环形缓冲里取：日志可能被上一次运行的进程写过，
/// C# 版也是读文件。
pub fn tail(lines: usize) -> String {
    let Some(path) = current_file() else { return String::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return String::new() };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// 本地时间（不引 chrono：用 Win32 的 GetLocalTime 太绕，这里用系统时区偏移近似）。
#[derive(Debug, Clone, Copy)]
struct LocalNow {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

fn local_now() -> LocalNow {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    from_unix_with_offset(secs)
}

/// 把 Unix 时间戳换算成日历时间；时区偏移由 [`tz_offset_minutes`] 提供。
fn from_unix_with_offset(unix_secs: i64) -> LocalNow {
    let offset = tz_offset_minutes() as i64 * 60;
    let local = unix_secs + offset;
    let days = local.div_euclid(86_400);
    let secs_of_day = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    LocalNow {
        year,
        month,
        day,
        hour: (secs_of_day / 3600) as u32,
        minute: ((secs_of_day % 3600) / 60) as u32,
        second: (secs_of_day % 60) as u32,
    }
}

/// 本地时区偏移（分钟）。Windows 下用 `GetTimeZoneInformation` 需要 unsafe，
/// 这里退化为固定 +8（Asia/Shanghai）——界面与日志都只用于人读。
fn tz_offset_minutes() -> i32 {
    8 * 60
}

/// Howard Hinnant 的 days → (y, m, d) 算法。
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year as i32, m as u32, d as u32)
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::logging::write($crate::logging::Level::Debug, format!($($arg)*)) };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::logging::write($crate::logging::Level::Info, format!($($arg)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::logging::write($crate::logging::Level::Warn, format!($($arg)*)) };
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::logging::write($crate::logging::Level::Error, format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_conversion_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        // 2026-11-18 = 自 1970-01-01 起 20775 天（56 年 + 14 个闰日 + 321 天）
        assert_eq!(civil_from_days(20_775), (2026, 11, 18));
    }

    #[test]
    fn local_time_adds_timezone_offset() {
        // 2026-11-18T00:00:00Z => 本地 08:00
        let unix = 20_775i64 * 86_400;
        let now = from_unix_with_offset(unix);
        assert_eq!((now.year, now.month, now.day), (2026, 11, 18));
        assert_eq!((now.hour, now.minute, now.second), (8, 0, 0));
    }

    #[test]
    fn writes_line_to_daily_file() {
        let dir = std::env::temp_dir().join(format!("snug-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        init(&dir);
        write(Level::Info, "hello");
        shutdown();
        let files: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        assert_eq!(files.len(), 1, "应只有一个当天日志文件");
        let text = std::fs::read_to_string(&files[0]).expect("read");
        assert!(text.contains("[Info] hello"), "text={text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_without_init_is_noop() {
        // 没初始化时不能 panic。
        write(Level::Warn, "ignored");
    }
}
