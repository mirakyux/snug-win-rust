use std::time::{Duration, Instant};

use driver_logger::DriverLogger;
use log::{error, info, Level};
use wdf_umdf::{
    IddCxDeviceInitConfig, IddCxDeviceInitialize, WdfDeviceCreate,
    WdfDeviceInitSetPnpPowerEventCallbacks, WdfDriverCreate,
};
use wdf_umdf_sys::{
    IDD_CX_CLIENT_CONFIG, NTSTATUS, WDFDEVICE_INIT, WDFDRIVER__, WDFOBJECT, WDF_DRIVER_CONFIG,
    WDF_OBJECT_ATTRIBUTES, WDF_PNPPOWER_EVENT_CALLBACKS, _DRIVER_OBJECT, _UNICODE_STRING,
};

use crate::callbacks::{
    adapter_commit_modes, adapter_init_finished, assign_swap_chain, device_d0_entry,
    monitor_get_default_modes, monitor_query_modes, parse_monitor_description, unassign_swap_chain,
};
use crate::context::DeviceContext;

//
// Our driver's entry point
// See windows::Wdk::System::SystemServices::DRIVER_INITIALIZE
//
#[no_mangle]
extern "C-unwind" fn DriverEntry(
    driver_object: *mut _DRIVER_OBJECT,
    registry_path: *mut _UNICODE_STRING,
) -> NTSTATUS {
    // During system bootup, `RegisterEventSourceW` fails and causes the driver to not bootup
    // Pretty unfortunate, therefore, we will run this on a thread until it succeeds and let the rest of
    // the driver start. I know this is suboptimal considering it's our main code to catch panics.
    //
    // It always starts immediately when the computer is already booted up.
    // If you have a better solution, please by all means open an issue report
    let init_log = || {
        let mut logger = DriverLogger::new(if cfg!(debug_assertions) {
            Level::Debug
        } else {
            Level::Info
        });

        if cfg!(debug_assertions) {
            logger.debug();
        } else if logger.name("VirtualDisplayDriver").is_err() {
            return NTSTATUS::STATUS_UNSUCCESSFUL;
        }

        let status = logger
            .init()
            .map_err(|_| NTSTATUS::STATUS_FAILED_DRIVER_ENTRY)
            .into();

        if status == NTSTATUS::STATUS_SUCCESS {
            info!(
                "Initialized Virtual Display Driver v{} @ {}",
                env!("CARGO_PKG_VERSION"),
                env!("VERGEN_GIT_SHA")
            );
        }

        status
    };

    let status = init_log();

    if !status.is_success() {
        // Okay, let's try another method then
        std::thread::spawn(move || {
            let time_waited = Instant::now();
            // 5 minutes
            let timeout_duration = Duration::from_secs(60 * 5);
            // in ms
            let sleep_for = 500;

            loop {
                let status = init_log();
                if status.is_success() {
                    info!(
                        "Service took {} seconds to start",
                        time_waited.elapsed().as_secs()
                    );
                    break;
                }
                // Logging is optional. Expiry stops retries without invalidating
                // any device; DriverEntry's PDRIVER_OBJECT is not a WDFDEVICE.
                if time_waited.elapsed() >= timeout_duration {
                    break;
                }
                std::thread::sleep(Duration::from_millis(sleep_for));
            }
        });
    }

    // set the panic hook to capture and log panics
    crate::panic::set_hook();

    let mut attributes = WDF_OBJECT_ATTRIBUTES::init();

    let mut config = WDF_DRIVER_CONFIG::init(Some(driver_add));

    unsafe {
        WdfDriverCreate(
            driver_object,
            registry_path,
            Some(&mut attributes),
            &mut config,
            None,
        )
    }
    .into()
}

extern "C-unwind" fn driver_add(
    _driver: *mut WDFDRIVER__,
    mut init: *mut WDFDEVICE_INIT,
) -> NTSTATUS {
    let mut callbacks = WDF_PNPPOWER_EVENT_CALLBACKS::init();

    callbacks.EvtDeviceD0Entry = Some(device_d0_entry);

    unsafe {
        _ = WdfDeviceInitSetPnpPowerEventCallbacks(init, &mut callbacks);
    }

    let Some(mut config) = IDD_CX_CLIENT_CONFIG::init() else {
        error!("Failed to create IDD_CX_CLIENT_CONFIG");
        return NTSTATUS::STATUS_NOT_FOUND;
    };

    config.EvtIddCxAdapterInitFinished = Some(adapter_init_finished);

    config.EvtIddCxParseMonitorDescription = Some(parse_monitor_description);
    config.EvtIddCxMonitorGetDefaultDescriptionModes = Some(monitor_get_default_modes);
    config.EvtIddCxMonitorQueryTargetModes = Some(monitor_query_modes);
    config.EvtIddCxAdapterCommitModes = Some(adapter_commit_modes);
    config.EvtIddCxMonitorAssignSwapChain = Some(assign_swap_chain);
    config.EvtIddCxMonitorUnassignSwapChain = Some(unassign_swap_chain);

    let init_data = unsafe { &mut *init };
    let status = unsafe { IddCxDeviceInitConfig(init_data, &config) };
    if let Err(e) = status {
        error!("Failed to init iddcx config: {e:?}");
        return e.into();
    }

    let mut attributes =
        WDF_OBJECT_ATTRIBUTES::init_context_type(unsafe { DeviceContext::get_type_info() });

    attributes.EvtCleanupCallback = Some(event_cleanup);

    let mut device = std::ptr::null_mut();

    let status = unsafe { WdfDeviceCreate(&mut init, Some(&mut attributes), &mut device) };
    if let Err(e) = status {
        error!("Failed to create device: {e:?}");
        return e.into();
    }

    let status = unsafe { IddCxDeviceInitialize(device) };
    if let Err(e) = status {
        error!("Failed to init iddcx device: {e:?}");
        return e.into();
    }

    let context = DeviceContext::new(device);

    unsafe { context.init(device as WDFOBJECT).into() }
}

unsafe extern "C-unwind" fn event_cleanup(wdf_object: WDFOBJECT) {
    _ = unsafe { DeviceContext::drop(wdf_object) };
}
