//! Durable intent for relative input. A pending intent is never replayed automatically.
use crate::{record::PhoneRecord, windows::WindowKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum ZoomSource {
    Screen(String),
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum ZoomResult {
    Pending,
    Applied,
    NotApplied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ZoomOperation {
    pub source: ZoomSource,
    pub before: PhoneRecord,
    pub after: PhoneRecord,
    pub target: WindowKey,
    pub result: ZoomResult,
}

impl ZoomOperation {
    pub fn resolved_record(&self) -> PhoneRecord {
        let mut record = self.before.clone();
        record.zoom = 0;
        record
    }

    pub fn matches(&self, record: &PhoneRecord) -> bool {
        *record == self.before || *record == self.after || *record == self.resolved_record()
    }
}
