#![allow(
    dead_code,
    non_snake_case,
    non_camel_case_types,
    unused_variables,
    unused_unsafe
)]
extern crate std as real_std;
use real_std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

macro_rules! info { ($($t:tt)*) => { let _ = format_args!($($t)*); } }

static CLOCK_MS: AtomicUsize = AtomicUsize::new(0);
static SLEEPS: AtomicUsize = AtomicUsize::new(0);
static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
static DEVICE_FAILURES: AtomicUsize = AtomicUsize::new(0);

// Only the retry thread's clock/sleep/spawn are fakes. No driver or WDF API runs.
mod std {
    pub mod thread {
        pub fn spawn(callback: impl FnOnce()) {
            callback();
        }
        pub fn sleep(duration: super::super::Duration) {
            super::super::SLEEPS.fetch_add(1, super::super::Ordering::SeqCst);
            super::super::CLOCK_MS.fetch_add(
                duration.as_millis() as usize,
                super::super::Ordering::SeqCst,
            );
        }
    }
}
struct Instant(usize);
impl Instant {
    fn now() -> Self {
        Self(CLOCK_MS.load(Ordering::SeqCst))
    }
    fn elapsed(&self) -> Duration {
        Duration::from_millis((CLOCK_MS.load(Ordering::SeqCst) - self.0) as u64)
    }
}
#[derive(Clone, Copy)]
struct NTSTATUS(bool);
impl NTSTATUS {
    fn is_success(self) -> bool {
        self.0
    }
}
struct Sendable(*mut u8);
impl Sendable {
    fn cast(&self) -> *mut u8 {
        self.0
    }
}
enum WDF_DEVICE_FAILED_ACTION {
    WdfDeviceFailedNoRestart,
}
unsafe fn WdfDeviceSetFailed(_: *mut u8, _: WDF_DEVICE_FAILED_ACTION) {
    DEVICE_FAILURES.fetch_add(1, Ordering::SeqCst);
}
fn run_retry(success_on: usize, success_reaches_timeout: bool) {
    CLOCK_MS.store(0, Ordering::SeqCst);
    SLEEPS.store(0, Ordering::SeqCst);
    ATTEMPTS.store(0, Ordering::SeqCst);
    DEVICE_FAILURES.store(0, Ordering::SeqCst);
    let device = Sendable(real_std::ptr::NonNull::<u8>::dangling().as_ptr());
    let init_log = move || {
        let attempt = ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
        let success = attempt == success_on;
        if success && success_reaches_timeout {
            CLOCK_MS.store(300_000, Ordering::SeqCst);
        }
        assert!(attempt <= 601, "retry must terminate at its deadline");
        NTSTATUS(success)
    };
    // RETRY_BODY
}

// PRODUCTION_ITEMS

fn feed(message: &mut Vec<u8>, bytes: &[u8], frames: &mut Vec<Vec<u8>>) -> Result<(), ()> {
    let mut input = bytes;
    while !input.is_empty() {
        if buffer_message(message, &mut input)? {
            assert_eq!(message.pop(), Some(EOF as u8));
            frames.push(message.clone());
            message.clear();
        }
    }
    Ok(())
}

#[test]
fn unterminated_input_is_rejected_before_buffer_growth() {
    let mut message = Vec::new();
    let chunk = vec![b'x'; 4096];
    for _ in 0..MAX_MESSAGE_SIZE / chunk.len() {
        assert!(!buffer_message(&mut message, &mut &chunk[..]).unwrap());
    }
    assert_eq!(message.len(), MAX_MESSAGE_SIZE);
    assert_eq!(buffer_message(&mut message, &mut &b"x"[..]), Err(()));
    assert_eq!(message.len(), MAX_MESSAGE_SIZE);
}

#[test]
fn oversized_complete_frame_is_rejected_without_appending() {
    let mut message = vec![b'x'; MAX_MESSAGE_SIZE - 2];
    let before = message.len();
    assert_eq!(buffer_message(&mut message, &mut &b"xxx\x04"[..]), Err(()));
    assert_eq!(message.len(), before);
}

#[test]
fn exact_limit_payload_accepts_a_separate_delimiter() {
    let mut message = vec![b'x'; MAX_MESSAGE_SIZE];
    let mut frames = Vec::new();
    assert_eq!(feed(&mut message, b"\x04", &mut frames), Ok(()));
    assert_eq!(frames[0].len(), MAX_MESSAGE_SIZE);
    assert!(message.is_empty());
}

#[test]
fn coalesced_frames_use_individual_limits_and_keep_partial_tail() {
    let mut bytes = vec![b'a'; MAX_MESSAGE_SIZE];
    bytes.push(EOF as u8);
    bytes.extend(vec![b'b'; MAX_MESSAGE_SIZE]);
    bytes.extend_from_slice(b"\x04tail");
    let mut message = Vec::new();
    let mut frames = Vec::new();
    assert_eq!(feed(&mut message, &bytes, &mut frames), Ok(()));
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], vec![b'a'; MAX_MESSAGE_SIZE]);
    assert_eq!(frames[1], vec![b'b'; MAX_MESSAGE_SIZE]);
    assert_eq!(message, b"tail");
    feed(&mut message, b"end\x04\x04last\x04", &mut frames).unwrap();
    assert_eq!(
        &frames[2..],
        &[b"tailend".to_vec(), Vec::new(), b"last".to_vec()]
    );
}

#[test]
fn oversized_later_frame_preserves_prior_completed_frame() {
    let mut bytes = b"valid\x04".to_vec();
    bytes.extend(vec![b'x'; MAX_MESSAGE_SIZE + 1]);
    let mut frames = Vec::new();
    assert_eq!(feed(&mut Vec::new(), &bytes, &mut frames), Err(()));
    assert_eq!(frames, vec![b"valid".to_vec()]);
}

#[test]
fn current_maximum_monitor_request_fits_in_frame_limit() {
    // All 16 monitors, 256 distinct large modes each, four 3-digit rates,
    // 256-byte names escaped as six-byte JSON codepoints, and an owner token.
    let mut request = String::from(
        "{\"SnugCreate\":{\"owner\":\"0123456789abcdef0123456789abcdef\",\"monitors\":[",
    );
    for id in 0..16 {
        if id != 0 {
            request.push(',');
        }
        request.push_str(&format!(
            "{{\"id\":{id},\"name\":\"{}\",\"enabled\":false,\"modes\":[",
            "\\u0001".repeat(256)
        ));
        for index in 0..256 {
            if index != 0 {
                request.push(',');
            }
            request.push_str(&format!(
                "{{\"width\":16384,\"height\":{},\"refresh_rates\":[237,238,239,240]}}",
                16384 - index
            ));
        }
        request.push_str("]}");
    }
    request.push_str("]}}\x04");
    assert!(request.len() > 128 * 1024);
    assert!(request.len() < MAX_MESSAGE_SIZE);
    let mut frames = Vec::new();
    let mut message = Vec::new();
    for chunk in request.as_bytes().chunks(4096) {
        feed(&mut message, chunk, &mut frames).unwrap();
    }
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0], &request.as_bytes()[..request.len() - 1]);
    assert!(message.is_empty());
}

#[test]
fn log_timeout_stops_retries_without_failing_a_device() {
    run_retry(usize::MAX, false);
    assert_eq!(CLOCK_MS.load(Ordering::SeqCst), 300_000);
    assert_eq!(DEVICE_FAILURES.load(Ordering::SeqCst), 0);
}

#[test]
fn immediate_log_success_needs_no_extra_sleep() {
    run_retry(1, false);
    assert_eq!(ATTEMPTS.load(Ordering::SeqCst), 1);
    assert_eq!(SLEEPS.load(Ordering::SeqCst), 0);
    assert_eq!(DEVICE_FAILURES.load(Ordering::SeqCst), 0);
}

#[test]
fn log_success_on_deadline_wins_over_timeout() {
    run_retry(600, true);
    assert_eq!(ATTEMPTS.load(Ordering::SeqCst), 600);
    assert_eq!(SLEEPS.load(Ordering::SeqCst), 599);
    assert_eq!(DEVICE_FAILURES.load(Ordering::SeqCst), 0);
}
