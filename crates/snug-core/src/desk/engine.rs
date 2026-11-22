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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
#[cfg(test)]
use std::time::Duration;

use crate::config::ScreenConfig;
use crate::geometry::Rect;
use crate::left_behind::{LeftBehindBook, LeftBehindEntry};
use crate::paths::Paths;
use crate::phone_models::PHONE_SCREEN_NAME;
use crate::record::PhoneRecord;
use crate::screen_fit::ScreenFit;
use crate::screen_plan::ScreenBehavior;
use crate::store::{JsonFileStore, RestoreRecordStore};
use crate::touch::CloseButton;
use crate::windows::{AppInfo, DisplayModeInfo, OtherDisplay, RunningApp, WindowInfo, WindowKey};
use crate::zoom::{self, ZoomBook, ZoomScope};

use super::lane::SerialLane;
use super::model::{
    DeskGone, DeskNotice, DeskState, DeskWork, DeskWorkKind, PhoneScreenPhase, PlacedWindow,
};
use super::services::{
    AppControl, CancelToken, PhoneDisplays, SystemTimeline, Timeline, WindowControl, WindowLook,
};

/// C# `PhoneScreenSpec.MinimumLifetime`：手机屏建出来至少存在 2.5 秒，
/// 否则刚加就撤会让系统拓扑抖动。
pub const MINIMUM_LIFETIME: f64 = 2.5;

/// C# `TopologySettle`：改拓扑后等系统稳定。
const TOPOLOGY_SETTLE: f64 = 1.0;
/// C# `RetryDelays`。
const RETRY_DELAYS: [f64; 3] = [5.0, 15.0, 30.0];
/// 删除命令失败或系统尚未下线时，限制再次发送删除命令的频率。
const RETIRE_RETRY_DELAY: f64 = 5.0;
/// C# SettleDelay：摆好/改字号后等一下再继续。
const SETTLE_DELAY: f64 = 0.3;
/// 收尾时等"撤屏"这条动作的上限。超过就不等了：一次在飞的建屏可能 8 秒 × 4 个候选，
/// 再叠最短存活与拓扑沉降；进程不能为了撤屏卡到用户以为死机（屏由归属记录兜底）。
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// C# RecheckDelay：显示器配置变化后的重查延迟。
const RECHECK_DELAY: f64 = 1.5;
/// C# ReplaceWait：窗口被外部关掉后，等它换新窗口回来的窗口期。
const REPLACE_WAIT: f64 = 10.0;
/// C# `WindowPoll`：等新窗口出现时的轮询间隔。
const WINDOW_POLL: f64 = 0.1;
/// C# `LaunchWait`：从零启动 App 等窗口的上限。
const LAUNCH_WAIT: f64 = 30.0;
/// C# `WindowWait`：已有进程里新建窗口的等待上限。
const WINDOW_WAIT: f64 = 10.0;
/// C# `WatchPoll`：watch 轮询间隔。
const WATCH_POLL: f64 = 0.25;
/// C# `QuitWait`：等 App 退出的上限。
const QUIT_WAIT: f64 = 10.0;
/// C# `CloseWait`：等窗口关闭的上限。
const CLOSE_WAIT: f64 = 5.0;
/// C# `AnswerGrace`：询问窗口消失后再等一会儿才判定"用户取消"。
const ANSWER_GRACE: f64 = 2.0;
/// C# `StalledFollow`：退出超时后继续跟踪的时长。
const STALLED_FOLLOW: f64 = 60.0;
/// C# `StalledPoll`：跟踪退出时的轮询间隔。
const STALLED_POLL: f64 = 1.0;

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
    fn for_quit(pid: i32) -> Self {
        Self { is_quit: true, pid, window: WindowKey::default() }
    }

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
    retry_after: f64,
}

/// 此刻摆在手机屏上的窗口（对应 C# 私有类 `Placement`）。
#[derive(Debug, Clone)]
struct Placement {
    window: WindowInfo,
    record: PhoneRecord,
    width: i32,
    settled: Option<Rect>,
    known: HashSet<WindowKey>,
    asking: bool,
}

/// 手机屏上的窗口被外部关掉后，等同一进程里的新窗口回来（对应 C# `_replaced`）。
#[derive(Debug, Clone)]
#[allow(dead_code)] // 字段在 watch 分块（FollowReplacement）里读
struct Replaced {
    pid: i32,
    known: HashSet<WindowKey>,
    until: f64,
}

/// 多块屏共用的 zoom.json / left-behind.json。
///
/// C# 的 `_zoomBook` / `_leftBehind` 是所有 `PhoneDesk` 共用的**同一个对象**；
/// Rust 版把值放进 `Arc<Mutex<_>>`，这样各屏看到的是同一份记忆，不会互相覆盖文件。
pub(crate) struct SharedDeskBooks {
    pub(crate) zoom_book: Arc<Mutex<ZoomBook>>,
    pub(crate) zoom_file: Arc<JsonFileStore<ZoomBook>>,
    pub(crate) left_behind: Arc<Mutex<LeftBehindBook>>,
    pub(crate) left_behind_file: Arc<JsonFileStore<LeftBehindBook>>,
    /// 每个 App 上次铺满用的屏宽，键是 `"<比例>|<app_id>"`。
    ///
    /// 存盘是为了**跨进程**也省掉换档阶梯：真机上 QQ 第一次要从 540 一档档试到 850，
    /// 每档一次显示模式变更，合计 4.8 秒；记下来以后直接从 850 起步。
    /// 键里带比例，换机型/换方向后旧记录自然失效。
    pub(crate) learned_widths: Arc<Mutex<HashMap<String, i32>>>,
    pub(crate) learned_widths_file: Arc<JsonFileStore<HashMap<String, i32>>>,
}

impl SharedDeskBooks {
    /// 从数据目录读一次（读坏的文件已被隔离）。
    pub(crate) fn load(paths: &Paths) -> Self {
        let zoom_file = Arc::new(JsonFileStore::<ZoomBook>::new(paths.zoom_book_file()));
        let zoom_book = Arc::new(Mutex::new(zoom_file.load_or_quarantine().unwrap_or_default()));
        let left_behind_file =
            Arc::new(JsonFileStore::<LeftBehindBook>::new(paths.left_behind_file()));
        let left_behind =
            Arc::new(Mutex::new(left_behind_file.load_or_quarantine().unwrap_or_default()));
        let learned_widths_file =
            Arc::new(JsonFileStore::<HashMap<String, i32>>::new(paths.learned_widths_file()));
        let learned_widths =
            Arc::new(Mutex::new(learned_widths_file.load_or_quarantine().unwrap_or_default()));
        Self {
            zoom_book,
            zoom_file,
            left_behind,
            left_behind_file,
            learned_widths,
            learned_widths_file,
        }
    }
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
    /// 每块屏一份还原记录（`phone-screen.json` / `screen-<id>.json`）。
    ledger: RestoreRecordStore<PhoneRecord>,
    books: Arc<SharedDeskBooks>,
    /// 屏身份（C# `PhoneDesk` 的 `_screenId` / `_screenName` / `_behavior`）。
    screen_id: String,
    screen_name: Mutex<String>,
    behavior: Mutex<ScreenBehavior>,
    events: DeskEvents,
}

/// 共享串行队列的状态：所有屏的 [`DeskCore`] 都放在这里。
///
/// 对应 C# `DeskSet` 里那条所有 `PhoneDesk` 共享的 `SerialLane`：一个工作线程、
/// 按入队顺序执行，所以跨屏操作（转移窗口、退出 App）可以在同一个动作里同步做完。
#[derive(Default)]
pub(crate) struct DeskHub {
    desks: HashMap<String, DeskCore>,
    retired: Vec<DeskCore>,
}

impl DeskHub {
    /// 屏 id 大小写不敏感（C# 字典用 `StringComparer.OrdinalIgnoreCase`）。
    pub(crate) fn key(screen_id: &str) -> String {
        screen_id.to_lowercase()
    }

    pub(crate) fn core_mut(&mut self, id: &str) -> Option<&mut DeskCore> {
        self.desks.get_mut(&Self::key(id))
    }

    pub(crate) fn insert(&mut self, core: DeskCore) {
        let key = Self::key(core.screen_id());
        self.desks.insert(key, core);
    }

    pub(crate) fn remove(&mut self, id: &str) {
        if let Some(core) = self.desks.remove(&Self::key(id)) {
            // 愿望已消失，但下线确认可能比删除命令晚。保留善后状态，不能随句柄一起丢掉。
            core.shared.remote.store(false, Ordering::SeqCst);
            if core.retiring.is_some() || core.persist_owed {
                self.retired.push(core);
            }
        }
    }

    pub(crate) fn settle_retired(&mut self) {
        self.retired.retain_mut(|core| {
            core.settle_displays();
            core.retiring.is_some() || core.persist_owed
        });
    }

    pub(crate) fn ids(&self) -> Vec<String> {
        self.desks.keys().cloned().collect()
    }

    pub(crate) fn clear(&mut self) {
        self.desks.clear();
        self.retired.clear();
    }

    /// 跨屏转移：除 `except` 之外，哪块屏正铺着这个窗口就把它摘下来。
    pub(crate) fn detach_for_transfer(
        &mut self,
        except: &str,
        window: WindowKey,
    ) -> Option<PhoneRecord> {
        let except = Self::key(except);
        let source = self
            .desks
            .iter()
            .find(|(id, core)| **id != except && core.placed_window_key() == Some(window))
            .map(|(id, _)| id.clone())?;
        self.desks.get_mut(&source)?.detach_placement_for_transfer()
    }
}

/// 指向共享队列里「某一块屏」的句柄：入队的动作只作用在这块屏的 [`DeskCore`] 上。
#[derive(Clone)]
pub(crate) struct DeskSlot {
    lane: SerialLane<DeskHub>,
    id: String,
}

impl DeskSlot {
    pub(crate) fn new(lane: SerialLane<DeskHub>, id: impl Into<String>) -> Self {
        Self { lane, id: id.into() }
    }

    /// 这三个方法都带 `#[track_caller]` 并把位置继续传给 lane：
    /// lane 的慢动作日志要指出"是哪一行入队的动作慢"，否则只能靠时间戳猜。
    #[track_caller]
    fn run(&self, operation: impl FnOnce(&mut DeskCore) + Send + 'static) {
        let site = std::panic::Location::caller();
        let id = self.id.clone();
        self.lane.run_at(site, move |hub: &mut DeskHub| {
            if let Some(core) = hub.core_mut(&id) {
                operation(core);
            }
        });
    }

    #[track_caller]
    fn call(&self, operation: impl FnOnce(&mut DeskCore) + Send + 'static) {
        let site = std::panic::Location::caller();
        let id = self.id.clone();
        self.lane.call_at(site, move |hub: &mut DeskHub| {
            if let Some(core) = hub.core_mut(&id) {
                operation(core);
            }
        });
    }

    /// 同 [`Self::call`]，但最多等 `timeout`（收尾用）。
    #[track_caller]
    fn call_timeout(
        &self,
        timeout: std::time::Duration,
        operation: impl FnOnce(&mut DeskCore) + Send + 'static,
    ) -> bool {
        let site = std::panic::Location::caller();
        let id = self.id.clone();
        self.lane.call_timeout_at(site, timeout, move |hub: &mut DeskHub| {
            if let Some(core) = hub.core_mut(&id) {
                operation(core);
            }
        })
    }

    /// 只给 `idle()`（测试）用：生产路径不等队列，见 `docs/desk-threading.md`。
    #[cfg(test)]
    fn drain(&self) {
        self.lane.drain();
    }
}

/// 共享队列的拥有者：最后一个 owner 消失时关掉工作线程。
///
/// 单屏 [`Desk`] 自己持有；[`super::set::DeskSet`] 持有所有屏共用的那一个。
pub(crate) struct HubOwner {
    pub(crate) lane: SerialLane<DeskHub>,
}

impl Drop for HubOwner {
    fn drop(&mut self) {
        self.lane.stop();
    }
}

struct DeskHandle {
    slot: DeskSlot,
    shared: Arc<DeskShared>,
    /// 已经排了一个 `check_returns` 在队列里（去重，防止每拍入队把队列堵死）。
    check_pending: Arc<AtomicBool>,
    /// 已经为哪个窗口排了"铺开"（去重：用户连点时只认第一次）。
    ///
    /// 一次铺开在真机上要 1–5 秒（换屏宽、挪窗口、找关闭按钮都在里面）。
    /// 用户在等待期间再点几次，原先会排成好几条同样的动作，一条比一条更晚，
    /// 表现就是"点了半天没反应，然后突然连着动几下"。
    pull_pending: Arc<Mutex<Option<WindowKey>>>,
    /// 共享队列的拥有者；字段本身不读，靠它的 `Drop` 在工作线程用完时退出。
    _owner: Option<Arc<HubOwner>>,
}

/// 手机屏桌面。可克隆，所有方法线程安全。
#[derive(Clone)]
pub struct Desk {
    inner: Arc<DeskHandle>,
}

/// 工作线程独占的私有字段（对应 C# PhoneDesk 的实例字段）。
pub(crate) struct DeskCore {
    shared: Arc<DeskShared>,
    /// 用来从 watch/重试线程把动作排回共享队列。
    slot: DeskSlot,
    display: Option<PendingDisplay>,
    retiring: Option<Retiring>,
    persist_owed: bool,
    settling: bool,
    failures: usize,
    placement: Option<Placement>,
    recheck_generation: u64,
    verifying: bool,
    checking_returns: bool,
    /// 手机屏上的窗口被外部关掉后，等同一进程的新窗口回来。
    #[allow(dead_code)] // 跟进逻辑在 watch 分块（FollowReplacement）里接入
    replaced: Option<Replaced>,
}

impl Desk {
    /// 对应 C# `PhoneDesk(displays, windows, apps, paths, timeline, defaultZoom, fit)` 的单屏形态。
    ///
    /// 自己起一条串行队列、自己读 zoom/left-behind、用 legacy 屏 id 与 `phone-screen.json`；
    /// 现有单屏测试与 app 都用它，行为与 v1.2 的单屏版本一致。
    pub fn new(
        displays: Arc<dyn PhoneDisplays>,
        windows: Arc<dyn WindowControl>,
        apps: Arc<dyn AppControl>,
        paths: &Paths,
        options: DeskOptions,
    ) -> Desk {
        let DeskOptions { timeline, default_zoom, fit, events } = options;
        let fit = fit.unwrap_or_else(ScreenFit::standard);
        let timeline =
            timeline.unwrap_or_else(|| Arc::new(SystemTimeline::new()) as Arc<dyn Timeline>);

        let lane = SerialLane::<DeskHub>::start(|_| DeskHub::default());
        let owner = Arc::new(HubOwner { lane: lane.clone() });
        let books = Arc::new(SharedDeskBooks::load(paths));
        Desk::spawn(
            &lane,
            books,
            displays,
            windows,
            apps,
            paths,
            timeline,
            default_zoom,
            fit,
            ScreenConfig::LEGACY_PHONE_ID,
            PHONE_SCREEN_NAME,
            ScreenBehavior::TouchScreen,
            events,
            Some(owner),
        )
    }

    /// 在一条共享队列里起一块屏（C# 带 `sharedLane` / `sharedZoomBook` 等参数的构造函数）。
    ///
    /// `owner` 只有单屏 [`Desk::new`] 才传；[`super::set::DeskSet`] 自己攥着共享队列的 owner。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        lane: &SerialLane<DeskHub>,
        books: Arc<SharedDeskBooks>,
        displays: Arc<dyn PhoneDisplays>,
        windows: Arc<dyn WindowControl>,
        apps: Arc<dyn AppControl>,
        paths: &Paths,
        timeline: Arc<dyn Timeline>,
        default_zoom: i32,
        fit: ScreenFit,
        screen_id: impl Into<String>,
        screen_name: impl Into<String>,
        behavior: ScreenBehavior,
        events: DeskEvents,
        owner: Option<Arc<HubOwner>>,
    ) -> Desk {
        let screen_id: String = screen_id.into();
        let screen_name: String = screen_name.into();
        let screen_name = if screen_name.trim().is_empty() {
            PHONE_SCREEN_NAME.to_string()
        } else {
            screen_name
        };

        // 还原记录按屏分文件：legacy id 用 `phone-screen.json`，其余 `screen-<id>.json`。
        let ledger = RestoreRecordStore::new(paths.record_file_for_screen(Some(&screen_id)));
        let shared = Arc::new(DeskShared {
            state: Mutex::new(DeskState::default()),
            watches: Mutex::new(HashMap::new()),
            pending_retry: Mutex::new(None),
            wanted: AtomicBool::new(false),
            remote: AtomicBool::new(false),
            default_zoom: AtomicI32::new(default_zoom),
            fit: Mutex::new(fit),
            displays,
            windows,
            apps,
            timeline,
            ledger,
            books,
            screen_id: screen_id.clone(),
            screen_name: Mutex::new(screen_name),
            behavior: Mutex::new(behavior),
            events,
        });

        let slot = DeskSlot::new(lane.clone(), screen_id);
        let for_core = Arc::clone(&shared);
        let core_slot = slot.clone();
        lane.run(move |hub: &mut DeskHub| {
            hub.insert(DeskCore {
                shared: for_core,
                slot: core_slot,
                display: None,
                retiring: None,
                persist_owed: false,
                settling: false,
                failures: 0,
                placement: None,
                recheck_generation: 0,
                verifying: false,
                checking_returns: false,
                replaced: None,
            });
        });
        Desk {
            inner: Arc::new(DeskHandle {
                slot,
                shared,
                check_pending: Arc::new(AtomicBool::new(false)),
                pull_pending: Arc::new(Mutex::new(None)),
                _owner: owner,
            }),
        }
    }

    /// 屏 id（C# `ScreenId`）。
    pub fn screen_id(&self) -> String {
        self.inner.shared.screen_id.clone()
    }

    /// 屏名（C# `ScreenName`），可用 [`Self::update_wish`] 改。
    pub fn screen_name(&self) -> String {
        self.inner.shared.screen_name.lock().expect("screen name").clone()
    }

    /// 这块屏是触屏还是电脑副屏（C# `Behavior`）。
    pub fn behavior(&self) -> ScreenBehavior {
        *self.inner.shared.behavior.lock().expect("behavior")
    }

    /// 正铺在这块屏上的窗口（C# `PlacedWindowKey`）。
    pub fn placed_window_key(&self) -> Option<WindowKey> {
        self.state().placed.map(|placed| placed.window.key)
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

    pub(crate) fn log(&self, message: impl AsRef<str>) {
        if let Some(log) = self.inner.shared.events.log.as_ref() {
            log(message.as_ref());
        }
    }

    /// 对应 C# `UpdateWish`：换名字、换比例、换行为。
    pub fn update_wish(&self, name: impl Into<String>, fit: ScreenFit, behavior: ScreenBehavior) {
        let name = name.into();
        if !name.trim().is_empty() {
            *self.inner.shared.screen_name.lock().expect("screen name") = name;
        }
        *self.inner.shared.behavior.lock().expect("behavior") = behavior;
        self.set_fit(fit);
    }

    /// 对应 C# `SetWanted`：用户在手机上进入 / 离开手机屏。
    pub fn set_wanted(&self, on: bool) {
        if self.inner.shared.wanted.swap(on, Ordering::SeqCst) == on {
            return;
        }
        self.inner.slot.run(move |core: &mut DeskCore| {
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
        self.inner.slot.run(|core: &mut DeskCore| {
            core.settle_displays();
            if core.state().screen_phase == PhoneScreenPhase::Off && core.retiring.is_none() {
                core.reconcile();
            }
        });
    }

    /// 对应 C# `Pull`：把某个窗口放上手机屏。
    pub fn pull(&self, window: WindowInfo) {
        let label = window.label();
        let app = window.app_name.clone();
        if self.state().quitting.contains(&window.key.pid) {
            self.log(format!("{app} 正在退出，不放 {label}"));
            return;
        }
        self.inner.slot.run(move |core: &mut DeskCore| {
            core.pull_with(window, None);
        });
    }

    /// 对应 C# `PutBack`：把手机屏上的窗口放回原位。
    pub fn put_back(&self) {
        self.inner.slot.run(|core: &mut DeskCore| {
            if core.placement.is_none() {
                return;
            }
            core.begin(DeskWork::new(DeskWorkKind::PuttingBack));
            if let Some(placement) = core.placement.clone()
                && !core.return_to_original(&placement, true)
            {
                let app = placement.window.app_name.clone();
                core.set_state(
                    core.state().with_notice(Some(DeskNotice::WindowUnresponsive { app })),
                );
            }
            core.match_width();
            core.set_state(core.state().with_work(None));
        });
    }

    /// 对应 C# `ReturnToPlaced`：从 App 资源库回到已摆好的窗口。
    pub fn return_to_placed(&self) {
        self.inner.slot.run(|core: &mut DeskCore| {
            let Some(placement) = core.placement.clone() else {
                return;
            };
            core.begin(DeskWork::with_label(DeskWorkKind::Placing, placement.window.label()));
            core.reveal();
            core.set_state(core.state().with_work(None));
        });
    }

    /// 对应 C# `Zoom`：手机屏上按 +/- 调字号。
    pub fn zoom(&self, delta: i32) {
        let Some(placed) = self.state().placed else {
            return;
        };
        if !placed.zoomable() {
            return;
        }
        let clamped = (placed.zoom + delta).clamp(zoom::MIN_STEPS, zoom::MAX_STEPS);
        if clamped == placed.zoom {
            return;
        }
        self.inner.slot.run(move |core: &mut DeskCore| {
            core.begin(DeskWork::new(DeskWorkKind::Zooming));
            if core.change_zoom(delta)
                && let Some(placement) = core.placement.clone()
                && let Some(app_id) = placement.window.app_id.clone()
            {
                core.zoom_book().remember(placement.record.zoom, &app_id);
                core.save_zoom_book();
            }
            core.set_state(core.state().with_work(None));
        });
    }

    /// 对应 C# `SetDefaultZoom`：改了默认字号后，忘掉各 App 记住的级数。
    pub fn set_default_zoom(&self, steps: i32) {
        self.inner.shared.default_zoom.store(steps, Ordering::SeqCst);
        self.inner.slot.run(|core: &mut DeskCore| {
            if core.zoom_book().remembered.is_empty() {
                return;
            }
            core.zoom_book().forget_all();
            core.save_zoom_book();
        });
    }

    /// 对应 C# `SetFit`：换手机屏比例（按需重建宽度）。
    pub fn set_fit(&self, fit: ScreenFit) {
        {
            let mut current = self.inner.shared.fit.lock().expect("desk fit");
            if *current == fit {
                return;
            }
            *current = fit;
        }
        self.inner.slot.run(move |core: &mut DeskCore| {
            if core.state().screen_phase == PhoneScreenPhase::Off {
                return;
            }
            if core.state().work.is_none() && core.display.is_some() {
                core.begin(DeskWork::new(DeskWorkKind::Reshaping));
            }
            let fit = core.fit();
            if let Some(display) = core.display
                && core.shared.displays.fit_of(display.id).as_ref() != Some(&fit)
            {
                core.begin(DeskWork::new(DeskWorkKind::Reshaping));
                core.reshape(display.id);
            }
            if core.state().work.as_ref().map(|work| work.kind) == Some(DeskWorkKind::Reshaping) {
                core.set_state(core.state().with_work(None));
            }
        });
    }

    /// 对应 C# `DismissScreen`：用户在手机上主动退出这次手机屏。
    pub fn dismiss_screen(&self) {
        if !self.inner.shared.wanted.load(Ordering::SeqCst) || self.state().dismissed {
            return;
        }
        self.inner.slot.run(|core: &mut DeskCore| {
            let state = core
                .state()
                .with_dismissed(true)
                .with_notice(None)
                .with_work(Some(DeskWork::new(DeskWorkKind::Dismissing)));
            core.set_state(state);
            core.log("手机上退出了手机屏：放回窗口、撤屏，这次远程不再加");
            core.reconcile();
            core.set_state(core.state().with_work(None));
        });
    }

    /// 对应 C# `ReenterScreen`（`DeskSet.ReenterAllDismissedTouch` 的单屏版）：
    /// 同一次远程里用户点「重新进入手机屏」，清掉 dismissed 与 Notice 并立刻恢复建屏。
    /// 没标记 dismissed（没退出过）或这块屏本来就不该有（`wanted == false`）时什么也不做。
    pub fn reenter_screen(&self) {
        if !self.inner.shared.wanted.load(Ordering::SeqCst) || !self.state().dismissed {
            return;
        }
        self.inner.slot.run(|core: &mut DeskCore| {
            core.set_state(core.state().with_dismissed(false).with_notice(None));
            core.log("同会话重新进入手机屏：恢复建屏");
            core.reconcile();
        });
    }

    /// `reenter_screen` 的旧名字，保留给已经接好的调用方。
    pub fn reenter(&self) {
        self.reenter_screen();
    }

    /// 对应 C# `DetachPlacementForTransfer`：把正铺着的记录摘出来，给跨屏转移用。
    ///
    /// 清掉摆放状态并回到 App 资源库；返回的记录里仍是**原来那块屏上的还原坐标**。
    /// `DeskSet` 在共享队列内部直接调 [`DeskCore`] 版本，这个包装留给单屏/测试用。
    #[allow(dead_code)]
    pub(crate) fn detach_placement_for_transfer(&self) -> Option<PhoneRecord> {
        let slot = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&slot);
        self.inner.slot.call(move |core: &mut DeskCore| {
            *sink.lock().expect("transfer") = core.detach_placement_for_transfer();
        });
        slot.lock().expect("transfer").take()
    }

    /// 对应 C# `ReturnWindowIfPlacedAsync`：铺着的窗口属于这个 pid 就先还回原位再对宽度。
    pub fn return_window_if_placed(&self, pid: i32) {
        self.inner.slot.run(move |core: &mut DeskCore| core.return_window_if_placed(pid));
    }

    /// 对应 C# `EvacuateComputerScreenWindowsAsync`：电脑副屏撤掉前，把落在它矩形内的
    /// 窗口搬到主屏居中（否则它们会留在已经消失的坐标上）。
    ///
    /// C# 在 `RemoveScreenAsync` 里调用；屏没就绪（`_display` 为空）时什么都不做。
    pub fn evacuate_computer_screen_windows(&self) {
        self.inner.slot.run(|core: &mut DeskCore| core.evacuate_computer_screen_windows());
    }

    /// 对应 C# `VerifyPlacement`：确认手机屏上的窗口还在。
    pub fn verify_placement(&self) {
        self.inner.slot.run(|core: &mut DeskCore| core.verify_placement());
    }

    /// 对应 C# `DisplaysChanged`。
    pub fn displays_changed(&self) {
        self.inner.slot.run(|core: &mut DeskCore| core.displays_changed());
    }

    /// 对应 C# `Recover`：启动时把上次没还回去的窗口还回去。
    pub fn recover(&self) {
        self.inner.slot.run(|core: &mut DeskCore| {
            core.put_back_leftover();
        });
    }

    /// 对应 C# `Open`：打开 App 并把出现的窗口放上手机屏。
    pub fn open(&self, app: AppInfo) {
        let label = app.name.clone();
        self.inner.slot.run(move |core: &mut DeskCore| {
            core.begin(DeskWork::with_label(DeskWorkKind::Opening, label));
            core.open_window(app, None);
            core.set_state(core.state().with_work(None));
        });
    }

    /// 对应 C# `NewWindow`：让已经在跑的 App 再开一个窗口。
    pub fn new_window(&self, app: RunningApp) {
        if self.state().quitting.contains(&app.pid) {
            self.log(format!("{} 正在退出，不新建窗口", app.app.name));
            return;
        }
        let label = app.app.name.clone();
        let pid = app.pid;
        self.inner.slot.run(move |core: &mut DeskCore| {
            core.begin(DeskWork::with_label(DeskWorkKind::Opening, label));
            core.open_window(app.app, Some(pid));
            core.set_state(core.state().with_work(None));
        });
    }

    /// 对应 C# `Quit`：先放回窗口，再请 App 退出。
    pub fn quit(&self, app: RunningApp) {
        if self.state().quitting.contains(&app.pid) {
            return;
        }
        self.inner.slot.run(move |core: &mut DeskCore| core.quit_app(app));
    }

    /// 对应 C# `ForceQuit`。
    pub fn force_quit(&self, pid: i32, app_name: impl Into<String>) {
        let app_name = app_name.into();
        self.inner.slot.run(move |core: &mut DeskCore| {
            core.unwatch(WatchedKey::for_quit(pid));
            core.set_state(core.state().with_notice(None));
            core.warn(format!("强制退出 {app_name}（pid {pid}）"));
            if !core.shared.apps.force_quit(pid) {
                let notice = DeskNotice::WindowUnresponsive { app: app_name.clone() };
                core.set_state(core.state().with_notice(Some(notice)));
            }
        });
    }

    /// 对应 C# `Close`。
    pub fn close(&self, window: WindowInfo) {
        self.inner.slot.run(move |core: &mut DeskCore| core.close_window(window));
    }

    /// 对应 C# `ClosePlaced`：手机上点了关闭按钮，先放回原位再关。
    pub fn close_placed(&self, window_handle: u64) {
        self.inner.slot.run(move |core: &mut DeskCore| {
            let Some(placement) = core.placement.clone() else {
                return;
            };
            if placement.window.key.handle != window_handle {
                return;
            }
            core.log(format!(
                "手机上点了 {} 的关闭按钮：先放回原位再关",
                placement.window.label()
            ));
            core.close_window(placement.window);
        });
    }

    /// 对应 C# `DismissNotice`。
    pub fn dismiss_notice(&self) {
        self.inner.slot.run(|core: &mut DeskCore| {
            if let Some(DeskNotice::QuitStalled { pid, .. }) = core.state().notice {
                core.unwatch(WatchedKey::for_quit(pid));
            }
            core.set_state(core.state().with_notice(None));
        });
    }

    /// 对应 C# `ShowList`：回到 App 资源库。
    pub fn show_list(&self) {
        self.inner.slot.run(|core: &mut DeskCore| {
            core.set_state(core.state().with_list_visible(true));
            core.match_width();
        });
    }

    /// 对应 C# `CheckReturns`：看看有没有窗口带着手机屏上的样子回来了。
    pub fn check_returns(&self) {
        // **去重**：app worker 每拍（500ms）都会调这里，而 `check_returns` 在有
        // left-behind 条目时要枚举窗口、读标题，单次能到 3 秒。不去重就是生产 2 个/秒、
        // 消费 0.33 个/秒 → 队列无限积压，用户点 App 要排一分钟以上才执行（真机 2026-11-19
        // 实测：`[lane] 动作排队 52.3s 才轮到`）。队列里已经有一个就不再加。
        if self.inner.check_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let pending = Arc::clone(&self.inner.check_pending);
        self.inner.slot.run(move |core: &mut DeskCore| {
            core.check_returns();
            pending.store(false, Ordering::SeqCst);
        });
    }

    /// 连点去重：这个窗口已经在排队的路上就返回 `false`（调用方直接放弃这次点击）。
    ///
    /// 一次铺开 1–5 秒，用户在等待期间通常会再点两下。若每次都入队，第二条、第三条
    /// 就排在自己后面，看起来像"点了半天没反应，然后连着动几下"。
    pub(crate) fn claim_pull(&self, window: WindowKey) -> bool {
        let mut slot = self.inner.pull_pending.lock().unwrap_or_else(|e| e.into_inner());
        if *slot == Some(window) {
            return false;
        }
        *slot = Some(window);
        true
    }

    /// 给入队闭包用的把手：动作跑完后清掉"正在铺开"的标记。
    pub(crate) fn pull_guard(&self) -> Arc<Mutex<Option<WindowKey>>> {
        Arc::clone(&self.inner.pull_pending)
    }

    /// 对应 C# `ShutdownAsync`：退出前把手机屏撤干净（阻塞到做完）。
    /// 撤屏并等它做完；返回 `false` 表示超时没等完（调用方要留痕）。
    ///
    /// 真机上踩过两次，改动前先读：
    /// 1. **先在队列外取消重试**。重试线程每档（5/15/30 秒）都会往共享 lane 里塞一个
    ///    `reconcile`，而每次建屏最长 8 秒 × 4 个候选——退出时正好撞上重试阶梯，
    ///    收尾会排在它们后面等到天荒地老（2026-11-19 实测卡了 3 分钟以上、进程不响应）。
    /// 2. 即使取消重试，仍可能有一个**已经在跑**的建屏动作（32 秒）加最短存活 2.5 秒
    ///    加拓扑沉降 1 秒。所以这里带 20 秒上限，超时就放弃等待、由归属记录兜底。
    pub fn shutdown(&self) -> bool {
        self.inner.shared.wanted.store(false, Ordering::SeqCst);
        if let Some(handle) = self
            .inner
            .shared
            .pending_retry
            .lock()
            .expect("retry")
            .take()
        {
            handle.token.cancel();
            handle.finished.store(true, Ordering::SeqCst);
        }
        self.inner.slot.call_timeout(SHUTDOWN_TIMEOUT, |core: &mut DeskCore| {
            core.cancel_retry();
            core.reconcile();
        })
    }

    /// 只入队撤屏、不等它做完（`DeskSet` 摘掉一块屏时用，避免界面线程被长动作卡住）。
    pub(crate) fn shutdown_non_blocking(&self) {
        self.inner.shared.wanted.store(false, Ordering::SeqCst);
        self.inner.slot.run(|core: &mut DeskCore| {
            core.cancel_retry();
            core.reconcile();
        });
    }

    /// 此刻这块屏是否真的静止（没有挂着的重试、没有 watch）。
    #[cfg(test)]
    pub(crate) fn is_quiet(&self) -> bool {
        self.inner.shared.pending_retry.lock().expect("retry").is_none()
            && self.inner.shared.watches.lock().expect("watches").is_empty()
    }

    /// 对应 C# `IdleAsync`：等重试跑完、队列排空、所有 watch 结束。
    ///
    /// **只给测试用**（`#[cfg(test)]` 之外不存在）。它等的是"所有 desk 都 quiet"，
    /// watch 最长能挂 30s/10s/60s，而且是忙等吃满一个核；更要紧的是这期间别的
    /// 动作入不了队——生产路径每拍调它，会让"会话结束撤屏"被推迟到 watch 结束。
    /// 真机撞过一次"退出远控虚拟屏还挂着"，就是这类调用引起的。
    /// 生产代码要推进时间驱动的动作，靠 retry/watch 自己的线程。
    #[cfg(test)]
    pub fn idle(&self) {
        loop {
            let retry = self.inner.shared.pending_retry.lock().expect("retry").clone();
            if let Some(handle) = retry.as_ref() {
                while !handle.finished.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            self.inner.slot.drain();
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
        self.inner.slot.call(move |core: &mut DeskCore| {
            *sink.lock().expect("slot") = core.retiring.is_some();
        });
        let value = *slot.lock().expect("slot");
        value
    }

    /// 测试用：left-behind 里还有几条记录。
    #[cfg(test)]
    pub(crate) fn left_behind_len(&self) -> usize {
        let slot = Arc::new(Mutex::new(0usize));
        let sink = Arc::clone(&slot);
        self.inner.slot.call(move |core: &mut DeskCore| {
            *sink.lock().expect("slot") = core.left_behind().entries.len();
        });
        let value = *slot.lock().expect("slot");
        value
    }
}

impl DeskCore {
    fn screen_id(&self) -> &str {
        &self.shared.screen_id
    }

    fn behavior(&self) -> ScreenBehavior {
        *self.shared.behavior.lock().expect("behavior")
    }

    /// 共享的 left-behind 书（所有屏同一份）。
    fn left_behind(&self) -> MutexGuard<'_, LeftBehindBook> {
        self.shared.books.left_behind.lock().expect("left_behind")
    }

    /// 共享的字号记忆（所有屏同一份）。
    fn zoom_book(&self) -> MutexGuard<'_, ZoomBook> {
        self.shared.books.zoom_book.lock().expect("zoom_book")
    }

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

    fn default_zoom(&self) -> i32 {
        self.shared.default_zoom.load(Ordering::SeqCst)
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
        self.settle_displays();
        if self.retiring.is_some() {
            // 同一 desk 重新进入时也必须等旧屏真正下线，避免覆盖旧屏的善后记录。
            return;
        }
        self.log("加屏：开始");
        self.set_state(self.state().with_screen_phase(PhoneScreenPhase::Preparing));
        let before = self.capture_surroundings("加屏");
        self.topology_changing();
        let fit = self.fit();
        let name = self.shared.screen_name.lock().expect("screen name").clone();
        match self.shared.displays.create_named(fit, &name) {
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
        self.log("加屏：结束");
    }

    /// 对应 C# `RemoveScreenAsync`。
    fn remove_screen(&mut self) {
        self.log("撤屏：开始");
        if let Some(placement) = self.placement.clone() {
            let mut record = placement.record;
            self.put_back(&mut record, true);
        }
        self.set_placement(None);
        // 电脑副屏撤掉前，把落在它矩形里的窗口搬回主屏（C# 在 RetireDisplayAsync 之前调用）。
        self.evacuate_computer_screen_windows();
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
        self.log("撤屏：结束");
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
        let pending = Retiring {
            id: display.id,
            before,
            permanently: !self.remote(),
            retry_after: self.now() + RETIRE_RETRY_DELAY,
        };
        if offline {
            self.settle(pending.before.clone(), "撤屏", pending.permanently);
            self.log(format!("已撤掉手机屏 #{}", display.id));
        } else {
            self.retiring = Some(pending);
            self.log(format!("手机屏 #{} 尚未下线，稍后自动善后", display.id));
        }
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
        self.log(format!("{after_what}：等拓扑沉降 {TOPOLOGY_SETTLE}s"));
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

        // 正在摆放的那个窗口不参与"挪回去"，否则会把它从手机屏上拽回来。
        let except = self.placement.as_ref().map(|placement| placement.window.key);
        self.shared.windows.restore(&before.windows, except);    }

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

        let lane = self.slot.clone();
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
        if let Some(mut retiring) = self.retiring.clone() {
            if self.shared.displays.is_online(retiring.id) {
                if self.now() < retiring.retry_after {
                    return;
                }
                self.topology_changing();
                let offline = self.shared.displays.retire(retiring.id);
                retiring.retry_after = self.now() + RETIRE_RETRY_DELAY;
                self.retiring = Some(retiring.clone());
                if !offline {
                    return;
                }
            } else if !self.shared.displays.retire(retiring.id) {
                // 下线可能发生在上次轮询超时后；让平台完成本地映射和持久归属销账。
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

    // ---- 以下对应 C# PhoneDesk 的摆放相关方法 ----

    /// 对应 C# `Begin`：开始一件动作，清掉旧提示。
    fn begin(&mut self, work: DeskWork) {
        self.set_state(self.state().with_notice(None).with_work(Some(work)));
    }

    /// 对应 C# `CurrentScreenFit`：手机屏当前实际用的比例。
    fn current_screen_fit(&self) -> ScreenFit {
        match self.display {
            Some(display) => self.shared.displays.fit_of(display.id).unwrap_or_else(|| self.fit()),
            None => self.fit(),
        }
    }

    /// 对应 C# `WantedWidth`：列表可见时用基础宽度，窗口铺满时用窗口宽度。
    fn wanted_width(&self) -> i32 {
        match (&self.placement, self.state().list_visible) {
            (Some(placement), false) => placement.width,
            _ => self.current_screen_fit().base_width(),
        }
    }

    /// 对应 C# `MatchWidthAsync`。
    fn match_width(&self) {
        if let Some(display) = self.display {
            self.switch_width(display.id, self.wanted_width());
        }
    }

    /// 对应 C# `SwitchWidthAsync`。
    fn switch_width(&self, id: u32, width: i32) {
        if self.shared.displays.width_of(id) != Some(width) {
            self.shared.displays.set_width(id, width);
        }
    }

    /// 对应 C# `FillAsync`：把窗口挪进手机屏（必要时换更宽的档位）。
    fn fill(&mut self, placement: &mut Placement, id: u32) -> Option<Rect> {
        self.fill_steps(placement, id, 0)
    }

    /// 换档最多走几轮。
    ///
    /// C# 是无限递归（换到不再需要换为止），但它没遇到过"窗口永远比可放区域高一截"
    /// 的情况。真机上 QQ 就是这样：850 宽时窗口高 2990、可放区域 2818，差 172；
    /// 加到 910 又差 178；加到 970 还差——**每加宽一档，窗口跟着长高**，屏幕只会
    /// 被无限推宽，而每次加宽都是一次显示模式变更（0.5–0.9 秒）。
    /// 所以这里封顶：走两轮还铺不满就先这样，别再为它改显示配置。
    const WIDEN_STEPS_LIMIT: usize = 2;

    fn fill_steps(&mut self, placement: &mut Placement, id: u32, steps: usize) -> Option<Rect> {
        // 每一步都量一下：真机上"点 App 要等 5 秒"，光看日志看不出是换屏宽、
        // 挪窗口还是 UIA 找关闭按钮吃掉的。单步超过 300ms 就记一行。
        let mut started = std::time::Instant::now();

        self.switch_width(id, placement.width);
        let ms_switch = started.elapsed().as_secs_f64() * 1000.0;

        started = std::time::Instant::now();
        let area = self.shared.displays.window_area_of(id)?;
        let screen_bounds = self.shared.displays.bounds_of(id)?;
        let mut got = self.shared.windows.move_window(placement.window.key, area)?;
        if !Self::window_origin_on_screen(got, screen_bounds) {
            self.warn(format!(
                "{} 首次未能落到目标屏（实际 {}），经主屏中转再放",
                placement.window.label(),
                got.short()
            ));
            let main = self.shared.displays.main_area()?;
            let relay_x = if got.width <= main.width {
                main.x + (main.width - got.width) / 2.0
            } else {
                main.x
            };
            let relay_y = if got.height <= main.height {
                main.y + (main.height - got.height) / 2.0
            } else {
                main.y
            };
            self.shared.windows.move_window(
                placement.window.key,
                Rect::new(relay_x, relay_y, got.width, got.height),
            )?;
            got = self.shared.windows.move_window(placement.window.key, area)?;
            if !Self::window_origin_on_screen(got, screen_bounds) {
                self.warn(format!(
                    "{} 重试后仍未落到目标屏（实际 {}，目标 {}），放弃摆放",
                    placement.window.label(),
                    got.short(),
                    screen_bounds.short()
                ));
                return None;
            }
        }
        let ms_move = started.elapsed().as_secs_f64() * 1000.0;

        placement.settled = Some(got);
        placement.record.filled = Some(got);
        self.write_record(&placement.record);

        // 先算"要不要换更宽的档"：要换的话这一轮就白铺了，关闭按钮的位置也立刻作废，
        // 不必去查（真机单轮 500–1500ms）。阶梯换成最终档后才查一次。
        let placed_at_origin = (got.min_x() - area.min_x()).abs() < 2.0
            && (got.min_y() - area.min_y()).abs() < 2.0;
        // **只有高度溢出一点点时不要换档。** 窗口的最小尺寸常常跟着屏宽一起长：
        // 真机上 QQ 在 850 宽时高 2990、可放区域 2818（差 6%），加到 910 还是差 6%，
        // 加到 970 还差——每加宽一档它就再长高一点，屏幕被无限推宽，而每次加宽
        // 都是一次 0.5–0.9 秒的显示模式变更。宽度已经放得下就别再追高度了。
        let height_overflow_is_small = got.width <= area.width + 2.0 && got.height < area.height * 1.1;
        let next_width = self
            .shared
            .displays
            .width_of(id)
            .filter(|_| placed_at_origin && !height_overflow_is_small)
            .map(|current| {
                self.current_screen_fit()
                    .width_to_fit(got.size(), area.size(), current)
            })
            .filter(|next| {
                self.shared.displays.width_of(id).is_some_and(|current| {
                    *next > current && *next != placement.width
                })
            });

        let mut ms_close = 0.0;
        if placement.asking {
            self.set_state(self.state().with_close_button(None));
        } else if next_width.is_none() {
            // 找关闭按钮要问 UIA，真机 0.6–1.5 秒。它挡在"窗口已经铺好"这条状态
            // **前面**，用户看到的就是"点完等半天界面才变"。改成排在当前动作之后再查：
            // 铺好的状态立刻发布，按钮晚一秒出现（拖动窗口时才用得上）。
            let key = placement.window.key;
            self.slot.run(move |core: &mut DeskCore| core.refresh_close_button(key));
            ms_close = 0.0;
        }
        for (what, ms) in [("换屏宽", ms_switch), ("挪窗口", ms_move), ("找关闭按钮", ms_close)] {
            if ms >= 300.0 {
                self.log(format!("[耗时] 铺开·{what} {ms:.0}ms"));
            }
        }

        let Some(next_width) = next_width else {
            // 这一档就是最终档：记下它，下次铺同一个 App 直接从这档起步。
            self.learn_width(&placement.window, placement.width);
            return Some(got);
        };
        if steps >= Self::WIDEN_STEPS_LIMIT {
            // 已经换过几档还铺不满：窗口自己不肯缩（比如它的最小尺寸跟着屏宽一起长）。
            // 再换下去只会无限推宽屏幕、每轮多花半秒多，先按现状铺着，并记住这一档，
            // 免得下次又从基础宽重走一遍。
            self.log(format!(
                "{} 换了 {} 档屏宽仍铺不满（可放区域 {}，实际 {}），先按 {} 宽铺着",
                placement.window.label(),
                steps,
                area.short(),
                got.short(),
                placement.width
            ));
            self.learn_width(&placement.window, placement.width);
            return Some(got);
        }
        self.log(format!(
            "{} 在 {} 宽铺不满（可放区域 {}，实际 {}），换到 {} 宽",
            placement.window.label(),
            self.shared.displays.width_of(id).unwrap_or_default(),
            area.short(),
            got.short(),
            next_width
        ));
        placement.width = next_width;
        self.fill_steps(placement, id, steps + 1)
    }

    fn window_origin_on_screen(frame: Rect, bounds: Rect) -> bool {
        frame.x >= bounds.min_x() - 2.0
            && frame.x < bounds.max_x() + 2.0
            && frame.y >= bounds.min_y() - 2.0
            && frame.y < bounds.max_y() + 2.0
    }

    /// 后排一次"去问 UIA 要关闭按钮位置"（真机 0.6–1.5 秒，不能挡在铺好状态前面）。
    fn refresh_close_button(&mut self, key: WindowKey) {
        let started = std::time::Instant::now();
        let rect = self.shared.windows.close_button(key);
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        if ms >= 300.0 {
            self.log(format!("[耗时] 找关闭按钮 {ms:.0}ms（铺好之后补）"));
        }
        // 期间用户可能已经回列表或换了窗口，结果只认还是同一个铺着的窗口。
        let Some(placement) = self.placement.clone() else {
            return;
        };
        if placement.window.key != key {
            return;
        }
        let Some(window_frame) = placement.settled else {
            return;
        };
        let button = rect.map(|frame| CloseButton { frame, window_handle: key.handle, window_frame });
        self.set_state(self.state().with_close_button(button));
    }

    /// 记住"这个 App 上次铺满用的是哪一档屏宽"（存在共享盘上，跨进程有效）。
    ///
    /// C# 每次都从基础宽（540）起步，靠 `FillAsync` 递归一档档试——真机上 QQ
    /// 要 540 → 720 → 820 → 850，每换一档就是一次显示模式变更（400–700ms）
    /// 加一次挪窗口加一次找关闭按钮，合计 4.9 秒，用户看到的就是"点了没反应"。
    /// 记住上一轮的档位，下次直接从它起步。
    fn learn_width(&mut self, window: &WindowInfo, width: i32) {
        let Some(key) = self.width_key(window) else {
            return;
        };
        let books = Arc::clone(&self.shared.books);
        let changed = {
            let mut map = books.learned_widths.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(key, width) != Some(width)
        };
        if changed {
            let map = books.learned_widths.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let _ = books.learned_widths_file.save(&map);
        }
    }

    /// 铺某个窗口该从哪一档起步：学过的档位，否则基础宽。
    fn start_width(&self, window: &WindowInfo) -> i32 {
        let fit = self.current_screen_fit();
        let base = fit.base_width();
        let Some(learned) = self
            .width_key(window)
            .and_then(|key| {
                self.shared
                    .books
                    .learned_widths
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&key)
                    .copied()
            })
        else {
            return base;
        };
        if fit.widths().contains(&learned) { learned } else { base }
    }

    /// 学到的屏宽的键：比例 + App。比例换了（换机型/方向）旧记录自然失效。
    fn width_key(&self, window: &WindowInfo) -> Option<String> {
        Some(format!("{}|{}", self.current_screen_fit(), app_key(window)?))
    }

    /// 对应 C# `RevealAsync`：从 App 资源库回到已经摆好的窗口。
    fn reveal(&mut self) {
        let (Some(display), Some(placement)) = (self.display, self.placement.clone()) else {
            return;
        };
        self.switch_width(display.id, placement.width);
        self.refill("回到窗口");
        self.set_state(self.state().with_list_visible(false));
        self.shared.windows.focus(placement.window.key);
    }

    /// 对应 C# `RefillAsync`：窗口被挪走 / 屏宽变了，重新铺满。
    fn refill(&mut self, reason: &str) {
        let (Some(display), Some(placement)) = (self.display, self.placement.clone()) else {
            return;
        };
        let Some(look) = self.shared.windows.look(placement.window.key) else {
            return;
        };
        let moved = match placement.settled {
            None => true,
            Some(settled) => !look.frame.matches(settled, 2.0),
        };
        if !moved && self.shared.displays.width_of(display.id) == Some(placement.width) {
            return;
        }
        self.log(format!("{reason}，{} 重新铺满", placement.window.label()));
        let mut placement = placement;
        self.fill(&mut placement, display.id);
        self.set_placement(Some(placement));
    }

    /// 对应 C# `RecheckAsync`。
    fn recheck(&mut self) {
        if self.state().list_visible {
            self.match_width();
        } else {
            self.refill("屏幕配置变了");
        }
    }

    /// 对应 C# `ReshapeAsync`：换屏幕比例。
    fn reshape(&mut self, id: u32) {
        let fit = self.fit();
        if !self.shared.displays.reshape(id, fit.clone()) {
            self.warn(format!("手机屏 #{id} 没能换成新比例（{fit}），下次加屏时按新的建"));
            self.set_state(self.state().with_notice(Some(DeskNotice::ReshapeFailed)));
            return;
        }
        self.set_state(self.state().with_notice(Some(DeskNotice::Reshaped)));

        let Some(mut placement) = self.placement.clone() else {
            return;
        };
        placement.width = fit.base_width();
        placement.settled = None;
        // 不用清学到的屏宽：键里带比例，换比例后自然查不到旧的。
        if !self.state().list_visible {
            self.log(format!("换了比例，{} 按新比例重新铺满", placement.window.label()));
            self.fill(&mut placement, id);
        }
        self.set_placement(Some(placement));
    }

    /// 对应 C# `OriginalFrame`：窗口原本就落在手机屏上时，给它一个回到主屏中央的位置。
    fn original_frame(&self, frame: Rect, phone_display_id: u32) -> Rect {
        if let Some(phone) = self.shared.displays.bounds_of(phone_display_id)
            && phone.contains(frame.center())
            && let Some(main) = self.shared.displays.main_area()
        {
            let width = frame.width.min(main.width);
            let height = frame.height.min(main.height);
            // C# 用 Math.Round（银行家舍入），这里用 round_ties_even 保持一致。
            return Rect::new(
                (main.mid_x() - width / 2.0).round_ties_even(),
                (main.mid_y() - height / 2.0).round_ties_even(),
                width,
                height,
            );
        }
        frame
    }

    /// 对应 C# `ReadyDisplay`：没有手机屏或读不到可放区域时给提示。
    fn ready_display(&self, for_what: &str) -> Option<u32> {
        let Some(display) = self.display else {
            self.warn(format!("{for_what}：没有手机屏（{:?}）", self.state().screen_phase));
            return None;
        };
        if self.shared.displays.window_area_of(display.id).is_none() {
            self.warn(format!("{for_what}：读不到手机屏 #{} 的可放区域", display.id));
            return None;
        }
        Some(display.id)
    }

    /// 对应 C# `ReachAsync`：读窗口状态，读不到就提示。
    fn reach(&mut self, window: &WindowInfo) -> Option<WindowLook> {
        if let Some(look) = self.shared.windows.look(window.key) {
            return Some(look);
        }
        self.warn(format!(
            "{}（{:#x}）碰不到：窗口已关或读不到状态，不铺",
            window.label(),
            window.key.handle
        ));
        let notice = self.unreachable(window);
        self.set_state(self.state().with_notice(Some(notice)));
        None
    }

    /// 对应 C# `UnreachableAsync`。
    fn unreachable(&self, window: &WindowInfo) -> DeskNotice {
        if self.shared.windows.exists(window.key) {
            DeskNotice::WindowUnresponsive { app: window.app_name.clone() }
        } else {
            DeskNotice::WindowGone { app: window.app_name.clone() }
        }
    }

    /// 对应 C# `Pull`：入队前的 Begin + PlaceAsync + 清 work。
    pub(crate) fn pull_with(&mut self, window: WindowInfo, transfer: Option<PhoneRecord>) {
        self.begin(DeskWork::with_label(DeskWorkKind::Placing, window.label()));
        self.place(window, true, transfer);
        self.set_state(self.state().with_work(None));
    }

    /// 对应 C# `PlaceAsync`：把窗口放上手机屏（`zoom = false` 用于"询问窗口"）。
    ///
    /// `transfer` 非空表示这个窗口是从别的屏转移过来的：沿用当时的原始还原坐标
    /// （C# 的 `transferredRecord with { Window = window.Key }`）。
    fn place(&mut self, window: WindowInfo, zoom: bool, transfer: Option<PhoneRecord>) {
        let started = self.now();
        let Some(id) = self.ready_display(&format!("放 {}", window.label())) else {
            self.set_state(self.state().with_notice(Some(DeskNotice::ScreenNotReady)));
            return;
        };

        if self.placement.as_ref().map(|placement| placement.window.key) == Some(window.key) {
            // 同一个窗口再点一次 = "回到它"，不是重新铺。留一行日志，
            // 否则真机上"点了没反应"分不清是这条还是根本没进来。
            self.log(format!("{} 已经铺在手机上，回到它", window.label()));
            self.reveal();
            return;
        }
        if self.reach(&window).is_none() {
            return;
        }

        if let Some(current) = self.placement.clone() {
            if !self.return_to_original(&current, true) {
                self.warn(format!(
                    "{} 放不回去，先不换成 {}",
                    current.window.label(),
                    window.label()
                ));
                let app = current.window.app_name.clone();
                self.set_state(self.state().with_notice(Some(DeskNotice::WindowUnresponsive { app })));
                return;
            }
        }

        if let Some(stuck) = self.put_back_leftover() {
            self.warn(format!("{} 还放不回去，先不放 {}", stuck.label, window.label()));
            let app = stuck.app.clone();
            self.set_state(self.state().with_notice(Some(DeskNotice::WindowUnresponsive { app })));
            return;
        }

        let Some(look) = self.reach(&window) else {
            return;
        };

        let known: HashSet<WindowKey> = self
            .shared
            .windows
            .shown_windows(window.key.pid)
            .into_iter()
            .map(|shown| shown.key)
            .filter(|key| *key != window.key)
            .collect();

        let (owed_index, record) = {
            let book = self.left_behind();
            // 跨屏转移时不查 left-behind：记录直接沿用来源屏的。
            let owed_index =
                if transfer.is_some() { None } else { book.match_window(&window, look.frame) };
            let record = match transfer {
                Some(mut record) => {
                    record.window = window.key;
                    self.log(format!("{} 直接跨屏转移，沿用原始还原坐标", window.label()));
                    record
                }
                None => match owed_index {
                    Some(index) => book.entries[index].returned_as(window.key),
                    None => PhoneRecord {
                        window: window.key,
                        app: window.app_name.clone(),
                        label: window.label(),
                        original: self.original_frame(look.frame, id),
                        was_full_screen: look.full_screen,
                        was_minimized: look.minimized,
                        zoom: 0,
                        app_id: window.app_id.clone(),
                        title: Some(window.title.clone()),
                        filled: None,
                    },
                },
            };
            (owed_index, record)
        };

        let mut next = Placement {
            window: window.clone(),
            record,
            width: self.start_width(&window),
            settled: None,
            known,
            asking: !zoom,
        };
        if !self.write_record(&next.record) {
            self.set_state(self.state().with_notice(Some(DeskNotice::RecordUnwritable)));
            return;
        }
        if let Some(index) = owed_index {
            self.log(format!("{} 带着手机屏上的样子回来了，接着放", window.label()));
            self.left_behind().remove_at(index);
            self.save_left_behind();
        }
        if look.minimized {
            self.shared.windows.set_minimized(window.key, false);
        }
        if look.full_screen {
            self.shared.windows.set_full_screen(window.key, false);
            if let Some(windowed) = self.shared.windows.look(window.key) {
                next.record.original = self.original_frame(windowed.frame, id);
                self.write_record(&next.record);
            }
        }

        self.set_placement(Some(next.clone()));
        let Some(got) = self.fill(&mut next, id) else {
            let notice = self.unreachable(&window);
            self.warn(format!("{} 挪不上手机屏，放回去", window.label()));
            self.set_placement(None);
            let mut record = next.record.clone();
            self.put_back(&mut record, true);
            self.match_width();
            self.set_state(self.state().with_notice(Some(notice)));
            return;
        };

        self.set_placement(Some(next.clone()));
        self.shared.windows.focus(window.key);
        self.set_state(self.state().with_list_visible(false));
        self.log(format!(
            "{} 放上手机屏（{:.1}s）：原位 {}，当前 {}",
            window.label(),
            self.now() - started,
            next.record.original.short(),
            got.short()
        ));

        let steps =
            self.zoom_book().steps_for(window.app_id.as_deref(), self.default_zoom())
                - next.record.zoom;
        if !zoom || steps == 0 {
            return;
        }
        self.set_state(self.state().with_work(Some(DeskWork::new(DeskWorkKind::Zooming))));
        self.sleep(SETTLE_DELAY);
        self.change_zoom(steps);
    }

    /// 对应 C# `ReturnToOriginalAsync`：把窗口放回原位并清掉摆放状态。
    fn return_to_original(&mut self, placement: &Placement, restoring_state: bool) -> bool {
        let mut placement = placement.clone();
        let mut record = placement.record.clone();
        if let Some(left) = self.put_back(&mut record, restoring_state) {
            placement.record = left;
            self.set_placement(Some(placement));
            return false;
        }
        self.set_placement(None);
        self.set_state(self.state().with_list_visible(true));
        true
    }

    /// 对应 C# `PutBackAsync`：撤字号、挪回原位、恢复最大化 / 最小化。
    /// 返回 `Some(record)` 表示"窗口还在但放不回去"。
    fn put_back(&mut self, record: &mut PhoneRecord, restoring_state: bool) -> Option<PhoneRecord> {
        let zoom = record.zoom;
        if zoom != 0 {
            if !self.shared.windows.zoom(record.window, -zoom) {
                return self.stuck_or_gone(record, "缩不回字号");
            }
            record.zoom = 0;
            self.write_record(record);
        }
        if self.shared.windows.move_window(record.window, record.original).is_none() {
            return self.stuck_or_gone(record, "挪不动");
        }

        if restoring_state && record.was_full_screen {
            self.sleep(SETTLE_DELAY);
            self.shared.windows.set_full_screen(record.window, true);
        } else if restoring_state && record.was_minimized {
            self.shared.windows.set_minimized(record.window, true);
        }
        self.settle_record();
        self.log(format!("{} 已放回原位", record.label));
        None
    }

    /// 对应 C# `PutBackLeftoverAsync`：上次崩溃 / 退出留下的还原记录。
    fn put_back_leftover(&mut self) -> Option<PhoneRecord> {
        if self.placement.is_some() {
            return None;
        }
        let Some(record) = self.shared.ledger.pending() else {
            return None;
        };
        if !self.shared.windows.exists(record.window) {
            self.log(format!("还原记录里的 {} 已经不在了，等它回来再还", record.label));
            self.leave_behind(&record);
            return None;
        }
        self.log(format!(
            "{} 上次没放回去，现在放回 {}",
            record.label,
            record.original.short()
        ));
        let mut record = record;
        self.put_back(&mut record, true)
    }

    /// 对应 C# `StuckOrGoneAsync`。
    fn stuck_or_gone(&mut self, record: &PhoneRecord, what: &str) -> Option<PhoneRecord> {
        if self.shared.windows.exists(record.window) {
            self.warn(format!(
                "{} 还在，但{what}，还原记录留着，稍后再放回",
                record.label
            ));
            return Some(record.clone());
        }
        self.log(format!("{} 已经关了，等它回来再还", record.label));
        self.leave_behind(record);
        None
    }

    /// 对应 C# `LeaveBehindAsync`：窗口已经不在了，把"回来的样子"记下来。
    fn leave_behind(&mut self, record: &PhoneRecord) {
        let mut record = record.clone();
        if record.zoom != 0 {
            match zoom::scope_of(record.app_id.as_deref()) {
                Some(ZoomScope::App) => {
                    if !self.shared.apps.is_running(record.window.pid)
                        || self.repay_app_zoom(&record)
                    {
                        record.zoom = 0;
                    }
                }
                Some(ZoomScope::Workspace) => {
                    let notice = DeskNotice::ZoomLeftBehind {
                        app: record.app.clone(),
                        steps: record.zoom,
                    };
                    self.set_state(self.state().with_notice(Some(notice)));
                }
                _ => {}
            }
        }
        let now = now_ms();
        {
            let mut book = self.left_behind();
            book.add(LeftBehindEntry::new(record, now));
            book.prune(now);
        }
        self.save_left_behind();
        self.settle_record();
    }

    /// 对应 C# `RepayAppZoomAsync`：把字号还给同一进程里的另一个窗口。
    fn repay_app_zoom(&self, record: &PhoneRecord) -> bool {
        let Some(other) = self
            .shared
            .windows
            .shown_windows(record.window.pid)
            .into_iter()
            .find(|window| window.key != record.window)
        else {
            return false;
        };
        self.shared.windows.zoom(other.key, -record.zoom)
    }

    /// 对应 C# `ChangeZoomAsync`。
    fn change_zoom(&mut self, delta: i32) -> bool {
        let Some(placement) = self.placement.clone() else {
            return false;
        };
        let mut placement = placement;
        let before = placement.record.zoom;
        let target = (before + delta).clamp(zoom::MIN_STEPS, zoom::MAX_STEPS);
        if target == before {
            return false;
        }

        placement.record.zoom = target;
        if !self.write_record(&placement.record) {
            placement.record.zoom = before;
            self.set_state(self.state().with_notice(Some(DeskNotice::RecordUnwritable)));
            return false;
        }
        if !self.shared.windows.zoom(placement.window.key, target - before) {
            placement.record.zoom = before;
            self.write_record(&placement.record);
            let notice = self.unreachable(&placement.window);
            self.set_state(self.state().with_notice(Some(notice)));
            return false;
        }
        self.set_placement(Some(placement.clone()));
        let sign = if target > 0 { "+" } else { "" };
        self.log(format!("{} 字号 {sign}{target} 级", placement.window.label()));
        self.sleep(SETTLE_DELAY);
        if !self.state().list_visible {
            self.refill("字号改了");
        }
        true
    }

    /// 对应 C# `SetPlacement`。
    fn set_placement(&mut self, placement: Option<Placement>) {
        let old_key = self.placement.as_ref().map(|current| current.window.key);
        self.placement = placement;
        let placed = self.placement.as_ref().map(|current| PlacedWindow {
            window: current.window.clone(),
            zoom: current.record.zoom,
        });
        let close_button = if self.placement.as_ref().map(|current| current.window.key) != old_key {
            None
        } else {
            self.state().close_button
        };
        self.set_state(self.state().with_placed(placed).with_close_button(close_button));
    }

    /// 对应 C# `WriteRecord`。
    fn write_record(&self, record: &PhoneRecord) -> bool {
        match self.shared.ledger.record(record) {
            Ok(()) => true,
            Err(error) => {
                self.error(format!("还原记录写不进去：{error}"));
                false
            }
        }
    }

    /// 对应 C# `SettleRecord`。
    fn settle_record(&self) {
        self.shared.ledger.settle();
    }

    /// 对应 C# `SaveLeftBehind`。
    fn save_left_behind(&self) {
        let book = self.left_behind();
        if let Err(error) = self.shared.books.left_behind_file.save(&book) {
            self.error(format!("left-behind.json 保存失败：{error}"));
        }
    }

    /// 对应 C# `SaveZoomBook`。
    fn save_zoom_book(&self) {
        let book = self.zoom_book();
        if let Err(error) = self.shared.books.zoom_file.save(&book) {
            self.error(format!("zoom.json 保存失败：{error}"));
        }
    }

    /// 对应 C# `VerifyPlacement`。
    fn verify_placement(&mut self) {
        if self.placement.is_none() || self.state().work.is_some() || self.verifying {
            return;
        }
        self.verifying = true;
        if let Some(placement) = self.placement.clone() {
            if !self.shared.windows.exists(placement.window.key) {
                self.log(format!(
                    "手机屏上的 {} 在外部关闭，回到 App 资源库",
                    placement.window.label()
                ));
                self.set_placement(None);
                self.set_state(self.state().with_list_visible(true));
                let mut known = placement.known.clone();
                known.insert(placement.window.key);
                self.replaced = Some(Replaced {
                    pid: placement.window.key.pid,
                    known,
                    until: self.now() + REPLACE_WAIT,
                });
                self.leave_behind(&placement.record);
                self.match_width();
            } else if !self.state().list_visible
                && !self.shared.windows.is_shown(placement.window.key)
            {
                self.log(format!(
                    "手机屏上的 {} 不在屏上了，回到 App 资源库",
                    placement.window.label()
                ));
                self.set_state(self.state().with_list_visible(true));
                self.match_width();
            }
        }
        self.verifying = false;
    }

    /// 对应 C# `DisplaysChanged`：显示器配置变了，等一下再重查。
    fn displays_changed(&mut self) {
        self.settle_displays();
        if self.display.is_none() {
            return;
        }
        self.recheck_generation += 1;
        let generation = self.recheck_generation;
        let lane = self.slot.clone();
        let timeline = Arc::clone(&self.shared.timeline);
        std::thread::Builder::new()
            .name("snug-desk-recheck".to_string())
            .spawn(move || {
                timeline.sleep(RECHECK_DELAY, &CancelToken::new());
                lane.run(move |core: &mut DeskCore| {
                    if generation == core.recheck_generation {
                        core.recheck();
                    }
                });
            })
            .expect("创建屏幕配置重查线程失败");
    }

    // ---- 以下对应 C# 的打开 / 退出 / 关闭与 watch ----

    /// 对应 C# `OpenWindowAsync`：打开 App 并把它的新窗口放上手机屏。
    fn open_window(&mut self, app: AppInfo, new_window_in: Option<i32>) {
        let started = self.now();
        if self.ready_display(&format!("打开 {}", app.name)).is_none() {
            self.set_state(self.state().with_notice(Some(DeskNotice::ScreenNotReady)));
            return;
        }

        let known_pid = new_window_in.or_else(|| self.shared.apps.pid_of(&app));
        if let Some(pid) = known_pid
            && self.state().quitting.contains(&pid)
        {
            self.log(format!("{} 正在退出，不打开", app.name));
            return;
        }

        let mut before: HashSet<WindowKey> = HashSet::new();
        if let Some(pid) = known_pid {
            for window in self.shared.windows.shown_windows(pid) {
                before.insert(window.key);
            }
        }
        if new_window_in.is_none() {
            for window in self.shared.apps.app_windows(&app, known_pid) {
                before.insert(window.key);
            }
        }

        let pid = match new_window_in {
            Some(running_pid) => {
                if !self.shared.apps.new_window(running_pid) {
                    self.warn(format!("{} 无法新建窗口", app.name));
                    let name = app.name.clone();
                    self.set_state(self.state().with_notice(Some(DeskNotice::NoNewWindow { app: name })));
                    return;
                }
                running_pid
            }
            None => match self.shared.apps.open(&app) {
                Some(opened_pid) => opened_pid,
                None => {
                    self.warn(format!("打不开 {}", app.name));
                    let name = app.name.clone();
                    self.set_state(self.state().with_notice(Some(DeskNotice::OpenFailed { app: name })));
                    return;
                }
            },
        };

        let wait = if known_pid.is_none() { LAUNCH_WAIT } else { WINDOW_WAIT };
        let deadline = self.now() + wait;
        let exit_grace_deadline = self.now() + 3.0;
        while self.wanted() {
            let pid_windows = self.shared.windows.shown_windows(pid);
            let app_windows = if new_window_in.is_none() {
                self.shared.apps.app_windows(&app, Some(pid))
            } else {
                Vec::new()
            };
            let app_windows_empty = app_windows.is_empty();
            let appeared = pid_windows
                .into_iter()
                .chain(app_windows)
                .find(|window| !before.contains(&window.key));
            if let Some(appeared) = appeared {
                self.log(format!(
                    "{}（{:.1}s）出现 {}，放上手机屏",
                    app.name,
                    self.now() - started,
                    appeared.label()
                ));
                self.set_state(
                    self.state()
                        .with_work(Some(DeskWork::with_label(DeskWorkKind::Placing, appeared.label()))),
                );
                self.place(appeared, true, None);
                return;
            }
            if !self.shared.apps.is_running(pid)
                && self.now() >= exit_grace_deadline
                && app_windows_empty
            {
                self.warn(format!("{} 打开后已退出", app.name));
                let name = app.name.clone();
                self.set_state(self.state().with_notice(Some(DeskNotice::OpenFailed { app: name })));
                return;
            }
            if self.now() >= deadline {
                self.warn(format!("{} 打开超时无窗口", app.name));
                let name = app.name.clone();
                self.set_state(self.state().with_notice(Some(DeskNotice::NoWindow { app: name })));
                return;
            }
            self.sleep(WINDOW_POLL);
        }
    }

    /// 对应 C# `Quit` 里排进队列的那段。
    pub(crate) fn quit_app(&mut self, app: RunningApp) {
        let pid = app.pid;
        self.set_state(self.state().with_notice(None).with_quitting_added(pid));

        if let Some(placement) = self.placement.clone()
            && placement.window.key.pid == pid
        {
            if !self.return_to_original(&placement, true) {
                self.warn(format!(
                    "{} 放不回去，先不退出 {}",
                    placement.window.label(),
                    app.app.name
                ));
                self.remove_quitting(pid);
                let name = placement.window.app_name.clone();
                self.set_state(self.state().with_notice(Some(DeskNotice::WindowUnresponsive { app: name })));
                return;
            }
            self.match_width();
        }

        self.log(format!("请 {} 退出", app.app.name));
        if !self.shared.apps.quit(pid) {
            self.remove_quitting(pid);
            let name = app.app.name.clone();
            self.set_state(self.state().with_notice(Some(DeskNotice::WindowUnresponsive { app: name })));
            return;
        }
        self.watch_quit(app);
    }

    /// 对应 C# `Close` 里排进队列的那段。
    fn close_window(&mut self, window: WindowInfo) {
        let key = window.key;
        let watch_key = WatchedKey::for_close(key);
        if self.shared.watches.lock().expect("watches").contains_key(&watch_key) {
            return;
        }
        self.set_state(self.state().with_notice(None).with_closing_added(key));

        if self.placement.as_ref().map(|placement| placement.window.key) == Some(key)
            && let Some(placement) = self.placement.clone()
        {
            if !self.return_to_original(&placement, false) {
                self.warn(format!("{} 放不回去，先不关", placement.window.label()));
                self.remove_closing(key);
                let name = placement.window.app_name.clone();
                self.set_state(self.state().with_notice(Some(DeskNotice::WindowUnresponsive { app: name })));
                return;
            }
            self.match_width();
        }

        if let Some(look) = self.shared.windows.look(key)
            && look.full_screen
        {
            self.shared.windows.set_full_screen(key, false);
        }

        let was_shown = self.shared.windows.is_shown(key);
        if !self.shared.windows.close(key) {
            let notice = self.unreachable(&window);
            self.warn(format!("关不了 {}", window.label()));
            self.remove_closing(key);
            self.set_state(self.state().with_notice(Some(notice)));
            return;
        }
        self.log(format!("关闭 {}", window.label()));
        self.watch_close(window, was_shown);
    }

    /// 对应 C# `Watch`：起一个线程跑监视体，登记取消令牌。
    fn watch(&self, key: WatchedKey, body: impl FnOnce(WatchContext) + Send + 'static) {
        let token = CancelToken::new();
        {
            let mut watches = self.shared.watches.lock().expect("watches");
            if let Some(previous) = watches.insert(key, token.clone()) {
                previous.cancel();
            }
        }
        let shared = Arc::clone(&self.shared);
        let context = WatchContext {
            shared: Arc::clone(&shared),
            lane: self.slot.clone(),
            token: token.clone(),
        };
        std::thread::Builder::new()
            .name("snug-desk-watch".to_string())
            .spawn(move || {
                body(context);
                if !token.is_cancelled() {
                    shared.watches.lock().expect("watches").remove(&key);
                }
            })
            .expect("创建 PhoneDesk 监视线程失败");
    }

    /// 对应 C# `Unwatch`。
    fn unwatch(&self, key: WatchedKey) {
        let removed = self.shared.watches.lock().expect("watches").remove(&key);
        if let Some(token) = removed {
            token.cancel();
        }
    }

    /// 对应 C# `WatchQuit`。
    fn watch_quit(&self, app: RunningApp) {
        let pid = app.pid;
        self.watch(WatchedKey::for_quit(pid), move |context| {
            watch_quit_body(context, app);
        });
    }

    /// 对应 C# `WatchClose`。
    fn watch_close(&self, window: WindowInfo, hides: bool) {
        self.watch(WatchedKey::for_close(window.key), move |context| {
            watch_close_body(context, window, hides);
        });
    }

    /// 对应 C# `RemoveQuitting`。
    fn remove_quitting(&mut self, pid: i32) {
        let state = self.state().with_quitting_removed(pid);
        self.set_state(state);
    }

    /// 对应 C# `RemoveClosing`。
    fn remove_closing(&mut self, key: WindowKey) {
        let state = self.state().with_closing_removed(key);
        self.set_state(state);
    }

    /// 对应 C# `CheckReturns`。
    pub(crate) fn check_returns(&mut self) {
        if (self.replaced.is_none() && self.left_behind().entries.is_empty())
            || self.state().work.is_some()
            || self.checking_returns
        {
            return;
        }
        self.checking_returns = true;
        self.follow_replacement();
        self.take_back_returned();
        self.checking_returns = false;
    }

    /// 对应 C# `FollowReplacementAsync`。
    fn follow_replacement(&mut self) {
        let Some(replaced) = self.replaced.clone() else {
            return;
        };
        if self.placement.is_some() || self.now() >= replaced.until {
            self.replaced = None;
            return;
        }
        if self.display.is_none() {
            return;
        }
        let next = self
            .shared
            .windows
            .shown_windows(replaced.pid)
            .into_iter()
            .find(|window| !replaced.known.contains(&window.key));
        let Some(next) = next else {
            return;
        };
        self.log(format!(
            "{} 换了个窗口，接着放上手机屏：{}",
            next.app_name,
            next.label()
        ));
        if self.place_on_its_own(next) {
            self.replaced = None;
        }
    }

    /// 对应 C# `PlaceOnItsOwnAsync`。
    fn place_on_its_own(&mut self, window: WindowInfo) -> bool {
        if self.state().work.is_some() {
            return false;
        }
        self.set_state(
            self.state()
                .with_notice(None)
                .with_work(Some(DeskWork::with_label(DeskWorkKind::Placing, window.label()))),
        );
        self.place(window, true, None);
        if matches!(
            self.state().work.as_ref().map(|work| work.kind),
            Some(DeskWorkKind::Placing) | Some(DeskWorkKind::Zooming)
        ) {
            self.set_state(self.state().with_work(None));
        }
        true
    }

    /// 对应 C# `TakeBackReturnedAsync`。
    fn take_back_returned(&mut self) {
        let pruned = {
            let mut book = self.left_behind();
            let count = book.entries.len();
            book.prune(now_ms());
            book.entries.len() != count
        };
        // 原先只清内存，重启后过期项又会从磁盘加载；有效记录不重写。
        if pruned {
            self.save_left_behind();
        }
        self.repay_app_wide_zoom();

        let mut skip: HashSet<WindowKey> = HashSet::new();
        if let Some(placement) = &self.placement {
            skip.insert(placement.window.key);
        }
        if let Some(pending) = self.shared.ledger.pending() {
            skip.insert(pending.window);
        }

        // C# 用 `Distinct()`：保留首次出现的顺序。
        let mut app_ids: Vec<String> = Vec::new();
        {
            let book = self.left_behind();
            for entry in &book.entries {
                if let Some(app_id) = entry.record.app_id.clone()
                    && !app_ids.contains(&app_id)
                {
                    app_ids.push(app_id);
                }
            }
        }

        for app_id in app_ids {
            for pid in self.shared.apps.pids(&app_id) {
                let candidates: HashMap<WindowKey, Rect> = self
                    .shared
                    .windows
                    .frames(pid)
                    .into_iter()
                    .filter(|(key, frame)| {
                        !skip.contains(key) && self.left_behind().awaits(&app_id, frame.size())
                    })
                    .collect();
                if candidates.is_empty() {
                    continue;
                }
                for window in self.shared.windows.shown_windows(pid) {
                    let Some(frame) = candidates.get(&window.key).copied() else {
                        continue;
                    };
                    let Some(index) = self.left_behind().match_window(&window, frame) else {
                        continue;
                    };
                    self.take_back(index, window, frame);
                    return;
                }
            }
        }
    }

    /// 对应 C# `TakeBackAsync`。
    fn take_back(&mut self, index: usize, window: WindowInfo, frame: Rect) {
        if self.wanted() && self.placement.is_none() {
            if let Some(display) = self.display
                && self
                    .shared
                    .displays
                    .bounds_of(display.id)
                    .map(|bounds| bounds.contains(frame.center()))
                    == Some(true)
            {
                self.place_on_its_own(window);
                return;
            }
        }

        let owed = self.left_behind().entries[index].returned_as(window.key);
        if owed.zoom != 0 {
            if !self.shared.windows.zoom(window.key, -owed.zoom) {
                return;
            }
            self.left_behind().entries[index].record.zoom = 0;
            self.save_left_behind();
        }
        if self.shared.windows.move_window(window.key, owed.original).is_none() {
            return;
        }
        self.left_behind().remove_at(index);
        self.save_left_behind();
        self.log(format!(
            "{} 带着手机屏上的样子回来了，放回原位 {}",
            window.label(),
            owed.original.short()
        ));
    }

    /// 对应 C# `RepayAppWideZoomAsync`。
    fn repay_app_wide_zoom(&mut self) {
        let mut changed = false;
        let count = self.left_behind().entries.len();
        for index in 0..count {
            let entry = self.left_behind().entries[index].clone();
            if entry.scope() != Some(ZoomScope::App) || entry.record.zoom == 0 {
                continue;
            }
            if self.shared.apps.is_running(entry.record.window.pid)
                && !self.shared.windows.shown_windows(entry.record.window.pid).is_empty()
                && !self.repay_app_zoom(&entry.record)
            {
                continue;
            }
            self.left_behind().entries[index].record.zoom = 0;
            changed = true;
        }
        if changed {
            self.save_left_behind();
        }
    }

    /// 对应 C# `DetachPlacementForTransfer`：摘出还原记录、清摆放状态、回到资源库。
    pub(crate) fn detach_placement_for_transfer(&mut self) -> Option<PhoneRecord> {
        let placement = self.placement.as_ref()?;
        let transferred = placement.record.clone();
        self.set_placement(None);
        self.set_state(self.state().with_list_visible(true));
        Some(transferred)
    }

    /// 对应 C# `ReturnWindowIfPlacedAsync`：铺着的窗口属于这个 pid 就先还回原位再对宽度。
    pub(crate) fn return_window_if_placed(&mut self, pid: i32) {
        let Some(placement) = self.placement.clone() else {
            return;
        };
        if placement.window.key.pid != pid {
            return;
        }
        self.return_to_original(&placement, true);
        self.match_width();
    }

    /// 对应 C# `EvacuateComputerScreenWindowsAsync`：电脑副屏撤掉前把落在它矩形内的窗口
    /// 搬到主屏居中（`w`/`h` 取 `min(窗口, 主屏)`，坐标再居中夹到 ≥0）。
    pub(crate) fn evacuate_computer_screen_windows(&mut self) {
        if self.behavior() != ScreenBehavior::Computer {
            return;
        }
        let Some(display) = self.display else {
            return;
        };
        let Some(bounds) = self.shared.displays.bounds_of(display.id) else {
            return;
        };
        let Some(main) = self.shared.displays.main_area() else {
            return;
        };
        for (key, frame) in self.shared.windows.snapshot() {
            if !bounds.contains(frame.center()) {
                continue;
            }
            let w = frame.width.min(main.width);
            let h = frame.height.min(main.height);
            let x = main.x + (main.width - w).max(0.0) / 2.0;
            let y = main.y + (main.height - h).max(0.0) / 2.0;
            self.shared.windows.move_window(key, Rect::new(x, y, w, h));
        }
    }

    /// 对应 C# `Quit` 里"先在别的屏上把这个 App 的窗口还回去"的前置判断。
    pub(crate) fn placed_pid(&self) -> Option<i32> {
        self.placement.as_ref().map(|placement| placement.window.key.pid)
    }

    /// 对应 C# `PlacedWindowKey`。
    pub(crate) fn placed_window_key(&self) -> Option<WindowKey> {
        self.placement.as_ref().map(|placement| placement.window.key)
    }
}

/// watch 线程手里的上下文：平台接口、把动作排回桌面的队列、取消令牌。
struct WatchContext {
    shared: Arc<DeskShared>,
    lane: DeskSlot,
    token: CancelToken,
}

impl WatchContext {
    fn now(&self) -> f64 {
        self.shared.timeline.now()
    }

    fn sleep(&self, seconds: f64) {
        self.shared.timeline.sleep(seconds, &self.token);
    }

    fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    fn log(&self, message: impl AsRef<str>) {
        if let Some(log) = self.shared.events.log.as_ref() {
            log(message.as_ref());
        }
    }

    fn warn(&self, message: impl AsRef<str>) {
        self.log(format!("警告：{}", message.as_ref()));
    }

    /// 把一次状态改动排回桌面队列（C# watch 线程直接改 State，这里串行化）。
    fn set_state(&self, change: impl FnOnce(DeskState) -> DeskState + Send + 'static) {
        self.lane.call(move |core: &mut DeskCore| {
            let next = change(core.state());
            core.set_state(next);
        });
    }

    fn emit_gone(&self, gone: DeskGone) {
        if let Some(callback) = self.shared.events.gone.as_ref() {
            callback(&gone);
        }
    }

    /// 对应 C# `PlaceAskingAsync`：把正在询问的窗口放上手机屏（不调字号）。
    fn place_asking(&self, window: WindowInfo) {
        let label = window.label();
        self.lane.call(move |core: &mut DeskCore| {
            let state = core
                .state()
                .with_notice(None)
                .with_work(Some(DeskWork::with_label(DeskWorkKind::Placing, label)));
            core.set_state(state);
            core.place(window, false, None);
            core.set_state(core.state().with_work(None));
        });
    }

    fn remove_quitting(&self, pid: i32) {
        self.lane.call(move |core: &mut DeskCore| core.remove_quitting(pid));
    }

    fn remove_closing(&self, key: WindowKey) {
        self.lane.call(move |core: &mut DeskCore| core.remove_closing(key));
    }
}

/// 等待结果的三种结局（对应 C# `AnswerKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnswerKind {
    Done,
    Canceled,
    Stalled,
}

/// 对应 C# `AwaitAnswerAsync`：等 App 退出 / 窗口关闭，期间如果它在弹询问就把询问窗口放上屏。
fn await_answer(
    context: &WatchContext,
    pid: i32,
    seconds: f64,
    done: &mut dyn FnMut() -> bool,
) -> AnswerKind {
    let deadline = context.now() + seconds;
    let mut asked: Option<WindowKey> = None;
    let mut answered_at: Option<f64> = None;
    while !context.is_cancelled() {
        if done() {
            return AnswerKind::Done;
        }
        if let Some(asking) = context.shared.windows.asking_window(pid) {
            answered_at = None;
            if asked != Some(asking.key) {
                asked = Some(asking.key);
                context.log(format!(
                    "{} 在询问，把 {} 放上手机屏",
                    asking.app_name,
                    asking.label()
                ));
                context.place_asking(asking);
            }
        } else if asked.is_some() {
            let at = *answered_at.get_or_insert_with(|| context.now());
            if context.now() - at >= ANSWER_GRACE {
                return AnswerKind::Canceled;
            }
        } else if context.now() >= deadline {
            return AnswerKind::Stalled;
        }
        context.sleep(WATCH_POLL);
    }
    AnswerKind::Canceled
}

/// 对应 C# `WatchQuit` 的监视体。
fn watch_quit_body(context: WatchContext, app: RunningApp) {
    let pid = app.pid;
    let mut done = || !context.shared.apps.is_running(pid);
    let answer = await_answer(&context, pid, QUIT_WAIT, &mut done);
    if !context.is_cancelled() {
        match answer {
            AnswerKind::Done => {
                context.log(format!("{} 已退出", app.app.name));
                context.emit_gone(DeskGone::App { pid });
            }
            AnswerKind::Canceled => {
                context.log(format!("{} 没退出（用户取消）", app.app.name));
            }
            AnswerKind::Stalled => {
                context.warn(format!("{} 退出超时", app.app.name));
                let stalled = DeskNotice::QuitStalled { app: app.app.name.clone(), pid };
                let notice = stalled.clone();
                context.set_state(move |state| state.with_notice(Some(notice)));
                context.remove_quitting(pid);

                let until = context.now() + STALLED_FOLLOW;
                while !context.is_cancelled()
                    && context.now() < until
                    && context.shared.apps.is_running(pid)
                {
                    context.sleep(STALLED_POLL);
                }
                if !context.is_cancelled() && !context.shared.apps.is_running(pid) {
                    context.log(format!("{} 后来自己退出了", app.app.name));
                    context.emit_gone(DeskGone::App { pid });
                    let stalled = stalled.clone();
                    context.lane.call(move |core: &mut DeskCore| {
                        if core.state().notice.as_ref() == Some(&stalled) {
                            core.set_state(core.state().with_notice(None));
                        }
                    });
                }
            }
        }
    }
    // C# 的 finally：不管怎样都清掉"正在退出"标记。
    context.remove_quitting(pid);
}

/// 对应 C# `WatchClose` 的监视体。
fn watch_close_body(context: WatchContext, window: WindowInfo, hides: bool) {
    let key = window.key;
    let mut done = || {
        if !context.shared.windows.exists(key) {
            return true;
        }
        hides && !context.shared.windows.is_shown(key)
    };
    let answer = await_answer(&context, key.pid, CLOSE_WAIT, &mut done);
    if !context.is_cancelled() {
        match answer {
            AnswerKind::Done => {
                context.log(format!("{} 已关闭", window.label()));
                context.emit_gone(DeskGone::Window { key });
            }
            AnswerKind::Canceled => {
                context.log(format!("{} 没关（用户取消）", window.label()));
            }
            AnswerKind::Stalled => {
                context.warn(format!("{} 关闭超时", window.label()));
                let app = window.app_name.clone();
                context.set_state(move |state| {
                    state.with_notice(Some(DeskNotice::WindowUnresponsive { app }))
                });
            }
        }
    }
    context.remove_closing(key);
}

/// `DateTimeOffset.Now` 的毫秒值，left-behind 记录用。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// 记"这个窗口属于哪个 App"用的键：优先可执行文件名，读不到就退回显示名。
///
/// 只用于"上次这 App 要多大屏宽"的记忆，所以不要求全局唯一，只要同一 App 稳定。
fn app_key(window: &WindowInfo) -> Option<String> {
    window
        .app_id
        .as_deref()
        .map(|id| id.to_lowercase())
        .or_else(|| (!window.app_name.is_empty()).then(|| window.app_name.to_lowercase()))
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
        assert_eq!(harness.displays.create_names(), vec![harness.desk.screen_name()]);
        assert_eq!(
            harness.displays.width_of(1001),
            Some(harness.desk.fit().base_width()),
            "建屏用当前 fit 的基础宽度"
        );
    }

    #[test]
    fn set_wanted_uses_the_current_configured_screen_name() {
        let harness = Harness::new();
        harness.desk.update_wish("书房平板", ScreenFit::standard(), harness.desk.behavior());
        ready_screen(&harness);

        harness.desk.set_wanted(false);
        harness.desk.idle();
        harness.desk.update_wish("客厅平板", ScreenFit::standard(), harness.desk.behavior());
        ready_screen(&harness);

        assert_eq!(harness.displays.create_names(), vec!["书房平板", "客厅平板"]);
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

    #[test]
    fn reenter_waits_for_pending_retirement_before_creating_another_screen() {
        let harness = Harness::new();
        harness.desk.set_wanted(true);
        harness.desk.idle();
        harness.displays.keep_online_after_retire();
        harness.desk.dismiss_screen();
        harness.desk.idle();
        assert!(harness.desk.retiring());

        harness.desk.reenter_screen();
        harness.desk.idle();
        assert_eq!(harness.displays.create_calls(), 1, "旧屏仍在线时不能覆盖善后记录");
        assert!(harness.desk.retiring());

        harness.displays.bring_offline();
        harness.desk.settle_displays();
        harness.desk.idle();
        assert_eq!(harness.displays.create_calls(), 2);
        assert_eq!(harness.desk.state().screen_phase, PhoneScreenPhase::Ready);
        assert!(!harness.desk.retiring());
    }

    // ---- 摆放分块 ----

    fn window(key: WindowKey, app_id: Option<&str>, title: &str) -> WindowInfo {
        WindowInfo {
            key,
            app_name: "VS Code".into(),
            app_id: app_id.map(str::to_string),
            title: title.into(),
            minimized: false,
        }
    }

    fn ready_screen(harness: &Harness) {
        harness.desk.set_wanted(true);
        harness.desk.idle();
        assert_eq!(harness.desk.state().screen_phase, PhoneScreenPhase::Ready);
    }

    /// 转写自 C# `PhoneDeskTests.Pull_StepsUpWidthWhenClamped_ZoomsAndRestoresOnPutBack`。
    #[test]
    fn pull_steps_up_width_when_clamped_zooms_and_restores_on_put_back() {
        let harness = Harness::new();
        let key = WindowKey::new(42, 0x100);
        let original = Rect::new(100.0, 100.0, 1500.0, 700.0);
        harness.windows.insert(key, original);
        // 窗口最小宽度把 540 撑到 700，铺不满时桌面应自动换到更宽档位。
        harness.windows.set_min_logical_width(700.0);
        ready_screen(&harness);

        harness.desk.pull(window(key, Some("Code.exe"), "README.md"));
        harness.desk.idle();

        assert_eq!(harness.displays.width_of(1001), Some(700), "应该换到 700 宽");
        assert_eq!(harness.windows.zoom_delta_total(), 2, "按默认字号放大 2 级");
        assert!(!harness.desk.state().list_visible);
        assert!(harness.desk.state().placed.is_some());

        harness.desk.put_back();
        harness.desk.idle();

        let state = harness.desk.state();
        assert!(state.placed.is_none());
        assert!(state.list_visible);
        assert_eq!(harness.displays.width_of(1001), Some(540), "列表要回到基础宽度");
        assert_eq!(harness.windows.zoom_delta_total(), 0, "字号应缩回");
        assert_eq!(harness.windows.frame_of(key), Some(original), "窗口应回原位");
        assert!(!harness.paths.phone_record_file().exists(), "还原记录应被清掉");
    }

    #[test]
    fn pull_relays_through_main_screen_after_window_manager_misplaces_target_move() {
        let harness = Harness::new();
        let key = WindowKey::new(42, 0x100);
        harness.windows.insert(key, Rect::new(100.0, 100.0, 800.0, 600.0));
        harness.windows.misplace_moves(1);
        ready_screen(&harness);

        harness.desk.pull(window(key, Some("Code.exe"), "README.md"));
        harness.desk.idle();

        let phone = harness.displays.bounds_of(1001).expect("virtual phone bounds");
        let frame = harness.windows.frame_of(key).expect("placed window");
        assert!(
            frame.x >= phone.min_x() - 2.0
                && frame.x < phone.max_x() + 2.0
                && frame.y >= phone.min_y() - 2.0
                && frame.y < phone.max_y() + 2.0,
            "retry should place the window on the requested virtual display: {frame:?} vs {phone:?}"
        );
        assert!(harness.desk.state().placed.is_some());
        let moves = harness.windows.move_calls();
        assert_eq!(moves.len(), 3, "一次目标移动、一次主屏中转、一次目标重试");
        let main = harness.displays.main_area().expect("main area");
        assert!(DeskCore::window_origin_on_screen(moves[1].1, main));
        assert_eq!(moves[0].1, moves[2].1, "中转后重试相同的目标区域");
    }

    #[test]
    fn pull_restores_original_when_relay_retry_still_misses_phone_screen() {
        let harness = Harness::new();
        let key = WindowKey::new(42, 0x100);
        let original = Rect::new(100.0, 100.0, 800.0, 600.0);
        harness.windows.insert(key, original);
        // 目标移动、中转和重试都留在主屏；之后允许正常恢复原始位置。
        harness.windows.misplace_moves(3);
        ready_screen(&harness);

        harness.desk.pull(window(key, Some("Code.exe"), "README.md"));
        harness.desk.idle();

        assert!(harness.desk.state().placed.is_none());
        assert!(harness.desk.state().notice.is_some());
        assert_eq!(harness.windows.frame_of(key), Some(original));
        let moves = harness.windows.move_calls();
        assert_eq!(moves.len(), 4, "三次尝试后只应再恢复原始位置");
        assert_eq!(moves[3].1, original);
        assert!(!harness.paths.phone_record_file().exists(), "恢复后清除还原记录");
    }

    /// 第二次铺同一个 App 不该重走换档阶梯（真机上一次铺垫 4.9 秒，几乎全是换档）。
    #[test]
    fn placing_the_same_app_again_starts_at_the_learned_width() {
        let harness = Harness::new();
        let key = WindowKey::new(42, 0x100);
        harness.windows.insert(key, Rect::new(100.0, 100.0, 1500.0, 700.0));
        harness.windows.set_min_logical_width(700.0);
        ready_screen(&harness);

        harness.desk.pull(window(key, Some("Code.exe"), "README.md"));
        harness.desk.idle();
        assert_eq!(harness.displays.width_of(1001), Some(700), "先走一遍阶梯");

        // 回列表：屏宽回到基础宽，但"这个 App 要 700"应该记住。
        harness.desk.put_back();
        harness.desk.idle();
        assert_eq!(harness.displays.width_of(1001), Some(540));

        harness.desk.pull(window(key, Some("Code.exe"), "README.md"));
        harness.desk.idle();
        assert_eq!(harness.displays.width_of(1001), Some(700), "第二次直接到 700");
    }

    /// 连点同一个 App：排队期间再点不该排第二条（真机上会变成"点三下等十五秒"）。
    #[test]
    fn repeated_pulls_while_one_is_queued_are_dropped() {
        let harness = Harness::new();
        let key = WindowKey::new(42, 0x100);
        ready_screen(&harness);

        assert!(harness.desk.claim_pull(key), "第一次要接");
        assert!(!harness.desk.claim_pull(key), "还没跑完时的第二次点击要丢掉");
        // 换个窗口不受影响（去重是按窗口的）。
        assert!(harness.desk.claim_pull(WindowKey::new(43, 0x101)));
    }

    /// 换了比例以后，旧比例学到的屏宽不该被套用（键里带比例）。
    #[test]
    fn learned_widths_are_per_screen_ratio() {
        let harness = Harness::new();
        let key = WindowKey::new(42, 0x100);
        harness.windows.insert(key, Rect::new(100.0, 100.0, 1500.0, 700.0));
        harness.windows.set_min_logical_width(700.0);
        ready_screen(&harness);

        harness.desk.pull(window(key, Some("Code.exe"), "README.md"));
        harness.desk.idle();
        assert_eq!(harness.displays.width_of(1001), Some(700));

        harness.desk.put_back();
        harness.desk.idle();
        // 换个比例：档位表跟着变，学到的 700 必须作废。
        harness.desk.set_fit(ScreenFit::new(0.75));
        harness.desk.idle();

        harness.desk.pull(window(key, Some("Code.exe"), "README.md"));
        harness.desk.idle();
        let width = harness.displays.width_of(1001).expect("屏还在");
        assert!(
            harness.desk.fit().widths().contains(&width),
            "换比例后铺出的宽度必须是新档位表里的档（{width}）"
        );
    }

    /// 转写自 C# `PhoneDeskTests.Recover_RestoresLeftoverRecordAfterSimulatedCrash`。
    #[test]
    fn recover_restores_leftover_record_after_simulated_crash() {
        let harness = Harness::new();
        let key = WindowKey::new(88, 0x200);
        let original = Rect::new(50.0, 60.0, 800.0, 600.0);
        // 上次崩溃时窗口还铺在手机屏上。
        harness.windows.insert(key, Rect::new(2560.0, 0.0, 1080.0, 1800.0));

        let ledger = RestoreRecordStore::<PhoneRecord>::new(harness.paths.phone_record_file());
        ledger
            .record(&PhoneRecord {
                window: key,
                app: "Windows Terminal".into(),
                label: "Terminal".into(),
                original,
                zoom: 2,
                app_id: Some("WindowsTerminal.exe".into()),
                ..PhoneRecord::default()
            })
            .expect("record");

        harness.desk.recover();
        harness.desk.idle();

        assert_eq!(harness.windows.frame_of(key), Some(original));
        assert_eq!(harness.windows.zoom_delta_total(), -2, "字号要缩回");
        assert!(ledger.pending().is_none(), "还原后记录应删除");
    }

    #[test]
    fn pulling_the_placed_window_again_just_reveals_it() {
        let harness = Harness::new();
        let key = WindowKey::new(42, 0x100);
        harness.windows.insert(key, Rect::new(100.0, 100.0, 1500.0, 700.0));
        ready_screen(&harness);

        harness.desk.pull(window(key, Some("Code.exe"), "README.md"));
        harness.desk.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 2);

        // 从资源库回到窗口时再"放"一次：只回到窗口，不重复调字号。
        harness.desk.pull(window(key, Some("Code.exe"), "README.md"));
        harness.desk.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 2);
        assert!(harness.desk.state().placed.is_some());
        assert!(!harness.desk.state().list_visible);
    }

    #[test]
    fn zoom_button_clamps_to_book_limits_and_ignores_unzoomable_apps() {
        let harness = Harness::new();
        let key = WindowKey::new(42, 0x100);
        harness.windows.insert(key, Rect::new(100.0, 100.0, 540.0, 700.0));
        ready_screen(&harness);
        harness.desk.pull(window(key, Some("Code.exe"), "README.md"));
        harness.desk.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 2);

        // 一次按到底：2 级 → -4 级（下限），delta = -6。
        harness.desk.zoom(-10);
        harness.desk.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 2 - 6);
        assert_eq!(harness.desk.state().placed.as_ref().map(|p| p.zoom), Some(zoom::MIN_STEPS));

        // 已经在下限，再按不产生新的字号变化。
        harness.desk.zoom(-10);
        harness.desk.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 2 - 6);
    }

    #[test]
    fn zoom_button_does_nothing_for_apps_without_zoom_support() {
        let harness = Harness::new();
        let key = WindowKey::new(43, 0x101);
        harness.windows.insert(key, Rect::new(100.0, 100.0, 540.0, 700.0));
        ready_screen(&harness);
        harness.desk.pull(window(key, Some("notepad.exe"), "notes.txt"));
        harness.desk.idle();

        harness.desk.zoom(1);
        harness.desk.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 0);
    }

    #[test]
    fn put_back_restores_full_screen_state() {
        let harness = Harness::new();
        let key = WindowKey::new(44, 0x102);
        harness.windows.insert(key, Rect::new(100.0, 100.0, 1500.0, 900.0));
        harness.windows.mark_full_screen(key);
        ready_screen(&harness);

        harness.desk.pull(window(key, Some("Code.exe"), "README.md"));
        harness.desk.idle();
        // 上屏前要退出最大化。
        assert!(
            harness.windows.full_screen_calls().contains(&(key, false)),
            "上屏前应退出最大化，实际 {:?}",
            harness.windows.full_screen_calls()
        );

        harness.desk.put_back();
        harness.desk.idle();
        assert!(
            harness.windows.full_screen_calls().contains(&(key, true)),
            "放回时应恢复最大化，实际 {:?}",
            harness.windows.full_screen_calls()
        );
    }

    #[test]
    fn set_fit_reshapes_the_existing_screen() {
        let harness = Harness::new();
        ready_screen(&harness);

        let narrow = ScreenFit::new(1.2);
        harness.desk.set_fit(narrow.clone());
        harness.desk.idle();

        assert_eq!(harness.displays.fit_of(1001), Some(narrow), "屏幕比例应改成新的");
        assert_eq!(harness.desk.state().notice, Some(DeskNotice::Reshaped));
        assert_eq!(harness.desk.fit(), ScreenFit::new(1.2));
        assert!(harness.desk.state().work.is_none(), "改完比例要清掉进行中的动作");
    }

    #[test]
    fn dismiss_puts_the_window_back_and_turns_the_screen_off() {
        let harness = Harness::new();
        let key = WindowKey::new(45, 0x103);
        let original = Rect::new(10.0, 20.0, 900.0, 600.0);
        harness.windows.insert(key, original);
        ready_screen(&harness);
        harness.desk.pull(window(key, Some("Code.exe"), "README.md"));
        harness.desk.idle();

        harness.desk.dismiss_screen();
        harness.desk.idle();

        let state = harness.desk.state();
        assert_eq!(state.screen_phase, PhoneScreenPhase::Off);
        assert!(state.dismissed, "这次远程不再加手机屏");
        assert!(state.placed.is_none());
        assert_eq!(harness.windows.frame_of(key), Some(original));
        assert!(!harness.paths.phone_record_file().exists());
    }

    #[test]
    fn reenter_after_dismiss_rebuilds_the_screen_in_the_same_episode() {
        let harness = Harness::new();
        ready_screen(&harness);
        assert!(harness.desk.state().display_id.is_some());

        harness.desk.dismiss_screen();
        harness.desk.idle();
        assert_eq!(harness.desk.state().screen_phase, PhoneScreenPhase::Off);
        assert!(harness.desk.state().dismissed);

        // 同一次远程里点「重新进入手机屏」：清 dismissed 并立刻重新建屏。
        harness.desk.reenter();
        harness.desk.idle();

        let state = harness.desk.state();
        assert!(!state.dismissed, "重新进入后要清掉标记");
        assert_eq!(state.screen_phase, PhoneScreenPhase::Ready);
        assert!(state.display_id.is_some(), "应该重新建了一块屏");
    }

    #[test]
    fn reenter_without_a_dismissed_screen_is_a_no_op() {
        let harness = Harness::new();
        ready_screen(&harness);
        let id = harness.desk.state().display_id;

        harness.desk.reenter();
        harness.desk.idle();

        assert_eq!(harness.desk.state().display_id, id, "没有 dismissed 就不该动屏");
        assert_eq!(harness.desk.state().screen_phase, PhoneScreenPhase::Ready);
    }

    // ---- 打开 / 退出 / 关闭与 watch 分块 ----

    fn app(name: &str, app_id: &str) -> AppInfo {
        AppInfo {
            app_id: app_id.into(),
            name: name.into(),
            executable_path: None,
            launch_target: None,
            arguments: None,
            working_directory: None,
        }
    }

    fn running(pid: i32, name: &str, app_id: &str, windows: Vec<WindowInfo>) -> RunningApp {
        RunningApp { app: app(name, app_id), pid, windows }
    }

    #[test]
    fn quit_puts_the_window_back_then_reports_the_app_gone() {
        let harness = Harness::new();
        let key = WindowKey::new(60, 0x200);
        let original = Rect::new(20.0, 30.0, 800.0, 600.0);
        harness.windows.insert(key, original);
        ready_screen(&harness);
        harness.desk.pull(window(key, Some("Code.exe"), "a.rs"));
        harness.desk.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 2);

        let info = window(key, Some("Code.exe"), "a.rs");
        harness.desk.quit(running(60, "VS Code", "Code.exe", vec![info]));
        harness.desk.idle();

        assert_eq!(harness.windows.frame_of(key), Some(original), "先放回原位");
        assert_eq!(harness.windows.zoom_delta_total(), 0, "字号也要缩回");
        assert!(harness.desk.state().quitting.is_empty(), "退出后清掉标记");
        assert_eq!(harness.gones(), vec![DeskGone::App { pid: 60 }]);
    }

    #[test]
    fn quit_failure_reports_unresponsive_and_keeps_the_window() {
        let harness = Harness::new();
        let key = WindowKey::new(61, 0x201);
        harness.windows.insert(key, Rect::new(20.0, 30.0, 800.0, 600.0));
        harness.apps.set_quit_result(false);
        ready_screen(&harness);

        harness.desk.quit(running(61, "VS Code", "Code.exe", vec![]));
        harness.desk.idle();

        assert_eq!(
            harness.desk.state().notice,
            Some(DeskNotice::WindowUnresponsive { app: "VS Code".into() })
        );
        assert!(harness.desk.state().quitting.is_empty());
        assert!(harness.gones().is_empty());
        assert!(harness.windows.frame_of(key).is_some());
    }

    #[test]
    fn quit_timeout_shows_a_stalled_notice_that_can_be_dismissed() {
        let harness = Harness::new();
        ready_screen(&harness);
        // 请求退出成功，但进程一直不退。
        harness.apps.set_quit_keeps_running(true);

        harness.desk.quit(running(62, "VS Code", "Code.exe", vec![]));
        harness.desk.idle();

        assert_eq!(
            harness.desk.state().notice,
            Some(DeskNotice::QuitStalled { app: "VS Code".into(), pid: 62 })
        );
        assert!(harness.desk.state().quitting.is_empty());
        assert!(harness.gones().is_empty(), "没退就不该报 Gone");

        harness.desk.dismiss_notice();
        harness.desk.idle();
        assert_eq!(harness.desk.state().notice, None);
    }

    #[test]
    fn close_removes_the_window_and_reports_it_gone() {
        let harness = Harness::new();
        let key = WindowKey::new(63, 0x300);
        let info = window(key, Some("Code.exe"), "a.rs");
        harness.windows.insert_window(info.clone(), Rect::new(5.0, 6.0, 700.0, 500.0));
        ready_screen(&harness);

        harness.desk.close(info);
        harness.desk.idle();

        assert!(harness.windows.frame_of(key).is_none());
        assert_eq!(harness.gones(), vec![DeskGone::Window { key }]);
        assert!(harness.desk.state().closing.is_empty());
    }

    #[test]
    fn close_placed_puts_the_window_back_before_closing() {
        let harness = Harness::new();
        let key = WindowKey::new(64, 0x400);
        let original = Rect::new(7.0, 8.0, 900.0, 600.0);
        harness.windows.insert(key, original);
        ready_screen(&harness);
        harness.desk.pull(window(key, Some("Code.exe"), "a.rs"));
        harness.desk.idle();

        harness.desk.close_placed(key.handle);
        harness.desk.idle();

        assert!(harness.windows.frame_of(key).is_none(), "先归位再关闭");
        assert_eq!(harness.windows.zoom_delta_total(), 0, "字号也缩回");
        assert!(harness.desk.state().list_visible);
        assert_eq!(harness.gones(), vec![DeskGone::Window { key }]);
        assert!(!harness.paths.phone_record_file().exists());
    }

    #[test]
    fn open_waits_for_the_window_then_places_it() {
        let harness = Harness::new();
        let pid = 777;
        let key = WindowKey::new(pid, 0x500);
        harness
            .windows
            .insert_window(window(key, Some("notepad.exe"), "notes.txt"), Rect::new(10.0, 10.0, 800.0, 600.0));
        harness.apps.set_open_result(Some(pid));
        ready_screen(&harness);

        harness.desk.open(app("记事本", "notepad.exe"));
        harness.desk.idle();

        let state = harness.desk.state();
        assert_eq!(state.placed.as_ref().map(|placed| placed.window.key), Some(key));
        assert!(!state.list_visible);
        assert!(state.work.is_none());
    }

    #[test]
    fn new_window_places_the_newly_created_window() {
        let harness = Harness::new();
        let pid = 888;
        let existing = WindowKey::new(pid, 0x600);
        harness.windows.insert_window(
            window(existing, Some("Code.exe"), "old"),
            Rect::new(1.0, 2.0, 600.0, 500.0),
        );
        let fresh_key = WindowKey::new(pid, 0x601);
        let fresh = window(fresh_key, Some("Code.exe"), "new");
        let windows = Arc::clone(&harness.windows);
        harness.apps.set_new_window_result(true);
        harness.apps.set_on_new_window(move || {
            windows.insert_window(fresh.clone(), Rect::new(3.0, 4.0, 700.0, 500.0));
        });
        ready_screen(&harness);

        harness.desk.new_window(running(pid, "VS Code", "Code.exe", vec![]));
        harness.desk.idle();

        assert_eq!(
            harness.desk.state().placed.as_ref().map(|placed| placed.window.key),
            Some(fresh_key),
            "新窗口应该被放上手机屏"
        );
    }

    #[test]
    fn verify_placement_returns_to_the_list_when_the_window_vanished() {
        let harness = Harness::new();
        let key = WindowKey::new(70, 0x700);
        harness.windows.insert(key, Rect::new(100.0, 100.0, 800.0, 600.0));
        ready_screen(&harness);
        harness.desk.pull(window(key, Some("Code.exe"), "a.rs"));
        harness.desk.idle();

        harness.windows.remove(key);
        harness.desk.verify_placement();
        harness.desk.idle();

        let state = harness.desk.state();
        assert!(state.placed.is_none());
        assert!(state.list_visible, "回到 App 资源库");
        assert_eq!(harness.desk.left_behind_len(), 1, "记下回来的样子");
        assert!(!harness.paths.phone_record_file().exists(), "当前记录已结算");
    }

    #[test]
    fn check_returns_persists_expired_record_cleanup() {
        let harness = Harness::new();
        let entry = left_behind_entry(now_ms() - crate::left_behind::KEEP_MILLIS - 60_000);
        seed_left_behind(&harness, vec![entry]);

        harness.desk.check_returns();
        harness.desk.idle();

        assert_eq!(harness.desk.left_behind_len(), 0);
        let books = &harness.desk.inner.shared.books;
        assert!(books.left_behind_file.load_or_quarantine().expect("saved book").entries.is_empty());
    }

    #[test]
    fn check_returns_preserves_unexpired_records_without_rewriting_them() {
        let harness = Harness::new();
        let entry = left_behind_entry(now_ms() - crate::left_behind::KEEP_MILLIS + 60_000);
        seed_left_behind(&harness, vec![entry.clone()]);
        let path = harness.paths.left_behind_file();
        let original = std::fs::read(&path).expect("original book");

        harness.desk.check_returns();
        harness.desk.idle();

        let books = &harness.desk.inner.shared.books;
        assert_eq!(books.left_behind.lock().expect("book").entries, vec![entry]);
        assert_eq!(std::fs::read(path).expect("unchanged book"), original);
    }

    #[test]
    fn check_returns_persists_cleanup_without_losing_live_records() {
        let harness = Harness::new();
        let expired = left_behind_entry(now_ms() - crate::left_behind::KEEP_MILLIS - 60_000);
        let live = left_behind_entry(now_ms());
        seed_left_behind(&harness, vec![expired, live.clone()]);

        harness.desk.check_returns();
        harness.desk.idle();

        let books = &harness.desk.inner.shared.books;
        assert_eq!(books.left_behind.lock().expect("book").entries, vec![live.clone()]);
        assert_eq!(books.left_behind_file.load_or_quarantine().expect("saved book").entries, vec![live]);
    }

    fn left_behind_entry(since: i64) -> LeftBehindEntry {
        LeftBehindEntry::new(
            PhoneRecord {
                app_id: Some("Code.exe".into()),
                filled: Some(Rect::new(0.0, 0.0, 540.0, 950.0)),
                ..PhoneRecord::default()
            },
            since,
        )
    }

    fn seed_left_behind(harness: &Harness, entries: Vec<LeftBehindEntry>) {
        let books = &harness.desk.inner.shared.books;
        let book = LeftBehindBook { entries };
        // 用紧凑 JSON 检查没有过期项时不会被无谓重写成 pretty JSON。
        std::fs::create_dir_all(&harness.dir).expect("data directory");
        std::fs::write(harness.paths.left_behind_file(), serde_json::to_string(&book).expect("json"))
            .expect("seed book");
        *books.left_behind.lock().expect("book") = book;
    }

    #[test]
    fn check_returns_consumes_a_left_behind_record_when_the_window_comes_back() {
        let harness = Harness::new();
        let key = WindowKey::new(71, 0x701);
        harness.windows.insert(key, Rect::new(100.0, 100.0, 800.0, 600.0));
        ready_screen(&harness);
        harness.desk.pull(window(key, Some("Code.exe"), "a.rs"));
        harness.desk.idle();
        let filled = harness.windows.frame_of(key).expect("filled");

        harness.windows.remove(key);
        harness.desk.verify_placement();
        harness.desk.idle();
        assert_eq!(harness.desk.left_behind_len(), 1);

        // App 带着手机屏上的尺寸回来：桌面会把它重新放上手机屏。
        harness
            .windows
            .insert_window(window(key, Some("Code.exe"), "a.rs"), filled);
        harness.apps.set_pids("Code.exe", vec![key.pid]);
        harness.desk.check_returns();
        harness.desk.idle();

        assert_eq!(harness.desk.left_behind_len(), 0, "记录应被消费");
        assert_eq!(
            harness.desk.state().placed.as_ref().map(|placed| placed.window.key),
            Some(key),
            "带着手机屏尺寸回来的窗口直接放回去"
        );
    }

    #[test]
    fn force_quit_clears_the_watch_and_reports_failure() {
        let harness = Harness::new();
        ready_screen(&harness);
        harness.apps.set_quit_keeps_running(true);
        harness.desk.quit(running(72, "VS Code", "Code.exe", vec![]));
        harness.desk.idle();
        assert!(matches!(harness.desk.state().notice, Some(DeskNotice::QuitStalled { .. })));

        harness.apps.set_force_quit_result(false);
        harness.desk.force_quit(72, "VS Code");
        harness.desk.idle();

        assert_eq!(
            harness.desk.state().notice,
            Some(DeskNotice::WindowUnresponsive { app: "VS Code".into() })
        );
    }
}
