//! Explicit, owner-scoped real-driver smoke test. Never starts the application.
use serde_json::{Value, json};
use snug_win::display::{DisplayApi, MonitorSnapshot, WinDisplay};
use snug_win::vdd::{DEFAULT_PIPE_NAME, RemoveDisposition};
use snug_win::{VddClient, VddError, VddMode, VddMonitor, VddProtocol};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type TestResult<T> = Result<T, String>;
const SCHEMA: &str = "snug-owner-hardware-smoke-v1";
const SETTLE: Duration = Duration::from_secs(15);

#[link(name = "user32")]
unsafe extern "system" {
    fn SetThreadDpiAwarenessContext(value: isize) -> isize;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetNamedPipeHandleState(
        pipe: *mut std::ffi::c_void,
        mode: *const u32,
        collection: *const u32,
        timeout: *const u32,
    ) -> i32;
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn write_new(path: &Path, value: &Value) -> TestResult<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(
        serde_json::to_string_pretty(value)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
    .and_then(|()| file.sync_all())
    .map_err(|e| e.to_string())
}

fn event(dir: &Path, phase: &str, detail: Value) -> TestResult<()> {
    let value = json!({"timeMs": now_ms(), "phase": phase, "detail": detail});
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("events.jsonl"))
        .map_err(|e| e.to_string())?;
    writeln!(file, "{value}")
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())?;
    println!("{value}");
    Ok(())
}

fn monitor_value(m: &MonitorSnapshot) -> Value {
    json!({
        "stableId": m.stable_id, "deviceName": m.device_name,
        "friendlyName": m.friendly_name, "deviceString": m.device_string,
        "virtualId": m.virtual_display_id,
        "adapterLow": m.adapter_id.low_part, "adapterHigh": m.adapter_id.high_part,
        "sourceId": m.source_id, "bounds": format!("{:?}", m.bounds),
        "workArea": format!("{:?}", m.work_area), "primary": m.is_primary,
        "width": m.mode.width, "height": m.mode.height,
        "refreshHz": m.mode.refresh_rate, "dpiPercent": m.mode.dpi_scale_percent,
    })
}

fn normalized(mut items: Vec<Value>) -> Vec<Value> {
    // Include all fields in the sort key; enumeration order is not identity.
    items.sort_by_key(Value::to_string);
    items
}

fn snapshot(client: &mut VddClient) -> TestResult<Value> {
    let state = client
        .state()
        .map_err(|e| e.to_string())?
        .ok_or("a replying V2 driver is required")?;
    if client.protocol() != Some(VddProtocol::V2) {
        return Err("driver protocol is not V2".into());
    }
    let desktop = WinDisplay::new().active_monitors();
    if desktop.is_empty() || desktop.iter().filter(|m| m.is_primary).count() != 1 {
        return Err(
            "desktop enumeration is empty or has no unique primary; refusing writes".into(),
        );
    }
    let mut ids = std::collections::BTreeSet::new();
    if state.iter().any(|m| !ids.insert(m.id)) {
        return Err("duplicate IDs in driver State".into());
    }
    let mut desktop_ids = std::collections::BTreeSet::new();
    if desktop
        .iter()
        .filter_map(|m| m.virtual_display_id)
        .any(|id| !desktop_ids.insert(id))
    {
        return Err("duplicate virtual IDs in desktop enumeration".into());
    }
    Ok(
        json!({"driver": normalized(state.iter().map(|m| json!(m)).collect()),
        "desktop": normalized(desktop.iter().map(monitor_value).collect())}),
    )
}

fn items<'a>(value: &'a Value, key: &str) -> TestResult<&'a Vec<Value>> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing array {key}"))
}

fn untouched(baseline: &Value, current: &Value, id: u32) -> TestResult<()> {
    for (key, field) in [("driver", "id"), ("desktop", "virtualId")] {
        let rest = normalized(
            items(current, key)?
                .iter()
                .filter(|m| m.get(field).and_then(Value::as_u64) != Some(u64::from(id)))
                .cloned()
                .collect(),
        );
        if rest != normalized(items(baseline, key)?.clone()) {
            return Err(format!(
                "an existing or unexpected {key} screen changed; see snapshots"
            ));
        }
    }
    Ok(())
}

// A virtual ID may disappear before CCD removes the screen. Freeze its full native
// address while ownership is still proved; never learn a new address after Release.
fn native_identity(monitor: &Value) -> TestResult<Value> {
    for key in ["stableId", "adapterLow", "sourceId"] {
        if !monitor[key]
            .as_u64()
            .is_some_and(|value| u32::try_from(value).is_ok())
        {
            return Err(format!("missing or invalid native identity field {key}"));
        }
    }
    if !monitor["adapterHigh"]
        .as_i64()
        .is_some_and(|value| i32::try_from(value).is_ok())
        || !monitor["deviceName"]
            .as_str()
            .is_some_and(|value| !value.trim().is_empty())
    {
        return Err("missing or invalid native adapter/device identity".into());
    }
    Ok(
        json!({"stableId": monitor["stableId"], "deviceName": monitor["deviceName"],
        "adapterLow": monitor["adapterLow"], "adapterHigh": monitor["adapterHigh"],
        "sourceId": monitor["sourceId"]}),
    )
}

fn identity_collides(left: &Value, right: &Value) -> bool {
    left["stableId"] == right["stableId"]
        || left["deviceName"]
            .as_str()
            .unwrap_or_default()
            .eq_ignore_ascii_case(right["deviceName"].as_str().unwrap_or_default())
        || (left["adapterLow"] == right["adapterLow"]
            && left["adapterHigh"] == right["adapterHigh"]
            && left["sourceId"] == right["sourceId"])
}

fn native_identities(snapshot: &Value) -> TestResult<Vec<Value>> {
    let mut identities = Vec::new();
    for monitor in items(snapshot, "desktop")? {
        let identity = native_identity(monitor)?;
        if identities
            .iter()
            .any(|other| identity_collides(other, &identity))
        {
            return Err("duplicate or ambiguous native desktop identity".into());
        }
        identities.push(identity);
    }
    Ok(identities)
}

fn bind_release_identity(
    baseline: &Value,
    current: &Value,
    id: u32,
    owner_confirmed: bool,
    saved: Option<&Value>,
) -> TestResult<Option<Value>> {
    let originals = native_identities(baseline)?;
    native_identities(current)?;
    let candidates: Vec<_> = items(current, "desktop")?
        .iter()
        .filter(|monitor| monitor["virtualId"].as_u64() == Some(u64::from(id)))
        .collect();
    if candidates.len() > 1 {
        return Err("cannot bind multiple desktop screens to one owned ID".into());
    }
    let live = candidates
        .first()
        .map(|monitor| native_identity(monitor))
        .transpose()?;
    let identity = if let Some(saved) = saved {
        let identity = native_identity(saved)?;
        if saved != &identity || live.as_ref().is_some_and(|live| live != &identity) {
            return Err("saved release identity conflicts with current owned screen".into());
        }
        Some(identity)
    } else if live.is_some() {
        if !owner_confirmed {
            return Err("cannot bind a native screen without confirmed ownership".into());
        }
        live
    } else {
        None
    };
    if identity.as_ref().is_some_and(|identity| {
        originals
            .iter()
            .any(|original| identity_collides(original, identity))
    }) {
        return Err("owned release identity aliases an original screen".into());
    }
    // Reject any unexplained screen before changing topology. A reservation with no
    // native screen needs no binding, but still has to preserve the exact originals.
    release_observation(baseline, current, id, identity.as_ref(), &mut 0)?;
    Ok(identity)
}

fn validate_release_record(journal: &Value, record: &Value) -> TestResult<Value> {
    if record["schema"] != "snug-release-identity-v1"
        || record["owner"] != journal["owner"]
        || !journal["owner"].as_str().is_some_and(valid_owner)
        || record["id"] != journal["id"]
        || !journal["id"]
            .as_u64()
            .is_some_and(|id| (4..16).contains(&id))
        || record["baseline"] != journal["baseline"]
    {
        return Err("release identity record does not match the recovery journal".into());
    }
    let identity = native_identity(&record["identity"])?;
    if record["identity"] != identity
        || native_identities(&journal["baseline"])?
            .iter()
            .any(|original| identity_collides(original, &identity))
    {
        return Err("invalid or aliased persisted release identity".into());
    }
    Ok(identity)
}

fn release_observation(
    baseline: &Value,
    current: &Value,
    id: u32,
    identity: Option<&Value>,
    consecutive: &mut usize,
) -> TestResult<bool> {
    let baseline_driver = normalized(items(baseline, "driver")?.clone());
    let baseline_desktop = normalized(items(baseline, "desktop")?.clone());
    let originals = native_identities(baseline)?;
    let current_identities = native_identities(current)?;
    if items(baseline, "driver")?
        .iter()
        .any(|monitor| monitor["id"].as_u64() == Some(u64::from(id)))
        || items(baseline, "desktop")?
            .iter()
            .any(|monitor| monitor["virtualId"].as_u64() == Some(u64::from(id)))
    {
        return Err("release ID was not vacant in the original snapshot".into());
    }
    if let Some(identity) = identity
        && (native_identity(identity)? != *identity
            || originals
                .iter()
                .any(|original| identity_collides(original, identity)))
    {
        return Err("invalid or aliased bound release identity".into());
    }
    let driver = items(current, "driver")?;
    let own_driver = driver
        .iter()
        .filter(|monitor| monitor["id"].as_u64() == Some(u64::from(id)))
        .count();
    let driver_rest = normalized(
        driver
            .iter()
            .filter(|monitor| monitor["id"].as_u64() != Some(u64::from(id)))
            .cloned()
            .collect(),
    );
    if own_driver > 1 || driver_rest != baseline_driver {
        return Err("an existing or unexpected driver screen changed during release".into());
    }
    let mut desktop_rest = Vec::new();
    for (monitor, observed) in items(current, "desktop")?.iter().zip(&current_identities) {
        if identity == Some(observed) {
            if !monitor["virtualId"].is_null()
                && monitor["virtualId"].as_u64() != Some(u64::from(id))
            {
                return Err("bound native screen acquired a different virtual ID".into());
            }
        } else {
            desktop_rest.push(monitor.clone());
        }
    }
    if normalized(desktop_rest) != baseline_desktop {
        return Err("an original or unbound desktop screen changed during release".into());
    }
    // Compare COMPLETE snapshots: a null-ID bound ghost remains pending even
    // after driver State is empty. Only two separated exact originals can pass.
    let exact = normalized(driver.clone()) == baseline_driver
        && normalized(items(current, "desktop")?.clone()) == baseline_desktop;
    *consecutive = if exact {
        consecutive.saturating_add(1)
    } else {
        0
    };
    Ok(*consecutive >= 2)
}

fn release_after_binding<T>(
    binding: TestResult<T>,
    release: impl FnOnce() -> TestResult<()>,
) -> TestResult<T> {
    // Observation/persistence errors must not suppress the authorized cleanup.
    release()?;
    binding
}

fn wait_for_release(
    client: &mut VddClient,
    dir: &Path,
    baseline: &Value,
    id: u32,
    identity: Option<&Value>,
) -> TestResult<()> {
    let deadline = Instant::now() + SETTLE;
    let mut consecutive = 0;
    loop {
        let current = snapshot(client)?;
        event(
            dir,
            "released-and-original-screens-preserved",
            current.clone(),
        )?;
        if release_observation(baseline, &current, id, identity, &mut consecutive)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("release: desktop did not converge to exact originals in 15s".into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn vacant_id(baseline: &Value) -> TestResult<u32> {
    // Leave the application's usual IDs 0..3 alone, even when they appear vacant.
    (4..16)
        .rev()
        .find(|&id| {
            [("driver", "id"), ("desktop", "virtualId")]
                .iter()
                .all(|(key, field)| {
                    baseline[*key].as_array().is_some_and(|list| {
                        list.iter()
                            .all(|m| m[*field].as_u64() != Some(u64::from(id)))
                    })
                })
        })
        .ok_or_else(|| "no vacant test ID in 4..15; no command sent".into())
}

fn is_not_found(result: Result<(), VddError>) -> TestResult<()> {
    match result {
        Err(VddError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => Err(format!(
            "expected a trusted owner-matched NotFound reply, got {other:?}"
        )),
    }
}

fn valid_owner(owner: &str) -> bool {
    owner.len() == 32 && owner.bytes().all(|c| c.is_ascii_hexdigit())
}

fn spec(id: u32, width: u32, height: u32) -> VddMonitor {
    VddMonitor {
        id,
        name: Some(format!("Snug hardware acceptance {id}")),
        enabled: true,
        modes: vec![VddMode {
            width,
            height,
            refresh_rates: vec![60],
        }],
    }
}

fn legacy_cases(
    id: u32,
    accepted: &VddMonitor,
    changed: &VddMonitor,
) -> Vec<(&'static str, Value)> {
    vec![
        ("legacy-notify-replace", json!({"Notify": [changed]})),
        ("legacy-notify-omit", json!({"Notify": []})),
        ("legacy-notify-identical", json!({"Notify": [accepted]})),
        ("legacy-remove", json!({"Remove": [id]})),
        ("legacy-remove-all", json!("RemoveAll")),
        (
            "tokenless-snug-update",
            json!({"SnugUpdate": {"monitors": [changed], "replacing": true}}),
        ),
    ]
}

// Use the real legacy wire commands, rather than VddClient::notify (which sends
// SnugUpdate on V2). PIPE_NOWAIT bounds this test-only polling connection; no
// FlushFileBuffers is used because it can wait for a peer indefinitely.
// The State frame on this same connection is a barrier after command handling.
fn legacy_exchange(command: &Value) -> TestResult<Vec<Value>> {
    let mut pipe = OpenOptions::new()
        .read(true)
        .write(true)
        .security_qos_flags(0x0001_0000) // SECURITY_ANONYMOUS
        .open(r"\\.\pipe\virtualdisplaydriver")
        .map_err(|e| e.to_string())?;
    let mode = 1_u32; // PIPE_READMODE_BYTE | PIPE_NOWAIT
    if unsafe {
        SetNamedPipeHandleState(
            pipe.as_raw_handle(),
            &mode,
            std::ptr::null(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err(format!(
            "cannot enable bounded legacy probe: {}",
            io::Error::last_os_error()
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bytes = serde_json::to_vec(command).map_err(|e| e.to_string())?;
    bytes.extend_from_slice(b"\x04\"State\"\x04");
    let mut offset = 0;
    while offset < bytes.len() {
        if Instant::now() >= deadline {
            return Err("legacy probe write timed out; no replay".into());
        }
        match pipe.write(&bytes[offset..]) {
            Ok(0) => std::thread::sleep(Duration::from_millis(10)),
            Ok(n) => offset += n,
            Err(e) if e.raw_os_error() == Some(232) => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let mut pending = Vec::new();
    let mut replies = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        if Instant::now() >= deadline {
            return Err("legacy probe State barrier timed out".into());
        }
        match pipe.read(&mut buffer) {
            Ok(0) => {
                // PIPE_NOWAIT may report a successful zero-byte read before the
                // asynchronous server has produced its reply. The deadline
                // distinguishes this from a permanently disconnected peer.
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Ok(n) => pending.extend_from_slice(&buffer[..n]),
            Err(e) if e.raw_os_error() == Some(232) => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(e) => return Err(e.to_string()),
        }
        if pending.len() > 2 * 1024 * 1024 {
            return Err("legacy probe reply exceeded bound".into());
        }
        while let Some(end) = pending.iter().position(|&x| x == 4) {
            let reply: Value =
                serde_json::from_slice(&pending[..end]).map_err(|e| e.to_string())?;
            pending.drain(..=end);
            let barrier = reply.get("State").is_some();
            replies.push(reply);
            if barrier {
                return Ok(replies);
            }
            if replies.len() >= 64 {
                return Err("legacy probe did not reach State within reply bound".into());
            }
        }
    }
}

fn verify_legacy_reply(command: &Value, replies: &[Value], expected: &[Value]) -> TestResult<()> {
    let last = replies
        .last()
        .ok_or("legacy probe returned no State barrier")?;
    let state = last["State"]
        .as_array()
        .ok_or("legacy probe has no explicit State array")?;
    if normalized(state.clone()) != normalized(expected.to_vec()) {
        return Err("legacy command changed owned or original State".into());
    }
    if command.get("SnugUpdate").is_some() {
        let acks: Vec<_> = replies.iter().filter_map(|v| v.get("SnugUpdate")).collect();
        if acks != [&json!("NotFound")] {
            return Err("tokenless SnugUpdate did not explicitly reject the owned ID".into());
        }
    }
    Ok(())
}

fn exercise_legacy_bypasses(
    client: &mut VddClient,
    dir: &Path,
    baseline: &Value,
    id: u32,
    owner: &str,
    accepted: &VddMonitor,
    changed: &VddMonitor,
) -> TestResult<()> {
    let expected = snapshot(client)?;
    for (phase, command) in legacy_cases(id, accepted, changed) {
        client
            .verify_owner(id, Some(owner))
            .map_err(|e| e.to_string())?;
        event(dir, &format!("{phase}-planned"), command.clone())?;
        let replies = legacy_exchange(&command)?;
        event(dir, &format!("{phase}-wire-replies"), json!(replies))?;
        verify_legacy_reply(&command, &replies, items(&expected, "driver")?)?;
        client
            .verify_owner(id, Some(owner))
            .map_err(|e| e.to_string())?;
        wait_for(
            client,
            dir,
            baseline,
            id,
            Some(accepted),
            &format!("{phase}-no-effect"),
        )?;
        event(
            dir,
            &format!("{phase}-owner-preserved"),
            json!({"id":id,"verified":true}),
        )?;
    }
    Ok(())
}

fn settled(current: &Value, id: u32, expected: Option<&VddMonitor>) -> TestResult<bool> {
    let driver: Vec<_> = items(current, "driver")?
        .iter()
        .filter(|m| m["id"].as_u64() == Some(u64::from(id)))
        .collect();
    let desktop: Vec<_> = items(current, "desktop")?
        .iter()
        .filter(|m| m["virtualId"].as_u64() == Some(u64::from(id)))
        .collect();
    let Some(expected) = expected else {
        return Ok(driver.is_empty() && desktop.is_empty());
    };
    let mode = &expected.modes[0];
    Ok(driver.len() == 1
        && driver[0] == &json!(expected)
        && desktop.len() == 1
        && desktop[0]["width"].as_u64() == Some(u64::from(mode.width))
        && desktop[0]["height"].as_u64() == Some(u64::from(mode.height))
        && desktop[0]["refreshHz"]
            .as_f64()
            .is_some_and(|hz| (hz - 60.0).abs() < 0.5))
}

fn wait_for(
    client: &mut VddClient,
    dir: &Path,
    baseline: &Value,
    id: u32,
    expected: Option<&VddMonitor>,
    phase: &str,
) -> TestResult<()> {
    let deadline = Instant::now() + SETTLE;
    let mut consecutive = 0;
    loop {
        let current = snapshot(client)?;
        event(dir, phase, current.clone())?;
        untouched(baseline, &current, id)?;
        consecutive = if settled(&current, id, expected)? {
            consecutive + 1
        } else {
            0
        };
        // Two separated snapshots avoid accepting a one-frame transition.
        if consecutive >= 2 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("{phase}: desktop did not converge in 15s"));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn exercise(
    client: &mut VddClient,
    dir: &Path,
    baseline: &Value,
    id: u32,
    owner: &str,
    wrong: &str,
) -> TestResult<()> {
    // A buggy RemoveAll must never endanger an original unowned virtual screen.
    // Physical CCD outputs are still included in every untouched comparison.
    if !items(baseline, "driver")?.is_empty() {
        return Err(
            "legacy bypass acceptance requires an empty original driver State; no reservation sent"
                .into(),
        );
    }
    event(dir, "reserve-planned", json!({"id": id}))?;
    if !client.reserve_owned(id, owner).map_err(|e| e.to_string())? {
        return Err("driver did not confirm an owner reservation".into());
    }
    // SnugCheck confirms an accepted monitor, not an empty reservation.
    is_not_found(client.verify_owner(id, Some(owner)))?;
    event(dir, "reserved", json!({"id": id}))?;
    let reserved = snapshot(client)?;
    event(dir, "reservation-no-topology-change", reserved.clone())?;
    if reserved != *baseline {
        return Err("reservation changed topology".into());
    }

    let first = spec(id, 1280, 720);
    event(dir, "create-planned", json!(first))?;
    client
        .create_owned(std::slice::from_ref(&first), owner)
        .map_err(|e| e.to_string())?;
    client
        .verify_owner(id, Some(owner))
        .map_err(|e| e.to_string())?;
    wait_for(
        client,
        dir,
        baseline,
        id,
        Some(&first),
        "created-mode-converged",
    )?;

    let second = spec(id, 1600, 900);
    exercise_legacy_bypasses(client, dir, baseline, id, owner, &first, &second)?;
    is_not_found(client.verify_owner(id, Some(wrong)))?;
    event(dir, "wrong-owner-check-rejected", json!({"id": id}))?;
    event(dir, "wrong-owner-replace-planned", json!(second))?;
    is_not_found(client.replace_owned(std::slice::from_ref(&second), wrong))?;
    event(dir, "wrong-owner-replace-rejected", json!({"id": id}))?;
    wait_for(
        client,
        dir,
        baseline,
        id,
        Some(&first),
        "wrong-owner-replace-no-effect",
    )?;
    event(dir, "wrong-owner-release-planned", json!({"id": id}))?;
    let release = client
        .remove_owned(&[id], wrong)
        .map_err(|e| e.to_string())?;
    if release != RemoveDisposition::NotOwned {
        return Err(format!(
            "wrong owner release unexpectedly returned {release:?}"
        ));
    }
    event(dir, "wrong-owner-release-rejected", json!({"id": id}))?;
    client
        .verify_owner(id, Some(owner))
        .map_err(|e| e.to_string())?;
    wait_for(
        client,
        dir,
        baseline,
        id,
        Some(&first),
        "wrong-owner-release-no-effect",
    )?;

    event(dir, "replace-planned", json!(second))?;
    client
        .replace_owned(std::slice::from_ref(&second), owner)
        .map_err(|e| e.to_string())?;
    wait_for(
        client,
        dir,
        baseline,
        id,
        Some(&second),
        "replaced-mode-converged",
    )?;
    client
        .verify_owner(id, Some(owner))
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn cleanup(client: &mut VddClient, dir: &Path, journal: &Value) -> TestResult<()> {
    let owner = journal["owner"]
        .as_str()
        .filter(|owner| valid_owner(owner))
        .ok_or("invalid journal owner")?;
    let id = journal["id"]
        .as_u64()
        .filter(|id| (4..16).contains(id))
        .ok_or("invalid journal ID")? as u32;
    if journal["schema"] != SCHEMA {
        return Err("unknown journal schema".into());
    }
    let baseline = &journal["baseline"];
    items(baseline, "driver")?;
    items(baseline, "desktop")?;
    let binding = (|| {
        let path = dir.join("release-identity.json");
        let saved = if path.exists() {
            let record: Value =
                serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            Some(validate_release_record(journal, &record)?)
        } else {
            None
        };
        let before = snapshot(client)?;
        let owner_confirmed = match client.verify_owner(id, Some(owner)) {
            Ok(()) => true,
            Err(VddError::Io(error)) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.to_string()),
        };
        let identity =
            bind_release_identity(baseline, &before, id, owner_confirmed, saved.as_ref())?;
        if saved.is_none()
            && let Some(identity) = &identity
        {
            write_new(
                &path,
                &json!({"schema": "snug-release-identity-v1", "owner": owner,
                "id": id, "baseline": baseline, "identity": identity}),
            )?;
        }
        let _ = event(
            dir,
            "cleanup-native-identity-bound",
            json!({"id": id, "identity": identity}),
        );
        Ok(identity)
    })();
    if let Err(error) = &binding {
        let _ = event(
            dir,
            "cleanup-native-identity-error",
            json!({"error": error}),
        );
    }
    let identity = release_after_binding(binding, || {
        // Logging and binding errors never suppress the owner-scoped cleanup.
        let _ = event(dir, "cleanup-release-planned", json!({"id": id}));
        let released = client
            .remove_owned(&[id], owner)
            .map_err(|error| error.to_string())?;
        if released == RemoveDisposition::SubmittedOnly {
            return Err("legacy cleanup is forbidden".into());
        }
        let _ = event(
            dir,
            "cleanup-release-reply",
            json!({"disposition": format!("{released:?}")}),
        );
        is_not_found(client.verify_owner(id, Some(owner)))
    })?;
    wait_for_release(client, dir, baseline, id, identity.as_ref())
}

fn run() -> TestResult<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h") {
        println!(
            "preflight RUN_DIR OWNER | smoke RUN_DIR OWNER WRONG_OWNER | cleanup RUN_DIR\nUse run-driver-hardware-acceptance.ps1. Smoke changes one vacant owner-scoped screen."
        );
        return Ok(());
    }
    let mode = args
        .get(1)
        .map(String::as_str)
        .ok_or("missing mode; use --help")?;
    if !matches!(mode, "preflight" | "smoke" | "cleanup") {
        return Err("unknown mode".into());
    }
    if !snug_win::is_elevated() {
        return Err(
            "an already elevated terminal is required; this tool never elevates or starts a broker"
                .into(),
        );
    }
    unsafe {
        SetThreadDpiAwarenessContext(-4);
    }
    let dir = PathBuf::from(args.get(2).ok_or("missing RUN_DIR")?);
    if !dir.is_absolute() {
        return Err("RUN_DIR must be absolute".into());
    }
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut client = VddClient::new(DEFAULT_PIPE_NAME);
    if mode == "cleanup" {
        let journal: Value = serde_json::from_slice(
            &fs::read(dir.join("owner-journal.json")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let result = cleanup(&mut client, &dir, &journal);
        event(
            &dir,
            "cleanup-finished",
            json!({"passed": result.is_ok(), "error": result.as_ref().err()}),
        )?;
        return result;
    }
    let owner = args
        .get(3)
        .filter(|value| valid_owner(value))
        .ok_or("OWNER must be 32 hex characters")?;
    let baseline = snapshot(&mut client)?;
    let id = vacant_id(&baseline)?;
    is_not_found(client.verify_owner(id, Some(owner)))?;
    let after = snapshot(&mut client)?;
    if baseline != after {
        return Err("read-only preflight observed changing topology".into());
    }
    event(
        &dir,
        "read-only-preflight-passed",
        json!({"id": id, "baseline": baseline}),
    )?;
    if mode == "preflight" {
        write_new(
            &dir.join("result.json"),
            &json!({"schema": SCHEMA, "mode": mode,
            "passed": true, "readOnly": true, "baseline": baseline}),
        )?;
        return Ok(());
    }
    let wrong = args
        .get(4)
        .filter(|value| valid_owner(value) && *value != owner)
        .ok_or("WRONG_OWNER must be a different 32 hex token")?;
    let journal = json!({"schema": SCHEMA, "owner": owner, "id": id,
        "wrongOwner": wrong, "createdMs": now_ms(), "baseline": baseline});
    // The recovery token and baseline are flushed before the first reservation.
    write_new(&dir.join("owner-journal.json"), &journal)?;
    let tested = exercise(&mut client, &dir, &baseline, id, owner, wrong);
    // Release is owner-scoped and idempotent, even after an unknown Create result.
    let cleaned = cleanup(&mut client, &dir, &journal);
    let passed = tested.is_ok() && cleaned.is_ok();
    write_new(
        &dir.join("result.json"),
        &json!({"schema": SCHEMA, "mode": mode,
        "passed": passed, "id": id, "exerciseError": tested.as_ref().err(),
        "cleanupError": cleaned.as_ref().err(), "cleanupPassed": cleaned.is_ok(),
        "noNativeDisplayWrites": true, "applicationStarted": false}),
    )?;
    event(&dir, "smoke-finished", json!({"passed": passed}))?;
    tested.and(cleaned)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "explicit read-only real-driver State probe"]
    fn read_only_raw_state_probe() {
        let replies = legacy_exchange(&json!("State")).unwrap();
        assert!(replies.last().unwrap()["State"].is_array());
    }
    #[test]
    fn legacy_cases_send_the_real_four_protocol_families() {
        let cases = legacy_cases(15, &spec(15, 1280, 720), &spec(15, 1600, 900));
        assert_eq!(cases.len(), 6);
        assert_eq!(cases[0].1["Notify"][0]["modes"][0]["width"], 1600);
        assert_eq!(cases[1].1, json!({"Notify":[]}));
        assert_eq!(cases[2].1["Notify"][0]["modes"][0]["width"], 1280);
        assert_eq!(cases[3].1, json!({"Remove":[15]}));
        assert_eq!(cases[4].1, json!("RemoveAll"));
        assert_eq!(cases[5].1["SnugUpdate"]["replacing"], true);
    }
    #[test]
    fn legacy_barrier_requires_preserved_state_and_explicit_update_rejection() {
        let expected = vec![json!(spec(15, 1280, 720))];
        let command = json!({"SnugUpdate":{"monitors":[spec(15,1600,900)],"replacing":true}});
        assert!(
            verify_legacy_reply(
                &command,
                &[json!({"SnugUpdate":"NotFound"}), json!({"State":expected})],
                &expected
            )
            .is_ok()
        );
        assert!(verify_legacy_reply(&command, &[json!({"State":expected})], &expected).is_err());
        assert!(
            verify_legacy_reply(
                &command,
                &[json!({"SnugUpdate":"Applied"}), json!({"State":expected})],
                &expected
            )
            .is_err()
        );
        assert!(
            verify_legacy_reply(&json!("RemoveAll"), &[json!({"State":[]})], &expected).is_err()
        );
        assert!(
            verify_legacy_reply(
                &json!({"Notify":[]}),
                &[json!({"State":expected})],
                &expected
            )
            .is_ok()
        );
    }
    fn baseline() -> Value {
        json!({"driver": [{"id": 0, "mode": "original"}],
            "desktop": [{"virtualId": null, "deviceName": "physical", "dpi": 100}]})
    }
    #[test]
    fn test_ids_leave_application_connectors_alone() {
        assert_eq!(vacant_id(&baseline()).unwrap(), 15);
        let occupied = json!({"driver": (4..16).map(|id| json!({"id": id})).collect::<Vec<_>>(), "desktop": []});
        assert!(vacant_id(&occupied).is_err());
    }
    #[test]
    fn selection_considers_desktop_even_if_driver_has_no_id() {
        assert_eq!(
            vacant_id(&json!({"driver": [], "desktop": [{"virtualId": 15}]})).unwrap(),
            14
        );
    }
    #[test]
    fn original_screen_changes_are_failures() {
        let mut current = baseline();
        current["desktop"][0]["dpi"] = json!(125);
        assert!(untouched(&baseline(), &current, 15).is_err());
        current = baseline();
        current["driver"][0]["mode"] = json!("changed");
        assert!(untouched(&baseline(), &current, 15).is_err());
    }
    #[test]
    fn only_this_runs_id_can_be_added_without_failure() {
        let mut current = baseline();
        current["driver"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": 15}));
        current["desktop"]
            .as_array_mut()
            .unwrap()
            .push(json!({"virtualId": 15}));
        assert!(untouched(&baseline(), &current, 15).is_ok());
        current["driver"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": 14}));
        assert!(untouched(&baseline(), &current, 15).is_err());
    }
    #[test]
    fn enumeration_order_does_not_change_comparison() {
        let initial = json!({"driver": [{"id": 0}, {"id": 1}], "desktop": []});
        let current = json!({"driver": [{"id": 1}, {"id": 0}], "desktop": []});
        assert!(untouched(&initial, &current, 15).is_ok());
    }
    #[test]
    fn wrong_owner_must_be_an_explicit_trusted_rejection() {
        assert!(is_not_found(Err(VddError::Io(io::Error::from(io::ErrorKind::NotFound)))).is_ok());
        assert!(is_not_found(Ok(())).is_err());
        assert!(
            is_not_found(Err(VddError::OutcomeUnknown {
                source: io::Error::from(io::ErrorKind::NotFound)
            }))
            .is_err()
        );
    }
    #[test]
    fn driver_acceptance_alone_does_not_prove_mode_convergence() {
        let requested = spec(15, 1280, 720);
        let mut current = json!({"driver": [requested], "desktop": []});
        assert!(!settled(&current, 15, Some(&spec(15, 1280, 720))).unwrap());
        current["desktop"] =
            json!([{"virtualId": 15, "width": 1280, "height": 720, "refreshHz": 60.0}]);
        assert!(settled(&current, 15, Some(&spec(15, 1280, 720))).unwrap());
        current["desktop"][0]["width"] = json!(1600);
        assert!(!settled(&current, 15, Some(&spec(15, 1280, 720))).unwrap());
    }
    fn release_originals() -> Value {
        json!({"driver": [], "desktop": [{"virtualId": null, "stableId": 100,
            "deviceName": "physical", "adapterLow": 1, "adapterHigh": 0,
            "sourceId": 0, "width": 2560, "height": 1440, "dpiPercent": 100}]})
    }
    fn release_live() -> Value {
        let mut current = release_originals();
        current["driver"] = json!([spec(15, 1600, 900)]);
        current["desktop"].as_array_mut().unwrap().push(json!({
            "virtualId": 15, "stableId": 200, "deviceName": "owned",
            "adapterLow": 2, "adapterHigh": -1, "sourceId": 0,
            "width": 1600, "height": 900, "dpiPercent": 100}));
        current
    }
    fn release_ghost() -> Value {
        let mut ghost = release_live();
        ghost["driver"] = json!([]);
        ghost["desktop"][1]["virtualId"] = Value::Null;
        ghost
    }
    fn bound_release() -> Value {
        bind_release_identity(&release_originals(), &release_live(), 15, true, None)
            .unwrap()
            .unwrap()
    }
    #[test]
    fn release_null_id_ghost_must_disappear_then_two_exact_frames() {
        let identity = bound_release();
        let mut consecutive = 0;
        assert!(
            !release_observation(
                &release_originals(),
                &release_ghost(),
                15,
                Some(&identity),
                &mut consecutive
            )
            .unwrap()
        );
        assert_eq!(consecutive, 0);
        assert!(
            !release_observation(
                &release_originals(),
                &release_originals(),
                15,
                Some(&identity),
                &mut consecutive
            )
            .unwrap()
        );
        assert_eq!(consecutive, 1);
        // A reappearing bound ghost invalidates the first exact frame.
        assert!(
            !release_observation(
                &release_originals(),
                &release_ghost(),
                15,
                Some(&identity),
                &mut consecutive
            )
            .unwrap()
        );
        assert_eq!(consecutive, 0);
        assert!(
            !release_observation(
                &release_originals(),
                &release_originals(),
                15,
                Some(&identity),
                &mut consecutive
            )
            .unwrap()
        );
        assert!(
            release_observation(
                &release_originals(),
                &release_originals(),
                15,
                Some(&identity),
                &mut consecutive
            )
            .unwrap()
        );
    }
    #[test]
    fn release_persistent_ghost_never_converges() {
        let identity = bound_release();
        let mut consecutive = 0;
        for _ in 0..40 {
            assert!(
                !release_observation(
                    &release_originals(),
                    &release_ghost(),
                    15,
                    Some(&identity),
                    &mut consecutive
                )
                .unwrap()
            );
        }
    }
    #[test]
    fn release_physical_or_unrelated_driver_changes_are_always_failures() {
        let identity = bound_release();
        let mut current = release_ghost();
        current["desktop"][0]["dpiPercent"] = json!(125);
        assert!(
            release_observation(&release_originals(), &current, 15, Some(&identity), &mut 0)
                .is_err()
        );
        current = release_ghost();
        current["driver"] = json!([{"id": 14}]);
        assert!(
            release_observation(&release_originals(), &current, 15, Some(&identity), &mut 0)
                .is_err()
        );
    }
    #[test]
    fn release_rejects_unrelated_extra_wrong_or_reassigned_native_identity() {
        let identity = bound_release();
        for (key, replacement) in [
            ("stableId", json!(201)),
            ("deviceName", json!("other")),
            ("adapterLow", json!(3)),
            ("adapterHigh", json!(0)),
            ("sourceId", json!(1)),
            ("virtualId", json!(14)),
        ] {
            let mut current = release_ghost();
            current["desktop"][1][key] = replacement;
            assert!(
                release_observation(&release_originals(), &current, 15, Some(&identity), &mut 0)
                    .is_err(),
                "accepted changed {key}"
            );
        }
        let mut current = release_ghost();
        current["desktop"]
            .as_array_mut()
            .unwrap()
            .push(json!({"virtualId": null,
            "stableId": 300, "deviceName": "unrelated", "adapterLow": 3,
            "adapterHigh": 0, "sourceId": 2}));
        assert!(
            release_observation(&release_originals(), &current, 15, Some(&identity), &mut 0)
                .is_err()
        );
    }
    #[test]
    fn release_rejects_duplicate_native_identity_and_missing_fields() {
        let identity = bound_release();
        let mut current = release_ghost();
        let duplicate = current["desktop"][1].clone();
        current["desktop"].as_array_mut().unwrap().push(duplicate);
        assert!(
            release_observation(&release_originals(), &current, 15, Some(&identity), &mut 0)
                .is_err()
        );
        for key in [
            "stableId",
            "deviceName",
            "adapterLow",
            "adapterHigh",
            "sourceId",
        ] {
            current = release_live();
            current["desktop"][1].as_object_mut().unwrap().remove(key);
            assert!(
                bind_release_identity(&release_originals(), &current, 15, true, None).is_err(),
                "accepted missing {key}"
            );
        }
        current = release_live();
        current["desktop"][1]["stableId"] = json!(100);
        assert!(bind_release_identity(&release_originals(), &current, 15, true, None).is_err());
    }
    #[test]
    fn release_binding_requires_confirmed_owner_and_unique_virtual_id() {
        assert!(
            bind_release_identity(&release_originals(), &release_live(), 15, false, None).is_err()
        );
        let mut current = release_live();
        let mut duplicate = current["desktop"][1].clone();
        duplicate["stableId"] = json!(300);
        duplicate["deviceName"] = json!("another");
        duplicate["adapterLow"] = json!(3);
        current["desktop"].as_array_mut().unwrap().push(duplicate);
        assert!(bind_release_identity(&release_originals(), &current, 15, true, None).is_err());
    }
    #[test]
    fn release_reservation_only_and_normal_release_require_two_exact_frames() {
        let reservation = json!({"driver": [], "desktop": release_originals()["desktop"]});
        let identity =
            bind_release_identity(&release_originals(), &reservation, 15, true, None).unwrap();
        assert!(identity.is_none());
        let mut consecutive = 0;
        assert!(
            !release_observation(
                &release_originals(),
                &reservation,
                15,
                None,
                &mut consecutive
            )
            .unwrap()
        );
        assert!(
            release_observation(
                &release_originals(),
                &reservation,
                15,
                None,
                &mut consecutive
            )
            .unwrap()
        );
        assert!(
            bind_release_identity(&release_originals(), &release_ghost(), 15, false, None).is_err()
        );
        let identity = bound_release();
        consecutive = 0;
        assert!(
            !release_observation(
                &release_originals(),
                &release_originals(),
                15,
                Some(&identity),
                &mut consecutive
            )
            .unwrap()
        );
        assert!(
            release_observation(
                &release_originals(),
                &release_originals(),
                15,
                Some(&identity),
                &mut consecutive
            )
            .unwrap()
        );
    }
    #[test]
    fn release_observation_or_persistence_failure_never_suppresses_scoped_cleanup() {
        for error in [
            "snapshot failed",
            "owner verify failed",
            "persist failed",
            "corrupt saved identity",
        ] {
            let mut releases = 0;
            let result = release_after_binding::<()>(Err(error.into()), || {
                releases += 1;
                Ok(())
            });
            assert_eq!(releases, 1);
            assert_eq!(result.unwrap_err(), error);
        }
        let result = release_after_binding::<()>(Err("binding failed".into()), || {
            Err("release failed".into())
        });
        assert_eq!(result.unwrap_err(), "release failed");
    }
    #[test]
    fn release_retry_record_binds_schema_owner_id_baseline_and_full_identity() {
        let journal = json!({"owner": "0123456789abcdef0123456789abcdef", "id": 15,
            "baseline": release_originals()});
        let record = json!({"schema": "snug-release-identity-v1", "owner": journal["owner"],
            "id": 15, "baseline": release_originals(), "identity": bound_release()});
        let identity = validate_release_record(&journal, &record).unwrap();
        assert_eq!(
            bind_release_identity(
                &release_originals(),
                &release_ghost(),
                15,
                false,
                Some(&identity)
            )
            .unwrap(),
            Some(identity.clone())
        );
        assert_eq!(
            bind_release_identity(
                &release_originals(),
                &release_originals(),
                15,
                false,
                Some(&identity)
            )
            .unwrap(),
            Some(identity)
        );
        for (key, bad) in [
            ("schema", json!("wrong")),
            ("owner", json!("foreign")),
            ("id", json!(14)),
            ("baseline", json!({"driver":[],"desktop":[]})),
            ("identity", json!({"stableId":200})),
        ] {
            let mut corrupted = record.clone();
            corrupted[key] = bad;
            assert!(
                validate_release_record(&journal, &corrupted).is_err(),
                "accepted corrupt {key}"
            );
        }
    }
    #[test]
    fn recovery_owner_is_strictly_validated() {
        assert!(valid_owner("0123456789abcdef0123456789abcdef"));
        assert!(!valid_owner("0123456789abcdef0123456789abcdeg"));
        assert!(!valid_owner("short"));
    }
}
