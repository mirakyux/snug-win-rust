#![allow(non_snake_case, non_camel_case_types, dead_code)]
extern crate self as log;
extern crate self as wdf_umdf;
extern crate self as wdf_umdf_sys;
extern crate self as windows;
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    },
};
#[macro_export]
macro_rules! error {($($args:tt)*)=>{{let _=format_args!($($args)*);}};}
#[macro_export]
macro_rules! debug {($($args:tt)*)=>{{let _=format_args!($($args)*);}};}
#[macro_export]
macro_rules! w {
    ($value:literal) => {
        0usize
    };
}
pub mod core {
    pub use crate::w;
    pub trait Interface {
        fn cast<T: Default>(&self) -> Result<T, ()> {
            Ok(T::default())
        }
    }
}
pub mod Win32 {
    pub mod Foundation {
        #[derive(Debug, Copy, Clone)]
        pub struct HANDLE(pub *mut std::ffi::c_void);
    }
    pub mod Graphics {
        pub mod Dxgi {
            #[derive(Default, Debug)]
            pub struct IDXGIDevice;
            impl IDXGIDevice {
                pub fn as_raw(&self) -> *mut std::ffi::c_void {
                    std::ptr::null_mut()
                }
            }
            pub struct IDXGIResource;
            impl IDXGIResource {
                pub unsafe fn from_raw(_: *mut std::ffi::c_void) -> Self {
                    Self
                }
            }
        }
    }
    pub mod System {
        pub mod Threading {
            use crate::Win32::Foundation::HANDLE;
            use crate::{PRIORITY_FAIL, REVERT_COUNT};
            use std::sync::atomic::Ordering;
            pub unsafe fn AvSetMmThreadCharacteristicsW(
                _: usize,
                _: &mut u32,
            ) -> Result<HANDLE, ()> {
                if PRIORITY_FAIL.load(Ordering::SeqCst) {
                    Err(())
                } else {
                    Ok(HANDLE(1usize as _))
                }
            }
            pub unsafe fn AvRevertMmThreadCharacteristics(_: HANDLE) -> Result<(), ()> {
                REVERT_COUNT.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            pub unsafe fn WaitForSingleObject(_: HANDLE, _: u32) -> crate::WaitResult {
                crate::WaitResult(0)
            }
        }
    }
}
pub mod direct_3d_device {
    #[derive(Default)]
    pub struct Direct3DDevice {
        pub device: FakeDevice,
    }
    #[derive(Default)]
    pub struct FakeDevice;
    impl crate::core::Interface for FakeDevice {}
}
pub mod helpers {
    pub struct Sendable<T>(T);
    unsafe impl<T> Send for Sendable<T> {}
    impl<T> Sendable<T> {
        pub unsafe fn new(value: T) -> Self {
            Self(value)
        }
    }
    impl<T> std::ops::Deref for Sendable<T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.0
        }
    }
}
pub type HANDLE = *mut c_void;
pub type WDFOBJECT = *mut c_void;
pub type IDDCX_SWAPCHAIN = *mut c_void;
pub const WAIT_TIMEOUT: u32 = 258;
pub struct WaitResult(pub u32);
#[derive(Debug, Copy, Clone)]
pub struct NTSTATUS(i32);
impl NTSTATUS {
    pub fn is_success(self) -> bool {
        self.0 >= 0
    }
}
impl From<NTSTATUS> for u32 {
    fn from(value: NTSTATUS) -> u32 {
        value.0 as u32
    }
}
impl From<Result<NTSTATUS, ()>> for NTSTATUS {
    fn from(value: Result<NTSTATUS, ()>) -> Self {
        value.unwrap_or(Self(-1))
    }
}
pub struct IDARG_IN_SWAPCHAINSETDEVICE {
    pub pDevice: *mut c_void,
}
pub struct Metadata {
    pub pSurface: *mut c_void,
}
pub struct IDARG_OUT_RELEASEANDACQUIREBUFFER {
    pub MetaData: Metadata,
}
impl Default for IDARG_OUT_RELEASEANDACQUIREBUFFER {
    fn default() -> Self {
        Self {
            MetaData: Metadata {
                pSurface: std::ptr::null_mut(),
            },
        }
    }
}
pub unsafe fn IddCxSwapChainSetDevice(
    _: IDDCX_SWAPCHAIN,
    _: &IDARG_IN_SWAPCHAINSETDEVICE,
) -> Result<(), ()> {
    Ok(())
}
pub unsafe fn IddCxSwapChainReleaseAndAcquireBuffer(
    _: IDDCX_SWAPCHAIN,
    _: &mut IDARG_OUT_RELEASEANDACQUIREBUFFER,
) -> Result<NTSTATUS, ()> {
    PROCESS_COUNT.fetch_add(1, Ordering::SeqCst);
    if STREAMING.load(Ordering::SeqCst) && !STOP_STREAM.load(Ordering::SeqCst) {
        Ok(NTSTATUS(0))
    } else {
        Err(())
    }
}
pub unsafe fn IddCxSwapChainFinishedProcessingFrame(_: IDDCX_SWAPCHAIN) -> Result<(), ()> {
    Ok(())
}
pub unsafe fn WdfObjectDelete(_: WDFOBJECT) -> Result<(), ()> {
    DELETE_COUNT.fetch_add(1, Ordering::SeqCst);
    Ok(())
}
static PRIORITY_FAIL: AtomicBool = AtomicBool::new(false);
static PROCESS_COUNT: AtomicUsize = AtomicUsize::new(0);
static DELETE_COUNT: AtomicUsize = AtomicUsize::new(0);
static REVERT_COUNT: AtomicUsize = AtomicUsize::new(0);
static TEST_LOCK: Mutex<()> = Mutex::new(());
mod actual_swap_chain_processor {
    include!(env!("SNUG_SWAPCHAIN_SOURCE"));
}
fn exercise(priority_fails: bool) {
    let _lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    PRIORITY_FAIL.store(priority_fails, Ordering::SeqCst);
    PROCESS_COUNT.store(0, Ordering::SeqCst);
    DELETE_COUNT.store(0, Ordering::SeqCst);
    REVERT_COUNT.store(0, Ordering::SeqCst);
    let mut processor = actual_swap_chain_processor::SwapChainProcessor::new();
    processor.run(
        1usize as _,
        direct_3d_device::Direct3DDevice::default(),
        std::ptr::null_mut(),
    );
    drop(processor);
    assert_eq!(
        PROCESS_COUNT.load(Ordering::SeqCst),
        1,
        "priority registration failure must not skip frame processing"
    );
    assert_eq!(
        DELETE_COUNT.load(Ordering::SeqCst),
        1,
        "assigned swapchain must be deleted exactly once"
    );
    assert_eq!(
        REVERT_COUNT.load(Ordering::SeqCst),
        usize::from(!priority_fails)
    );
}
#[test]
fn mmcss_failure_still_processes_and_releases_swapchain() {
    exercise(true)
}
#[test]
fn successful_mmcss_registration_is_reverted_after_release() {
    exercise(false)
}

static STREAMING: AtomicBool = AtomicBool::new(false);
static STOP_STREAM: AtomicBool = AtomicBool::new(false);

#[test]
fn sustained_successful_acquire_honors_drop_stop_request() {
    use std::{
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };
    let _lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    PRIORITY_FAIL.store(false, Ordering::SeqCst);
    PROCESS_COUNT.store(0, Ordering::SeqCst);
    DELETE_COUNT.store(0, Ordering::SeqCst);
    REVERT_COUNT.store(0, Ordering::SeqCst);
    STOP_STREAM.store(false, Ordering::SeqCst);
    STREAMING.store(true, Ordering::SeqCst);
    let mut processor = actual_swap_chain_processor::SwapChainProcessor::new();
    processor.run(
        1usize as _,
        direct_3d_device::Direct3DDevice::default(),
        std::ptr::null_mut(),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while PROCESS_COUNT.load(Ordering::SeqCst) < 100 && Instant::now() < deadline {
        thread::yield_now();
    }
    let started_processing = PROCESS_COUNT.load(Ordering::SeqCst) >= 100;
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let dropper = thread::spawn(move || {
        started_tx.send(()).unwrap();
        drop(processor);
        done_tx.send(()).unwrap();
    });
    let drop_started = started_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    let stopped_without_native_fault = done_rx.recv_timeout(Duration::from_millis(200)).is_ok();
    let frames_before_native_fault = PROCESS_COUNT.load(Ordering::SeqCst);
    // Always release the fake stream and join before asserting, even on the red case.
    STOP_STREAM.store(true, Ordering::SeqCst);
    let joined = dropper.join().is_ok();
    STREAMING.store(false, Ordering::SeqCst);
    println!("frames_before_native_fault={frames_before_native_fault}; stopped_without_native_fault={stopped_without_native_fault}; delete_count={}", DELETE_COUNT.load(Ordering::SeqCst));
    assert!(
        started_processing && drop_started && joined,
        "proof setup or cleanup failed"
    );
    assert!(stopped_without_native_fault, "Drop ignored terminate while Acquire continuously succeeded; only a forced native error released join");
    assert_eq!(DELETE_COUNT.load(Ordering::SeqCst), 1);
}
