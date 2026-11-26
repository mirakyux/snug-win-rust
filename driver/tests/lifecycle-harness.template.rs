#![deny(warnings)]
#![allow(
    non_snake_case,
    non_camel_case_types,
    dead_code,
    private_interfaces,
    improper_ctypes_definitions
)]
use std::{
    collections::{BTreeMap, BTreeSet},
    mem::{self, size_of, MaybeUninit},
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        LazyLock, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};
macro_rules! error { ($($t:tt)*) => { let _ = format_args!($($t)*); } }
macro_rules! warn { ($($t:tt)*) => { let _ = format_args!($($t)*); } }
macro_rules! anyhow {
    ($($t:tt)*) => {
        "stub context error"
    };
}
type ContextError = &'static str;
type WDFOBJECT = *mut u8;
type IDDCX_MONITOR__ = u8;
type IDDCX_ADAPTER = *mut u8;
type WDF_OBJECT_ATTRIBUTES = u8;
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mode {
    width: u32,
    height: u32,
    refresh_rates: Vec<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Monitor {
    id: u32,
    name: Option<String>,
    enabled: bool,
    modes: Vec<Mode>,
}
#[derive(Debug)]
struct MonitorObject {
    object: Option<NonNull<IDDCX_MONITOR__>>,
    data: Monitor,
}
unsafe impl Send for MonitorObject {}
unsafe impl Sync for MonitorObject {}
static MONITOR_MODES: LazyLock<Mutex<Vec<MonitorObject>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
static MONITOR_MUTATIONS: Mutex<()> = Mutex::new(());
static ARRIVAL_FAIL: AtomicBool = AtomicBool::new(false);
static ARRIVAL_FAIL_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static ARRIVAL_ATTEMPTS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static INIT_FAIL: AtomicBool = AtomicBool::new(false);
static DEPARTURE_FAIL: AtomicBool = AtomicBool::new(false);
static DEPARTURE_FAIL_OBJECT: AtomicUsize = AtomicUsize::new(0);
static DEPARTURE_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
static DELETIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static LIVE: LazyLock<Mutex<BTreeSet<usize>>> = LazyLock::new(|| Mutex::new(BTreeSet::new()));
struct DeviceContext {
    adapter: Option<IDDCX_ADAPTER>,
}
struct AdapterObject(NonNull<u8>);
unsafe impl Send for AdapterObject {}
unsafe impl Sync for AdapterObject {}
static ADAPTER: OnceLock<AdapterObject> = OnceLock::new();
impl DeviceContext {
    unsafe fn get_mut<F>(_: *mut u8, callback: F) -> Result<(), ContextError>
    where
        F: FnOnce(&mut Self),
    {
        callback(&mut Self {
            adapter: Some(NonNull::<u8>::dangling().as_ptr()),
        });
        Ok(())
    }
}
#[derive(Default)]
struct Sender<T>(Mutex<Vec<T>>);
impl<T> Sender<T> {
    fn send(&self, message: T) -> Result<(), ()> {
        self.0.lock().unwrap().push(message);
        Ok(())
    }
}
struct MonitorContext;
impl MonitorContext {
    fn object_attributes() -> WDF_OBJECT_ATTRIBUTES {
        0
    }
    fn new(_: *mut u8) -> Self {
        Self
    }
    unsafe fn init(self, _: WDFOBJECT) -> Result<(), ContextError> {
        if INIT_FAIL.load(Ordering::SeqCst) {
            Err("injected init failure")
        } else {
            Ok(())
        }
    }
}
mod edid {
    // PRODUCTION_EDID
}
use edid::Edid;
static EDID_CAPTURES: Mutex<Vec<(u32, Vec<u8>)>> = Mutex::new(Vec::new());
#[repr(C)]
struct GUID([u8; 16]);
impl GUID {
    fn new() -> Result<Self, ContextError> {
        Ok(Self([0; 16]))
    }
}
mod wdf_umdf_sys {
    #[repr(C)]
    pub struct _GUID(pub [u8; 16]);
}
enum DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY {
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,
}
enum IDDCX_MONITOR_DESCRIPTION_TYPE {
    IDDCX_MONITOR_DESCRIPTION_TYPE_EDID,
}
struct IDDCX_MONITOR_DESCRIPTION {
    Size: u32,
    Type: IDDCX_MONITOR_DESCRIPTION_TYPE,
    DataSize: u32,
    pData: *mut u8,
}
struct IDDCX_MONITOR_INFO {
    Size: u32,
    MonitorContainerId: wdf_umdf_sys::_GUID,
    MonitorType: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY,
    ConnectorIndex: u32,
    MonitorDescription: IDDCX_MONITOR_DESCRIPTION,
}
struct IDARG_IN_MONITORCREATE {
    ObjectAttributes: *mut WDF_OBJECT_ATTRIBUTES,
    pMonitorInfo: *mut IDDCX_MONITOR_INFO,
}
struct IDARG_OUT_MONITORCREATE {
    MonitorObject: *mut u8,
}
impl Default for IDARG_OUT_MONITORCREATE {
    fn default() -> Self {
        Self {
            MonitorObject: std::ptr::null_mut(),
        }
    }
}
#[derive(Default)]
struct IDARG_OUT_MONITORARRIVAL;
unsafe fn IddCxMonitorCreate(
    _: *mut u8,
    input: &IDARG_IN_MONITORCREATE,
    out: &mut IDARG_OUT_MONITORCREATE,
) -> Result<(), ContextError> {
    assert!(
        MONITOR_MODES.try_lock().is_ok(),
        "native create must run unlocked"
    );
    let info = &*input.pMonitorInfo;
    let description = &info.MonitorDescription;
    let bytes = std::slice::from_raw_parts(description.pData, description.DataSize as usize);
    assert_eq!(Edid::get_serial(bytes).unwrap(), info.ConnectorIndex);
    EDID_CAPTURES
        .lock()
        .unwrap()
        .push((info.ConnectorIndex, bytes.to_vec()));
    out.MonitorObject = Box::into_raw(Box::new((*input.pMonitorInfo).ConnectorIndex as u8));
    Ok(())
}
unsafe fn WdfObjectDelete(object: *mut u8) -> Result<(), ContextError> {
    DELETIONS.fetch_add(1, Ordering::SeqCst);
    drop(Box::from_raw(object));
    Ok(())
}
unsafe fn IddCxMonitorArrival(
    object: *mut u8,
    _: &mut IDARG_OUT_MONITORARRIVAL,
) -> Result<(), ContextError> {
    assert!(
        MONITOR_MODES.try_lock().is_ok(),
        "native arrival must run unlocked"
    );
    let id = u32::from(*object);
    ARRIVAL_ATTEMPTS.lock().unwrap().push(id);
    if ARRIVAL_FAIL.load(Ordering::SeqCst)
        && (ARRIVAL_FAIL_ID.load(Ordering::SeqCst) == usize::MAX
            || ARRIVAL_FAIL_ID.load(Ordering::SeqCst) == id as usize)
    {
        Err("injected arrival failure")
    } else {
        LIVE.lock().unwrap().insert(object as usize);
        Ok(())
    }
}
unsafe fn IddCxMonitorDeparture(object: &mut u8) -> Result<(), ContextError> {
    DEPARTURE_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
    let address = object as *mut u8 as usize;
    if DEPARTURE_FAIL.load(Ordering::SeqCst)
        && (DEPARTURE_FAIL_OBJECT.load(Ordering::SeqCst) == 0
            || DEPARTURE_FAIL_OBJECT.load(Ordering::SeqCst) == address)
    {
        Err("injected departure failure")
    } else {
        LIVE.lock().unwrap().remove(&address);
        Ok(())
    }
}
#[derive(Clone, Copy, Default)]
struct DISPLAYCONFIG_RATIONAL {
    Numerator: u32,
    Denominator: u32,
}
#[derive(Clone, Copy, Default)]
struct DISPLAYCONFIG_2DREGION {
    cx: u32,
    cy: u32,
}
#[derive(Clone, Copy, Default)]
enum DISPLAYCONFIG_SCANLINE_ORDERING {
    #[default]
    DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct __BindgenBitfieldUnit<T>(T);
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1__bindgen_ty_1([u8; 4]);
impl DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1__bindgen_ty_1 {
    fn new_bitfield_1(_: u32, _: u32, _: u32) -> __BindgenBitfieldUnit<[u8; 4]> {
        __BindgenBitfieldUnit([0; 4])
    }
}
#[derive(Clone, Copy, Default)]
struct DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1 {
    AdditionalSignalInfo: DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1__bindgen_ty_1,
}
#[derive(Clone, Copy, Default)]
struct DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
    pixelRate: u64,
    hSyncFreq: DISPLAYCONFIG_RATIONAL,
    vSyncFreq: DISPLAYCONFIG_RATIONAL,
    activeSize: DISPLAYCONFIG_2DREGION,
    totalSize: DISPLAYCONFIG_2DREGION,
    __bindgen_anon_1: DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1,
    scanLineOrdering: DISPLAYCONFIG_SCANLINE_ORDERING,
}
#[derive(Clone, Copy, Default)]
struct DISPLAYCONFIG_TARGET_MODE {
    targetVideoSignalInfo: DISPLAYCONFIG_VIDEO_SIGNAL_INFO,
}
#[derive(Clone, Copy, Default)]
struct IDDCX_TARGET_MODE {
    Size: u32,
    TargetVideoSignalInfo: DISPLAYCONFIG_TARGET_MODE,
}
#[derive(Clone, Copy, Default)]
enum IDDCX_MONITOR_MODE_ORIGIN {
    #[default]
    IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR,
}
#[derive(Clone, Copy, Default)]
struct IDDCX_MONITOR_MODE {
    Size: u32,
    Origin: IDDCX_MONITOR_MODE_ORIGIN,
    MonitorVideoSignalInfo: DISPLAYCONFIG_VIDEO_SIGNAL_INFO,
}
struct IDARG_IN_PARSEMONITORDESCRIPTION {
    MonitorDescription: IDDCX_MONITOR_DESCRIPTION,
    MonitorModeBufferInputCount: u32,
    pMonitorModes: *mut IDDCX_MONITOR_MODE,
}
#[derive(Default)]
struct IDARG_OUT_PARSEMONITORDESCRIPTION {
    MonitorModeBufferOutputCount: u32,
    PreferredMonitorModeIdx: u32,
}
struct IDARG_IN_QUERYTARGETMODES {
    TargetModeBufferInputCount: u32,
    pTargetModes: *mut IDDCX_TARGET_MODE,
}
#[derive(Default)]
struct IDARG_OUT_QUERYTARGETMODES {
    TargetModeBufferOutputCount: u32,
}
#[derive(PartialEq, Eq, Debug)]
enum NTSTATUS {
    STATUS_SUCCESS,
    STATUS_BUFFER_TOO_SMALL,
    STATUS_DRIVER_INTERNAL_ERROR,
    STATUS_INVALID_VIEW_SIZE,
}
type Dimen = u32;
type RefreshRate = u32;
// PRODUCTION_ITEMS

fn data() -> Monitor {
    Monitor {
        id: 1,
        name: None,
        enabled: true,
        modes: vec![Mode {
            width: 1920,
            height: 1080,
            refresh_rates: vec![60, 120],
        }],
    }
}
fn reset() -> DeviceContext {
    EDID_CAPTURES.clear_poison();
    EDID_CAPTURES.lock().unwrap().clear();
    MONITOR_RESERVATIONS.clear_poison();
    MONITOR_RESERVATIONS.lock().unwrap().clear();
    MONITOR_MODES.clear_poison();
    LIVE.clear_poison();
    MONITOR_MUTATIONS.clear_poison();
    MONITOR_MODES.lock().unwrap().clear();
    LIVE.lock().unwrap().clear();
    DELETIONS.store(0, Ordering::SeqCst);
    ARRIVAL_FAIL.store(false, Ordering::SeqCst);
    ARRIVAL_FAIL_ID.store(usize::MAX, Ordering::SeqCst);
    ARRIVAL_ATTEMPTS.clear_poison();
    ARRIVAL_ATTEMPTS.lock().unwrap().clear();
    INIT_FAIL.store(false, Ordering::SeqCst);
    DEPARTURE_FAIL.store(false, Ordering::SeqCst);
    DEPARTURE_FAIL_OBJECT.store(0, Ordering::SeqCst);
    DEPARTURE_ATTEMPTS.store(0, Ordering::SeqCst);
    ADAPTER.get_or_init(|| AdapterObject(NonNull::<u8>::dangling()));
    MONITOR_MODES.lock().unwrap().push(MonitorObject {
        object: None,
        data: data(),
    });
    DeviceContext {
        adapter: Some(NonNull::<u8>::dangling().as_ptr()),
    }
}

// EDID_REGRESSIONS

const OWNER: &str = "0123456789abcdef0123456789abcdef";
const OTHER_OWNER: &str = "fedcba9876543210fedcba9876543210";

fn reset_owned() {
    reset();
    MONITOR_MODES.lock().unwrap().clear();
}

fn reservations() -> Vec<(u32, String, Instant)> {
    MONITOR_RESERVATIONS
        .lock()
        .unwrap()
        .iter()
        .map(|(id, reservation)| (*id, reservation.owner.clone(), reservation.expires_at))
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct MutationSnapshot {
    monitors: Vec<Monitor>,
    reservations: Vec<(u32, String, Instant)>,
    objects: Vec<(u32, Option<usize>)>,
    arrivals: Vec<u32>,
    departures: usize,
    live: BTreeSet<usize>,
    deletions: usize,
}

fn mutation_snapshot() -> MutationSnapshot {
    MutationSnapshot {
        monitors: monitor_state(),
        reservations: reservations(),
        objects: MONITOR_MODES
            .lock()
            .unwrap()
            .iter()
            .map(|monitor| {
                (
                    monitor.data.id,
                    monitor.object.map(|object| object.as_ptr() as usize),
                )
            })
            .collect(),
        arrivals: ARRIVAL_ATTEMPTS.lock().unwrap().clone(),
        departures: DEPARTURE_ATTEMPTS.load(Ordering::SeqCst),
        live: LIVE.lock().unwrap().clone(),
        deletions: DELETIONS.load(Ordering::SeqCst),
    }
}

fn owned_and_legacy_monitors() -> Monitor {
    reset_owned();
    assert_eq!(snug_reserve(OWNER, 1), SnugUpdateResult::Applied);
    assert_eq!(snug_create(OWNER, vec![data()]), SnugUpdateResult::Applied);
    let mut legacy = data();
    legacy.id = 2;
    snug_update(vec![legacy.clone()], false).unwrap();
    legacy
}

#[test]
fn ownership_check_requires_creation_after_a_fresh_reservation_and_stays_read_only() {
    reset_owned();
    assert_eq!(snug_reserve(OWNER, 1), SnugUpdateResult::Applied);
    let reserved = mutation_snapshot();
    assert!(reserved.monitors.is_empty());
    assert_eq!(reserved.reservations.len(), 1);
    assert!(reserved.reservations[0].2 > Instant::now());
    assert_eq!(snug_check(OWNER, 1), SnugUpdateResult::NotFound);
    assert_eq!(mutation_snapshot(), reserved);

    assert_eq!(snug_create(OWNER, vec![data()]), SnugUpdateResult::Applied);
    let created = mutation_snapshot();
    assert_eq!(created.monitors, vec![data()]);
    assert_eq!(snug_check(OWNER, 1), SnugUpdateResult::Applied);
    assert_eq!(mutation_snapshot(), created);
}

#[test]
fn ownership_check_is_read_only_for_owned_foreign_absent_and_empty_ids() {
    reset_owned();
    assert_eq!(snug_reserve(OWNER, 1), SnugUpdateResult::Applied);
    assert_eq!(snug_create(OWNER, vec![data()]), SnugUpdateResult::Applied);
    assert_eq!(snug_reserve(OTHER_OWNER, 2), SnugUpdateResult::Applied);
    // A probe must neither refresh an accepted lease nor expire a pending one.
    for reservation in MONITOR_RESERVATIONS.lock().unwrap().values_mut() {
        reservation.expires_at = Instant::now() - Duration::from_secs(1);
    }
    let before_reservations = reservations();
    let before_monitors = monitor_state();
    let before_objects: Vec<_> = MONITOR_MODES
        .lock()
        .unwrap()
        .iter()
        .map(|monitor| monitor.object)
        .collect();
    let before_arrivals = ARRIVAL_ATTEMPTS.lock().unwrap().clone();
    let before_live = LIVE.lock().unwrap().clone();
    let before_deletions = DELETIONS.load(Ordering::SeqCst);

    for _ in 0..2 {
        assert_eq!(snug_check(OWNER, 1), SnugUpdateResult::Applied);
        assert_eq!(snug_check(OTHER_OWNER, 1), SnugUpdateResult::NotFound);
        assert_eq!(snug_check(OTHER_OWNER, 2), SnugUpdateResult::NotFound);
        assert_eq!(snug_check(OWNER, 3), SnugUpdateResult::NotFound);
    }

    assert_eq!(reservations(), before_reservations);
    assert_eq!(monitor_state(), before_monitors);
    assert_eq!(
        MONITOR_MODES
            .lock()
            .unwrap()
            .iter()
            .map(|monitor| monitor.object)
            .collect::<Vec<_>>(),
        before_objects
    );
    assert_eq!(*ARRIVAL_ATTEMPTS.lock().unwrap(), before_arrivals);
    assert_eq!(*LIVE.lock().unwrap(), before_live);
    assert_eq!(DELETIONS.load(Ordering::SeqCst), before_deletions);
}

#[test]
fn ownership_check_accepts_cached_intent_without_retrying_native_arrival() {
    reset_owned();
    snug_reserve(OWNER, 1);
    ARRIVAL_FAIL.store(true, Ordering::SeqCst);
    assert_eq!(snug_create(OWNER, vec![data()]), SnugUpdateResult::Applied);
    assert!(MONITOR_MODES.lock().unwrap()[0].object.is_none());
    let before_arrivals = ARRIVAL_ATTEMPTS.lock().unwrap().clone();
    let before_reservations = reservations();
    ARRIVAL_FAIL.store(false, Ordering::SeqCst);
    assert_eq!(snug_check(OWNER, 1), SnugUpdateResult::Applied);
    assert!(MONITOR_MODES.lock().unwrap()[0].object.is_none());
    assert!(LIVE.lock().unwrap().is_empty());
    assert_eq!(*ARRIVAL_ATTEMPTS.lock().unwrap(), before_arrivals);
    assert_eq!(reservations(), before_reservations);
}

#[test]
fn ownership_check_rejects_a_cached_monitor_without_a_reservation() {
    reset();
    assert_eq!(snug_check(OWNER, 1), SnugUpdateResult::NotFound);
    assert_eq!(monitor_state(), vec![data()]);
    assert!(MONITOR_RESERVATIONS.lock().unwrap().is_empty());
}

#[test]
fn ownership_check_rejects_invalid_tokens_and_connector_ids_without_reserving() {
    reset_owned();
    for owner in [
        "",
        "short",
        "g123456789abcdef0123456789abcdef",
        "0123456789abcdef0123456789abcdef0",
    ] {
        assert_eq!(snug_check(owner, 1), SnugUpdateResult::InvalidData);
    }
    for id in [16, u32::MAX] {
        assert_eq!(snug_check(OWNER, id), SnugUpdateResult::InvalidData);
    }
    for id in [0, 15] {
        assert_eq!(snug_check(OWNER, id), SnugUpdateResult::NotFound);
    }
    assert!(MONITOR_RESERVATIONS.lock().unwrap().is_empty());
    assert!(monitor_state().is_empty());
}

#[test]
fn ownership_check_waits_for_the_monitor_mutation_and_observes_its_result() {
    use std::sync::mpsc;
    reset_owned();
    snug_reserve(OWNER, 1);
    snug_create(OWNER, vec![data()]);
    let mutation = MONITOR_MUTATIONS.lock().unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let check = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        result_tx.send(snug_check(OWNER, 1)).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        result_rx.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    assert_eq!(snug_release(OWNER, &[1]), SnugUpdateResult::Applied);
    drop(mutation);
    assert_eq!(
        result_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        SnugUpdateResult::NotFound
    );
    check.join().unwrap();
    assert!(monitor_state().is_empty());
    assert!(MONITOR_RESERVATIONS.lock().unwrap().is_empty());
}

#[test]
fn reservation_has_no_topology_side_effect_and_conflict_cannot_overwrite_it() {
    reset_owned();
    assert_eq!(snug_reserve(OWNER, 1), SnugUpdateResult::Applied);
    assert_eq!(snug_reserve(OWNER, 1), SnugUpdateResult::Applied);
    assert!(monitor_state().is_empty());
    assert!(LIVE.lock().unwrap().is_empty());
    assert_eq!(
        snug_reserve(OTHER_OWNER, 1),
        SnugUpdateResult::AlreadyExists
    );
    assert_eq!(
        MONITOR_RESERVATIONS.lock().unwrap().get(&1).unwrap().owner,
        OWNER
    );
}

#[test]
fn create_requires_the_confirmed_reservation_and_keeps_foreign_monitors() {
    reset_owned();
    assert_eq!(snug_create(OWNER, vec![data()]), SnugUpdateResult::NotFound);
    assert_eq!(snug_reserve(OWNER, 1), SnugUpdateResult::Applied);
    assert_eq!(
        snug_create(OTHER_OWNER, vec![data()]),
        SnugUpdateResult::NotFound
    );
    let mut foreign = data();
    foreign.id = 2;
    snug_update(vec![foreign.clone()], false).unwrap();
    assert_eq!(snug_create(OWNER, vec![data()]), SnugUpdateResult::Applied);
    assert_eq!(monitor_state(), vec![foreign.clone(), data()]);
    // The create reply can be lost; a token-guarded release still finds it.
    assert_eq!(snug_release(OWNER, &[1, 2]), SnugUpdateResult::Applied);
    assert_eq!(monitor_state(), vec![foreign]);
}

#[test]
fn release_before_a_delayed_create_prevents_late_resource_creation() {
    reset_owned();
    assert_eq!(snug_reserve(OWNER, 1), SnugUpdateResult::Applied);
    assert_eq!(snug_release(OWNER, &[1]), SnugUpdateResult::NotFound);
    assert_eq!(snug_create(OWNER, vec![data()]), SnugUpdateResult::NotFound);
    assert!(monitor_state().is_empty());
}

#[test]
fn a_lost_conflict_reply_never_grants_ownership_of_the_occupied_id() {
    reset_owned();
    snug_update(vec![data()], false).unwrap();
    assert_eq!(snug_reserve(OWNER, 1), SnugUpdateResult::AlreadyExists);
    assert_eq!(snug_release(OWNER, &[1]), SnugUpdateResult::NotFound);
    assert_eq!(monitor_state(), vec![data()]);
    assert_eq!(LIVE.lock().unwrap().len(), 1);
}

#[test]
fn failed_owned_departure_retains_token_until_a_successful_retry() {
    reset_owned();
    snug_reserve(OWNER, 1);
    snug_create(OWNER, vec![data()]);
    DEPARTURE_FAIL.store(true, Ordering::SeqCst);
    assert_eq!(snug_release(OWNER, &[1]), SnugUpdateResult::ApplyFailed);
    assert_eq!(monitor_state(), vec![data()]);
    assert_eq!(
        MONITOR_RESERVATIONS.lock().unwrap().get(&1).unwrap().owner,
        OWNER
    );
    DEPARTURE_FAIL.store(false, Ordering::SeqCst);
    assert_eq!(snug_release(OWNER, &[1]), SnugUpdateResult::Applied);
    assert!(monitor_state().is_empty());
    assert!(MONITOR_RESERVATIONS.lock().unwrap().is_empty());
}

#[test]
fn guarded_replace_preserves_ownership_and_rejects_foreign_tokens() {
    reset_owned();
    snug_reserve(OWNER, 1);
    snug_create(OWNER, vec![data()]);
    let mut replacement = data();
    replacement.modes[0].width = 2560;
    assert_eq!(
        snug_replace(OTHER_OWNER, vec![replacement.clone()]),
        SnugUpdateResult::NotFound
    );
    assert_eq!(monitor_state(), vec![data()]);
    assert_eq!(
        snug_replace(OWNER, vec![replacement.clone()]),
        SnugUpdateResult::Applied
    );
    assert_eq!(monitor_state(), vec![replacement]);
    assert_eq!(snug_release(OWNER, &[1]), SnugUpdateResult::Applied);
    assert!(monitor_state().is_empty());
}

#[test]
fn guarded_replace_rejects_a_recycled_connector_after_a_successful_owner_check() {
    reset_owned();
    assert_eq!(snug_reserve(OWNER, 1), SnugUpdateResult::Applied);
    assert_eq!(snug_create(OWNER, vec![data()]), SnugUpdateResult::Applied);
    assert_eq!(snug_check(OWNER, 1), SnugUpdateResult::Applied);

    assert_eq!(snug_release(OWNER, &[1]), SnugUpdateResult::Applied);
    assert_eq!(snug_reserve(OTHER_OWNER, 1), SnugUpdateResult::Applied);
    let mut foreign = data();
    foreign.modes[0].width = 2560;
    assert_eq!(
        snug_create(OTHER_OWNER, vec![foreign.clone()]),
        SnugUpdateResult::Applied
    );
    let before_reservations = reservations();
    let before_live = LIVE.lock().unwrap().clone();
    let before_arrivals = ARRIVAL_ATTEMPTS.lock().unwrap().clone();
    let before_deletions = DELETIONS.load(Ordering::SeqCst);

    assert_eq!(
        snug_replace(OWNER, vec![data()]),
        SnugUpdateResult::NotFound
    );
    assert_eq!(monitor_state(), vec![foreign]);
    assert_eq!(reservations(), before_reservations);
    assert_eq!(*LIVE.lock().unwrap(), before_live);
    assert_eq!(*ARRIVAL_ATTEMPTS.lock().unwrap(), before_arrivals);
    assert_eq!(DELETIONS.load(Ordering::SeqCst), before_deletions);
}

#[test]
fn guarded_replace_failure_keeps_token_for_rollback_and_cleanup() {
    reset_owned();
    snug_reserve(OWNER, 1);
    snug_create(OWNER, vec![data()]);
    ARRIVAL_FAIL.store(true, Ordering::SeqCst);
    let mut replacement = data();
    replacement.modes[0].width = 2560;
    assert_eq!(
        snug_replace(OWNER, vec![replacement]),
        SnugUpdateResult::ApplyFailed
    );
    assert_eq!(
        MONITOR_RESERVATIONS.lock().unwrap().get(&1).unwrap().owner,
        OWNER
    );
    ARRIVAL_FAIL.store(false, Ordering::SeqCst);
    assert_eq!(snug_replace(OWNER, vec![data()]), SnugUpdateResult::Applied);
    assert_eq!(snug_release(OWNER, &[1]), SnugUpdateResult::Applied);
    assert!(monitor_state().is_empty());
}

#[test]
fn legacy_remove_rejects_the_whole_batch_when_any_id_is_owned() {
    owned_and_legacy_monitors();
    let before = mutation_snapshot();
    let tx = Sender::default();
    mutate_monitors(1, &tx, || remove(&[2, 1]));
    assert_eq!(mutation_snapshot(), before);
    assert!(tx.0.lock().unwrap().is_empty());
}

#[test]
fn legacy_notify_rejects_the_whole_replacement_when_an_id_is_owned() {
    let mut legacy = owned_and_legacy_monitors();
    let before = mutation_snapshot();
    legacy.modes[0].width = 3840;
    let mut replacement = data();
    replacement.modes[0].width = 2560;
    let tx = Sender::default();
    mutate_monitors(1, &tx, || notify(vec![legacy, replacement]));
    assert_eq!(mutation_snapshot(), before);
    assert!(tx.0.lock().unwrap().is_empty());
}

#[test]
fn legacy_notify_cannot_omit_an_owned_monitor_or_retry_its_pending_arrival() {
    reset_owned();
    snug_reserve(OWNER, 1);
    ARRIVAL_FAIL.store(true, Ordering::SeqCst);
    snug_create(OWNER, vec![data()]);
    ARRIVAL_FAIL.store(false, Ordering::SeqCst);
    let before = mutation_snapshot();
    let tx = Sender::default();
    mutate_monitors(1, &tx, || notify(vec![data()]));
    assert_eq!(mutation_snapshot(), before);
    mutate_monitors(1, &tx, || notify(vec![]));
    assert_eq!(mutation_snapshot(), before);
    assert!(tx.0.lock().unwrap().is_empty());
}

#[test]
fn legacy_remove_all_preserves_owned_and_unowned_monitors_without_native_calls() {
    owned_and_legacy_monitors();
    let before = mutation_snapshot();
    let tx = Sender::default();
    mutate_monitors(1, &tx, remove_all);
    assert_eq!(mutation_snapshot(), before);
    assert!(tx.0.lock().unwrap().is_empty());
}

#[test]
fn legacy_incremental_replace_rejects_a_batch_including_any_owned_id() {
    let mut legacy = owned_and_legacy_monitors();
    let before = mutation_snapshot();
    legacy.modes[0].width = 3840;
    let mut replacement = data();
    replacement.modes[0].width = 2560;
    let tx = Sender::default();
    assert_eq!(
        mutate_monitors(1, &tx, || snug_update(vec![legacy, replacement], true)),
        Err(SnugUpdateResult::NotFound)
    );
    assert_eq!(mutation_snapshot(), before);
    assert!(tx.0.lock().unwrap().is_empty());
}

#[test]
fn legacy_writes_cannot_consume_cancel_or_refresh_an_empty_reservation() {
    for operation in 0..5 {
        reset_owned();
        snug_reserve(OWNER, 1);
        let before = mutation_snapshot();
        let tx = Sender::default();
        mutate_monitors(1, &tx, || match operation {
            0 => notify(vec![data()]),
            1 => remove(&[1]),
            2 => remove_all(),
            3 => assert_eq!(
                snug_update(vec![data()], false),
                Err(SnugUpdateResult::NotFound)
            ),
            4 => assert_eq!(
                snug_update(vec![data()], true),
                Err(SnugUpdateResult::NotFound)
            ),
            _ => unreachable!(),
        });
        assert_eq!(mutation_snapshot(), before, "operation {operation}");
        assert!(tx.0.lock().unwrap().is_empty());
        assert_eq!(snug_create(OWNER, vec![data()]), SnugUpdateResult::Applied);
    }
}

#[test]
fn legacy_unowned_state_can_be_cleared_restored_updated_and_removed() {
    reset_owned();
    let original = vec![data()];
    let tx = Sender::default();
    mutate_monitors(1, &tx, || notify(original.clone()));
    mutate_monitors(1, &tx, || notify(vec![]));
    assert!(monitor_state().is_empty());
    mutate_monitors(1, &tx, || notify(original.clone()));
    assert_eq!(monitor_state(), original);
    let mut updated = data();
    updated.modes[0].width = 2560;
    mutate_monitors(1, &tx, || snug_update(vec![updated.clone()], true)).unwrap();
    assert_eq!(monitor_state(), vec![updated]);
    mutate_monitors(1, &tx, || remove(&[1]));
    assert!(monitor_state().is_empty());
    mutate_monitors(1, &tx, || snug_update(original.clone(), false)).unwrap();
    mutate_monitors(1, &tx, remove_all);
    assert!(monitor_state().is_empty());
    assert!(MONITOR_RESERVATIONS.lock().unwrap().is_empty());
}

#[test]
fn legacy_unrelated_incremental_changes_preserve_accepted_and_pending_owners() {
    let mut legacy = owned_and_legacy_monitors();
    assert_eq!(snug_reserve(OTHER_OWNER, 3), SnugUpdateResult::Applied);
    let before_reservations = reservations();
    let owned_object = MONITOR_MODES.lock().unwrap()[0].object;
    legacy.modes[0].width = 3840;
    let tx = Sender::default();
    mutate_monitors(1, &tx, || snug_update(vec![legacy.clone()], true)).unwrap();
    assert_eq!(monitor_state(), vec![data(), legacy]);
    assert_eq!(MONITOR_MODES.lock().unwrap()[0].object, owned_object);
    assert_eq!(reservations(), before_reservations);
    mutate_monitors(1, &tx, || remove(&[2]));
    assert_eq!(monitor_state(), vec![data()]);
    assert_eq!(reservations(), before_reservations);
}

#[test]
fn legacy_mutations_observe_a_reservation_created_while_they_wait_for_the_lock() {
    for operation in 0..4 {
        reset_owned();
        let tx = std::sync::Arc::new(Sender::default());
        let mutation = MONITOR_MUTATIONS.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let caller_tx = tx.clone();
        let legacy = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            mutate_monitors(1, &caller_tx, || match operation {
                0 => notify(vec![]),
                1 => remove(&[1]),
                2 => remove_all(),
                3 => assert_eq!(
                    snug_update(vec![data()], true),
                    Err(SnugUpdateResult::NotFound)
                ),
                _ => unreachable!(),
            });
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(snug_reserve(OWNER, 1), SnugUpdateResult::Applied);
        assert_eq!(snug_create(OWNER, vec![data()]), SnugUpdateResult::Applied);
        let before = mutation_snapshot();
        drop(mutation);
        legacy.join().unwrap();
        assert_eq!(mutation_snapshot(), before, "operation {operation}");
        assert!(tx.0.lock().unwrap().is_empty());
    }
}

#[test]
fn expired_empty_reservation_rejects_create_and_frees_the_id_for_a_new_owner() {
    reset_owned();
    snug_reserve(OWNER, 1);
    MONITOR_RESERVATIONS
        .lock()
        .unwrap()
        .get_mut(&1)
        .unwrap()
        .expires_at = Instant::now() - Duration::from_secs(1);
    assert_eq!(snug_create(OWNER, vec![data()]), SnugUpdateResult::NotFound);
    assert_eq!(snug_reserve(OTHER_OWNER, 1), SnugUpdateResult::Applied);
    assert_eq!(
        snug_create(OTHER_OWNER, vec![data()]),
        SnugUpdateResult::Applied
    );
}

#[test]
fn a_late_reserve_after_cancellation_expires_without_stranding_the_id() {
    reset_owned();
    assert_eq!(snug_release(OWNER, &[1]), SnugUpdateResult::NotFound);
    snug_reserve(OWNER, 1);
    let after_lease = MONITOR_RESERVATIONS
        .lock()
        .unwrap()
        .get(&1)
        .unwrap()
        .expires_at
        + Duration::from_secs(1);
    expire_pending_reservations(after_lease);
    assert!(MONITOR_RESERVATIONS.lock().unwrap().is_empty());
    assert_eq!(snug_reserve(OTHER_OWNER, 1), SnugUpdateResult::Applied);
}

#[test]
fn accepted_monitor_ownership_never_expires_even_when_native_arrival_failed() {
    reset_owned();
    snug_reserve(OWNER, 1);
    ARRIVAL_FAIL.store(true, Ordering::SeqCst);
    assert_eq!(snug_create(OWNER, vec![data()]), SnugUpdateResult::Applied);
    let after_lease = MONITOR_RESERVATIONS
        .lock()
        .unwrap()
        .get(&1)
        .unwrap()
        .expires_at
        + Duration::from_secs(1);
    expire_pending_reservations(after_lease);
    assert_eq!(
        MONITOR_RESERVATIONS.lock().unwrap().get(&1).unwrap().owner,
        OWNER
    );
    assert_eq!(snug_release(OWNER, &[1]), SnugUpdateResult::Applied);
    assert!(monitor_state().is_empty());
}
#[test]
fn arrival_failure_rolls_back_object_and_allows_identical_retry() {
    let mut ctx = reset();
    ARRIVAL_FAIL.store(true, Ordering::SeqCst);
    assert!(ctx.create_monitor(1).is_err());
    assert_eq!(DELETIONS.load(Ordering::SeqCst), 1);
    assert!(MONITOR_MODES.lock().unwrap()[0].object.is_none());
    ARRIVAL_FAIL.store(false, Ordering::SeqCst);
    let arrivals = notify_locked(&mut MONITOR_MODES.lock().unwrap(), vec![data()]);
    assert_eq!(arrivals, vec![(1, true)]);
    assert!(LIVE.lock().unwrap().is_empty());
    ctx.create_monitor(1).unwrap();
    assert_eq!(LIVE.lock().unwrap().len(), 1);
}
#[test]
fn context_init_failure_deletes_unpublished_object_and_allows_retry() {
    let mut ctx = reset();
    INIT_FAIL.store(true, Ordering::SeqCst);
    assert!(ctx.create_monitor(1).is_err());
    assert_eq!(DELETIONS.load(Ordering::SeqCst), 1);
    assert!(MONITOR_MODES.lock().unwrap()[0].object.is_none());
    INIT_FAIL.store(false, Ordering::SeqCst);
    assert_eq!(
        notify_locked(&mut MONITOR_MODES.lock().unwrap(), vec![data()]),
        vec![(1, true)]
    );
}
#[test]
fn failed_departure_remove_keeps_live_object_and_state() {
    let mut ctx = reset();
    ctx.create_monitor(1).unwrap();
    DEPARTURE_FAIL.store(true, Ordering::SeqCst);
    remove(&[1]);
    assert_eq!(MONITOR_MODES.lock().unwrap().len(), 1);
    assert_eq!(LIVE.lock().unwrap().len(), 1);
    DEPARTURE_FAIL.store(false, Ordering::SeqCst);
    remove(&[1]);
    assert!(MONITOR_MODES.lock().unwrap().is_empty());
    assert!(LIVE.lock().unwrap().is_empty());
}
#[test]
fn failed_departure_remove_all_keeps_live_object_and_state() {
    let mut ctx = reset();
    ctx.create_monitor(1).unwrap();
    DEPARTURE_FAIL.store(true, Ordering::SeqCst);
    remove_all();
    assert_eq!(MONITOR_MODES.lock().unwrap().len(), 1);
    assert_eq!(LIVE.lock().unwrap().len(), 1);
    DEPARTURE_FAIL.store(false, Ordering::SeqCst);
    remove(&[1]);
    assert!(MONITOR_MODES.lock().unwrap().is_empty());
    assert!(LIVE.lock().unwrap().is_empty());
}
#[test]
fn failed_departure_replace_preserves_old_configuration_and_object() {
    let mut ctx = reset();
    ctx.create_monitor(1).unwrap();
    DEPARTURE_FAIL.store(true, Ordering::SeqCst);
    let mut updated = data();
    updated.modes[0].width = 2560;
    assert_eq!(
        notify_locked(&mut MONITOR_MODES.lock().unwrap(), vec![updated]),
        vec![(1, false)]
    );
    let cache = MONITOR_MODES.lock().unwrap();
    assert_eq!(cache[0].data, data());
    assert!(cache[0].object.is_some());
    assert_eq!(LIVE.lock().unwrap().len(), 1);
}
#[test]
fn valid_8k_240_mode_uses_full_width_height_and_u64_clock() {
    let info = display_info(7680, 4320, 240);
    assert_eq!(info.pixelRate, 240_u64 * 7680 * 4320);
    assert_eq!(info.hSyncFreq.Numerator, 240 * 4320);
    assert_eq!(info.hSyncFreq.Denominator, 1);
    assert_eq!(info.vSyncFreq.Numerator, 240);
    assert_eq!(info.vSyncFreq.Denominator, 1);
}
#[test]
fn portrait_monitor_and_target_timings_are_consistent() {
    let info = display_info(1080, 1920, 60);
    let target = target_mode(1080, 1920, 60)
        .TargetVideoSignalInfo
        .targetVideoSignalInfo;
    assert_eq!(info.pixelRate, 60_u64 * 1080 * 1920);
    assert_eq!(
        info.pixelRate * u64::from(info.hSyncFreq.Denominator),
        u64::from(info.hSyncFreq.Numerator) * u64::from(info.totalSize.cx)
    );
    assert_eq!(
        info.pixelRate * u64::from(info.vSyncFreq.Denominator),
        u64::from(info.vSyncFreq.Numerator)
            * u64::from(info.totalSize.cx)
            * u64::from(info.totalSize.cy)
    );
    assert_eq!(info.pixelRate, target.pixelRate);
    assert_eq!(info.activeSize.cx, info.totalSize.cx);
    assert_eq!(info.activeSize.cy, info.totalSize.cy);
}

#[test]
fn failed_create_arrival_accepts_intent_and_replacement_can_retry() {
    reset();
    MONITOR_MODES.lock().unwrap().clear();
    let tx = Sender::default();
    ARRIVAL_FAIL.store(true, Ordering::SeqCst);
    let accepted = mutate_monitors(1, &tx, || snug_update(vec![data()], false)).unwrap();
    assert_eq!(accepted, vec![data()]);
    assert_eq!(DELETIONS.load(Ordering::SeqCst), 1);
    assert!(MONITOR_MODES.lock().unwrap()[0].object.is_none());
    assert_eq!(tx.0.lock().unwrap().as_slice(), &[(1, vec![data()])]);
    ARRIVAL_FAIL.store(false, Ordering::SeqCst);
    mutate_monitors(1, &tx, || snug_update(vec![data()], true)).unwrap();
    assert_eq!(LIVE.lock().unwrap().len(), 1);
}

#[test]
fn failed_replace_departure_reports_failure_and_preserves_known_state() {
    let mut ctx = reset();
    ctx.create_monitor(1).unwrap();
    let tx = Sender::default();
    DEPARTURE_FAIL.store(true, Ordering::SeqCst);
    let mut replacement = data();
    replacement.modes[0].width = 2560;
    assert_eq!(
        mutate_monitors(1, &tx, || snug_update(vec![replacement], true)),
        Err(SnugUpdateResult::ApplyFailed)
    );
    assert_eq!(monitor_state(), vec![data()]);
    assert_eq!(LIVE.lock().unwrap().len(), 1);
    assert!(tx.0.lock().unwrap().is_empty());
}

#[test]
fn failed_replace_arrival_reports_failure_and_original_mode_can_be_restored() {
    let mut ctx = reset();
    ctx.create_monitor(1).unwrap();
    let tx = Sender::default();
    ARRIVAL_FAIL.store(true, Ordering::SeqCst);
    let mut replacement = data();
    replacement.modes[0].width = 2560;
    assert_eq!(
        mutate_monitors(1, &tx, || snug_update(vec![replacement.clone()], true)),
        Err(SnugUpdateResult::ApplyFailed)
    );
    assert_eq!(monitor_state(), vec![replacement.clone()]);
    assert!(LIVE.lock().unwrap().is_empty());
    assert!(MONITOR_MODES.lock().unwrap()[0].object.is_none());
    assert_eq!(tx.0.lock().unwrap().as_slice(), &[(1, vec![replacement])]);
    ARRIVAL_FAIL.store(false, Ordering::SeqCst);
    mutate_monitors(1, &tx, || snug_update(vec![data()], true)).unwrap();
    assert_eq!(monitor_state(), vec![data()]);
    assert_eq!(LIVE.lock().unwrap().len(), 1);
}

#[test]
fn remove_all_partial_failure_broadcasts_only_successful_removals() {
    let mut ctx = reset();
    ctx.create_monitor(1).unwrap();
    let failed_object = MONITOR_MODES.lock().unwrap()[0].object.unwrap().as_ptr() as usize;
    let mut second = data();
    second.id = 2;
    MONITOR_MODES.lock().unwrap().push(MonitorObject {
        object: None,
        data: second,
    });
    ctx.create_monitor(2).unwrap();
    DEPARTURE_FAIL_OBJECT.store(failed_object, Ordering::SeqCst);
    DEPARTURE_FAIL.store(true, Ordering::SeqCst);
    let tx = Sender::default();
    mutate_monitors(1, &tx, remove_all);
    assert_eq!(monitor_state(), vec![data()]);
    assert_eq!(LIVE.lock().unwrap().len(), 1);
    assert_eq!(tx.0.lock().unwrap().as_slice(), &[(1, vec![data()])]);
    DEPARTURE_FAIL.store(false, Ordering::SeqCst);
    mutate_monitors(1, &tx, remove_all);
    assert!(monitor_state().is_empty());
    assert!(LIVE.lock().unwrap().is_empty());
    assert_eq!(tx.0.lock().unwrap()[1], (1, vec![]));
}

#[test]
fn notify_rejection_never_broadcasts_unaccepted_input() {
    reset();
    let tx = Sender::default();
    let mut invalid = data();
    invalid.modes.clear();
    mutate_monitors(1, &tx, || notify(vec![invalid]));
    let mut unsupported = data();
    unsupported.modes[0].height = 16385;
    mutate_monitors(1, &tx, || notify(vec![unsupported]));
    assert_eq!(monitor_state(), vec![data()]);
    assert!(tx.0.lock().unwrap().is_empty());
    assert!(LIVE.lock().unwrap().is_empty());
}

#[test]
fn replacement_batch_failure_keeps_each_successful_native_result() {
    let mut ctx = reset();
    ctx.create_monitor(1).unwrap();
    let failed_object = MONITOR_MODES.lock().unwrap()[0].object.unwrap().as_ptr() as usize;
    let mut second = data();
    second.id = 2;
    MONITOR_MODES.lock().unwrap().push(MonitorObject {
        object: None,
        data: second.clone(),
    });
    ctx.create_monitor(2).unwrap();
    DEPARTURE_FAIL_OBJECT.store(failed_object, Ordering::SeqCst);
    DEPARTURE_FAIL.store(true, Ordering::SeqCst);
    let mut first = data();
    first.modes[0].width = 2560;
    second.modes[0].width = 3840;
    let tx = Sender::default();
    assert_eq!(
        mutate_monitors(1, &tx, || snug_update(vec![first, second.clone()], true)),
        Err(SnugUpdateResult::ApplyFailed)
    );
    assert_eq!(monitor_state(), vec![data(), second.clone()]);
    assert_eq!(LIVE.lock().unwrap().len(), 2);
    assert_eq!(
        tx.0.lock().unwrap().as_slice(),
        &[(1, vec![data(), second])]
    );
}

#[test]
fn concurrent_mutations_publish_serialized_actual_snapshots() {
    reset();
    let tx = std::sync::Arc::new(Sender::default());
    let start = std::sync::Arc::new(std::sync::Barrier::new(3));
    let handles = [2, 3].map(|id| {
        let tx = tx.clone();
        let start = start.clone();
        std::thread::spawn(move || {
            let mut monitor = data();
            monitor.id = id;
            start.wait();
            mutate_monitors(id as usize, &tx, || snug_update(vec![monitor], false)).unwrap();
        })
    });
    start.wait();
    for handle in handles {
        handle.join().unwrap();
    }
    let events = tx.0.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].1.len(), 2);
    assert_eq!(events[1].1.len(), 3);
    assert!(events[0]
        .1
        .iter()
        .all(|monitor| events[1].1.contains(monitor)));
    assert_eq!(events[1].1, monitor_state());
}

#[test]
fn incremental_replace_ignores_unrelated_pending_monitor_but_target_can_retry() {
    let mut ctx = reset();
    ctx.create_monitor(1).unwrap();
    let mut pending = data();
    pending.id = 2;
    MONITOR_MODES.lock().unwrap().push(MonitorObject {
        object: None,
        data: pending.clone(),
    });
    ARRIVAL_ATTEMPTS.lock().unwrap().clear();
    ARRIVAL_FAIL_ID.store(2, Ordering::SeqCst);
    ARRIVAL_FAIL.store(true, Ordering::SeqCst);
    let mut target = data();
    target.modes[0].width = 2560;
    let tx = Sender::default();
    mutate_monitors(1, &tx, || snug_update(vec![target.clone()], true)).unwrap();
    assert_eq!(ARRIVAL_ATTEMPTS.lock().unwrap().as_slice(), &[1]);
    assert_eq!(monitor_state(), vec![target.clone(), pending.clone()]);
    assert!(MONITOR_MODES.lock().unwrap()[1].object.is_none());

    // The same pending monitor must still be attempted when it is the target.
    assert_eq!(
        mutate_monitors(1, &tx, || snug_update(vec![pending.clone()], true)),
        Err(SnugUpdateResult::ApplyFailed)
    );
    assert_eq!(ARRIVAL_ATTEMPTS.lock().unwrap().as_slice(), &[1, 2]);
    assert!(MONITOR_MODES.lock().unwrap()[1].object.is_none());
    ARRIVAL_FAIL.store(false, Ordering::SeqCst);
    mutate_monitors(1, &tx, || snug_update(vec![pending.clone()], true)).unwrap();
    assert_eq!(ARRIVAL_ATTEMPTS.lock().unwrap().as_slice(), &[1, 2, 2]);
    assert!(MONITOR_MODES.lock().unwrap()[1].object.is_some());
    assert_eq!(monitor_state(), vec![target, pending]);
    assert_eq!(LIVE.lock().unwrap().len(), 2);
}

#[test]
fn legacy_notify_still_retries_every_pending_monitor() {
    reset();
    let mut pending = data();
    pending.id = 2;
    MONITOR_MODES.lock().unwrap().push(MonitorObject {
        object: None,
        data: pending.clone(),
    });
    let tx = Sender::default();
    mutate_monitors(1, &tx, || notify(vec![data(), pending]));
    assert_eq!(ARRIVAL_ATTEMPTS.lock().unwrap().as_slice(), &[1, 2]);
    assert!(MONITOR_MODES
        .lock()
        .unwrap()
        .iter()
        .all(|monitor| monitor.object.is_some()));
    assert_eq!(LIVE.lock().unwrap().len(), 2);
}

fn mode_callback_monitor() -> *mut IDDCX_MONITOR__ {
    let mut context = reset();
    context.create_monitor(1).unwrap();
    MONITOR_MODES.lock().unwrap()[0].object.unwrap().as_ptr()
}

fn parse_modes(
    input_count: u32,
    modes: *mut IDDCX_MONITOR_MODE,
    out: &mut IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    let mut edid = Edid::generate_with(1);
    let args = IDARG_IN_PARSEMONITORDESCRIPTION {
        MonitorDescription: IDDCX_MONITOR_DESCRIPTION {
            Size: size_of::<IDDCX_MONITOR_DESCRIPTION>() as u32,
            Type: IDDCX_MONITOR_DESCRIPTION_TYPE::IDDCX_MONITOR_DESCRIPTION_TYPE_EDID,
            DataSize: edid.len() as u32,
            pData: edid.as_mut_ptr(),
        },
        MonitorModeBufferInputCount: input_count,
        pMonitorModes: modes,
    };
    parse_monitor_description(&args, out)
}

#[test]
fn target_modes_null_buffer_only_reports_count_for_any_capacity() {
    let monitor = mode_callback_monitor();
    for count in [0, 1, 2, u32::MAX] {
        let args = IDARG_IN_QUERYTARGETMODES {
            TargetModeBufferInputCount: count,
            pTargetModes: std::ptr::null_mut(),
        };
        let mut out = IDARG_OUT_QUERYTARGETMODES::default();
        assert_eq!(
            monitor_query_modes(monitor, &args, &mut out),
            NTSTATUS::STATUS_SUCCESS
        );
        assert_eq!(out.TargetModeBufferOutputCount, 2);
    }
}

#[test]
fn target_modes_zero_and_small_capacity_preserve_buffer_then_full_query_writes_modes() {
    let monitor = mode_callback_monitor();
    let mut modes = [IDDCX_TARGET_MODE::default(); 2];
    for count in [0, 1] {
        let args = IDARG_IN_QUERYTARGETMODES {
            TargetModeBufferInputCount: count,
            pTargetModes: modes.as_mut_ptr(),
        };
        let mut out = IDARG_OUT_QUERYTARGETMODES::default();
        assert_eq!(
            monitor_query_modes(monitor, &args, &mut out),
            NTSTATUS::STATUS_SUCCESS
        );
        assert_eq!(out.TargetModeBufferOutputCount, 2);
        assert!(modes.iter().all(|mode| mode.Size == 0));
    }
    let args = IDARG_IN_QUERYTARGETMODES {
        TargetModeBufferInputCount: 2,
        pTargetModes: modes.as_mut_ptr(),
    };
    let mut out = IDARG_OUT_QUERYTARGETMODES::default();
    assert_eq!(
        monitor_query_modes(monitor, &args, &mut out),
        NTSTATUS::STATUS_SUCCESS
    );
    assert_eq!(out.TargetModeBufferOutputCount, 2);
    for (mode, rate) in modes.iter().zip([60, 120]) {
        assert_eq!(mode.Size, size_of::<IDDCX_TARGET_MODE>() as u32);
        assert_eq!(
            mode.TargetVideoSignalInfo.targetVideoSignalInfo.pixelRate,
            rate * 1920 * 1080
        );
    }
}

#[test]
fn description_modes_null_buffer_only_reports_count_for_any_capacity() {
    reset();
    for count in [0, 1, 2, u32::MAX] {
        let mut out = IDARG_OUT_PARSEMONITORDESCRIPTION::default();
        assert_eq!(
            parse_modes(count, std::ptr::null_mut(), &mut out),
            NTSTATUS::STATUS_SUCCESS
        );
        assert_eq!(out.MonitorModeBufferOutputCount, 2);
    }
}

#[test]
fn description_modes_zero_and_small_capacity_preserve_buffer_then_full_query_writes_modes() {
    reset();
    let mut modes = [IDDCX_MONITOR_MODE::default(); 2];
    let mut out = IDARG_OUT_PARSEMONITORDESCRIPTION::default();
    assert_eq!(
        parse_modes(0, modes.as_mut_ptr(), &mut out),
        NTSTATUS::STATUS_SUCCESS
    );
    assert_eq!(out.MonitorModeBufferOutputCount, 2);
    assert!(modes.iter().all(|mode| mode.Size == 0));
    assert_eq!(
        parse_modes(1, modes.as_mut_ptr(), &mut out),
        NTSTATUS::STATUS_BUFFER_TOO_SMALL
    );
    assert_eq!(out.MonitorModeBufferOutputCount, 2);
    assert!(modes.iter().all(|mode| mode.Size == 0));
    assert_eq!(
        parse_modes(2, modes.as_mut_ptr(), &mut out),
        NTSTATUS::STATUS_SUCCESS
    );
    assert_eq!(out.MonitorModeBufferOutputCount, 2);
    assert_eq!(out.PreferredMonitorModeIdx, 0);
    for (mode, rate) in modes.iter().zip([60, 120]) {
        assert_eq!(mode.Size, size_of::<IDDCX_MONITOR_MODE>() as u32);
        assert_eq!(mode.MonitorVideoSignalInfo.pixelRate, rate * 1920 * 1080);
    }
}
