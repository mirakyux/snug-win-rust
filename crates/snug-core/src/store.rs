//! JSON 文件存储，移植自 C# 版 `SnugWin.Core.Persistence.JsonFileStore`。
//!
//! 读坏文件时不删原文件，而是改名为 `.corrupt-<时间戳>` 备份，然后当作不存在。

use std::fs;
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::Deserialize;

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

// ---------------------------------------------------------------------------
// 虚拟屏归属记录（C# `OwnedDisplayBook` / `OwnedDisplayStore`）
// ---------------------------------------------------------------------------

/// 「哪些虚拟屏是本程序建的」记账，启动清理只拆这里记着的屏
/// （C# `SnugWin.Core.Persistence.OwnedDisplayBook`）。
///
/// 同机装着 C# 版时两版共用一个数据目录，所以文件名与 JSON 字段都必须和 C#
/// 完全一致（`owned-displays.json`、`DisplayIds`），两版才能互相读懂、
/// 各拆各的屏。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct OwnedDisplayBook {
    pub display_ids: Vec<u32>,
}

impl OwnedDisplayBook {
    /// 加一条归属；已经在书里就原样返回，不产生重复项（C# `WithAdded`）。
    pub fn with_added(&self, display_id: u32) -> Self {
        if self.display_ids.contains(&display_id) {
            return self.clone();
        }
        let mut next = self.display_ids.clone();
        next.push(display_id);
        Self { display_ids: next }
    }

    /// 去掉一条归属；本来就不在书里就原样返回（C# `WithRemoved`）。
    pub fn with_removed(&self, display_id: u32) -> Self {
        if !self.display_ids.contains(&display_id) {
            return self.clone();
        }
        Self {
            display_ids: self
                .display_ids
                .iter()
                .copied()
                .filter(|id| *id != display_id)
                .collect(),
        }
    }
}

/// 归属记录的落盘（C# `OwnedDisplayStore`）。
///
/// 读坏文件走 [`JsonFileStore::load_or_quarantine`]：备份后当作空书，不会让
/// 启动失败；写失败也只影响下次清理，不影响本次建屏。
pub struct OwnedDisplayStore {
    store: JsonFileStore<OwnedDisplayBook>,
}

impl OwnedDisplayStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { store: JsonFileStore::new(path) }
    }

    fn load(&self) -> OwnedDisplayBook {
        self.store.load_or_quarantine().unwrap_or_default()
    }

    /// 书里记着的屏 id（文件不存在/读坏都返回空）。
    pub fn load_ids(&self) -> Vec<u32> {
        self.load().display_ids
    }

    /// 建屏确认上线后记账（C# `RecordOwned`）。
    pub fn record_owned(&self, display_id: u32) {
        let _ = self.store.save(&self.load().with_added(display_id));
    }

    /// 屏撤掉后销账；最后一条删掉时连文件一起删（C# `ReleaseOwned`）。
    pub fn release_owned(&self, display_id: u32) {
        let book = self.load().with_removed(display_id);
        if book.display_ids.is_empty() {
            self.store.delete();
        } else {
            let _ = self.store.save(&book);
        }
    }

    /// 整本清空（C# `ClearAll`）。
    pub fn clear_all(&self) {
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

    // ----- 虚拟屏归属记录 -----

    fn owned_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("snug-owned-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("owned-displays.json")
    }

    #[test]
    fn owned_book_add_and_remove_are_idempotent() {
        let empty = OwnedDisplayBook::default();
        assert_eq!(empty.with_added(2).display_ids, vec![2]);
        // 重复加不会产生第二条。
        assert_eq!(empty.with_added(2).with_added(2).display_ids, vec![2]);
        let book = OwnedDisplayBook { display_ids: vec![1, 2] };
        assert_eq!(book.with_removed(1).display_ids, vec![2]);
        // 不在书里的 id 原样返回。
        assert_eq!(book.with_removed(9).display_ids, vec![1, 2]);
    }

    #[test]
    fn owned_store_records_and_releases_until_file_is_gone() {
        let path = owned_path("rotation");
        let store = OwnedDisplayStore::new(&path);
        assert!(store.load_ids().is_empty(), "没有文件就是空书");

        store.record_owned(1);
        store.record_owned(2);
        store.record_owned(1); // 重复记账不该出现第二次
        assert_eq!(store.load_ids(), vec![1, 2]);

        store.release_owned(1);
        assert_eq!(store.load_ids(), vec![2]);
        // 已经不在书里的再释放一次也不该炸。
        store.release_owned(1);
        assert_eq!(store.load_ids(), vec![2]);

        store.release_owned(2);
        assert!(store.load_ids().is_empty());
        assert!(!path.exists(), "最后一条销账后文件要删掉");

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn owned_store_reads_the_csharp_json_shape() {
        // C# 的 JsonFileStore 用默认属性名（PascalCase）+ 缩进。
        let path = owned_path("csharp");
        fs::create_dir_all(path.parent().unwrap()).expect("dir");
        fs::write(&path, "{\n  \"DisplayIds\": [2, 3]\n}").expect("write");
        let store = OwnedDisplayStore::new(&path);
        assert_eq!(store.load_ids(), vec![2, 3]);

        store.record_owned(4);
        let text = fs::read_to_string(&path).expect("read");
        assert!(text.contains("\"DisplayIds\""), "写回去也要是 C# 认识的字段名：{text}");

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn owned_store_survives_corrupt_json_and_keeps_going() {
        let path = owned_path("corrupt");
        fs::create_dir_all(path.parent().unwrap()).expect("dir");
        fs::write(&path, "{ 这不是 json").expect("write");
        let store = OwnedDisplayStore::new(&path);

        assert!(store.load_ids().is_empty(), "坏文件当作空书，不能炸启动");
        let quarantined = fs::read_dir(path.parent().unwrap())
            .expect("read dir")
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains("corrupt-"));
        assert!(quarantined, "坏文件应被改名备份");

        // 之后还能正常记账（等同新书）。
        store.record_owned(7);
        assert_eq!(store.load_ids(), vec![7]);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
