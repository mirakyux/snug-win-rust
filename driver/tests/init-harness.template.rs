#![allow(non_camel_case_types, non_snake_case, dead_code, private_interfaces)]
use std::{
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        OnceLock,
    },
};

macro_rules! error { ($($t:tt)*) => { let _ = format_args!($($t)*); } }

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NTSTATUS(i32);
impl NTSTATUS {
    const STATUS_SUCCESS: Self = Self(0);
    const STATUS_INVALID_ADDRESS: Self = Self(0xc0000141u32 as i32);
    const STATUS_ADAPTER_HARDWARE_ERROR: Self = Self(0xc00000c2u32 as i32);
    const STATUS_INSUFFICIENT_RESOURCES: Self = Self(0xc000009au32 as i32);
    const STATUS_DRIVER_INTERNAL_ERROR: Self = Self(0xc0000183u32 as i32);
    const STATUS_INVALID_HANDLE: Self = Self(0xc0000008u32 as i32);
    fn is_success(self) -> bool {
        self.0 >= 0
    }
}
impl From<()> for NTSTATUS {
    fn from(_: ()) -> Self {
        Self::STATUS_SUCCESS
    }
}
impl<R: Into<NTSTATUS>, E: Into<NTSTATUS>> From<Result<R, E>> for NTSTATUS {
    fn from(value: Result<R, E>) -> Self {
        match value {
            Ok(result) => result.into(),
            Err(error) => error.into(),
        }
    }
}

type IDDCX_ADAPTER__ = u8;
type WDFDEVICE = *mut u8;
type WDF_POWER_DEVICE_STATE = i32;
#[repr(C)]
pub struct IDARG_IN_ADAPTER_INIT_FINISHED {
    AdapterInitStatus: NTSTATUS,
}
struct AdapterObject(NonNull<IDDCX_ADAPTER__>);
unsafe impl Sync for AdapterObject {}
unsafe impl Send for AdapterObject {}
static ADAPTER: OnceLock<AdapterObject> = OnceLock::new();
static STARTUPS: AtomicUsize = AtomicUsize::new(0);
static INIT_KIND: AtomicUsize = AtomicUsize::new(0);
static INIT_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
static LOOKUP_FAIL: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
struct WdfError(NTSTATUS);
impl From<WdfError> for NTSTATUS {
    fn from(error: WdfError) -> Self {
        error.0
    }
}
#[derive(Debug)]
struct IddCxError(NTSTATUS);
impl From<IddCxError> for NTSTATUS {
    fn from(error: IddCxError) -> Self {
        error.0
    }
}
#[derive(Debug)]
enum ContextError {
    Ntstatus(NTSTATUS),
    IddCx(IddCxError),
    Wdf(WdfError),
    Other,
}
struct DeviceContext;
impl DeviceContext {
    unsafe fn get_mut<F>(_: WDFDEVICE, callback: F) -> Result<(), WdfError>
    where
        F: FnOnce(&mut DeviceContext),
    {
        if LOOKUP_FAIL.load(Ordering::SeqCst) {
            return Err(WdfError(NTSTATUS::STATUS_INVALID_HANDLE));
        }
        callback(&mut Self);
        Ok(())
    }
    fn init_adapter(&mut self) -> Result<(), ContextError> {
        INIT_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        let failure = NTSTATUS::STATUS_INSUFFICIENT_RESOURCES;
        match INIT_KIND.load(Ordering::SeqCst) {
            0 => Ok(()),
            1 => Err(ContextError::Ntstatus(failure)),
            2 => Err(ContextError::IddCx(IddCxError(failure))),
            3 => Err(ContextError::Wdf(WdfError(failure))),
            _ => Err(ContextError::Other),
        }
    }
    fn finish_init() -> NTSTATUS {
        STARTUPS.fetch_add(1, Ordering::SeqCst);
        NTSTATUS::STATUS_SUCCESS
    }
}

// PRODUCTION_ITEMS

#[test]
fn async_failure_does_not_publish_and_allows_later_success() {
    let failed = IDARG_IN_ADAPTER_INIT_FINISHED {
        AdapterInitStatus: NTSTATUS::STATUS_INSUFFICIENT_RESOURCES,
    };
    let mut first_adapter = 1u8;
    // The notification itself is handled successfully, as in IddSample.
    assert_eq!(
        adapter_init_finished(&mut first_adapter, &failed),
        NTSTATUS::STATUS_SUCCESS
    );
    assert!(ADAPTER.get().is_none());
    assert_eq!(STARTUPS.load(Ordering::SeqCst), 0);

    let success = IDARG_IN_ADAPTER_INIT_FINISHED {
        AdapterInitStatus: NTSTATUS::STATUS_SUCCESS,
    };
    assert_eq!(
        adapter_init_finished(std::ptr::null_mut(), &success),
        NTSTATUS::STATUS_INVALID_ADDRESS
    );
    assert_eq!(
        adapter_init_finished(&mut first_adapter, std::ptr::null()),
        NTSTATUS::STATUS_INVALID_ADDRESS
    );
    assert!(ADAPTER.get().is_none());
    assert_eq!(STARTUPS.load(Ordering::SeqCst), 0);

    let mut ready_adapter = 2u8;
    assert_eq!(
        adapter_init_finished(&mut ready_adapter, &success),
        NTSTATUS::STATUS_SUCCESS
    );
    assert_eq!(
        ADAPTER.get().unwrap().0.as_ptr(),
        std::ptr::addr_of_mut!(ready_adapter)
    );
    assert_eq!(STARTUPS.load(Ordering::SeqCst), 1);
    assert_eq!(
        adapter_init_finished(&mut first_adapter, &success),
        NTSTATUS::STATUS_ADAPTER_HARDWARE_ERROR
    );
    assert_eq!(
        ADAPTER.get().unwrap().0.as_ptr(),
        std::ptr::addr_of_mut!(ready_adapter)
    );
    assert_eq!(STARTUPS.load(Ordering::SeqCst), 1);
}

fn reset() {
    INIT_KIND.store(0, Ordering::SeqCst);
    INIT_ATTEMPTS.store(0, Ordering::SeqCst);
    LOOKUP_FAIL.store(false, Ordering::SeqCst);
}

#[test]
fn synchronous_native_initialization_failures_keep_the_original_status() {
    reset();
    let mut device = 0u8;
    for kind in 1..=3 {
        INIT_KIND.store(kind, Ordering::SeqCst);
        assert_eq!(
            device_d0_entry(&mut device, 0),
            NTSTATUS::STATUS_INSUFFICIENT_RESOURCES
        );
    }
    assert_eq!(INIT_ATTEMPTS.load(Ordering::SeqCst), 3);
}

#[test]
fn synchronous_non_native_failure_returns_an_error() {
    reset();
    INIT_KIND.store(4, Ordering::SeqCst);
    assert_eq!(
        device_d0_entry(&mut 0u8, 0),
        NTSTATUS::STATUS_DRIVER_INTERNAL_ERROR
    );
}

#[test]
fn synchronous_success_is_preserved() {
    reset();
    assert_eq!(device_d0_entry(&mut 0u8, 0), NTSTATUS::STATUS_SUCCESS);
    assert_eq!(INIT_ATTEMPTS.load(Ordering::SeqCst), 1);
}

#[test]
fn context_lookup_failure_does_not_attempt_initialization() {
    reset();
    LOOKUP_FAIL.store(true, Ordering::SeqCst);
    assert_eq!(
        device_d0_entry(&mut 0u8, 0),
        NTSTATUS::STATUS_INVALID_HANDLE
    );
    assert_eq!(INIT_ATTEMPTS.load(Ordering::SeqCst), 0);
}

#[test]
fn null_device_does_not_attempt_initialization() {
    reset();
    assert_eq!(
        device_d0_entry(std::ptr::null_mut(), 0),
        NTSTATUS::STATUS_INVALID_ADDRESS
    );
    assert_eq!(INIT_ATTEMPTS.load(Ordering::SeqCst), 0);
}
