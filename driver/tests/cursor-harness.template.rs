#![allow(non_snake_case, non_camel_case_types, dead_code)]
extern crate self as wdf_umdf_sys;
use std::{
    cell::UnsafeCell,
    ffi::c_void,
    mem::{size_of, MaybeUninit},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicPtr, Ordering},
        Arc, Mutex, RwLock,
    },
};
macro_rules! error { ($($args:tt)*) => {{ let _ = format_args!($($args)*); }}; }
macro_rules! warn { ($($args:tt)*) => {{ let _ = format_args!($($args)*); }}; }
macro_rules! s {
    ($value:literal) => {
        concat!($value, "\0").as_ptr()
    };
}
type IDDCX_MONITOR = *mut c_void;
struct SwapChainProcessor;
#[derive(Debug, Copy, Clone)]
struct HANDLE(*mut c_void);
struct Bool(i32);
const TRUE: Bool = Bool(1);
#[derive(Debug, Copy, Clone)]
struct NTSTATUS(i32);
impl NTSTATUS {
    fn is_warning(self) -> bool {
        self.0 as u32 >> 30 == 2
    }
    fn is_error(self) -> bool {
        self.0 as u32 >> 30 == 3
    }
}
#[derive(Debug)]
struct IddCxError;
#[derive(Debug)]
pub enum WdfError {
    Unknown,
    UpgradeFailed,
    LockFailed,
}
type WDFOBJECT = *mut c_void;
struct _WDF_OBJECT_CONTEXT_TYPE_INFO {
    Size: u32,
    ContextName: *const i8,
    ContextSize: usize,
    UniqueType: *const _WDF_OBJECT_CONTEXT_TYPE_INFO,
    EvtDriverGetUniqueContextType: Option<unsafe fn()>,
}
struct WDF_OBJECT_ATTRIBUTES {
    ContextTypeInfo: *const _WDF_OBJECT_CONTEXT_TYPE_INFO,
    EvtCleanupCallback: Option<unsafe extern "C-unwind" fn(WDFOBJECT)>,
    EvtDestroyCallback: Option<unsafe extern "C-unwind" fn(WDFOBJECT)>,
}
impl WDF_OBJECT_ATTRIBUTES {
    fn init_context_type(info: &_WDF_OBJECT_CONTEXT_TYPE_INFO) -> Self {
        Self {
            ContextTypeInfo: info,
            EvtCleanupCallback: None,
            EvtDestroyCallback: None,
        }
    }
}
unsafe fn WdfObjectGetTypedContextWorker(
    object: WDFOBJECT,
    _: &_WDF_OBJECT_CONTEXT_TYPE_INFO,
) -> Result<*mut c_void, WdfError> {
    if object.is_null() {
        Err(WdfError::Unknown)
    } else {
        Ok(object)
    }
}
#[derive(Default)]
struct IDDCX_CURSOR_CAPS {
    Size: u32,
    AlphaCursorSupport: i32,
    MaxX: u32,
    MaxY: u32,
    ColorXorCursorSupport: IDDCX_XOR_CURSOR_SUPPORT,
}
#[derive(Default)]
enum IDDCX_XOR_CURSOR_SUPPORT {
    #[default]
    IDDCX_XOR_CURSOR_SUPPORT_NONE,
}
struct IDARG_IN_SETUP_HWCURSOR {
    CursorInfo: IDDCX_CURSOR_CAPS,
    hNewCursorDataAvailable: *mut c_void,
}
static REGISTER_FAIL: AtomicBool = AtomicBool::new(false);
static REGISTERED: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static TEST_LOCK: Mutex<()> = Mutex::new(());
#[link(name = "kernel32")]
extern "system" {
    #[link_name = "CreateEventA"]
    fn native_create_event(
        attributes: *const c_void,
        manual: i32,
        initial: i32,
        name: *const u8,
    ) -> *mut c_void;
    fn GetHandleInformation(handle: *mut c_void, flags: *mut u32) -> i32;
}
unsafe fn CreateEventA(
    _: Option<()>,
    manual: bool,
    initial: bool,
    name: *const u8,
) -> Result<HANDLE, std::io::Error> {
    let handle = unsafe { native_create_event(ptr::null(), manual.into(), initial.into(), name) };
    if handle.is_null() {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(HANDLE(handle))
    }
}
unsafe fn IddCxMonitorSetupHardwareCursor(
    _: IDDCX_MONITOR,
    args: &IDARG_IN_SETUP_HWCURSOR,
) -> Result<NTSTATUS, IddCxError> {
    REGISTERED.store(args.hNewCursorDataAvailable, Ordering::SeqCst);
    if REGISTER_FAIL.load(Ordering::SeqCst) {
        Err(IddCxError)
    } else {
        Ok(NTSTATUS(0))
    }
}
fn valid(handle: *mut c_void) -> bool {
    let mut flags = 0;
    unsafe { GetHandleInformation(handle, &mut flags) != 0 }
}

// ACTUAL_PRODUCTION_ITEMS

#[test]
fn cursor_event_closes_with_monitor_context() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    REGISTER_FAIL.store(false, Ordering::SeqCst);
    let mut context = MonitorContext::new(ptr::null_mut());
    context.setup_hw_cursor();
    let handle = REGISTERED.load(Ordering::SeqCst);
    assert!(valid(handle));
    drop(context);
    assert!(
        !valid(handle),
        "registered event leaked after monitor context destruction"
    );
}
#[test]
fn failed_cursor_registration_closes_new_event() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    REGISTER_FAIL.store(true, Ordering::SeqCst);
    let mut context = MonitorContext::new(ptr::null_mut());
    context.setup_hw_cursor();
    let handle = REGISTERED.load(Ordering::SeqCst);
    assert!(
        !valid(handle),
        "new event leaked when IddCx registration failed"
    );
}
#[test]
fn repeated_cursor_setup_keeps_one_live_event() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    REGISTER_FAIL.store(false, Ordering::SeqCst);
    let mut context = MonitorContext::new(ptr::null_mut());
    context.setup_hw_cursor();
    let first = REGISTERED.load(Ordering::SeqCst);
    context.setup_hw_cursor();
    let second = REGISTERED.load(Ordering::SeqCst);
    assert_eq!(
        first, second,
        "mode changes allocate a new event instead of retaining the monitor event"
    );
    drop(context);
    assert!(!valid(first));
}
#[test]
fn uninitialized_and_repeated_wdf_cleanup_are_safe() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut storage: MonitorContextStorage = unsafe { std::mem::zeroed() };
    let object = (&mut storage as *mut MonitorContextStorage).cast();
    let attributes = MonitorContext::object_attributes();
    assert!(
        attributes.EvtDestroyCallback.is_none(),
        "cleanup must be registered only once"
    );
    let callback = attributes
        .EvtCleanupCallback
        .expect("monitor cleanup must be registered");
    unsafe {
        callback(object);
        callback(object)
    };
    assert!(unsafe {
        MonitorContext::get_mut(object, |_| {
            panic!("uninitialized context became accessible")
        })
    }
    .is_err());
}
#[test]
fn initialized_wdf_context_get_mut_then_cleanup_releases_event_once() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    REGISTER_FAIL.store(false, Ordering::SeqCst);
    let mut storage: MonitorContextStorage = unsafe { std::mem::zeroed() };
    let object = (&mut storage as *mut MonitorContextStorage).cast();
    let mut context = MonitorContext::new(ptr::null_mut());
    context.setup_hw_cursor();
    let event = REGISTERED.load(Ordering::SeqCst);
    unsafe {
        context.init(object).unwrap();
        MonitorContext::get_mut(object, |context| context.device = 7usize as _).unwrap()
    };
    let held = storage.get().unwrap();
    assert_eq!(held.read().unwrap().device, 7usize as _);
    let weak = Arc::downgrade(&held);
    drop(held);
    let callback = MonitorContext::object_attributes()
        .EvtCleanupCallback
        .unwrap();
    unsafe { callback(object) };
    assert!(!valid(event));
    assert!(
        weak.upgrade().is_none(),
        "cleanup retained the Rust context allocation"
    );
    unsafe { callback(object) };
    assert!(storage.get().is_err());
}
#[test]
fn cleanup_waits_for_existing_rust_context_owner() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    REGISTER_FAIL.store(false, Ordering::SeqCst);
    let storage: MonitorContextStorage = unsafe { std::mem::zeroed() };
    let mut context = MonitorContext::new(ptr::null_mut());
    context.setup_hw_cursor();
    let event = REGISTERED.load(Ordering::SeqCst);
    storage.initialize(context).unwrap();
    let held = storage.get().unwrap();
    storage.clear();
    assert!(valid(event));
    drop(held);
    assert!(!valid(event));
    storage.clear();
}
#[test]
fn reinitialization_does_not_replace_a_live_context() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    REGISTER_FAIL.store(false, Ordering::SeqCst);
    let storage: MonitorContextStorage = unsafe { std::mem::zeroed() };
    let mut first = MonitorContext::new(ptr::null_mut());
    first.setup_hw_cursor();
    let first_event = REGISTERED.load(Ordering::SeqCst);
    storage.initialize(first).unwrap();
    let mut second = MonitorContext::new(ptr::null_mut());
    second.setup_hw_cursor();
    let second_event = REGISTERED.load(Ordering::SeqCst);
    assert!(storage.initialize(second).is_err());
    assert!(!valid(second_event));
    assert!(valid(first_event));
    storage.clear();
    assert!(!valid(first_event));
}
#[test]
fn failed_cursor_reconfiguration_preserves_previously_borrowed_event() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    REGISTER_FAIL.store(false, Ordering::SeqCst);
    let mut context = MonitorContext::new(ptr::null_mut());
    context.setup_hw_cursor();
    let event = REGISTERED.load(Ordering::SeqCst);
    REGISTER_FAIL.store(true, Ordering::SeqCst);
    context.setup_hw_cursor();
    assert_eq!(event, REGISTERED.load(Ordering::SeqCst));
    assert!(valid(event));
    drop(context);
    assert!(!valid(event));
}
#[test]
fn context_cloning_and_cleanup_can_overlap() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    REGISTER_FAIL.store(false, Ordering::SeqCst);
    for _ in 0..100 {
        let storage: Arc<MonitorContextStorage> = Arc::new(unsafe { std::mem::zeroed() });
        let mut context = MonitorContext::new(ptr::null_mut());
        context.setup_hw_cursor();
        let event = REGISTERED.load(Ordering::SeqCst);
        storage.initialize(context).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(5));
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let storage = storage.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..1000 {
                        if let Ok(context) = storage.get() {
                            let _guard = context.read().unwrap();
                        }
                    }
                })
            })
            .collect();
        barrier.wait();
        std::thread::yield_now();
        storage.clear();
        for reader in readers {
            reader.join().unwrap()
        }
        storage.clear();
        assert!(storage.get().is_err());
        assert!(!valid(event));
    }
}
