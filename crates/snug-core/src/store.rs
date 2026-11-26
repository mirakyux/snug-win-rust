//! JSON 文件存储，移植自 C# 版 `SnugWin.Core.Persistence.JsonFileStore`。
//!
//! 读坏文件时不删原文件，而是改名为 `.corrupt-<时间戳>` 备份，然后当作不存在。

use std::fs;
use std::io::{self, Write};
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

    /// Safety journals must remain blocking when malformed or temporarily unreadable.
    pub fn try_load_strict(&self) -> io::Result<Option<T>> {
        match fs::read(&self.path) {
            Ok(json) => serde_json::from_slice(&json).map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::symlink_metadata(&self.path) {
                    Ok(_) => Err(error),
                    Err(missing) if missing.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    pub fn try_delete(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// 文件不存在或读坏时返回 None；暂时读不到时保留原文件。
    pub fn load_or_quarantine(&self) -> Option<T> {
        self.try_load_or_quarantine().ok().flatten()
    }

    /// 区分暂时的读取错误与损坏内容，供读后改写的调用方阻止覆盖。
    pub fn try_load_or_quarantine(&self) -> io::Result<Option<T>> {
        let json = match fs::read(&self.path) {
            Ok(json) => json,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return match fs::symlink_metadata(&self.path) {
                    // A dangling link is still a source whose contents are unknown.
                    Ok(_) => Err(error),
                    Err(missing) if missing.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(metadata_error) => Err(metadata_error),
                };
            }
            Err(error) => return Err(error),
        };
        match serde_json::from_slice(&json) {
            Ok(value) => Ok(Some(value)),
            Err(_) => {
                let stamp = timestamp();
                let corrupt = self.path.with_extension(format!("corrupt-{stamp}"));
                fs::rename(&self.path, corrupt)?;
                Ok(None)
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
        let result = (|| {
            let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, &self.path)
        })();
        if result.is_err() { let _ = fs::remove_file(&tmp); }
        result
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

    /// 暂时读不到时返回错误，避免后续记录覆盖仍待还原的数据。
    pub fn try_pending(&self) -> io::Result<Option<T>> {
        self.store.try_load_or_quarantine()
    }

    pub fn try_pending_strict(&self) -> io::Result<Option<T>> {
        self.store.try_load_strict()
    }

    pub fn record(&self, value: &T) -> io::Result<()> {
        self.store.save(value)
    }

    /// 同目录原子移交还原账本；目标存在时绝不覆盖，也不先删除来源。
    pub fn try_transfer_to(&self, target: &Self) -> io::Result<()> {
        let source = self.store.path();
        let target = target.store.path();
        if source.parent() != target.parent() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "还原记录只能在同目录移交"));
        }
        if !fs::symlink_metadata(source)?.file_type().is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "来源还原记录不是普通文件"));
        }
        match fs::symlink_metadata(target) {
            Ok(_) => return Err(io::Error::new(io::ErrorKind::AlreadyExists, "目标还原记录已存在")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn MoveFileExW(source: *const u16, target: *const u16, flags: u32) -> i32;
            }
            let source: Vec<_> = source.as_os_str().encode_wide().chain([0]).collect();
            let target: Vec<_> = target.as_os_str().encode_wide().chain([0]).collect();
            if source[..source.len() - 1].contains(&0) || target[..target.len() - 1].contains(&0) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "还原记录路径含空字符"));
            }
            // WRITE_THROUGH only: no REPLACE_EXISTING and no cross-volume copy fallback.
            if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0x8) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(target_os = "linux")]
        {
            use std::ffi::{CString, c_char, c_int};
            use std::os::unix::ffi::OsStrExt;
            unsafe extern "C" {
                fn renameat2(old_dir: c_int, old: *const c_char, new_dir: c_int, new: *const c_char, flags: u32) -> c_int;
            }
            let source = CString::new(source.as_os_str().as_bytes())?;
            let target = CString::new(target.as_os_str().as_bytes())?;
            // AT_FDCWD and RENAME_NOREPLACE keep the existence check atomic with the move.
            if unsafe { renameat2(-100, source.as_ptr(), -100, target.as_ptr(), 1) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            Err(io::Error::new(io::ErrorKind::Unsupported, "当前平台不支持原子无覆盖移交"))
        }
    }

    pub fn settle(&self) {
        let _ = self.try_settle();
    }

    /// 只有源记录确实删除后才算结算；已不存在也视作成功。
    pub fn try_settle(&self) -> io::Result<()> {
        match fs::remove_file(self.store.path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
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
/// 损坏内容备份后当作空书；I/O 错误保留原记录并阻止读后改写。
/// 建屏等需要完整归属证据的调用方使用 `try_*` 方法处理错误。
pub struct OwnedDisplayStore {
    store: JsonFileStore<OwnedDisplayBook>,
    reservations: JsonFileStore<std::collections::BTreeMap<u32, String>>,
    /// A stable inode used for the cross-process ownership transaction lock.
    ///
    /// The file is deliberately never removed: Windows releases the kernel file
    /// lock when the owning process exits, while deleting/recreating the lock file
    /// would let two processes acquire different inodes during cleanup.
    lock_path: PathBuf,
}

impl OwnedDisplayStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            reservations: JsonFileStore::new(path.with_extension("reservations.json")),
            lock_path: path.with_extension("lock"),
            store: JsonFileStore::new(path),
        }
    }

    /// Serialize every ownership read/modify/write operation across processes.
    ///
    /// `JsonFileStore::save` uses replace-by-rename, so locking the data files
    /// themselves would not protect the newly-created inode. A separate stable
    /// lock file gives all instances the same kernel object, and `File::lock`
    /// is released by the OS if a process crashes.
    fn acquire_lock(&self) -> io::Result<fs::File> {
        if let Some(dir) = self.lock_path.parent()
            && !dir.as_os_str().is_empty()
        {
            fs::create_dir_all(dir)?;
        }
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)?;
        lock.lock()?;
        Ok(lock)
    }

    fn with_lock<T>(&self, operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let _lock = self.acquire_lock()?;
        operation()
    }

    // Keep uncertain intentions out of the C#-compatible DisplayIds list. An
    // older client must never treat an unacknowledged reservation as ownership.
    // Unlike the legacy book, corrupt intentions fail closed instead of being
    // quarantined as empty: forgetting their tokens could strand a late create.
    fn try_load_reservations(&self) -> io::Result<std::collections::BTreeMap<u32, String>> {
        match fs::read(self.reservations.path()) {
            Ok(json) => {
                let reservations: std::collections::BTreeMap<u32, String> = serde_json::from_slice(&json)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if reservations.iter().any(|(id, owner)| {
                    *id >= 16 || owner.len() != 32 || !owner.bytes().all(|byte| byte.is_ascii_hexdigit())
                }) {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "虚拟屏创建意图记录无效"));
                }
                Ok(reservations)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Default::default()),
            Err(error) => Err(error),
        }
    }

    pub fn try_reserve(&self, display_id: u32, owner: &str) -> io::Result<()> {
        if display_id >= 16 || owner.len() != 32 || !owner.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "无效的虚拟屏归属标记"));
        }
        self.with_lock(|| {
            let mut reservations = self.try_load_reservations()?;
            if reservations.get(&display_id).is_some_and(|previous| previous != owner) {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "虚拟屏仍有待处理的创建意图"));
            }
            reservations.insert(display_id, owner.into());
            self.reservations.save(&reservations)
        })
    }

    pub fn try_owner(&self, display_id: u32) -> io::Result<Option<String>> {
        self.with_lock(|| Ok(self.try_load_reservations()?.remove(&display_id)))
    }

    pub fn try_load_reserved_ids(&self) -> io::Result<Vec<u32>> {
        self.with_lock(|| Ok(self.try_load_reservations()?.into_keys().collect()))
    }

    pub fn try_release_reserved_if_owner(&self, display_id: u32, expected_owner: &str) -> io::Result<bool> {
        self.with_lock(|| {
            let reservations = self.try_load_reservations()?;
            if reservations.get(&display_id).map(String::as_str) != Some(expected_owner) {
                return Ok(false);
            }
            self.try_release_reserved_unlocked(display_id)?;
            Ok(true)
        })
    }

    fn try_release_reserved_unlocked(&self, display_id: u32) -> io::Result<()> {
        let mut reservations = self.try_load_reservations()?;
        reservations.remove(&display_id);
        if reservations.is_empty() {
            self.reservations.try_delete()
        } else {
            self.reservations.save(&reservations)
        }
    }

    fn load(&self) -> io::Result<OwnedDisplayBook> {
        Ok(self.store.try_load_or_quarantine()?.unwrap_or_default())
    }

    /// 只读便利方法；需要据此修改驱动或账本时使用 `try_load_ids`。
    pub fn load_ids(&self) -> Vec<u32> {
        self.try_load_ids().unwrap_or_default()
    }

    pub fn try_load_ids(&self) -> io::Result<Vec<u32>> {
        self.with_lock(|| Ok(self.load()?.display_ids))
    }

    /// 兼容 C# `RecordOwned`；读取失败时保留原账本，不写入空书。
    pub fn record_owned(&self, display_id: u32) {
        let _ = self.try_record_owned(display_id);
    }

    pub fn try_record_owned(&self, display_id: u32) -> io::Result<()> {
        self.with_lock(|| self.store.save(&self.load()?.with_added(display_id)))
    }

    /// 屏撤掉后销账；最后一条删掉时连文件一起删（C# `ReleaseOwned`）。
    pub fn release_owned(&self, display_id: u32) {
        let _ = self.try_release_owned(display_id);
    }

    pub fn try_release_owned(&self, display_id: u32) -> io::Result<()> {
        self.with_lock(|| self.try_release_owned_unlocked(display_id))
    }

    fn try_release_owned_unlocked(&self, display_id: u32) -> io::Result<()> {
        let book = self.load()?.with_removed(display_id);
        if book.display_ids.is_empty() {
            self.store.try_delete()
        } else {
            self.store.save(&book)
        }
    }

    /// Release an ID only while the persisted reservation still belongs to the
    /// expected owner. A mismatch is a normal, safe no-op and never touches
    /// either ledger; this is the conditional boundary used after a driver says
    /// `NotOwned`, where another process may have taken the numeric ID.
    pub fn try_release_owned_if_owner(&self, display_id: u32, expected_owner: &str) -> io::Result<bool> {
        self.with_lock(|| {
            let reservations = self.try_load_reservations()?;
            if reservations.get(&display_id).map(String::as_str) != Some(expected_owner) {
                return Ok(false);
            }
            // Remove the compatibility ID first. If the second file operation
            // fails, the reservation remains and a later retry can finish safely;
            // the reverse order could strand an unguarded DisplayIds entry.
            self.try_release_owned_unlocked(display_id)?;
            self.try_release_reserved_unlocked(display_id)?;
            Ok(true)
        })
    }

    /// 整本清空（C# `ClearAll`）。
    pub fn clear_all(&self) {
        if let Ok(_lock) = self.acquire_lock() {
            self.store.delete();
        }
    }
}

fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
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
    fn read_errors_do_not_quarantine_the_path() {
        let path = temp_path("unreadable");
        fs::create_dir_all(&path).expect("directory at file path");
        let store = JsonFileStore::<Demo>::new(&path);
        assert_eq!(store.load_or_quarantine(), None);
        assert!(path.is_dir(), "an I/O failure must leave the original path intact");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn broken_junction_is_a_read_failure_until_the_source_path_is_replaced() {
        use std::process::Command;

        let path = temp_path("broken-junction");
        let target = path.parent().unwrap().join("missing-target");
        fs::create_dir_all(&target).unwrap();
        let status = Command::new("powershell.exe").args([
            "-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:SNUG_TEST_LINK -Target $env:SNUG_TEST_TARGET | Out-Null",
        ]).env("SNUG_TEST_LINK", &path).env("SNUG_TEST_TARGET", &target).status().unwrap();
        assert!(status.success(), "create isolated junction fixture");
        fs::remove_dir(&target).unwrap();
        assert!(!path.exists());
        assert!(fs::symlink_metadata(&path).is_ok());
        let store = JsonFileStore::<Demo>::new(&path);
        let result = store.try_load_or_quarantine();
        fs::remove_dir(&path).unwrap();
        assert!(result.is_err(), "an existing broken link must not be treated as an absent source");
        store.save(&Demo { value: 9 }).unwrap();
        assert_eq!(store.try_load_or_quarantine().unwrap(), Some(Demo { value: 9 }));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn transient_read_lock_preserves_ownership_and_blocks_updates() {
        use std::os::windows::fs::OpenOptionsExt;

        let path = owned_path("sharing-read");
        let store = OwnedDisplayStore::new(&path);
        store.record_owned(0);
        store.record_owned(1);
        // Allow writes and rename, but deny read sharing: a quarantine or overwrite
        // could succeed even though the initial read failed.
        let guard = fs::OpenOptions::new()
            .read(true)
            .share_mode(0x2 | 0x4)
            .open(&path)
            .expect("lock reads");
        assert_eq!(fs::read_to_string(&path).unwrap_err().raw_os_error(), Some(32));
        assert!(store.load_ids().is_empty());
        assert!(path.exists(), "a temporary read error must not quarantine valid JSON");
        store.record_owned(2);
        store.release_owned(0);
        drop(guard);
        assert_eq!(store.load_ids(), vec![0, 1], "unreadable ownership must not be rewritten");
        assert!(fs::read_dir(path.parent().unwrap()).unwrap().any(|entry| {
            entry.ok().is_some_and(|entry| entry.file_name() == "owned-displays.json")
        }));
        store.record_owned(2);
        assert_eq!(store.load_ids(), vec![0, 1, 2]);
        let _ = fs::remove_dir_all(path.parent().unwrap());
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
        store.try_settle().expect("already settled");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn restore_store_reports_failed_settlement_and_can_retry() {
        use std::os::windows::fs::OpenOptionsExt;

        let path = temp_path("restore-settlement");
        let store = RestoreRecordStore::<Demo>::new(&path);
        store.record(&Demo { value: 7 }).expect("record");
        let guard = fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2)
            .open(&path)
            .expect("deny delete");
        assert_eq!(store.try_settle().unwrap_err().raw_os_error(), Some(32));
        assert_eq!(store.pending(), Some(Demo { value: 7 }));

        drop(guard);
        store.try_settle().expect("retry settlement");
        assert_eq!(store.pending(), None);
        store.try_settle().expect("idempotent settlement");
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
    fn reserved_intentions_survive_restart_without_entering_the_csharp_book() {
        let path = owned_path("reserved-restart");
        let owner = "0123456789abcdef0123456789abcdef";
        let store = OwnedDisplayStore::new(&path);
        store.try_record_owned(3).unwrap();
        store.try_reserve(1, owner).unwrap();
        store.try_reserve(1, owner).unwrap();
        let restarted = OwnedDisplayStore::new(&path);
        assert_eq!(restarted.try_owner(1).unwrap().as_deref(), Some(owner));
        assert_eq!(restarted.try_load_reserved_ids().unwrap(), vec![1]);
        assert_eq!(restarted.try_load_ids().unwrap(), vec![3]);
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\n  \"DisplayIds\": [\n    3\n  ]\n}");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn reserved_intentions_reject_conflicts_and_release_only_one_record() {
        let path = owned_path("reserved-release");
        let owner = "0123456789abcdef0123456789abcdef";
        let other = "fedcba9876543210fedcba9876543210";
        let store = OwnedDisplayStore::new(&path);
        store.try_record_owned(1).unwrap();
        store.try_reserve(1, owner).unwrap();
        store.try_reserve(2, other).unwrap();
        assert_eq!(store.try_reserve(1, other).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert!(store.try_release_reserved_if_owner(1, owner).unwrap());
        assert_eq!(store.try_load_ids().unwrap(), vec![1]);
        assert_eq!(store.try_load_reserved_ids().unwrap(), vec![2]);
        assert_eq!(store.try_owner(2).unwrap().as_deref(), Some(other));
        assert!(store.try_release_reserved_if_owner(2, other).unwrap());
        assert!(!path.with_extension("reservations.json").exists());
        assert!(path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn conditional_release_never_deletes_a_different_owner() {
        let path = owned_path("conditional-release");
        let owner = "0123456789abcdef0123456789abcdef";
        let other = "fedcba9876543210fedcba9876543210";
        let store = OwnedDisplayStore::new(&path);
        store.try_record_owned(1).unwrap();
        store.try_reserve(1, owner).unwrap();

        assert!(!store.try_release_owned_if_owner(1, other).unwrap());
        assert_eq!(store.try_load_ids().unwrap(), vec![1]);
        assert_eq!(store.try_owner(1).unwrap().as_deref(), Some(owner));

        assert!(store.try_release_owned_if_owner(1, owner).unwrap());
        assert!(store.try_load_ids().unwrap().is_empty());
        assert!(store.try_load_reserved_ids().unwrap().is_empty());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn owned_store_lock_serializes_distinct_processes() {
        use std::process::Command;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        const CHILD_PATH: &str = "SNUG_OWNER_LOCK_CHILD_PATH";
        const READY_PATH: &str = "SNUG_OWNER_LOCK_READY_PATH";
        const RELEASE_PATH: &str = "SNUG_OWNER_LOCK_RELEASE_PATH";

        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let ready = PathBuf::from(std::env::var_os(READY_PATH).expect("ready path"));
            let release = PathBuf::from(std::env::var_os(RELEASE_PATH).expect("release path"));
            let store = OwnedDisplayStore::new(path);
            let _lock = store.acquire_lock().expect("child acquires ownership lock");
            fs::write(&ready, b"ready").expect("signal lock acquired");
            let deadline = Instant::now() + Duration::from_secs(8);
            while !release.exists() {
                assert!(Instant::now() < deadline, "parent did not release child lock");
                std::thread::sleep(Duration::from_millis(10));
            }
            return;
        }

        let path = owned_path("multiprocess-lock");
        let ready = path.with_extension("ready");
        let release = path.with_extension("release");
        let _ = fs::remove_file(&ready);
        let _ = fs::remove_file(&release);
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .arg("owned_store_lock_serializes_distinct_processes")
            .arg("--nocapture")
            .env(CHILD_PATH, &path)
            .env(READY_PATH, &ready)
            .env(RELEASE_PATH, &release)
            .spawn()
            .expect("spawn lock holder");
        let child = output;
        let deadline = Instant::now() + Duration::from_secs(8);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "child did not acquire ownership lock");
            std::thread::sleep(Duration::from_millis(10));
        }

        let (acquired_tx, acquired_rx) = mpsc::channel();
        let competing_path = path.clone();
        let competing = std::thread::spawn(move || {
            let result = OwnedDisplayStore::new(competing_path).with_lock(|| {
                acquired_tx.send(()).map_err(io::Error::other)?;
                Ok(())
            });
            assert!(result.is_ok());
        });
        let remained_blocked = acquired_rx.recv_timeout(Duration::from_millis(200)).is_err();
        fs::write(&release, b"release").expect("release child lock");
        let child_output = child.wait_with_output().expect("wait for lock holder");
        let acquired_after_exit = acquired_rx.recv_timeout(Duration::from_secs(3)).is_ok();
        competing.join().expect("competing lock attempt completes");
        assert!(child_output.status.success(), "child test failed: {}", String::from_utf8_lossy(&child_output.stderr));
        assert!(remained_blocked, "a second process entered the ownership critical section concurrently");
        assert!(acquired_after_exit, "kernel lock was not released after the holder exited");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_reserved_intentions_fail_closed_and_are_never_rewritten() {
        for (name, content) in [
            ("syntax", "{broken"),
            ("id", r#"{"16":"0123456789abcdef0123456789abcdef"}"#),
            ("owner", r#"{"1":"untrusted"}"#),
        ] {
            let path = owned_path(&format!("reserved-corrupt-{name}"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let sidecar = path.with_extension("reservations.json");
            fs::write(&sidecar, content).unwrap();
            let store = OwnedDisplayStore::new(&path);
            assert_eq!(store.try_load_reserved_ids().unwrap_err().kind(), io::ErrorKind::InvalidData);
            assert!(store.try_reserve(2, "0123456789abcdef0123456789abcdef").is_err());
            assert!(store.try_release_reserved_if_owner(1, "0123456789abcdef0123456789abcdef").is_err());
            assert_eq!(fs::read_to_string(&sidecar).unwrap(), content);
            let _ = fs::remove_dir_all(path.parent().unwrap());
        }
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
