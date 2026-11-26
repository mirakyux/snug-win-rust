use serde::{Deserialize, Serialize};

pub type Id = u32;
pub type Dimen = u32;
pub type RefreshRate = u32;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, PartialOrd)]
pub struct Monitor {
    // identifier
    pub id: Id,
    pub name: Option<String>,
    pub enabled: bool,
    pub modes: Vec<Mode>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, PartialOrd)]
pub struct Mode {
    pub width: Dimen,
    pub height: Dimen,
    pub refresh_rates: Vec<RefreshRate>,
}

#[non_exhaustive]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum DriverCommand {
    // Single line of communication client->server
    // Driver commands
    //
    // Notify of monitor changes (whether adding or updating)
    Notify(Vec<Monitor>),
    // Remove a monitor from system
    Remove(Vec<Id>),
    // Remove all monitors from system
    RemoveAll,
}

/// Request command sent from client->server
#[non_exhaustive]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum RequestCommand {
    // Request information on the current system monitor state
    State,
    // Atomically create or replace only these monitors, preserving all other IDs.
    SnugUpdate {
        monitors: Vec<Monitor>,
        replacing: bool,
    },
    // Check accepted monitor ownership without reserving, refreshing or changing it.
    SnugCheck {
        owner: String,
        id: Id,
    },
    // Reserve without changing topology. Creation and release require the same
    // token, so an acknowledgement failure never grants ownership of another ID.
    SnugReserve {
        owner: String,
        id: Id,
    },
    SnugCreate {
        owner: String,
        monitors: Vec<Monitor>,
    },
    SnugReplace {
        owner: String,
        monitors: Vec<Monitor>,
    },
    SnugRelease {
        owner: String,
        ids: Vec<Id>,
    },
}

/// Reply command sent from server->client
#[non_exhaustive]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum ReplyCommand {
    // Reply to previous current system monitor state request
    State(Vec<Monitor>),
    SnugUpdate(SnugUpdateResult),
    SnugCheck {
        owner: String,
        result: SnugUpdateResult,
    },
    SnugReserve {
        owner: String,
        result: SnugUpdateResult,
    },
    SnugCreate {
        owner: String,
        result: SnugUpdateResult,
    },
    SnugReplace {
        owner: String,
        result: SnugUpdateResult,
    },
    SnugRelease {
        owner: String,
        result: SnugUpdateResult,
    },
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub enum SnugUpdateResult {
    Applied,
    AlreadyExists,
    NotFound,
    InvalidData,
    // A physical operation failed while replacing existing monitor IDs.
    ApplyFailed,
}

/// Validate the monitor data shared by incremental and legacy full updates.
pub fn validate_monitors(monitors: &[Monitor]) -> Result<(), SnugUpdateResult> {
    if monitors.len() > 16 {
        return Err(SnugUpdateResult::InvalidData);
    }
    for (index, monitor) in monitors.iter().enumerate() {
        if monitor.id >= 16
            || monitor
                .name
                .as_ref()
                .is_some_and(|name| name.len() > 256 || name.contains('\0'))
            || monitors[..index]
                .iter()
                .any(|previous| previous.id == monitor.id)
            || monitor.modes.is_empty()
            || monitor.modes.len() > 256
            || monitor.modes.iter().enumerate().any(|(index, mode)| {
                !(1..=16384).contains(&mode.width)
                    || !(1..=16384).contains(&mode.height)
                    || mode.refresh_rates.is_empty()
                    || mode.refresh_rates.len() > 4
                    || mode.refresh_rates.iter().enumerate().any(|(index, rate)| {
                        !(1..=240).contains(rate) || mode.refresh_rates[..index].contains(rate)
                    })
                    || monitor.modes[..index].iter().any(|previous| {
                        previous.width == mode.width && previous.height == mode.height
                    })
            })
        {
            return Err(SnugUpdateResult::InvalidData);
        }
    }
    Ok(())
}

/// Prepare an update while the caller holds the driver's monitor-state lock.
/// No partial result is returned when any entry is invalid or conflicts.
pub fn merge_snug_update(
    current: &[Monitor],
    updates: Vec<Monitor>,
    replacing: bool,
) -> Result<Vec<Monitor>, SnugUpdateResult> {
    if current.len() > 16 {
        return Err(SnugUpdateResult::InvalidData);
    }
    validate_monitors(&updates)?;
    for monitor in &updates {
        let exists = current.iter().any(|existing| existing.id == monitor.id);
        if exists && !replacing {
            return Err(SnugUpdateResult::AlreadyExists);
        }
        if !exists && replacing {
            return Err(SnugUpdateResult::NotFound);
        }
    }

    let mut merged = current.to_vec();
    for monitor in updates {
        if let Some(existing) = merged.iter_mut().find(|existing| existing.id == monitor.id) {
            *existing = monitor;
        } else {
            merged.push(monitor);
        }
    }
    if merged.len() > 16 {
        return Err(SnugUpdateResult::InvalidData);
    }
    Ok(merged)
}

/// An event happened
#[non_exhaustive]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum EventCommand {
    // Monitor state was changed while client was connected
    Changed(Vec<Monitor>),
}

/// An untagged enum of commands to be used with deserialization.
/// This makes the deserialization process much easier to handle
/// when a received command could be of multiple types
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerCommand {
    Driver(DriverCommand),
    Request(RequestCommand),
}

/// An untagged enum of commands to be used with deserialization.
/// This makes the deserialization process much easier to handle
/// when a received command could be of multiple types
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClientCommand {
    Reply(ReplyCommand),
    Event(EventCommand),
}

#[cfg(test)]
mod snug_update_tests {
    use super::*;

    fn monitor(id: u32) -> Monitor {
        Monitor {
            id,
            name: None,
            enabled: true,
            modes: vec![Mode {
                width: 1920,
                height: 1080,
                refresh_rates: vec![60],
            }],
        }
    }

    #[test]
    fn snug_update_wire_request_is_supported() {
        let request = r#"{"SnugUpdate":{"monitors":[],"replacing":false}}"#;
        assert!(serde_json::from_str::<ServerCommand>(request).is_ok());
    }

    #[test]
    fn snug_update_wire_reply_is_supported() {
        for outcome in [
            SnugUpdateResult::Applied,
            SnugUpdateResult::AlreadyExists,
            SnugUpdateResult::NotFound,
            SnugUpdateResult::InvalidData,
            SnugUpdateResult::ApplyFailed,
        ] {
            let reply = serde_json::to_string(&ReplyCommand::SnugUpdate(outcome)).unwrap();
            assert_eq!(reply, format!(r#"{{"SnugUpdate":"{outcome:?}"}}"#));
            assert!(serde_json::from_str::<ClientCommand>(&reply).is_ok());
        }
    }

    #[test]
    fn ownership_check_wire_is_distinct_from_a_reservation() {
        let request = r#"{"SnugCheck":{"owner":"0123456789abcdef0123456789abcdef","id":1}}"#;
        let decoded: ServerCommand = serde_json::from_str(request).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), request);
        for result in ["Applied", "NotFound", "InvalidData"] {
            let reply = format!(
                r#"{{"SnugCheck":{{"owner":"0123456789abcdef0123456789abcdef","result":"{result}"}}}}"#
            );
            let decoded: ClientCommand = serde_json::from_str(&reply).unwrap();
            assert_eq!(serde_json::to_string(&decoded).unwrap(), reply);
        }
    }

    #[test]
    fn owned_commands_preserve_token_and_result_on_wire() {
        let owner = "0123456789abcdef0123456789abcdef".to_string();
        let requests = [
            RequestCommand::SnugCheck {
                owner: owner.clone(),
                id: 1,
            },
            RequestCommand::SnugReserve {
                owner: owner.clone(),
                id: 1,
            },
            RequestCommand::SnugCreate {
                owner: owner.clone(),
                monitors: vec![monitor(1)],
            },
            RequestCommand::SnugReplace {
                owner: owner.clone(),
                monitors: vec![monitor(1)],
            },
            RequestCommand::SnugRelease {
                owner: owner.clone(),
                ids: vec![1],
            },
        ];
        for request in requests {
            let json = serde_json::to_string(&request).unwrap();
            let decoded: ServerCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&decoded).unwrap(), json);
            assert!(json.contains(&owner));
        }
        for result in [
            SnugUpdateResult::Applied,
            SnugUpdateResult::AlreadyExists,
            SnugUpdateResult::NotFound,
            SnugUpdateResult::InvalidData,
            SnugUpdateResult::ApplyFailed,
        ] {
            for reply in [
                ReplyCommand::SnugCheck {
                    owner: owner.clone(),
                    result,
                },
                ReplyCommand::SnugReserve {
                    owner: owner.clone(),
                    result,
                },
                ReplyCommand::SnugCreate {
                    owner: owner.clone(),
                    result,
                },
                ReplyCommand::SnugReplace {
                    owner: owner.clone(),
                    result,
                },
                ReplyCommand::SnugRelease {
                    owner: owner.clone(),
                    result,
                },
            ] {
                let json = serde_json::to_string(&reply).unwrap();
                let decoded: ClientCommand = serde_json::from_str(&json).unwrap();
                assert_eq!(serde_json::to_string(&decoded).unwrap(), json);
                assert!(json.contains(&owner));
                assert!(json.contains(&format!("{result:?}")));
            }
        }
    }

    #[test]
    fn snug_update_keeps_a_display_created_after_an_old_snapshot() {
        let old_snapshot = vec![monitor(0)];
        let after_b = merge_snug_update(&old_snapshot, vec![monitor(1)], false).unwrap();
        let after_a = merge_snug_update(&after_b, vec![monitor(2)], false).unwrap();
        assert_eq!(after_a, vec![monitor(0), monitor(1), monitor(2)]);
    }

    #[test]
    fn snug_update_rejects_occupied_disabled_ids_without_partial_changes() {
        let mut disabled = monitor(1);
        disabled.enabled = false;
        let current = vec![monitor(0), disabled];
        assert_eq!(
            merge_snug_update(&current, vec![monitor(2), monitor(1)], false),
            Err(SnugUpdateResult::AlreadyExists)
        );
        assert_eq!(current.len(), 2);
        assert!(!current[1].enabled);
    }

    #[test]
    fn snug_update_replace_requires_all_ids_and_preserves_other_entries() {
        let current = vec![monitor(0), monitor(1)];
        let mut updated = monitor(0);
        updated.modes[0].width = 1440;
        assert_eq!(
            merge_snug_update(&current, vec![updated.clone(), monitor(2)], true),
            Err(SnugUpdateResult::NotFound)
        );
        assert_eq!(
            merge_snug_update(&current, vec![updated.clone()], true).unwrap(),
            vec![updated, monitor(1)]
        );
    }

    #[test]
    fn snug_update_rejects_invalid_batches() {
        let mut duplicate_mode = monitor(0);
        duplicate_mode.modes.push(duplicate_mode.modes[0].clone());
        let mut duplicate_rate = monitor(0);
        duplicate_rate.modes[0].refresh_rates.push(60);
        let mut zero_dimension = monitor(0);
        zero_dimension.modes[0].height = 0;
        for updates in [
            vec![monitor(0), monitor(0)],
            vec![duplicate_mode],
            vec![duplicate_rate],
            vec![zero_dimension],
        ] {
            assert_eq!(
                merge_snug_update(&[], updates, false),
                Err(SnugUpdateResult::InvalidData)
            );
        }
    }

    #[test]
    fn snug_update_accepts_supported_connector_and_timing_limits() {
        let monitors: Vec<_> = (0..16)
            .map(|id| {
                let mut monitor = monitor(id);
                monitor.name = Some("a".repeat(256));
                monitor.modes = vec![Mode {
                    width: 16384,
                    height: 16384,
                    refresh_rates: vec![60, 120, 144, 240],
                }];
                monitor
            })
            .collect();
        assert!(validate_monitors(&monitors).is_ok());
        assert_eq!(
            merge_snug_update(&[], monitors.clone(), false).unwrap(),
            monitors
        );
        let mut many_modes = monitor(0);
        many_modes.modes = (1..=256)
            .map(|width| Mode {
                width,
                height: 16384,
                refresh_rates: vec![240],
            })
            .collect();
        assert!(validate_monitors(&[many_modes]).is_ok());
    }

    #[test]
    fn snug_update_rejects_hardware_and_resource_limits_before_mutation() {
        let mut variants = Vec::new();
        let mut unsupported = monitor(16);
        variants.push(unsupported);
        unsupported = monitor(0);
        unsupported.modes[0].width = 16385;
        variants.push(unsupported);
        unsupported = monitor(0);
        unsupported.modes[0].height = 16385;
        variants.push(unsupported);
        unsupported = monitor(0);
        unsupported.modes[0].refresh_rates = vec![241];
        variants.push(unsupported);
        unsupported = monitor(0);
        unsupported.modes[0].refresh_rates = vec![1, 2, 3, 4, 5];
        variants.push(unsupported);
        unsupported = monitor(0);
        unsupported.modes = (1..=257)
            .map(|width| Mode {
                width,
                height: 1080,
                refresh_rates: vec![60],
            })
            .collect();
        variants.push(unsupported);
        unsupported = monitor(0);
        unsupported.name = Some("a".repeat(257));
        variants.push(unsupported);
        unsupported = monitor(0);
        unsupported.name = Some("a\0b".into());
        variants.push(unsupported);
        unsupported = monitor(0);
        unsupported.modes.clear();
        variants.push(unsupported);
        unsupported = monitor(0);
        unsupported.modes[0].refresh_rates.clear();
        variants.push(unsupported);
        for monitor in variants {
            assert_eq!(
                validate_monitors(std::slice::from_ref(&monitor)),
                Err(SnugUpdateResult::InvalidData)
            );
            assert_eq!(
                merge_snug_update(&[], vec![monitor], false),
                Err(SnugUpdateResult::InvalidData)
            );
        }
        assert_eq!(
            validate_monitors(&vec![monitor(0); 17]),
            Err(SnugUpdateResult::InvalidData)
        );
    }

    #[test]
    fn snug_update_concurrent_creates_keep_both_ids_and_reject_a_duplicate() {
        use std::sync::{Arc, Barrier, Mutex};
        let state = Arc::new(Mutex::new(vec![monitor(0)]));
        let start = Arc::new(Barrier::new(3));
        let handles = [1, 2].map(|id| {
            let state = state.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                let mut locked = state.lock().unwrap();
                *locked = merge_snug_update(&locked, vec![monitor(id)], false).unwrap();
            })
        });
        start.wait();
        for handle in handles {
            handle.join().unwrap();
        }
        let locked = state.lock().unwrap();
        let mut ids: Vec<_> = locked.iter().map(|monitor| monitor.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![0, 1, 2]);
        assert_eq!(
            merge_snug_update(&locked, vec![monitor(1)], false),
            Err(SnugUpdateResult::AlreadyExists)
        );
    }
}
