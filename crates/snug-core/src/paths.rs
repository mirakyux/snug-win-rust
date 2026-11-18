//! 数据目录布局，移植自 C# 版 `SnugWin.Core.Persistence.SnugPaths`。
//!
//! 与 C# 版共用一个数据目录，保证两边可以互换使用同一份配置与记录。

use std::path::{Path, PathBuf};

pub const DATA_FOLDER: &str = "SnugWin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    data_dir: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self::from_env().unwrap_or_else(|| Self::with_data_dir(std::env::temp_dir().join(DATA_FOLDER)))
    }
}

impl Paths {
    pub fn with_data_dir(dir: impl Into<PathBuf>) -> Self {
        Self { data_dir: dir.into() }
    }

    /// `%LOCALAPPDATA%\SnugWin`，与 C# 版 `SnugPaths.Default()` 相同。
    ///
    /// 环境变量 `SNUGWIN_HOME` 可以整体接管数据目录。这不是给最终用户用的开关，
    /// 而是**联调/测试必需品**：这台机器上装着 C# 正式版，两边共用
    /// `%LOCALAPPDATA%\SnugWin` 就会抢同一份 `config.json`、`logs/` 和单实例锁
    /// （`snug.lock`），互相把对方踢下线。跑 Rust 版时设成独立目录即可互不干扰。
    pub fn from_env() -> Option<Self> {
        Self::resolve(
            std::env::var_os("SNUGWIN_HOME"),
            std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("APPDATA")),
        )
    }

    /// `from_env` 的纯函数内核（便于测试，不去碰进程环境）。
    fn resolve(override_dir: Option<std::ffi::OsString>, app_data: Option<std::ffi::OsString>) -> Option<Self> {
        if let Some(dir) = override_dir {
            if !dir.is_empty() {
                return Some(Self::with_data_dir(dir));
            }
        }
        let base = app_data.map(PathBuf::from)?;
        Some(Self::with_data_dir(base.join(DATA_FOLDER)))
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn config_file(&self) -> PathBuf {
        self.data_dir.join("config.json")
    }

    pub fn phone_record_file(&self) -> PathBuf {
        self.data_dir.join("phone-screen.json")
    }

    pub fn zoom_book_file(&self) -> PathBuf {
        self.data_dir.join("zoom.json")
    }

    pub fn left_behind_file(&self) -> PathBuf {
        self.data_dir.join("left-behind.json")
    }

    pub fn log_directory(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    /// 当天日志文件，命名与 C# 版一致：`logs/snug-YYYYMMDD.log`。
    pub fn log_file_for(&self, date: (i32, u32, u32)) -> PathBuf {
        let (y, m, d) = date;
        self.log_directory().join(format!("snug-{y:04}{m:02}{d:02}.log"))
    }

    /// 建好数据目录与日志目录（已存在时无副作用）。
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(self.log_directory())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Paths {
        Paths::with_data_dir(r"C:\Users\test\AppData\Local\SnugWin")
    }

    #[test]
    fn file_layout_matches_csharp_version() {
        let p = sample();
        assert_eq!(p.config_file(), Path::new(r"C:\Users\test\AppData\Local\SnugWin\config.json"));
        assert_eq!(
            p.phone_record_file(),
            Path::new(r"C:\Users\test\AppData\Local\SnugWin\phone-screen.json")
        );
        assert_eq!(p.zoom_book_file(), Path::new(r"C:\Users\test\AppData\Local\SnugWin\zoom.json"));
        assert_eq!(
            p.left_behind_file(),
            Path::new(r"C:\Users\test\AppData\Local\SnugWin\left-behind.json")
        );
        assert_eq!(
            p.log_directory(),
            Path::new(r"C:\Users\test\AppData\Local\SnugWin\logs")
        );
    }

    #[test]
    fn log_file_uses_zero_padded_date() {
        let p = sample();
        assert_eq!(
            p.log_file_for((2026, 3, 7)),
            Path::new(r"C:\Users\test\AppData\Local\SnugWin\logs\snug-20260307.log")
        );
    }

    #[test]
    fn ensure_dirs_creates_both_levels() {
        let tmp = std::env::temp_dir().join(format!("snug-paths-test-{}", std::process::id()));
        let p = Paths::with_data_dir(&tmp);
        p.ensure_dirs().expect("create dirs");
        assert!(p.data_dir().is_dir());
        assert!(p.log_directory().is_dir());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn home_override_wins_over_local_appdata() {
        // 联调时用 SNUGWIN_HOME 把数据目录挪开，避免和已装的 C# 版抢配置与单锁。
        let p = Paths::resolve(
            Some(std::ffi::OsString::from(r"D:\snug-test")),
            Some(std::ffi::OsString::from(r"C:\Users\test\AppData\Local")),
        )
        .expect("有覆盖目录就该解析成功");
        assert_eq!(p.data_dir(), Path::new(r"D:\snug-test"));
    }

    #[test]
    fn empty_home_override_is_ignored() {
        let p = Paths::resolve(
            Some(std::ffi::OsString::new()),
            Some(std::ffi::OsString::from(r"C:\Users\test\AppData\Local")),
        )
        .expect("空覆盖要退回默认布局");
        assert_eq!(p.data_dir(), Path::new(r"C:\Users\test\AppData\Local\SnugWin"));
    }

    #[test]
    fn no_override_and_no_appdata_is_none() {
        assert!(Paths::resolve(None, None).is_none());
    }
}
