//! Resolve Windows tools and create privileged staging without using caller-supplied paths.

use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

// Owner Rights suppresses the owner's implicit WRITE_DAC, including on inherited children.
const STAGING_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;RC;;;OW)";

pub fn system_directory() -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 260];
    loop {
        let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if (length as usize) < buffer.len() {
            buffer.truncate(length as usize);
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer)));
        }
        buffer.resize(length as usize + 1, 0);
    }
}

pub fn powershell() -> io::Result<PathBuf> {
    Ok(system_directory()?.join(r"WindowsPowerShell\v1.0\powershell.exe"))
}

/// Explorer is installed beside System32 in the Windows directory.
pub fn explorer() -> io::Result<PathBuf> {
    let system = system_directory()?;
    let windows = system
        .parent()
        .ok_or_else(|| io::Error::other("invalid Windows system directory"))?;
    Ok(windows.join("explorer.exe"))
}

/// Resolve a Windows-bundled command by basename inside the trusted system directory.
pub fn system_tool(name: &str) -> io::Result<PathBuf> {
    let valid_exe = name
        .strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"));
    if !valid_exe.is_some_and(|stem| {
        !stem.is_empty()
            && stem
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "system tool must be a basename",
        ));
    }
    Ok(system_directory()?.join(name))
}

/// The parent must itself be protected against deletion and replacement of children.
pub fn create_protected_directory(path: &Path) -> io::Result<()> {
    if path.parent() != Some(system_directory()?.as_path())
        || !matches!(path.components().next_back(), Some(Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "staging must be a direct child of System32",
        ));
    }
    create_directory_with_sddl(path, STAGING_SDDL)
}

fn create_directory_with_sddl(path: &Path, sddl: &str) -> io::Result<()> {
    let mut path: Vec<u16> = path.as_os_str().encode_wide().collect();
    if path.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "staging path contains NUL",
        ));
    }
    path.push(0);
    let descriptor = SecurityDescriptor::from_sddl(sddl)?;
    let attributes = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor: descriptor.0,
        inherit_handle: 0,
    };
    if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct SecurityDescriptor(*mut std::ffi::c_void);

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let sddl: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
        let mut descriptor = std::ptr::null_mut();
        let mut descriptor_size = 0u32;
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                &mut descriptor_size,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut std::ffi::c_void,
    inherit_handle: i32,
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sddl: *const u16,
        revision: u32,
        descriptor: *mut *mut std::ffi::c_void,
        descriptor_size: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateDirectoryW(path: *const u16, attributes: *const SecurityAttributes) -> i32;
    fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
    fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    const DACL_SECURITY_INFORMATION: u32 = 4;
    const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
    const UNPROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x2000_0000;
    const SE_FILE_OBJECT: i32 = 1;
    const SE_DACL_PROTECTED: u16 = 0x1000;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn GetSecurityInfo(
            handle: *mut std::ffi::c_void,
            object_type: i32,
            information: u32,
            owner: *mut *mut std::ffi::c_void,
            group: *mut *mut std::ffi::c_void,
            dacl: *mut *mut std::ffi::c_void,
            sacl: *mut *mut std::ffi::c_void,
            descriptor: *mut *mut std::ffi::c_void,
        ) -> u32;
        fn SetSecurityInfo(
            handle: *mut std::ffi::c_void,
            object_type: i32,
            information: u32,
            owner: *mut std::ffi::c_void,
            group: *mut std::ffi::c_void,
            dacl: *mut std::ffi::c_void,
            sacl: *mut std::ffi::c_void,
        ) -> u32;
        fn GetSecurityDescriptorDacl(
            descriptor: *mut std::ffi::c_void,
            present: *mut i32,
            dacl: *mut *mut std::ffi::c_void,
            defaulted: *mut i32,
        ) -> i32;
        fn GetSecurityDescriptorControl(
            descriptor: *mut std::ffi::c_void,
            control: *mut u16,
            revision: *mut u32,
        ) -> i32;
        fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor: *mut std::ffi::c_void,
            revision: u32,
            information: u32,
            text: *mut *mut u16,
            length: *mut u32,
        ) -> i32;
    }

    fn dacl(descriptor: &SecurityDescriptor) -> *mut std::ffi::c_void {
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted)
            },
            0
        );
        assert_ne!(present, 0);
        assert!(!acl.is_null());
        acl
    }

    fn descriptor_of(file: &std::fs::File) -> SecurityDescriptor {
        let mut descriptor = std::ptr::null_mut();
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(
            status,
            0,
            "GetSecurityInfo: {}",
            io::Error::from_raw_os_error(status as i32)
        );
        SecurityDescriptor(descriptor)
    }

    fn dacl_text(descriptor: &SecurityDescriptor) -> String {
        let mut text = std::ptr::null_mut();
        let mut length = 0;
        assert_ne!(
            unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor.0,
                    1,
                    DACL_SECURITY_INFORMATION,
                    &mut text,
                    &mut length,
                )
            },
            0
        );
        let result = unsafe {
            String::from_utf16_lossy(std::slice::from_raw_parts(text, length as usize - 1))
        };
        unsafe { LocalFree(text.cast()) };
        result
    }

    /// Keep WRITE_DAC handles obtained before hardening solely to restore this test's ACLs.
    /// Production never opens an exposed directory or repairs its ACL after creation.
    struct AclFixture {
        path: PathBuf,
        parent: std::fs::File,
        child: std::fs::File,
        original_parent: SecurityDescriptor,
        original_child: SecurityDescriptor,
    }

    impl AclFixture {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("snug-acl-test-{}-{nonce}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            let child_path = path.join("payload.exe");
            std::fs::write(&child_path, b"verified payload").unwrap();
            // Explicit access (not MAXIMUM_ALLOWED) lets Windows propagate inherited ACEs.
            let open = |path: &Path, directory| {
                std::fs::OpenOptions::new()
                    .access_mode(0x0002_0000 | 0x0004_0000) // READ_CONTROL | WRITE_DAC
                    .share_mode(1 | 2 | 4)
                    .custom_flags(if directory { 0x0200_0000 } else { 0 })
                    .open(path)
                    .unwrap()
            };
            let parent = open(&path, true);
            let child = open(&child_path, false);
            let original_parent = descriptor_of(&parent);
            let original_child = descriptor_of(&child);
            Self {
                path,
                parent,
                child,
                original_parent,
                original_child,
            }
        }
    }

    fn set_dacl(file: &std::fs::File, descriptor: &SecurityDescriptor, protected: bool) -> u32 {
        unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION
                    | if protected {
                        PROTECTED_DACL_SECURITY_INFORMATION
                    } else {
                        UNPROTECTED_DACL_SECURITY_INFORMATION
                    },
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl(descriptor),
                std::ptr::null_mut(),
            )
        }
    }

    impl Drop for AclFixture {
        fn drop(&mut self) {
            let restored = set_dacl(&self.parent, &self.original_parent, false);
            let child_restored = set_dacl(&self.child, &self.original_child, false);
            if restored != 0 || child_restored != 0 {
                eprintln!(
                    "ACL test cleanup could not restore {}: parent={restored}, child={child_restored}",
                    self.path.display()
                );
                return;
            }
            // This fixture can remove only its own direct child of the temp directory.
            assert_eq!(self.path.parent(), Some(std::env::temp_dir().as_path()));
            if let Err(error) = std::fs::remove_dir_all(&self.path) {
                eprintln!("ACL test cleanup {}: {error}", self.path.display());
            }
        }
    }

    #[test]
    fn staging_rejects_unprotected_parents_and_windows_accepts_its_descriptor() {
        let path = std::env::temp_dir().join("nested").join("snug-test");
        assert_eq!(
            create_protected_directory(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(SecurityDescriptor::from_sddl(STAGING_SDDL).is_ok());
    }

    #[test]
    fn system_tools_resolve_only_safe_executable_basenames() {
        assert!(system_tool("schtasks.exe").unwrap().is_absolute());
        for name in ["", ".exe", "..\\evil.exe", "curl.com", "tool.exe/other"] {
            assert_eq!(
                system_tool(name).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn production_dacl_reaches_existing_children_and_blocks_standard_user_writes() {
        let fixture = AclFixture::new();
        let descriptor = SecurityDescriptor::from_sddl(STAGING_SDDL).unwrap();
        assert_eq!(set_dacl(&fixture.parent, &descriptor, true), 0);

        let parent = descriptor_of(&fixture.parent);
        let parent_text = dacl_text(&parent);
        assert!(parent_text.starts_with("D:P"), "{parent_text}");
        for ace in ["(A;OICI;FA;;;SY)", "(A;OICI;FA;;;BA)", "(A;OICI;RC;;;OW)"] {
            assert!(parent_text.contains(ace), "missing {ace}: {parent_text}");
        }
        let mut control = 0;
        let mut revision = 0;
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(parent.0, &mut control, &mut revision) },
            0
        );
        assert_ne!(control & SE_DACL_PROTECTED, 0);

        let child_text = dacl_text(&descriptor_of(&fixture.child));
        for ace in ["(A;ID;FA;;;SY)", "(A;ID;FA;;;BA)", "(A;ID;RC;;;OW)"] {
            assert!(
                child_text.contains(ace),
                "missing inherited {ace}: {child_text}"
            );
        }
        assert_eq!(
            child_text.matches('(').count(),
            3,
            "unexpected child grants: {child_text}"
        );
        if !crate::elevation::is_elevated() {
            assert_eq!(
                std::fs::write(fixture.path.join("new-file"), b"blocked")
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                std::fs::write(fixture.path.join("payload.exe"), b"blocked")
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                std::fs::OpenOptions::new()
                    .access_mode(0x0004_0000)
                    .open(fixture.path.join("payload.exe"))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn atomic_directory_creation_refuses_existing_content() {
        let fixture = AclFixture::new();
        let error = create_directory_with_sddl(&fixture.path, STAGING_SDDL).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(fixture.path.join("payload.exe")).unwrap(),
            b"verified payload"
        );
    }

    /// Run explicitly from an elevated terminal; never triggers UAC itself:
    /// `cargo test -p snug-win --offline elevated_atomic_staging_inherits_production_acl -- --ignored --exact system_paths::tests::elevated_atomic_staging_inherits_production_acl`
    #[test]
    #[ignore = "requires an elevated Windows token and creates only an isolated System32 staging directory"]
    fn elevated_atomic_staging_inherits_production_acl() {
        assert!(
            crate::elevation::is_elevated(),
            "run this explicit acceptance test from an elevated terminal"
        );
        let system = system_directory().unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = system.join(format!("snug-acl-elevated-{}-{nonce}", std::process::id()));
        create_protected_directory(&path).unwrap();

        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                assert_eq!(self.0.parent(), Some(system_directory().unwrap().as_path()));
                assert!(
                    self.0
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("snug-acl-elevated-")
                );
                std::fs::remove_dir_all(&self.0).expect("remove isolated elevated staging fixture");
            }
        }
        let _cleanup = Cleanup(path.clone());
        let parent = std::fs::OpenOptions::new()
            .access_mode(0x0002_0000)
            .share_mode(1 | 2 | 4)
            .custom_flags(0x0200_0000)
            .open(&path)
            .unwrap();
        let parent_text = dacl_text(&descriptor_of(&parent));
        assert!(parent_text.starts_with("D:P"), "{parent_text}");
        for ace in ["(A;OICI;FA;;;SY)", "(A;OICI;FA;;;BA)", "(A;OICI;RC;;;OW)"] {
            assert!(parent_text.contains(ace), "missing {ace}: {parent_text}");
        }
        assert_eq!(
            parent_text.matches('(').count(),
            3,
            "unexpected parent grants: {parent_text}"
        );
        let child_path = path.join("payload.exe");
        std::fs::write(&child_path, b"verified payload").unwrap();
        let child = std::fs::OpenOptions::new()
            .access_mode(0x0002_0000)
            .share_mode(1 | 2 | 4)
            .open(&child_path)
            .unwrap();
        let child_text = dacl_text(&descriptor_of(&child));
        for ace in ["(A;ID;FA;;;SY)", "(A;ID;FA;;;BA)", "(A;ID;RC;;;OW)"] {
            assert!(
                child_text.contains(ace),
                "missing inherited {ace}: {child_text}"
            );
        }
        assert_eq!(
            child_text.matches('(').count(),
            3,
            "unexpected child grants: {child_text}"
        );
        assert_eq!(
            create_protected_directory(&path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&child_path).unwrap(), b"verified payload");
    }
}
