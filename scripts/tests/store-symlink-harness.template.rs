//! Run only after the launcher has proved that ordinary Windows symlinks are available.
//! The implementation under test is the workspace's real store module.
#![cfg(test)]

use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use snug_core::store::{JsonFileStore, RestoreRecordStore};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateSymbolicLinkW(link: *const u16, target: *const u16, flags: u32) -> u8;
}

fn fixture(name: &str) -> PathBuf {
    let root = PathBuf::from(std::env::var_os("SNUG_SYMLINK_FIXTURE_ROOT").expect("isolated test root"));
    let path = root.join(name);
    fs::create_dir(&path).unwrap();
    path
}

fn symlink(link: &Path, target: &Path, directory: bool) {
    let wide = |path: &Path| path.as_os_str().encode_wide().chain([0]).collect::<Vec<_>>();
    let link_wide = wide(link);
    let target_wide = wide(target);
    let result = unsafe {
        CreateSymbolicLinkW(link_wide.as_ptr(), target_wide.as_ptr(), 2 | u32::from(directory))
    };
    assert_ne!(result, 0, "ordinary symlink creation: {}", std::io::Error::last_os_error());
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
}

#[test]
fn broken_file_symlink_preserves_unknown_restore_source() {
    let root = fixture("store-file-link");
    let target = root.join("target.json");
    let link = root.join("restore.json");
    let store = JsonFileStore::<String>::new(&link);
    fs::write(&target, br#""retained""#).unwrap();
    symlink(&link, &target, false);
    assert_eq!(store.try_load_or_quarantine().unwrap(), Some("retained".into()));
    fs::remove_file(&target).unwrap();
    assert!(!link.exists());
    assert!(store.try_load_or_quarantine().is_err());
    assert!(RestoreRecordStore::<String>::new(&link).try_pending().is_err());
    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1, "no quarantine or replacement");
    fs::remove_file(&link).unwrap();
    store.save(&"recovered".into()).unwrap();
    assert_eq!(store.try_load_or_quarantine().unwrap(), Some("recovered".into()));
    fs::remove_file(&link).unwrap();
    fs::remove_dir(root).unwrap();
}

#[test]
fn broken_directory_symlink_is_not_an_absent_source() {
    let root = fixture("store-directory-link");
    let target = root.join("target");
    let link = root.join("restore.json");
    fs::create_dir(&target).unwrap();
    symlink(&link, &target, true);
    fs::remove_dir(&target).unwrap();
    assert!(!link.exists());
    let store = JsonFileStore::<String>::new(&link);
    assert!(store.try_load_or_quarantine().is_err());
    assert!(RestoreRecordStore::<String>::new(&link).try_pending().is_err());
    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1, "no quarantine or replacement");
    fs::remove_dir(&link).unwrap();
    store.save(&"recovered".into()).unwrap();
    assert_eq!(store.try_load_or_quarantine().unwrap(), Some("recovered".into()));
    fs::remove_file(&link).unwrap();
    fs::remove_dir(root).unwrap();
}
