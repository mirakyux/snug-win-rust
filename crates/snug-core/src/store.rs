//! JSON 文件存储，移植自 C# 版 `SnugWin.Core.Persistence.JsonFileStore`。
//!
//! 读坏文件时不删原文件，而是改名为 `.corrupt-<时间戳>` 备份，然后当作不存在。

use std::fs;
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

pub struct JsonFileStore<T> {
    path: PathBuf,
    _marker: PhantomData<T>,
}

impl<T> JsonFileStore<T>
where
    T: Serialize + DeserializeOwned,
{
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), _marker: PhantomData }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读不到或读坏了都返回 None；坏文件会被改名备份，方便事后排查。
    pub fn load_or_quarantine(&self) -> Option<T> {
        if !self.path.exists() {
            return None;
        }
        match fs::read_to_string(&self.path)
            .ok()
            .and_then(|json| serde_json::from_str::<T>(&json).ok())
        {
            Some(value) => Some(value),
            None => {
                let stamp = timestamp();
                let corrupt = self.path.with_extension(format!("corrupt-{stamp}"));
                let _ = fs::rename(&self.path, corrupt);
                None
            }
        }
    }

    /// 先写临时文件再改名，避免断电留下半个文件。
    pub fn save(&self, value: &T) -> io::Result<()> {
        if let Some(dir) = self.path.parent()
            && !dir.as_os_str().is_empty()
        {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(value)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = self.path.with_extension(format!("tmp-{}-{}", std::process::id(), timestamp()));
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn delete(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// 带"暂存-结算"语义的记录存储：复原成功后才删除记录。
pub struct RestoreRecordStore<T> {
    store: JsonFileStore<T>,
}

impl<T> RestoreRecordStore<T>
where
    T: Serialize + DeserializeOwned,
{
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { store: JsonFileStore::new(path) }
    }

    /// 待还原的记录。
    pub fn pending(&self) -> Option<T> {
        self.store.load_or_quarantine()
    }

    pub fn record(&self, value: &T) -> io::Result<()> {
        self.store.save(value)
    }

    pub fn settle(&self) {
        self.store.delete();
    }
}

fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Demo {
        value: i32,
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("snug-store-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        dir.join("data.json")
    }

    #[test]
    fn save_then_load_roundtrip() {
        let path = temp_path("roundtrip");
        let store = JsonFileStore::<Demo>::new(&path);
        assert_eq!(store.load_or_quarantine(), None);
        store.save(&Demo { value: 7 }).expect("save");
        assert_eq!(store.load_or_quarantine(), Some(Demo { value: 7 }));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_file_is_quarantined_not_deleted() {
        let path = temp_path("corrupt");
        fs::create_dir_all(path.parent().unwrap()).expect("dir");
        fs::write(&path, "{ not json").expect("write");
        let store = JsonFileStore::<Demo>::new(&path);
        assert_eq!(store.load_or_quarantine(), None);
        assert!(!path.exists(), "原文件应被改名");
        let dir = path.parent().unwrap();
        let quarantined = fs::read_dir(dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains("corrupt-"));
        assert!(quarantined, "应留下 .corrupt-* 备份");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn restore_store_records_and_settles() {
        let path = temp_path("restore");
        let store = RestoreRecordStore::<Demo>::new(&path);
        assert_eq!(store.pending(), None);
        store.record(&Demo { value: 1 }).expect("record");
        assert_eq!(store.pending(), Some(Demo { value: 1 }));
        store.settle();
        assert_eq!(store.pending(), None);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_creates_missing_directories() {
        let path = temp_path("mkdir").join("nested").join("data.json");
        let store = JsonFileStore::<Demo>::new(&path);
        store.save(&Demo { value: 3 }).expect("save");
        assert!(path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }
}
