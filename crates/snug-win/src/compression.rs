//! Small, bounded decompression helpers backed by Windows' system Cabinet API.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

const XPRESS_HUFF: u32 = 4;
const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;
const MAX_ASSET_SIZE: usize = 1024 * 1024;

type CreateDecompressor = unsafe extern "system" fn(u32, *const c_void, *mut *mut c_void) -> i32;
type Decompress = unsafe extern "system" fn(
    *mut c_void,
    *const c_void,
    usize,
    *mut c_void,
    usize,
    *mut usize,
) -> i32;
type CloseDecompressor = unsafe extern "system" fn(*mut c_void) -> i32;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(filename: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn FreeLibrary(module: *mut c_void) -> i32;
}

/// Decompress one XPRESS_HUFF buffer using only the copy of Cabinet.dll in System32.
///
/// The result is bounded to one MiB and must have exactly `expected_len` bytes.
/// Callers that require a particular asset must also verify its content hash.
pub fn decompress_xpress_huffman(input: &[u8], expected_len: usize) -> io::Result<Vec<u8>> {
    if input.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "compressed input is empty",
        ));
    }
    if input.len() > MAX_ASSET_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "compressed input exceeds the one MiB limit",
        ));
    }
    if expected_len == 0 || expected_len > MAX_ASSET_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "decompressed size must be between 1 byte and one MiB",
        ));
    }

    let library = CabinetLibrary::load()?;
    let api = library.api()?;
    let decoder = api.create_decompressor()?;

    let mut output = vec![0; expected_len];
    let mut actual_len = 0usize;
    let succeeded = unsafe {
        (api.decompress)(
            decoder.handle,
            input.as_ptr().cast(),
            input.len(),
            output.as_mut_ptr().cast(),
            output.len(),
            &mut actual_len,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    if actual_len != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "decompressed length mismatch: expected {expected_len} bytes, got {actual_len}"
            ),
        ));
    }

    // Close while the dynamically loaded function pointer and its module are both alive.
    decoder.close()?;
    Ok(output)
}

struct CabinetLibrary(*mut c_void);

impl CabinetLibrary {
    fn load() -> io::Result<Self> {
        let path = crate::system_paths::system_directory()?.join("cabinet.dll");
        let path: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let module =
            unsafe { LoadLibraryExW(path.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(module))
    }

    fn api(&self) -> io::Result<CabinetApi<'_>> {
        // SAFETY: each address is resolved by its exact export name from this
        // loaded Cabinet.dll and the function pointer remains scoped to it.
        let create = unsafe {
            std::mem::transmute::<*mut c_void, CreateDecompressor>(
                self.symbol(b"CreateDecompressor\0")?,
            )
        };
        let decompress = unsafe {
            std::mem::transmute::<*mut c_void, Decompress>(self.symbol(b"Decompress\0")?)
        };
        let close = unsafe {
            std::mem::transmute::<*mut c_void, CloseDecompressor>(
                self.symbol(b"CloseDecompressor\0")?,
            )
        };
        Ok(CabinetApi {
            _library: self,
            create,
            decompress,
            close,
        })
    }

    fn symbol(&self, name: &'static [u8]) -> io::Result<*mut c_void> {
        let address = unsafe { GetProcAddress(self.0, name.as_ptr()) };
        if address.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(address)
    }
}

impl Drop for CabinetLibrary {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
}

struct CabinetApi<'library> {
    // Keep the DLL loaded for every function pointer obtained from it.
    _library: &'library CabinetLibrary,
    create: CreateDecompressor,
    decompress: Decompress,
    close: CloseDecompressor,
}

impl<'library> CabinetApi<'library> {
    fn create_decompressor<'api>(&'api self) -> io::Result<Decompressor<'api, 'library>> {
        let mut handle = ptr::null_mut();
        let succeeded = unsafe { (self.create)(XPRESS_HUFF, ptr::null(), &mut handle) };
        if succeeded == 0 {
            // Capture the API error before attempting any cleanup.
            let error = io::Error::last_os_error();
            if !handle.is_null() {
                unsafe {
                    (self.close)(handle);
                }
            }
            return Err(error);
        }
        if handle.is_null() {
            return Err(io::Error::other(
                "CreateDecompressor succeeded without returning a handle",
            ));
        }
        Ok(Decompressor { api: self, handle })
    }
}

struct Decompressor<'api, 'library> {
    api: &'api CabinetApi<'library>,
    handle: *mut c_void,
}

impl Decompressor<'_, '_> {
    fn close(mut self) -> io::Result<()> {
        let handle = std::mem::replace(&mut self.handle, ptr::null_mut());
        if unsafe { (self.api.close)(handle) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Decompressor<'_, '_> {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                (self.api.close)(self.handle);
            }
            self.handle = ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_PLAIN: &[u8] = b"Snug font fixture\n";
    const GOLDEN_SHA256: &str = "90A28EF95E9176FD8C417C570DAED754B381B6282473A62DCB6613CDB6652846";
    // XPRESS_HUFF output from Windows Compression API, buffer mode, algorithm 4.
    const GOLDEN_COMPRESSED: &[u8] = &[
        0x0a, 0x51, 0xe5, 0xc0, 0x18, 0x00, 0xb3, 0x04, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x12, 0x00, 0x00, 0x00, 0x53, 0x6e,
        0x75, 0x67, 0x20, 0x66, 0x6f, 0x6e, 0x74, 0x20, 0x66, 0x69, 0x78, 0x74, 0x75, 0x72, 0x65,
        0x0a,
    ];

    #[test]
    fn restores_known_xpress_huffman_buffer() {
        assert_eq!(
            decompress_xpress_huffman(GOLDEN_COMPRESSED, GOLDEN_PLAIN.len()).unwrap(),
            GOLDEN_PLAIN
        );
    }

    #[test]
    fn rejects_empty_or_oversized_inputs_and_outputs_before_loading_cabinet() {
        assert_eq!(
            decompress_xpress_huffman(&[], GOLDEN_PLAIN.len())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            decompress_xpress_huffman(GOLDEN_COMPRESSED, 0)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            decompress_xpress_huffman(GOLDEN_COMPRESSED, MAX_ASSET_SIZE + 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            decompress_xpress_huffman(&vec![0; MAX_ASSET_SIZE + 1], GOLDEN_PLAIN.len())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn malformed_buffers_are_rejected_or_detected_by_the_asset_hash() {
        for input in [&GOLDEN_COMPRESSED[..GOLDEN_COMPRESSED.len() - 1], &[0; 8]] {
            if let Ok(output) = decompress_xpress_huffman(input, GOLDEN_PLAIN.len()) {
                assert_eq!(output.len(), GOLDEN_PLAIN.len());
                assert_ne!(crate::update::sha256_hex(&output), GOLDEN_SHA256);
            }
        }
    }

    #[test]
    fn rejects_wrong_output_lengths_and_accepts_a_larger_buffer_bound() {
        assert!(decompress_xpress_huffman(GOLDEN_COMPRESSED, GOLDEN_PLAIN.len() - 1).is_err());
        assert_eq!(
            decompress_xpress_huffman(GOLDEN_COMPRESSED, GOLDEN_PLAIN.len() + 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
