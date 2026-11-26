//! Read-only WDDM hardware video-encoding counters for the current UU server.

use std::cell::RefCell;
use std::mem::size_of;
use std::time::{Duration, Instant};

use windows::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
    PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery,
    PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::core::{PCWSTR, w};

const MAX_COUNTER_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct CounterState {
    counter: Option<EngineCounter>,
    retry_after: Option<Instant>,
}

thread_local! {
    static COUNTER: RefCell<CounterState> = RefCell::new(CounterState::default());
}

pub(super) fn sample_video_encode(pid: i32) -> bool {
    COUNTER.with(|state| {
        let mut state = state.borrow_mut();
        if state.counter.is_none() {
            let now = Instant::now();
            if state.retry_after.is_some_and(|retry| now < retry) {
                return false;
            }
            state.counter = EngineCounter::open();
            state.retry_after = Some(now + Duration::from_secs(15));
        }
        state
            .counter
            .as_mut()
            .is_some_and(|counter| counter.sample(pid))
    })
}

struct EngineCounter {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    buffer: Vec<PDH_FMT_COUNTERVALUE_ITEM_W>,
}

impl EngineCounter {
    fn open() -> Option<Self> {
        let mut query = PDH_HQUERY::default();
        // SAFETY: PDH initializes the out handle. The RAII object owns it once opened.
        if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } != 0 {
            return None;
        }
        let mut result = Self {
            query,
            counter: PDH_HCOUNTER::default(),
            buffer: Vec::new(),
        };
        // SAFETY: The query is live and the NUL-terminated counter path is static.
        let status = unsafe {
            PdhAddEnglishCounterW(
                result.query,
                w!(r"\GPU Engine(*)\Utilization Percentage"),
                0,
                &mut result.counter,
            )
        };
        (status == 0).then_some(result)
    }

    fn sample(&mut self, pid: i32) -> bool {
        let item_size = size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let mut needed = 0;
        let mut count = 0;
        // SAFETY: PDH owns the query and counter handles. A null buffer queries its size.
        unsafe {
            if PdhCollectQueryData(self.query) != 0
                || PdhGetFormattedCounterArrayW(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    &mut needed,
                    &mut count,
                    None,
                ) != PDH_MORE_DATA
                || needed as usize > MAX_COUNTER_BYTES
                || needed == 0
            {
                return false;
            }
        }
        self.buffer.resize(
            (needed as usize).div_ceil(item_size),
            PDH_FMT_COUNTERVALUE_ITEM_W::default(),
        );
        // SAFETY: The vector provides aligned writable storage for the byte count requested by PDH.
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut needed,
                &mut count,
                Some(self.buffer.as_mut_ptr()),
            )
        };
        if status != 0 || count as usize > self.buffer.len() {
            return false;
        }
        let prefix = format!("pid_{pid}_");
        self.buffer[..count as usize].iter().any(|item| {
            if !matches!(
                item.FmtValue.CStatus,
                PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA
            ) || item.szName.is_null()
            {
                return false;
            }
            // SAFETY: On success PDH returns the requested double and a NUL-terminated
            // string in the same live byte buffer.
            let utilization = unsafe { item.FmtValue.Anonymous.doubleValue };
            utilization > 0.0
                && unsafe { item.szName.to_string() }
                    .is_ok_and(|name| is_video_encode_instance(&name, &prefix))
        })
    }
}

impl Drop for EngineCounter {
    fn drop(&mut self) {
        // SAFETY: The query is owned by this object. Closing it releases its counters.
        unsafe { PdhCloseQuery(self.query) };
    }
}

fn is_video_encode_instance(name: &str, pid_prefix: &str) -> bool {
    name.starts_with(pid_prefix) && name.ends_with("_engtype_VideoEncode")
}

/// Require consecutive samples before admission. Keep brief zero-use intervals
/// from interrupting active streams when the remote image is static.
#[derive(Default)]
pub(super) struct EncodingActivity {
    pid: Option<i32>,
    first_positive: Option<f64>,
    last_positive: Option<f64>,
    positive_count: u8,
    active: bool,
}

impl EncodingActivity {
    const MIN_SAMPLE_COUNT: u8 = 3;
    const MIN_ACTIVE_SPAN: f64 = 2.0;
    const MAX_SAMPLE_GAP: f64 = 3.0;

    pub(super) fn observe(&mut self, pid: Option<i32>, encoding: bool, now: f64) -> bool {
        let recent = self
            .last_positive
            .is_some_and(|last| now > last && now - last <= Self::MAX_SAMPLE_GAP);
        if self.pid != pid || !recent || (!encoding && !self.active) {
            *self = Self {
                pid,
                ..Self::default()
            };
        }
        if pid.is_some() && encoding {
            let first = *self.first_positive.get_or_insert(now);
            self.positive_count = self.positive_count.saturating_add(1);
            self.last_positive = Some(now);
            self.active = self.positive_count >= Self::MIN_SAMPLE_COUNT
                && now - first >= Self::MIN_ACTIVE_SPAN;
        }
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_video_encode_for_the_server_pid() {
        assert!(is_video_encode_instance(
            "pid_193264_luid_0x0000_0x010F_phys_0_eng_6_engtype_VideoEncode",
            "pid_193264_"
        ));
        assert!(!is_video_encode_instance(
            "pid_193264_luid_0x0000_0x010F_phys_0_eng_3_engtype_3D",
            "pid_193264_"
        ));
        assert!(!is_video_encode_instance(
            "pid_1932640_luid_0x0000_0x010F_phys_0_eng_6_engtype_VideoEncode",
            "pid_193264_"
        ));
    }

    #[test]
    fn admission_needs_sustained_consecutive_encoding_for_one_server() {
        let mut encoding = EncodingActivity::default();
        let pid = Some(42);
        assert!(!encoding.observe(pid, true, 0.0));
        assert!(!encoding.observe(pid, true, 0.1));
        assert!(!encoding.observe(pid, true, 0.2));
        assert!(!encoding.observe(pid, false, 0.5));
        assert!(!encoding.observe(pid, true, 1.0));
        assert!(!encoding.observe(pid, true, 3.0));
        assert!(encoding.observe(pid, true, 3.5));
        assert!(encoding.observe(pid, false, 6.0));
        assert!(!encoding.observe(pid, false, 6.6));
        assert!(!encoding.observe(pid, true, 7.0));
        assert!(!encoding.observe(pid, true, 8.0));
        assert!(encoding.observe(pid, true, 9.0));
        assert!(!encoding.observe(Some(43), true, 9.5));
        assert!(!encoding.observe(None, true, 10.0));
    }
}
