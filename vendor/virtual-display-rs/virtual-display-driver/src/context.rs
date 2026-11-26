use std::{
    cell::UnsafeCell,
    mem::{self, size_of, MaybeUninit},
    num::{ParseIntError, TryFromIntError},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::{addr_of_mut, NonNull},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
};

use anyhow::anyhow;
use log::{error, warn};
use wdf_umdf::{
    IddCxAdapterInitAsync, IddCxError, IddCxMonitorArrival, IddCxMonitorCreate,
    IddCxMonitorSetupHardwareCursor, WdfError, WdfObjectDelete, WdfObjectGetTypedContextWorker,
    WDF_DECLARE_CONTEXT_TYPE,
};
use wdf_umdf_sys::{
    DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY, HANDLE, IDARG_IN_ADAPTER_INIT, IDARG_IN_MONITORCREATE,
    IDARG_IN_SETUP_HWCURSOR, IDARG_OUT_ADAPTER_INIT, IDARG_OUT_MONITORARRIVAL,
    IDARG_OUT_MONITORCREATE, IDDCX_ADAPTER, IDDCX_ADAPTER_CAPS, IDDCX_CURSOR_CAPS,
    IDDCX_ENDPOINT_DIAGNOSTIC_INFO, IDDCX_ENDPOINT_VERSION, IDDCX_FEATURE_IMPLEMENTATION,
    IDDCX_MONITOR, IDDCX_MONITOR_DESCRIPTION, IDDCX_MONITOR_DESCRIPTION_TYPE, IDDCX_MONITOR_INFO,
    IDDCX_SWAPCHAIN, IDDCX_TRANSMISSION_TYPE, IDDCX_XOR_CURSOR_SUPPORT, LUID, NTSTATUS, WDFDEVICE,
    WDFOBJECT, WDF_OBJECT_ATTRIBUTES,
};
use windows::{
    core::{s, w, GUID},
    Win32::{Foundation::TRUE, System::Threading::CreateEventA},
};

use crate::{
    direct_3d_device::Direct3DDevice,
    edid::Edid,
    ipc::{startup, MONITOR_MODES},
    swap_chain_processor::SwapChainProcessor,
};

// Maximum amount of monitors that can be connected
pub const MAX_MONITORS: u8 = 16;

pub struct DeviceContext {
    device: WDFDEVICE,
    adapter: Option<IDDCX_ADAPTER>,
}

// SAFETY: Raw ptr is managed by external library
unsafe impl Send for DeviceContext {}
unsafe impl Sync for DeviceContext {}

// for now, `device` is hardcoded into the macro, so it needs to be there even if unused
#[allow(unused)]
pub struct MonitorContext {
    device: IDDCX_MONITOR,
    swap_chain_processor: Option<SwapChainProcessor>,
    cursor_event: Option<OwnedHandle>,
}

// SAFETY: Raw ptr is managed by external library
unsafe impl Send for MonitorContext {}
unsafe impl Sync for MonitorContext {}

WDF_DECLARE_CONTEXT_TYPE!(pub DeviceContext);

// WDF zeroes context storage before initialization. Keep the Arc uninitialized
// until init succeeds so deletion of a partially created monitor is also safe.
#[repr(C)]
struct MonitorContextStorage {
    locked: AtomicBool,
    initialized: UnsafeCell<bool>,
    context: UnsafeCell<MaybeUninit<Arc<RwLock<MonitorContext>>>>,
}

// The zero-initialized lock serializes context publication, cloning and removal.
unsafe impl Sync for MonitorContextStorage {}

struct MonitorContextStorageGuard<'a>(&'a AtomicBool);
impl Drop for MonitorContextStorageGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl MonitorContextStorage {
    fn lock(&self) -> MonitorContextStorageGuard<'_> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
        MonitorContextStorageGuard(&self.locked)
    }

    fn initialize(&self, context: MonitorContext) -> Result<(), WdfError> {
        let context = Arc::new(RwLock::new(context));
        let _guard = self.lock();
        // SAFETY: All reads and writes to these cells hold the storage lock.
        if unsafe { *self.initialized.get() } {
            return Err(WdfError::Unknown);
        }
        unsafe { (*self.context.get()).write(context) };
        unsafe { *self.initialized.get() = true };
        Ok(())
    }

    fn get(&self) -> Result<Arc<RwLock<MonitorContext>>, WdfError> {
        let _guard = self.lock();
        // SAFETY: Cleanup cannot remove the Arc while this lock is held.
        if !unsafe { *self.initialized.get() } {
            return Err(WdfError::UpgradeFailed);
        }
        // SAFETY: Only initialize sets the flag, after writing the Arc.
        let context = unsafe { &*self.context.get() };
        Ok(unsafe { context.assume_init_ref() }.clone())
    }

    fn clear(&self) {
        let context = {
            let _guard = self.lock();
            // SAFETY: The lock protects the flag and the initialized Arc.
            if !unsafe { *self.initialized.get() } {
                return;
            }
            // Clear first, so cleanup cannot drop the same Arc twice.
            unsafe { *self.initialized.get() = false };
            let context = unsafe { &*self.context.get() };
            Some(unsafe { context.assume_init_read() })
        };
        // Context destruction can join a thread or reenter WDF. Do it unlocked.
        drop(context);
    }
}

#[repr(transparent)]
struct MonitorContextTypeInfo(UnsafeCell<wdf_umdf_sys::_WDF_OBJECT_CONTEXT_TYPE_INFO>);
// WDF receives the same stable metadata pointer throughout the driver's lifetime.
unsafe impl Sync for MonitorContextTypeInfo {}

static MONITOR_CONTEXT_TYPE_INFO: MonitorContextTypeInfo = MonitorContextTypeInfo(UnsafeCell::new(
    wdf_umdf_sys::_WDF_OBJECT_CONTEXT_TYPE_INFO {
        #[allow(clippy::cast_possible_truncation)]
        Size: size_of::<wdf_umdf_sys::_WDF_OBJECT_CONTEXT_TYPE_INFO>() as u32,
        ContextName: c"MonitorContext".as_ptr(),
        ContextSize: size_of::<MonitorContextStorage>(),
        UniqueType: std::ptr::from_ref(&MONITOR_CONTEXT_TYPE_INFO).cast(),
        EvtDriverGetUniqueContextType: None,
    },
));

unsafe extern "C-unwind" fn monitor_context_cleanup(object: WDFOBJECT) {
    if let Err(error) = unsafe { MonitorContext::drop(object) } {
        error!("Failed to clean up monitor context: {error:?}");
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("Failed to parse integer: {0:?}")]
    ParseInt(#[from] ParseIntError),
    #[error("Failed to convert integer: {0:?}")]
    TryFromInt(#[from] TryFromIntError),
    #[error("Failed to convert to NTSTATUS: {0:?}")]
    Ntstatus(#[from] NTSTATUS),
    #[error("Failed to convert to IddCxError: {0:?}")]
    IddCx(#[from] IddCxError),
    #[error("Failed to convert to WdfError: {0:?}")]
    Wdf(#[from] WdfError),
    #[error("Windows Error: {0:?}")]
    Win(#[from] windows::core::Error),
    #[error("{0:?}")]
    Other(#[from] anyhow::Error),
}

impl DeviceContext {
    pub fn new(device: WDFDEVICE) -> Self {
        Self {
            device,
            adapter: None,
        }
    }

    pub fn init_adapter(&mut self) -> Result<(), ContextError> {
        let mut version = IDDCX_ENDPOINT_VERSION {
            #[allow(clippy::cast_possible_truncation)]
            Size: size_of::<IDDCX_ENDPOINT_VERSION>() as u32,

            MajorVer: env!("CARGO_PKG_VERSION_MAJOR").parse::<u32>()?,
            MinorVer: env!("CARGO_PKG_VERSION_MINOR").parse::<u32>()?,
            Build: env!("CARGO_PKG_VERSION_PATCH").parse::<u32>()?,
            ..Default::default()
        };

        let mut adapter_caps = IDDCX_ADAPTER_CAPS {
            #[allow(clippy::cast_possible_truncation)]
            Size: size_of::<IDDCX_ADAPTER_CAPS>() as u32,

            MaxMonitorsSupported: u32::from(MAX_MONITORS),

            EndPointDiagnostics: IDDCX_ENDPOINT_DIAGNOSTIC_INFO {
                #[allow(clippy::cast_possible_truncation)]
                Size: size_of::<IDDCX_ENDPOINT_DIAGNOSTIC_INFO>() as u32,
                GammaSupport: IDDCX_FEATURE_IMPLEMENTATION::IDDCX_FEATURE_IMPLEMENTATION_NONE,
                TransmissionType: IDDCX_TRANSMISSION_TYPE::IDDCX_TRANSMISSION_TYPE_WIRED_OTHER,

                pEndPointFriendlyName: w!("Virtual Display Driver Adapter").as_ptr(),
                pEndPointManufacturerName: w!("Cherry").as_ptr(),
                pEndPointModelName: w!("Pro").as_ptr(),

                pFirmwareVersion: addr_of_mut!(version).cast(),
                pHardwareVersion: addr_of_mut!(version).cast(),
            },

            ..Default::default()
        };

        let mut attr = WDF_OBJECT_ATTRIBUTES::init_context_type(unsafe { Self::get_type_info() });

        let adapter_init = IDARG_IN_ADAPTER_INIT {
            // this is WdfDevice because that's what we set last
            WdfDevice: self.device,
            pCaps: addr_of_mut!(adapter_caps).cast(),
            ObjectAttributes: addr_of_mut!(attr).cast(),
        };

        let mut adapter_init_out = IDARG_OUT_ADAPTER_INIT::default();
        unsafe { IddCxAdapterInitAsync(&adapter_init, &mut adapter_init_out)? };

        self.adapter = Some(adapter_init_out.AdapterObject);

        unsafe { self.clone_into(adapter_init_out.AdapterObject as WDFOBJECT)? };

        Ok(())
    }

    pub fn finish_init() -> NTSTATUS {
        // start the socket listener to listen for messages from the client
        startup();

        NTSTATUS::STATUS_SUCCESS
    }

    pub fn create_monitor(&mut self, index: u32) -> Result<(), ContextError> {
        let mut attr = MonitorContext::object_attributes();

        // Copy only the preferred tuple; IddCx may reenter MONITOR_MODES below.
        let preferred = {
            let lock = MONITOR_MODES
                .lock()
                .map_err(|_| anyhow!("Failed to lock mutex"))?;
            lock.iter()
                .find(|monitor| monitor.data.id == index)
                .and_then(|monitor| monitor.data.modes.first())
                .and_then(|mode| {
                    mode.refresh_rates
                        .first()
                        .map(|refresh| (mode.width, mode.height, *refresh))
                })
        };
        // Retain the serial identity and every advertised mode/rate, even when
        // the first tuple cannot fit a preferred EDID detailed timing descriptor.
        let mut edid = preferred.map_or_else(
            || Edid::generate_with(index),
            |(width, height, refresh)| Edid::generate_for_mode(index, width, height, refresh),
        );

        let mut monitor_info = IDDCX_MONITOR_INFO {
            #[allow(clippy::cast_possible_truncation)]
            Size: size_of::<IDDCX_MONITOR_INFO>() as u32,
            // SAFETY: windows-rs + generated _GUID types are same size, with same fields, and repr C
            // see: https://microsoft.github.io/windows-docs-rs/doc/windows/core/struct.GUID.html
            // and: wmdf_umdf_sys::_GUID
            MonitorContainerId: unsafe {
                mem::transmute::<GUID, wdf_umdf_sys::_GUID>(GUID::new()?)
            },
            MonitorType:
                DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY::DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,

            ConnectorIndex: index,
            MonitorDescription: IDDCX_MONITOR_DESCRIPTION {
                #[allow(clippy::cast_possible_truncation)]
                Size: size_of::<IDDCX_MONITOR_DESCRIPTION>() as u32,

                Type: IDDCX_MONITOR_DESCRIPTION_TYPE::IDDCX_MONITOR_DESCRIPTION_TYPE_EDID,

                #[allow(clippy::cast_possible_truncation)]
                DataSize: edid.len() as u32,

                pData: edid.as_mut_ptr().cast(),
            },
        };

        let monitor_create = IDARG_IN_MONITORCREATE {
            ObjectAttributes: &mut attr,
            pMonitorInfo: &mut monitor_info,
        };

        let mut monitor_create_out = IDARG_OUT_MONITORCREATE::default();
        unsafe {
            IddCxMonitorCreate(
                self.adapter.ok_or(anyhow!("Failed to get adapter"))?,
                &monitor_create,
                &mut monitor_create_out,
            )?
        };

        let object = NonNull::new(monitor_create_out.MonitorObject)
            .ok_or(anyhow!("MonitorObject was null"))?;
        let result = (|| {
            unsafe {
                let context = MonitorContext::new(object.as_ptr());
                context.init(object.as_ptr() as WDFOBJECT)?;
            }

            // Publish before arrival because IddCx may query its modes during arrival.
            let mut lock = MONITOR_MODES
                .lock()
                .map_err(|_| anyhow!("Failed to lock mutex"))?;
            let monitor = lock
                .iter_mut()
                .find(|monitor| monitor.data.id == index)
                .ok_or(anyhow!("Monitor was removed before arrival"))?;
            if monitor.object.is_some() {
                return Err(anyhow!("Monitor already has an object").into());
            }
            monitor.object = Some(object);
            drop(lock);

            let mut arrival_out = IDARG_OUT_MONITORARRIVAL::default();
            unsafe { IddCxMonitorArrival(object.as_ptr(), &mut arrival_out)? };
            Ok(())
        })();
        if result.is_err() {
            // Keep accepted monitor data retryable, but never cache an unarrived object.
            if let Ok(mut lock) = MONITOR_MODES.lock() {
                for monitor in &mut *lock {
                    if monitor.object == Some(object) {
                        monitor.object = None;
                    }
                }
            }
            if let Err(error) = unsafe { WdfObjectDelete(object.as_ptr().cast()) } {
                error!("Failed to delete monitor {index} after creation failure: {error:?}");
            }
        }
        result
    }
}

impl MonitorContext {
    pub fn new(device: IDDCX_MONITOR) -> Self {
        Self {
            device,
            swap_chain_processor: None,
            cursor_event: None,
        }
    }

    fn object_attributes() -> WDF_OBJECT_ATTRIBUTES {
        let mut attributes =
            WDF_OBJECT_ATTRIBUTES::init_context_type(unsafe { Self::get_type_info() });
        attributes.EvtCleanupCallback = Some(monitor_context_cleanup);
        attributes
    }

    unsafe fn get_type_info() -> &'static wdf_umdf_sys::_WDF_OBJECT_CONTEXT_TYPE_INFO {
        // SAFETY: The metadata is initialized statically and only shared with WDF.
        unsafe { &*MONITOR_CONTEXT_TYPE_INFO.0.get() }
    }

    unsafe fn storage(object: WDFOBJECT) -> Result<*mut MonitorContextStorage, WdfError> {
        let context_type = unsafe { Self::get_type_info() };
        let context = unsafe { WdfObjectGetTypedContextWorker(object, context_type)? };
        Ok(context.cast())
    }

    pub unsafe fn init(self, object: WDFOBJECT) -> Result<(), WdfError> {
        // SAFETY: WDF allocated and zero-initialized this typed context storage.
        let storage = unsafe { Self::storage(object)? };
        unsafe { &*storage }.initialize(self)
    }

    pub unsafe fn get_mut<F>(object: WDFOBJECT, callback: F) -> Result<(), WdfError>
    where
        F: FnOnce(&mut Self),
    {
        // WDF's object lifetime protects storage while the Arc is being cloned.
        let storage = unsafe { Self::storage(object)? };
        let context = unsafe { &*storage }.get()?;
        let mut context = context.write().map_err(|_| WdfError::LockFailed)?;
        callback(&mut context);
        Ok(())
    }

    pub unsafe fn drop(object: WDFOBJECT) -> Result<(), WdfError> {
        // WDF calls cleanup once; clear also tolerates an uninitialized context.
        let storage = unsafe { Self::storage(object)? };
        unsafe { &*storage }.clear();
        Ok(())
    }

    pub fn assign_swap_chain(
        &mut self,
        swap_chain: IDDCX_SWAPCHAIN,
        render_adapter: LUID,
        new_frame_event: HANDLE,
    ) {
        // drop processing thread
        drop(self.swap_chain_processor.take());

        // transmute would work, but one less unsafe block, so why not
        let luid = windows::Win32::Foundation::LUID {
            LowPart: render_adapter.LowPart,
            HighPart: render_adapter.HighPart,
        };

        let device = Direct3DDevice::init(luid);

        if let Ok(device) = device {
            let mut processor = SwapChainProcessor::new();

            processor.run(swap_chain, device, new_frame_event);

            self.swap_chain_processor = Some(processor);

            self.setup_hw_cursor();
        } else {
            // It's important to delete the swap-chain if D3D initialization fails, so that the OS knows to generate a new
            // swap-chain and try again.

            unsafe {
                let _ = WdfObjectDelete(swap_chain.cast());
            }
        }
    }

    pub fn unassign_swap_chain(&mut self) {
        self.swap_chain_processor.take();
    }

    pub fn setup_hw_cursor(&mut self) {
        let new_event = if self.cursor_event.is_none() {
            let mouse_event = unsafe { CreateEventA(None, false, false, s!("vdd_mouse_event")) };
            let Ok(mouse_event) = mouse_event else {
                error!("CreateEventA failed: {mouse_event:?}");
                return;
            };
            // SAFETY: CreateEventA returned a new owned handle on success.
            Some(unsafe { OwnedHandle::from_raw_handle(mouse_event.0.cast()) })
        } else {
            None
        };
        let mouse_event = self.cursor_event.as_ref().or(new_event.as_ref()).unwrap();

        // setup hardware cursor
        let cursor_info = IDDCX_CURSOR_CAPS {
            #[allow(clippy::cast_possible_truncation)]
            Size: std::mem::size_of::<IDDCX_CURSOR_CAPS>() as u32,
            AlphaCursorSupport: TRUE.0,
            MaxX: 512,
            MaxY: 512,
            ColorXorCursorSupport: IDDCX_XOR_CURSOR_SUPPORT::IDDCX_XOR_CURSOR_SUPPORT_NONE,
        };

        let hw_cursor = IDARG_IN_SETUP_HWCURSOR {
            CursorInfo: cursor_info,
            hNewCursorDataAvailable: mouse_event.as_raw_handle().cast(),
        };

        let res = unsafe { IddCxMonitorSetupHardwareCursor(self.device, &hw_cursor) };
        let Ok(res) = res else {
            error!("IddCxMonitorSetupHardwareCursor() failed: {res:?}");
            return;
        };

        if res.is_warning() {
            warn!("IddCxMonitorSetupHardwareCursor() warn: {res:?}");
        }
        if res.is_error() {
            error!("IddCxMonitorSetupHardwareCursor() failed: {res:?}");
            return;
        }
        if let Some(event) = new_event {
            // Reuse this event after mode commits, which reset cursor support to
            // software. Retain it until monitor cleanup because IddCx borrows it.
            self.cursor_event = Some(event);
        }
    }
}
