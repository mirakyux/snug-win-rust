//! PhoneDesk 本体（第一段：构造、状态推送、加屏 / 撤屏 / 重试）。
//!
//! 对应 C# 类 `SnugWin.Core.PhoneScreen.PhoneDesk`。整份文件按小块移植，这里覆盖
//! 构造函数、`State` 属性与事件、串行队列接线，以及"手机屏本身"这一组逻辑：
//! `ReconcileAsync`、`AddScreenAsync`、`RemoveScreenAsync`、`RetireDisplayAsync`、
//! `CaptureSurroundingsAsync`、`SettleAsync`、`ScheduleRetry`/`CancelRetry`、
//! `SettleDisplays`、`ShutdownAsync`、`IdleAsync`。
//!
//! 线程模型（与 C# 的 `SerialLane` 对应）：
//! - 一个工作线程独占 [`DeskCore`]（C# 里的 `_state`、`_display`、`_placement` 等字段），
//!   动作按入队顺序执行；
//! - [`Desk`] 句柄可克隆，公开方法只入队，不阻塞调用方；
//! - 平台接口都是 `Send + Sync` 的 `&self` 方法，watch 线程可以并发查询；
//! - 状态通过 [`DeskEvents::state_changed`] 推给界面，`Desk::state()` 随时可读快照。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::geometry::Rect;
use crate::paths::Paths;
use crate::record::PhoneRecord;
use crate::screen_fit::ScreenFit;
use crate::store::RestoreRecordStore;
use crate::windows::{DisplayModeInfo, OtherDisplay, WindowKey};

use super::lane::SerialLane;
use super::model::{DeskGone, DeskState, PhoneScreenPhase};
use super::services::{
    AppControl, CancelToken, PhoneDisplays, SystemTimeline, Timeline, WindowControl,
};

/// C# `PhoneScreenSpec.MinimumLifetime`：手机屏建出来至少存在 2.5 秒，
/// 否则刚加就撤会让系统拓扑抖动。
pub const MINIMUM_LIFETIME: f64 = 2.5;

/// C# `TopologySettle`：改拓扑后等系统稳定。
const TOPOLOGY_SETTLE: f64 = 1.0;
/// C# `RetryDelays`。
const RETRY_DELAYS: [f64; 3] = [5.0, 15.0, 30.0];

/// 桌面往外界发的事件（对应 C# 的 `StateChanged` / `Gone` / `TopologyChanging`）。
#[derive(Clone, Default)]
pub struct DeskEvents {
    pub state_changed: Option<Arc<dyn Fn(&DeskState) + Send + Sync>>,
    pub gone: Option<Arc<dyn Fn(&DeskGone) + Send + Sync>>,
    /// 即将改动显示器拓扑（界面据此收起 / 重排窗口）。
    pub topology_changing: Option<Arc<dyn Fn() + Send + Sync>>,
    /// 诊断日志，对应 C# `SnugWin.Core.Support.Log`。
    pub log: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

/// 构造选项，对应 C# 构造函数的可选参数（`timeline`、`defaultZoom`、`fit`）。
pub struct DeskOptions {
    pub timeline: Option<Arc<dyn Timeline>>,
    pub default_zoom: i32,
    pub fit: Option<ScreenFit>,
    pub events: DeskEvents,
}

impl Default for DeskOptions {
    fn default() -> Self {
        Self { timeline: None, default_zoom: 2, fit: None, events: DeskEvents::default() }
    }
}

/// 一次加屏失败后的重试任务（对应 C# `_pendingRetryCts` + `_pendingRetryTask`）。
struct RetryHandle {
    token: CancelToken,
    finished: Arc<AtomicBool>,
}

impl Clone for RetryHandle {
    fn clone(&self) -> Self {
        Self { token: self.token.clone(), finished: self.finished.clone() }
    }
}

/// watch 的键（对应 C# `WatchedKey`）：等 App 退出，或等窗口关闭。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct WatchedKey {
    is_quit: bool,
    pid: i32,
    window: WindowKey,
}

impl WatchedKey {
    #[allow(dead_code)] // 退出监视在下一段接入
    fn for_quit(pid: i32) -> Self {
        Self { is_quit: true, pid, window: WindowKey::default() }
    }

    #[allow(dead_code)] // 关闭监视在下一段接入
    fn for_close(window: WindowKey) -> Self {
        Self { is_quit: false, pid: 0, window }
    }
}

/// 已建好的手机屏：编号 + 建屏时刻（用于 MinimumLifetime）。
#[derive(Debug, Clone, Copy)]
struct PendingDisplay {
    id: u32,
    born_at: f64,
}

/// 改拓扑前后的环境快照（对应 C# `Surroundings`）。
#[derive(Debug, Clone)]
struct Surroundings {
    windows: HashMap<WindowKey, Rect>,
    others: Vec<OtherDisplay>,
}

/// 已撤屏、但系统还没下线的善后记录（对应 C# `Retiring`）。
#[derive(Debug, Clone)]
struct Retiring {
    id: u32,
    before: Surroundings,
    permanently: bool,
}

/// 跨线程共享的部分：状态快照、watch 表、重试句柄、平台接口与存储。
struct DeskShared {
    state: Mutex<DeskState>,
    watches: Mutex<HashMap<WatchedKey, CancelToken>>,
    pending_retry: Mutex<Option<RetryHandle>>,
    /// C# 字段 `_wanted` / `_remote` 由界面线程直接读写，这里用原子量保持同样语义。
    wanted: AtomicBool,
    remote: AtomicBool,
    default_zoom: AtomicI32,
    fit: Mutex<ScreenFit>,
    displays: Arc<dyn PhoneDisplays>,
    windows: Arc<dyn WindowControl>,
    /// 打开 / 退出 App、列窗口在后续分块（Open / Quit / Close）里接入。
    #[allow(dead_code)]
    apps: Arc<dyn AppControl>,
    timeline: Arc<dyn Timeline>,
    /// 还原记录在摆放分块（Pull / PutBack / Recover）里接入。
    #[allow(dead_code)]
    ledger: RestoreRecordStore<PhoneRecord>,
    events: DeskEvents,
}

struct DeskHandle {
    lane: SerialLane<DeskCore>,
    shared: Arc<DeskShared>,
}

impl Drop for DeskHandle {
    fn drop(&mut self) {
        // 最后一个句柄消失时关掉工作线程；桌面状态自己攥着发送端，靠 Stop 才能退出。
        self.lane.stop();
    }
}

/// 手机屏桌面。可克隆，所有方法线程安全。
#[derive(Clone)]
pub struct Desk {
    inner: Arc<DeskHandle>,
}

/// 工作线程独占的私有字段（对应 C# PhoneDesk 的实例字段）。
struct DeskCore {
    shared: Arc<DeskShared>,
    /// 用来从 watch/重试线程把动作排回本队列。
    enqueue: SerialLane<DeskCore>,
    display: Option<PendingDisplay>,
    retiring: Option<Retiring>,
    persist_owed: bool,
    settling: bool,
    failures: usize,
}

impl Desk {
    /// 对应 C# `PhoneDesk(displays, windows, apps, paths, timeline, defaultZoom, fit)`。
    pub fn new(
        displays: Arc<dyn PhoneDisplays>,
        windows: Arc<dyn WindowControl>,
        apps: Arc<dyn AppControl>,
        paths: &Paths,
        options: DeskOptions,
    ) -> Desk {
        let fit = options.fit.clone().unwrap_or_else(ScreenFit::standard);
        let timeline =
            options.timeline.unwrap_or_else(|| Arc::new(SystemTimeline::new()) as Arc<dyn Timeline>);
        let shared = Arc::new(DeskShared {
            state: Mutex::new(DeskState::default()),
            watches: Mutex::new(HashMap::new()),
            pending_retry: Mutex::new(None),
            wanted: AtomicBool::new(false),
            remote: AtomicBool::new(false),
            default_zoom: AtomicI32::new(options.default_zoom),
            fit: Mutex::new(fit),
            displays,
            windows,
            apps,
            timeline,
            ledger: RestoreRecordStore::new(paths.phone_record_file()),
            events: options.events,
        });

        let for_core = Arc::clone(&shared);
        let lane = SerialLane::start(move |enqueue| DeskCore {
            shared: for_core,
            enqueue,
            display: None,
            retiring: None,
            persist_owed: false,
            settling: false,
            failures: 0,
        });
        Desk { inner: Arc::new(DeskHandle { lane, shared }) }
    }

    /// 当前状态快照。
    pub fn state(&self) -> DeskState {
        self.inner.shared.state.lock().expect("desk state").clone()
    }

    pub fn default_zoom(&self) -> i32 {
        self.inner.shared.default_zoom.load(Ordering::SeqCst)
    }

    pub fn fit(&self) -> ScreenFit {
        self.inner.shared.fit.lock().expect("desk fit").clone()
    }

    /// 对应 C# `SetWanted`：用户在手机上进入 / 离开手机屏。
    pub fn set_wanted(&self, on: bool) {
        if self.inner.shared.wanted.swap(on, Ordering::SeqCst) == on {
            return;
        }
        self.inner.lane.run(move |core: &mut DeskCore| {
            if !on {
                core.cancel_retry();
                core.set_state(core.state().with_dismissed(false));
            }
            core.reconcile();
        });
    }

    /// 对应 C# `SetRemote`：UU 远程是否连着。
    pub fn set_remote(&self, on: bool) {
        if self.inner.shared.remote.swap(on, Ordering::SeqCst) == on {
            return;
        }
        if !on {
            self.settle_displays();
        }
    }

    /// 对应 C# `SettleDisplays`：撤屏后系统还没下线 / 要持久化其它屏模式时善后。
    pub fn settle_displays(&self) {
        self.inner.lane.run(|core: &mut DeskCore| core.settle_displays());
    }

    /// 对应 C# `ShutdownAsync`：退出前把手机屏撤干净（阻塞到做完）。
    pub fn shutdown(&self) {
        self.inner.shared.wanted.store(false, Ordering::SeqCst);
        self.inner.lane.call(|core: &mut DeskCore| {
            core.cancel_retry();
            core.reconcile();
        });
    }

    /// 对应 C# `IdleAsync`：等重试跑完、队列排空、所有 watch 结束。
    /// 供测试使用，也是"此刻桌面真的静止了"的判据。
    pub fn idle(&self) {
        loop {
            let retry = self.inner.shared.pending_retry.lock().expect("retry").clone();
            if let Some(handle) = retry.as_ref() {
                while !handle.finished.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            self.inner.lane.drain();
            let unchanged = {
                let pending = self.inner.shared.pending_retry.lock().expect("retry");
                match (retry.as_ref(), pending.as_ref()) {
                    (Some(was), Some(now)) => was.token.same(&now.token),
                    (None, None) => true,
                    _ => false,
                }
            };
            let watches = self.inner.shared.watches.lock().expect("watches").len();
            if unchanged && watches == 0 {
                return;
            }
        }
    }

    /// 测试用：此刻是否还挂着加屏重试。
    #[cfg(test)]
    pub(crate) fn pending_retry_is_none(&self) -> bool {
        self.inner.shared.pending_retry.lock().expect("retry").is_none()
    }

    /// 测试用：是否有"撤屏但系统还没下线"的善后记录。
    #[cfg(test)]
    pub(crate) fn retiring(&self) -> bool {
        let slot = Arc::new(Mutex::new(false));
        let sink = Arc::clone(&slot);
        self.inner.lane.call(move |core: &mut DeskCore| {
            *sink.lock().expect("slot") = core.retiring.is_some();
        });
        let value = *slot.lock().expect("slot");
        value
    }
}

impl DeskCore {
    fn now(&self) -> f64 {
        self.shared.timeline.now()
    }

    /// 对应 C# `_timeline.SleepAsync`（不带取消）。
    fn sleep(&self, seconds: f64) {
        self.shared.timeline.sleep(seconds, &CancelToken::new());
    }

    fn wanted(&self) -> bool {
        self.shared.wanted.load(Ordering::SeqCst)
    }

    fn remote(&self) -> bool {
        self.shared.remote.load(Ordering::SeqCst)
    }

    fn fit(&self) -> ScreenFit {
        self.shared.fit.lock().expect("desk fit").clone()
    }

    fn state(&self) -> DeskState {
        self.shared.state.lock().expect("desk state").clone()
    }

    /// 对应 C# `State` 的 setter：内容没变就不触发 `StateChanged`。
    fn set_state(&self, next: DeskState) {
        {
            let mut current = self.shared.state.lock().expect("desk state");
            if *current == next {
                return;
            }
            *current = next;
        }
        if let Some(callback) = self.shared.events.state_changed.as_ref() {
            let snapshot = self.state();
            callback(&snapshot);
        }
    }

    fn log(&self, message: impl AsRef<str>) {
        if let Some(log) = self.shared.events.log.as_ref() {
            log(message.as_ref());
        }
    }

    fn warn(&self, message: impl AsRef<str>) {
        self.log(format!("警告：{}", message.as_ref()));
    }

    fn error(&self, message: impl AsRef<str>) {
        self.log(format!("错误：{}", message.as_ref()));
    }

    fn topology_changing(&self) {
        if let Some(callback) = self.shared.events.topology_changing.as_ref() {
            callback();
        }
    }

    /// 对应 C# `ReconcileAsync`：要不要有手机屏。
    fn reconcile(&mut self) {
        let on = self.wanted() && !self.state().dismissed;
        if on && self.display.is_none() {
            self.add_screen();
        } else if !on && self.state().screen_phase != PhoneScreenPhase::Off {
            self.remove_screen();
        }
    }

    /// 对应 C# `AddScreenAsync`。
    fn add_screen(&mut self) {
        self.set_state(self.state().with_screen_phase(PhoneScreenPhase::Preparing));
        if let Some(old) = self.retiring.take() {
            self.warn(format!("上一块手机屏 #{} 还没下线就又要加屏，放弃旧的善后记录", old.id));
        }
        let before = self.capture_surroundings("加屏");
        self.topology_changing();
        let fit = self.fit();
        match self.shared.displays.create(fit) {
            Ok(id) => {
                self.display = Some(PendingDisplay { id, born_at: self.now() });
                self.failures = 0;
                self.settle(before, "加屏", false);
                let width = self
                    .shared
                    .displays
                    .width_of(id)
                    .map(|width| width.to_string())
                    .unwrap_or_else(|| "?".to_string());
                let state = self
                    .state()
                    .with_notice(None)
                    .with_screen_phase(PhoneScreenPhase::Ready)
                    .with_display(Some(id));
                self.set_state(state);
                self.log(format!("手机屏 #{id} 已就绪（{width} 宽）"));
            }
            Err(error) => {
                let state =
                    self.state().with_screen_phase(PhoneScreenPhase::Failed).with_display(None);
                self.set_state(state);
                self.error(format!("加手机屏失败：{error}"));
                self.schedule_retry();
            }
        }
    }

    /// 对应 C# `RemoveScreenAsync`。摆放相关的那几步在后续分块里接上。
    fn remove_screen(&mut self) {
        // C#: if (_placement is { } p) await PutBackAsync(p.Record); SetPlacement(null);
        self.retire_display();

        let tokens = {
            let mut watches = self.shared.watches.lock().expect("watches");
            let tokens: Vec<CancelToken> = watches.values().cloned().collect();
            watches.clear();
            tokens
        };
        for token in tokens {
            token.cancel();
        }

        let state = self
            .state()
            .with_screen_phase(PhoneScreenPhase::Off)
            .with_display(None)
            .with_list_visible(true)
            .with_notice(None)
            .with_quitting_cleared()
            .with_closing_cleared();
        self.set_state(state);
    }

    /// 对应 C# `RetireDisplayAsync`。
    fn retire_display(&mut self) {
        let Some(display) = self.display else {
            return;
        };

        let age = self.now() - display.born_at;
        if age < MINIMUM_LIFETIME {
            self.sleep(MINIMUM_LIFETIME - age);
        }

        let gone_bounds = self.shared.displays.bounds_of(display.id);
        let mut before = self.capture_surroundings("撤屏");
        self.topology_changing();
        let offline = self.shared.displays.retire(display.id);
        self.display = None;

        if let Some(screen) = gone_bounds {
            // 已经被撤掉那块屏上的窗口不再需要归位。
            before.windows.retain(|_, frame| !screen.contains(frame.center()));
        }

        if self.remote() {
            self.persist_owed = true;
        }
        let pending = Retiring { id: display.id, before, permanently: !self.remote() };
        if offline {
            self.settle(pending.before.clone(), "撤屏", pending.permanently);
        } else {
            self.retiring = Some(pending);
            self.log(format!("手机屏 #{} 尚未下线，稍后自动善后", display.id));
        }
        self.log(format!("已撤掉手机屏 #{}", display.id));
    }

    /// 对应 C# `CaptureSurroundingsAsync`。
    fn capture_surroundings(&self, before_what: &str) -> Surroundings {
        let others = self.shared.displays.other_displays();
        let names = if others.is_empty() {
            "没有".to_string()
        } else {
            others
                .iter()
                .map(|other| format!("{} {}", other.friendly_name, other.mode))
                .collect::<Vec<_>>()
                .join("、")
        };
        self.log(format!("{before_what}前别的屏：{names}"));
        Surroundings { windows: self.shared.windows.snapshot(), others }
    }

    /// 对应 C# `SettleAsync`：等系统稳定，恢复被动改掉的其他屏模式，再把窗口挪回去。
    fn settle(&self, before: Surroundings, after_what: &str, permanently: bool) {
        self.sleep(TOPOLOGY_SETTLE);

        let now_modes: HashMap<u32, DisplayModeInfo> = self
            .shared
            .displays
            .other_displays()
            .into_iter()
            .map(|other| (other.display_id, other.mode))
            .collect();
        let mut drifted: Vec<OtherDisplay> = Vec::new();
        for was in &before.others {
            if let Some(current) = now_modes.get(&was.display_id)
                && !current.is_same_as(&was.mode)
            {
                drifted.push(was.clone());
            }
        }

        if !drifted.is_empty() {
            self.log(format!("{after_what}时系统改变了其它屏幕模式，恢复中"));
            self.topology_changing();
            for was in &drifted {
                self.shared.displays.set_mode(was.display_id, was.mode, permanently);
            }
            self.sleep(TOPOLOGY_SETTLE);
        }

        // C# 在这里还传 `_placement?.Window.Key` 作为例外窗口，接了摆放分块后再补。
        self.shared.windows.restore(&before.windows, None);
    }

    /// 对应 C# `ScheduleRetry`。
    fn schedule_retry(&mut self) {
        self.cancel_retry_keeping_failures();
        if self.failures >= RETRY_DELAYS.len() {
            self.error(format!("加手机屏连续失败 {} 次，这次远程不再重试", self.failures + 1));
            return;
        }
        let delay = RETRY_DELAYS[self.failures];
        self.failures += 1;
        self.log(format!("{} 秒后重试加手机屏（第 {} 次重试）", delay as i64, self.failures));

        let token = CancelToken::new();
        let finished = Arc::new(AtomicBool::new(false));
        *self.shared.pending_retry.lock().expect("retry") =
            Some(RetryHandle { token: token.clone(), finished: Arc::clone(&finished) });

        let lane = self.enqueue.clone();
        let timeline = Arc::clone(&self.shared.timeline);
        std::thread::Builder::new()
            .name("snug-desk-retry".to_string())
            .spawn(move || {
                timeline.sleep(delay, &token);
                if !token.is_cancelled() {
                    lane.run(|core: &mut DeskCore| core.reconcile());
                }
                finished.store(true, Ordering::SeqCst);
            })
            .expect("创建加屏重试线程失败");
    }

    /// 对应 C# `CancelRetry()`（默认重置失败计数）。
    fn cancel_retry(&mut self) {
        self.cancel_retry_keeping_failures();
        self.failures = 0;
    }

    /// 对应 C# `CancelRetry(resetFailures: false)`。
    fn cancel_retry_keeping_failures(&mut self) {
        let previous = self.shared.pending_retry.lock().expect("retry").take();
        if let Some(handle) = previous {
            handle.token.cancel();
            handle.finished.store(true, Ordering::SeqCst);
        }
    }

    /// 对应 C# `SettleDisplays` 里那段排进队列的善后逻辑。
    fn settle_displays(&mut self) {
        if self.settling || (self.retiring.is_none() && !self.persist_owed) {
            return;
        }
        self.settling = true;
        self.settle_displays_inner();
        self.settling = false;
    }

    fn settle_displays_inner(&mut self) {
        if let Some(retiring) = self.retiring.clone() {
            if self.shared.displays.is_online(retiring.id) {
                return;
            }
            self.retiring = None;
            self.log(format!("撤掉的手机屏 #{} 已下线", retiring.id));
            self.settle(retiring.before, "撤屏", retiring.permanently);
        }

        if !self.persist_owed || self.remote() || self.display.is_some() {
            return;
        }
        let others = self.shared.displays.other_displays();
        if others.iter().any(|other| other.asleep) {
            return;
        }
        self.persist_owed = false;
        for other in others {
            self.shared.displays.set_mode(other.display_id, other.mode, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desk::test_support::Harness;

    #[test]
    fn set_wanted_adds_screen_and_reports_ready() {
        let harness = Harness::new();
        assert_eq!(harness.desk.state().screen_phase, PhoneScreenPhase::Off);

        harness.desk.set_wanted(true);
        harness.desk.idle();

        let state = harness.desk.state();
        assert_eq!(state.screen_phase, PhoneScreenPhase::Ready);
        assert_eq!(state.display_id, Some(1001));
        assert!(state.list_visible);
        assert!(harness.displays.is_online(1001));
        assert_eq!(harness.displays.create_calls(), 1);
        assert_eq!(
            harness.displays.width_of(1001),
            Some(harness.desk.fit().base_width()),
            "建屏用当前 fit 的基础宽度"
        );
    }

    #[test]
    fn set_wanted_false_retires_screen_and_resets_lists() {
        let harness = Harness::new();
        harness.desk.set_wanted(true);
        harness.desk.idle();

        harness.desk.set_wanted(false);
        harness.desk.idle();

        let state = harness.desk.state();
        assert_eq!(state.screen_phase, PhoneScreenPhase::Off);
        assert_eq!(state.display_id, None);
        assert!(state.list_visible);
        assert!(state.quitting.is_empty());
        assert!(state.closing.is_empty());
        assert_eq!(harness.displays.retire_calls(), 1);
        assert!(!harness.displays.is_online(1001));
    }

    #[test]
    fn create_failure_is_retried_three_times_then_given_up() {
        let harness = Harness::new();
        harness.displays.fail_creates(100);

        harness.desk.set_wanted(true);
        harness.desk.idle();

        assert_eq!(harness.desk.state().screen_phase, PhoneScreenPhase::Failed);
        assert_eq!(harness.displays.create_calls(), 4, "首次 + 三次重试");
        // 放弃后不再挂着重试任务。
        assert!(harness.desk.pending_retry_is_none());
    }

    #[test]
    fn create_failure_retries_and_then_recovers() {
        let harness = Harness::new();
        harness.displays.fail_creates(1);

        harness.desk.set_wanted(true);
        harness.desk.idle();

        assert_eq!(harness.desk.state().screen_phase, PhoneScreenPhase::Ready);
        assert_eq!(harness.displays.create_calls(), 2);
    }

    #[test]
    fn screen_lives_at_least_minimum_lifetime() {
        let harness = Harness::new();
        harness.desk.set_wanted(true);
        harness.desk.idle();

        harness.desk.set_wanted(false);
        harness.desk.idle();

        assert!(
            harness.timeline.now() >= MINIMUM_LIFETIME,
            "撤屏时间 {:?} 早于最短存活 {:?}",
            harness.timeline.now(),
            MINIMUM_LIFETIME
        );
    }

    #[test]
    fn shutdown_turns_the_screen_off_synchronously() {
        let harness = Harness::new();
        harness.desk.set_wanted(true);
        harness.desk.idle();

        harness.desk.shutdown();

        assert_eq!(harness.desk.state().screen_phase, PhoneScreenPhase::Off);
        assert_eq!(harness.desk.state().display_id, None);
    }

    #[test]
    fn state_changes_are_pushed_to_the_callback() {
        let harness = Harness::new();
        harness.desk.set_wanted(true);
        harness.desk.idle();

        let phases: Vec<PhoneScreenPhase> =
            harness.states().iter().map(|state| state.screen_phase).collect();
        assert_eq!(
            phases,
            vec![PhoneScreenPhase::Preparing, PhoneScreenPhase::Ready],
            "C# 里 State setter 只在内容变化时触发"
        );
    }

    #[test]
    fn other_display_modes_are_restored_after_topology_change() {
        let harness = Harness::new();
        let original = harness.displays.push_other_display(3003, "主屏");
        // 系统在建票时"顺手"改了别的屏模式，加屏后应恢复。
        harness.displays.drift_others_on_create(3003, 1280, 720, 60.0);

        harness.desk.set_wanted(true);
        harness.desk.idle();

        let sets = harness.displays.mode_sets();
        assert!(
            sets.iter().any(|(id, mode, permanently)| *id == 3003 && *mode == original && !permanently),
            "应把被动改掉的模式恢复回原样，实际 {:?}",
            sets
        );
    }

    #[test]
    fn offline_retire_is_settled_later_when_system_reports_online() {
        let harness = Harness::new();
        harness.displays.keep_online_after_retire();

        harness.desk.set_wanted(true);
        harness.desk.idle();
        harness.desk.set_wanted(false);
        harness.desk.idle();

        // 撤屏时系统还没下线：先记着，等它下线再善后。
        assert!(harness.desk.retiring(), "系统还说它在线上，应留下善后记录");
        harness.desk.settle_displays();
        harness.desk.idle();
        assert!(harness.desk.retiring(), "还在线上时不做善后");

        harness.displays.bring_offline();
        harness.desk.settle_displays();
        harness.desk.idle();
        assert!(!harness.desk.retiring(), "下线后善后完成");
    }
}
