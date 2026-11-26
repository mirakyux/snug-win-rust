use std::{
    collections::BTreeMap,
    mem::size_of,
    ptr::{addr_of_mut, NonNull},
    sync::{LazyLock, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use driver_ipc::{
    merge_snug_update, validate_monitors, Dimen, DriverCommand, EventCommand, Mode, Monitor,
    RefreshRate, ReplyCommand, RequestCommand, ServerCommand, SnugUpdateResult,
};
use log::{error, warn};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt as _},
    net::windows::named_pipe::{NamedPipeServer, ServerOptions},
    sync::broadcast::{self, error::RecvError, Sender},
    task,
};
use wdf_umdf::IddCxMonitorDeparture;
use wdf_umdf_sys::{IDDCX_ADAPTER__, IDDCX_MONITOR__};
use windows::Win32::{
    Security::{
        InitializeSecurityDescriptor, SetSecurityDescriptorDacl, PSECURITY_DESCRIPTOR,
        SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    },
    System::SystemServices::SECURITY_DESCRIPTOR_REVISION1,
};

use crate::context::DeviceContext;

pub static ADAPTER: OnceLock<AdapterObject> = OnceLock::new();
pub static MONITOR_MODES: LazyLock<Mutex<Vec<MonitorObject>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
// WDF monitor arrival reacquires MONITOR_MODES. Keep mutations serialized across
// that unlocked interval so another client cannot remove an ID before arrival.
static MONITOR_MUTATIONS: Mutex<()> = Mutex::new(());
// Held under MONITOR_MUTATIONS for every protocol operation. Reservations
// survive disconnection, but not a driver restart; release is token guarded.
struct Reservation {
    owner: String,
    expires_at: Instant,
}
static MONITOR_RESERVATIONS: LazyLock<Mutex<BTreeMap<u32, Reservation>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[derive(Debug)]
pub struct AdapterObject(pub NonNull<IDDCX_ADAPTER__>);
unsafe impl Sync for AdapterObject {}
unsafe impl Send for AdapterObject {}

#[derive(Debug)]
pub struct MonitorObject {
    pub object: Option<NonNull<IDDCX_MONITOR__>>,
    pub data: Monitor,
}
unsafe impl Sync for MonitorObject {}
unsafe impl Send for MonitorObject {}

const BUFFER_SIZE: u32 = 4096;
// Includes the largest supported 16-monitor request even with escaped names.
// This is a per-frame payload limit; the EOT delimiter does not count.
const MAX_MESSAGE_SIZE: usize = 512 * 1024;
// EOT
const EOF: char = '\x04';

// Consume only one frame from the newly read bytes, retaining incomplete input.
// The caller processes and clears a complete frame before consuming the next.
fn buffer_message(message: &mut Vec<u8>, input: &mut &[u8]) -> Result<bool, ()> {
    let end = input.iter().position(|&byte| byte == EOF as u8);
    let size = end.unwrap_or(input.len());
    if size > MAX_MESSAGE_SIZE || message.len() > MAX_MESSAGE_SIZE - size {
        return Err(());
    }
    message.extend_from_slice(&input[..size]);
    if let Some(end) = end {
        message.push(EOF as u8);
        *input = &input[end + 1..];
        Ok(true)
    } else {
        *input = &[];
        Ok(false)
    }
}

// message processor
async fn process_message(
    id: usize,
    server: &mut NamedPipeServer,
    tx: &Sender<(usize, Vec<Monitor>)>,
    buf: &[u8],
    iter: impl Iterator<Item = usize>,
) -> Result<(), ()> {
    // process each message in the buffer
    let mut start = 0;
    for eidx in iter {
        let sidx = start;
        start = eidx + 1;

        let Ok(msg) = std::str::from_utf8(&buf[sidx..eidx]) else {
            continue;
        };

        let Ok(command) = serde_json::from_str::<ServerCommand>(msg) else {
            continue;
        };

        match command {
            // driver commands
            ServerCommand::Driver(cmd) => match cmd {
                DriverCommand::Notify(monitors) => {
                    mutate_monitors(id, tx, || notify(monitors));
                }

                DriverCommand::Remove(ids) => {
                    mutate_monitors(id, tx, || remove(&ids));
                }

                DriverCommand::RemoveAll => {
                    mutate_monitors(id, tx, remove_all);
                }

                _ => (),
            },

            // request commands
            ServerCommand::Request(RequestCommand::SnugUpdate {
                monitors,
                replacing,
            }) => {
                let outcome = mutate_monitors(id, tx, || snug_update(monitors, replacing))
                .map_or_else(|outcome| outcome, |_| SnugUpdateResult::Applied);
                let Ok(mut data) = serde_json::to_string(&ReplyCommand::SnugUpdate(outcome)) else {
                    error!("SnugUpdate - failed to serialize reply");
                    break;
                };
                data.push(EOF);
                if server.write_all(data.as_bytes()).await.is_err() {
                    return Err(());
                }
            }

            ServerCommand::Request(RequestCommand::SnugCheck {
                owner,
                id: display_id,
            }) => {
                let result = snug_check(&owner, display_id);
                let reply = ReplyCommand::SnugCheck { owner, result };
                if write_owned_reply(server, &reply).await.is_err() {
                    return Err(());
                }
            }
            ServerCommand::Request(RequestCommand::SnugReserve {
                owner,
                id: display_id,
            }) => {
                let result = mutate_monitors(id, tx, || snug_reserve(&owner, display_id));
                let reply = ReplyCommand::SnugReserve { owner, result };
                if write_owned_reply(server, &reply).await.is_err() {
                    return Err(());
                }
            }
            ServerCommand::Request(RequestCommand::SnugCreate { owner, monitors }) => {
                let result = mutate_monitors(id, tx, || snug_create(&owner, monitors));
                let reply = ReplyCommand::SnugCreate { owner, result };
                if write_owned_reply(server, &reply).await.is_err() {
                    return Err(());
                }
            }
            ServerCommand::Request(RequestCommand::SnugReplace { owner, monitors }) => {
                let result = mutate_monitors(id, tx, || snug_replace(&owner, monitors));
                let reply = ReplyCommand::SnugReplace { owner, result };
                if write_owned_reply(server, &reply).await.is_err() {
                    return Err(());
                }
            }
            ServerCommand::Request(RequestCommand::SnugRelease { owner, ids }) => {
                let result = mutate_monitors(id, tx, || snug_release(&owner, &ids));
                let reply = ReplyCommand::SnugRelease { owner, result };
                if write_owned_reply(server, &reply).await.is_err() {
                    return Err(());
                }
            }

            ServerCommand::Request(RequestCommand::State) => {
                let mut data = {
                    let lock = MONITOR_MODES.lock().unwrap();
                    let monitors = lock.iter().map(|m| m.data.clone()).collect();
                    let command = ReplyCommand::State(monitors);

                    let Ok(serialized) = serde_json::to_string(&command) else {
                        error!("Command::Request - failed to serialize reply");
                        break;
                    };

                    serialized
                };

                data.push(EOF);

                if server.write_all(data.as_bytes()).await.is_err() {
                    // a server error means we should completely stop trying
                    return Err(());
                }
            }

            // Everything else is an invalid command
            _ => (),
        }
    }

    Ok(())
}

async fn write_owned_reply(server: &mut NamedPipeServer, reply: &ReplyCommand) -> Result<(), ()> {
    let mut data = serde_json::to_vec(reply).map_err(|_| ())?;
    data.push(EOF as u8);
    server.write_all(&data).await.map_err(|_| ())
}

fn valid_owner(owner: &str) -> bool {
    owner.len() == 32 && owner.bytes().all(|byte| byte.is_ascii_hexdigit())
}

// This read-only operation takes the mutation lock itself. Cache presence is
// accepted intent even when native arrival failed; an empty reservation is not.
// Do not expire or refresh reservations here, or route probes through mutations.
fn snug_check(owner: &str, id: u32) -> SnugUpdateResult {
    let _mutation = MONITOR_MUTATIONS.lock().unwrap();
    if !valid_owner(owner) || id >= 16 {
        return SnugUpdateResult::InvalidData;
    }
    if !MONITOR_MODES
        .lock()
        .unwrap()
        .iter()
        .any(|monitor| monitor.data.id == id)
    {
        return SnugUpdateResult::NotFound;
    }
    if MONITOR_RESERVATIONS
        .lock()
        .unwrap()
        .get(&id)
        .is_some_and(|reservation| reservation.owner == owner)
    {
        SnugUpdateResult::Applied
    } else {
        SnugUpdateResult::NotFound
    }
}

fn expire_pending_reservations(now: Instant) {
    let current = monitor_state();
    MONITOR_RESERVATIONS
        .lock()
        .unwrap()
        .retain(|id, reservation| {
            // Accepted monitors keep their token even if native arrival failed.
            // A late Reserve after Release can only occupy an empty ID briefly.
            current.iter().any(|monitor| monitor.id == *id) || reservation.expires_at > now
        });
}

fn snug_reserve(owner: &str, id: u32) -> SnugUpdateResult {
    if !valid_owner(owner) || id >= 16 {
        return SnugUpdateResult::InvalidData;
    }
    expire_pending_reservations(Instant::now());
    let mut reservations = MONITOR_RESERVATIONS.lock().unwrap();
    if monitor_state().iter().any(|monitor| monitor.id == id)
        || reservations
            .get(&id)
            .is_some_and(|previous| previous.owner != owner)
    {
        return SnugUpdateResult::AlreadyExists;
    }
    reservations.insert(
        id,
        Reservation {
            owner: owner.into(),
            expires_at: Instant::now() + Duration::from_secs(30),
        },
    );
    SnugUpdateResult::Applied
}

fn snug_create(owner: &str, monitors: Vec<Monitor>) -> SnugUpdateResult {
    if !valid_owner(owner) || monitors.is_empty() || validate_monitors(&monitors).is_err() {
        return SnugUpdateResult::InvalidData;
    }
    expire_pending_reservations(Instant::now());
    if monitors.iter().any(|monitor| {
        MONITOR_RESERVATIONS
            .lock()
            .unwrap()
            .get(&monitor.id)
            .map(|reservation| reservation.owner.as_str())
            != Some(owner)
    }) {
        return SnugUpdateResult::NotFound;
    }
    apply_snug_update(monitors, false).map_or_else(|result| result, |_| SnugUpdateResult::Applied)
}

fn snug_release(owner: &str, ids: &[u32]) -> SnugUpdateResult {
    if !valid_owner(owner) || ids.len() > 16 || ids.iter().any(|id| *id >= 16) {
        return SnugUpdateResult::InvalidData;
    }
    let owned: Vec<_> = {
        let reservations = MONITOR_RESERVATIONS.lock().unwrap();
        ids.iter()
            .copied()
            .filter(|id| {
                reservations
                    .get(id)
                    .map(|reservation| reservation.owner.as_str())
                    == Some(owner)
            })
            .collect()
    };
    if owned.is_empty() {
        // The caller can discard stale local bindings without waiting for a
        // foreign monitor that reused this connector to go offline.
        return SnugUpdateResult::NotFound;
    }
    let had_monitor = monitor_state()
        .iter()
        .any(|monitor| owned.contains(&monitor.id));
    remove_owned(&owned);
    let current = monitor_state();
    let mut reservations = MONITOR_RESERVATIONS.lock().unwrap();
    for id in &owned {
        if !current.iter().any(|monitor| monitor.id == *id) {
            reservations.remove(id);
        }
    }
    if owned
        .iter()
        .any(|id| current.iter().any(|monitor| monitor.id == *id))
    {
        SnugUpdateResult::ApplyFailed
    } else if had_monitor {
        SnugUpdateResult::Applied
    } else {
        // An empty reservation was cancelled. No desktop belongs to this token.
        SnugUpdateResult::NotFound
    }
}

fn snug_replace(owner: &str, monitors: Vec<Monitor>) -> SnugUpdateResult {
    if !valid_owner(owner) || monitors.is_empty() || validate_monitors(&monitors).is_err() {
        return SnugUpdateResult::InvalidData;
    }
    if monitors.iter().any(|monitor| {
        MONITOR_RESERVATIONS
            .lock()
            .unwrap()
            .get(&monitor.id)
            .map(|reservation| reservation.owner.as_str())
            != Some(owner)
    }) {
        return SnugUpdateResult::NotFound;
    }
    // Preserve the token through mode changes, including a failed native arrival
    // that still leaves this owner's accepted configuration in the driver cache.
    apply_snug_update(monitors, true).map_or_else(|result| result, |_| SnugUpdateResult::Applied)
}

// The caller holds MONITOR_MUTATIONS through validation and native application.
// Legacy commands must not revoke or consume even an empty reservation.
fn legacy_ids_reserved(ids: impl IntoIterator<Item = u32>) -> bool {
    let reservations = MONITOR_RESERVATIONS.lock().unwrap();
    ids.into_iter().any(|id| reservations.contains_key(&id))
}

#[allow(clippy::too_many_lines)]
pub fn startup() {
    thread::spawn(move || {
        // These security attributes will allow anyone access, so local account does not need admin privileges to use it

        let mut sd = SECURITY_DESCRIPTOR::default();

        unsafe {
            InitializeSecurityDescriptor(
                PSECURITY_DESCRIPTOR(addr_of_mut!(sd).cast()),
                SECURITY_DESCRIPTOR_REVISION1,
            )
            .unwrap();
        }

        unsafe {
            SetSecurityDescriptorDacl(
                PSECURITY_DESCRIPTOR(addr_of_mut!(sd).cast()),
                true,
                None,
                false,
            )
            .unwrap();
        }

        let mut sa = SECURITY_ATTRIBUTES {
            #[allow(clippy::cast_possible_truncation)]
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: addr_of_mut!(sd).cast(),
            bInheritHandle: false.into(),
        };

        // async time!
        let pipe_server = async {
            let (tx, _rx) = broadcast::channel(1);

            let mut id = 0usize;

            loop {
                let mut server = unsafe {
                    ServerOptions::new()
                        .access_inbound(true)
                        .access_outbound(true)
                        .reject_remote_clients(true)
                        .in_buffer_size(BUFFER_SIZE)
                        .out_buffer_size(BUFFER_SIZE)
                        // default is unlimited instances
                        .create_with_security_attributes_raw(
                            r"\\.\pipe\virtualdisplaydriver",
                            std::ptr::from_mut::<SECURITY_ATTRIBUTES>(&mut sa).cast(),
                        )
                        .unwrap()
                };

                if server.connect().await.is_err() {
                    continue;
                }

                id += 1;

                let mut msg_buf: Vec<u8> = Vec::with_capacity(BUFFER_SIZE as usize);
                let mut buf = vec![0; BUFFER_SIZE as usize];
                let tx = tx.clone();
                let mut rx = tx.subscribe();

                task::spawn(async move {
                    loop {
                        tokio::select! {
                            val = server.read(&mut buf) =>  {
                                let size = match val {
                                    // 0 = no more data to read
                                    // or break on err
                                    Ok(0) | Err(_) => break,

                                    Ok(size) => size,
                                };
                                let mut input = &buf[..size];
                                while !input.is_empty() {
                                    let complete = match buffer_message(&mut msg_buf, &mut input) {
                                        Ok(complete) => complete,
                                        Err(()) => return,
                                    };
                                    if complete {
                                        let end = std::iter::once(msg_buf.len() - 1);
                                        if process_message(id, &mut server, &tx, &msg_buf, end).await.is_err() {
                                            return;
                                        }
                                        msg_buf.clear();
                                    }
                                }
                            },

                            val = rx.recv() => {
                                let command = match val {
                                    // ignore if this value was sent for the current client (current client doesn't need notification)
                                    Ok((client_id, _)) if client_id == id => continue,

                                    Ok((_, data)) => EventCommand::Changed(data),

                                    Err(RecvError::Lagged(_)) => continue,

                                    // closed
                                    Err(_) => break
                                };

                                let Ok(mut serialized) = serde_json::to_string(&command) else {
                                    error!("Command::Request - failed to serialize reply");
                                    break;
                                };

                                serialized.push(EOF);

                                if server.write_all(serialized.as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        };

        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed building the Runtime")
            .block_on(pipe_server);
    });
}

/// Notifies driver of new system monitor state
///
/// Adds, updates, or removes monitors as needed
///
/// Note that updated monitors causes a detach, update, and reattach. (Required for windows to see the changes)
///
/// Only detaches/reattaches if required
/// e.g. only a monitor name update would not detach/arrive a monitor
fn notify(monitors: Vec<Monitor>) {
    if validate_monitors(&monitors).is_err() {
        warn!("notify(): Invalid monitor data was detected; update aborted");
        return;
    }

    // Full snapshots also remove omitted IDs and retry pending arrivals. Even
    // an unchanged owned row would therefore grant a tokenless native write.
    let current = monitor_state();
    if legacy_ids_reserved(current.iter().chain(&monitors).map(|monitor| monitor.id)) {
        warn!("notify(): Owned or reserved monitor detected; update aborted");
        return;
    }

    let should_arrive = {
        let mut lock = MONITOR_MODES.lock().unwrap();
        notify_locked(&mut lock, monitors)
    };
    let _ = arrive_monitors(should_arrive);
}

fn monitor_state() -> Vec<Monitor> {
    MONITOR_MODES
        .lock()
        .unwrap()
        .iter()
        .map(|monitor| monitor.data.clone())
        .collect()
}

// Serialize the mutation and its actual-state event together. Rejected input
// and failed native departures must never broadcast the requested state.
fn mutate_monitors<T>(
    id: usize,
    tx: &Sender<(usize, Vec<Monitor>)>,
    mutation: impl FnOnce() -> T,
) -> T {
    let _mutation = MONITOR_MUTATIONS.lock().unwrap();
    let before = monitor_state();
    let result = mutation();
    let after = monitor_state();
    if before != after {
        _ = tx.send((id, after));
    }
    result
}

fn snug_update(monitors: Vec<Monitor>, replacing: bool) -> Result<Vec<Monitor>, SnugUpdateResult> {
    validate_monitors(&monitors)?;
    if legacy_ids_reserved(monitors.iter().map(|monitor| monitor.id)) {
        return Err(SnugUpdateResult::NotFound);
    }
    apply_snug_update(monitors, replacing)
}

/// Validate, merge and apply after the caller's ownership check, under the same
/// MONITOR_MUTATIONS guard. This helper must not be exposed to legacy requests.
fn apply_snug_update(
    monitors: Vec<Monitor>,
    replacing: bool,
) -> Result<Vec<Monitor>, SnugUpdateResult> {
    let requested_ids: Vec<_> = monitors.iter().map(|monitor| monitor.id).collect();
    let (merged, should_arrive) = {
        let mut lock = MONITOR_MODES.lock().unwrap();
        let current: Vec<_> = lock.iter().map(|monitor| monitor.data.clone()).collect();
        let merged = merge_snug_update(&current, monitors, replacing)?;
        let mut should_arrive = notify_locked(&mut lock, merged.clone());
        should_arrive.retain(|(id, _)| requested_ids.contains(id));
        (merged, should_arrive)
    };
    // create_monitor locks MONITOR_MODES itself, so release the state lock first.
    let arrived = arrive_monitors(should_arrive);
    let accepted = monitor_state();
    // Creation acknowledges accepted intent even if a monitor has not arrived:
    // the client must record ownership before waiting for its desktop presence.
    // Replacement IDs already belong to the caller, so failures can be reported
    // without leaving a newly accepted ID unaccounted for.
    if replacing && (!arrived || accepted != merged) {
        return Err(SnugUpdateResult::ApplyFailed);
    }
    Ok(accepted)
}

fn depart_monitor(monitor: &mut MonitorObject) -> bool {
    if let Some(mut object) = monitor.object {
        if let Err(error) = unsafe { IddCxMonitorDeparture(object.as_mut()) } {
            error!("Failed to remove monitor {}: {error:?}", monitor.data.id);
            return false;
        }
        monitor.object = None;
    }
    true
}

fn notify_locked(lock: &mut Vec<MonitorObject>, monitors: Vec<Monitor>) -> Vec<(u32, bool)> {
    // Remove monitors from internal list which are missing from the provided list

    lock.retain_mut(|mon| {
        let id = mon.data.id;
        let found = monitors.iter().any(|m| m.id == id);

        found || !depart_monitor(mon)
    });

    monitors
        .into_iter()
        .map(|monitor| {
            let id = monitor.id;

            let should_arrive;

            let cur_mon = lock.iter_mut().find(|mon| mon.data.id == id);

            if let Some(mon) = cur_mon {
                let modes_changed = mon.data.modes != monitor.modes;

                // Retain both the object and its old data until WDF confirms
                // departure. Creating a replacement after failure could orphan
                // an online monitor and reuse its connector index.
                if (modes_changed || !monitor.enabled) && !depart_monitor(mon) {
                    return (id, false);
                }

                #[allow(clippy::nonminimal_bool)]
                {
                    should_arrive =
                        // previously was disabled, and it was just enabled
                        (!mon.data.enabled && monitor.enabled) ||
                        // OR monitor is enabled and the display modes changed
                        (monitor.enabled && modes_changed) ||
                        // OR monitor is enabled and the monitor was disconnected
                        (monitor.enabled && mon.object.is_none());
                }

                // update monitor data
                mon.data = monitor;
            } else {
                should_arrive = monitor.enabled;

                lock.push(MonitorObject {
                    object: None,
                    data: monitor,
                });
            }

            (id, should_arrive)
        })
        .collect()
}

fn arrive_monitors(should_arrive: Vec<(u32, bool)>) -> bool {
    let adapter = ADAPTER.get().unwrap().0.as_ptr();
    let mut arrived = true;
    let cb = |context: &mut DeviceContext| {
        // arrive any monitors that need arriving
        for (id, arrive) in should_arrive {
            if arrive {
                if let Err(e) = context.create_monitor(id) {
                    error!("Failed to create monitor: {e:?}");
                    arrived = false;
                }
            }
        }
    };

    unsafe {
        if let Err(error) = DeviceContext::get_mut(adapter.cast(), cb) {
            error!("Failed to access adapter context: {error:?}");
            return false;
        }
    }
    arrived
}

fn remove_all() {
    if !MONITOR_RESERVATIONS.lock().unwrap().is_empty() {
        warn!("remove_all(): Owned or reserved monitor detected; removal aborted");
        return;
    }
    let mut lock = MONITOR_MODES.lock().unwrap();
    lock.retain_mut(|monitor| !depart_monitor(monitor));
}

fn remove(ids: &[u32]) {
    if legacy_ids_reserved(ids.iter().copied()) {
        warn!("remove(): Owned or reserved monitor detected; removal aborted");
        return;
    }
    remove_owned(ids);
}

// Only called after either an owner-token or a legacy reservation check, while
// MONITOR_MUTATIONS remains held through departure and reservation cleanup.
fn remove_owned(ids: &[u32]) {
    let mut lock = MONITOR_MODES.lock().unwrap();
    lock.retain_mut(|monitor| !ids.contains(&monitor.data.id) || !depart_monitor(monitor));
}

pub trait FlattenModes {
    fn flatten(&self) -> impl Iterator<Item = ModeItem>;
}

#[derive(Copy, Clone)]
pub struct ModeItem {
    pub width: Dimen,
    pub height: Dimen,
    pub refresh_rate: RefreshRate,
}

/// Takes a slice of modes and creates a flattened structure that can be iterated over
impl FlattenModes for Vec<Mode> {
    fn flatten(&self) -> impl Iterator<Item = ModeItem> {
        self.iter().flat_map(|m| {
            m.refresh_rates.iter().map(|&rr| ModeItem {
                width: m.width,
                height: m.height,
                refresh_rate: rr,
            })
        })
    }
}
