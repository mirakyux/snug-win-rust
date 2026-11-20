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
}
