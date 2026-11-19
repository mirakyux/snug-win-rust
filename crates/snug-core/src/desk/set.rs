//! 多屏管理，移植自 C# v1.3.0 `SnugWin.Core.PhoneScreen.DeskSet`。
//!
//! 一块屏一个 [`Desk`]，`DeskSet` 负责：
//! - 按「屏愿望」增量建 / 改 / 拆 desk（[`DeskSet::set_wishes`]）；
//! - 把远程连接、默认字号、显示器变化等事件扇出给每块屏；
//! - 提供跨屏操作：把同一窗口从 A 屏转移到 B 屏、退出 App 前先归还别的屏上的窗口、
//!   关闭「手机屏上点关闭按钮」的那个窗口。
//!
//! **所有 desk 共用一条串行队列**（C# 的 `SerialLane _lane`）：一个工作线程按入队顺序
//! 执行每块屏的动作，所以跨屏操作可以在同一个动作里同步访问别的屏的状态，不会有竞态。
//!
//! 状态变化只在真的变化时向上转发一次（`Desk::set_state` 自己会去重）。

use std::collections::HashMap;
use std::sync::Arc;

use crate::paths::Paths;
use crate::screen_plan::{ScreenBehavior, ScreenWish};
use crate::windows::{AppInfo, RunningApp, WindowInfo};

use super::engine::{Desk, DeskEvents, DeskHub, HubOwner, SharedDeskBooks};
use super::lane::SerialLane;
use super::model::{DeskGone, DeskState, PhoneScreenPhase};
use super::services::{AppControl, PhoneDisplays, SystemTimeline, Timeline, WindowControl};

/// `DeskSet` 往外界发的事件（C# `DeskSet` 上的三个 `event`）。
///
/// 与单屏 [`DeskEvents`] 的差别只在 `state_changed`：C# 的 set 级事件是没有参数的
/// `Action`，只表示"某块屏变了，自己去读状态"。
#[derive(Clone, Default)]
pub struct DeskSetEvents {
    pub state_changed: Option<Arc<dyn Fn() + Send + Sync>>,
    pub gone: Option<Arc<dyn Fn(&DeskGone) + Send + Sync>>,
    pub topology_changing: Option<Arc<dyn Fn() + Send + Sync>>,
    pub log: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

/// `DeskSet` 的构造选项，对应 C# 构造函数的可选参数（`timeline`、`defaultZoom`）与事件。
pub struct DeskSetOptions {
    pub timeline: Option<Arc<dyn Timeline>>,
    pub default_zoom: i32,
    pub events: DeskSetEvents,
}

impl Default for DeskSetOptions {
    fn default() -> Self {
        Self { timeline: None, default_zoom: 2, events: DeskSetEvents::default() }
    }
}

/// 多屏管理器（C# `DeskSet`）。
pub struct DeskSet {
    displays: Arc<dyn PhoneDisplays>,
    windows: Arc<dyn WindowControl>,
    apps: Arc<dyn AppControl>,
    paths: Paths,
    timeline: Arc<dyn Timeline>,
    default_zoom: i32,
    /// 所有屏共用的串行队列。
    lane: SerialLane<DeskHub>,
    /// 共享队列的拥有者，`DeskSet` 活着它就活着；最后一个 owner 消失时工作线程退出。
    _owner: Arc<HubOwner>,
    books: Arc<SharedDeskBooks>,
    /// key 是屏 id 的小写形式（C# 用 `OrdinalIgnoreCase`）。
    desks: HashMap<String, Desk>,
    wishes: Vec<ScreenWish>,
    remote: bool,
    events: DeskSetEvents,
}

impl DeskSet {
    pub fn new(
        displays: Arc<dyn PhoneDisplays>,
        windows: Arc<dyn WindowControl>,
        apps: Arc<dyn AppControl>,
        paths: &Paths,
        options: DeskSetOptions,
    ) -> DeskSet {
        let lane = SerialLane::<DeskHub>::start(|_| DeskHub::default());
        let owner = Arc::new(HubOwner { lane: lane.clone() });
        let timeline = options
            .timeline
            .unwrap_or_else(|| Arc::new(SystemTimeline::new()) as Arc<dyn Timeline>);
        let books = Arc::new(SharedDeskBooks::load(paths));
        DeskSet {
            displays,
            windows,
            apps,
            paths: paths.clone(),
            timeline,
            default_zoom: options.default_zoom,
            lane,
            _owner: owner,
            books,
            desks: HashMap::new(),
            wishes: Vec::new(),
            remote: false,
            events: options.events,
        }
    }

    /// 当前的屏愿望（C# `Wishes`）。
    pub fn wishes(&self) -> &[ScreenWish] {
        &self.wishes
    }

    /// 当前有几块屏（测试与状态摘要用）。
    pub fn len(&self) -> usize {
        self.desks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.desks.is_empty()
    }

    /// 对应 C# `SetWishes`：按愿望增量建 / 改 / 拆 desk。
    ///
    /// 注意 C# 对新建或复用的 desk 都调 `SetWanted(true)`——这只是"这块屏被需要"，
    /// 真要不要建屏由 desk 内部的 `wanted && !dismissed` 决定。
    pub fn set_wishes(&mut self, wishes: Vec<ScreenWish>) {
        self.wishes = wishes.clone();
        let next_keys: Vec<String> = wishes.iter().map(|wish| DeskHub::key(&wish.id)).collect();

        // 1) 消失的屏：先入队撤屏，再把 core 从共享队列里摘掉。
        let stale: Vec<String> = self
            .desks
            .keys()
            .filter(|key| !next_keys.contains(key))
            .cloned()
            .collect();
        for key in stale {
            if let Some(desk) = self.desks.remove(&key) {
                desk.shutdown_non_blocking();
                let lane = self.lane.clone();
                let removed = key.clone();
                lane.run(move |hub: &mut DeskHub| {
                    hub.remove(&removed);
                });
            }
        }

        // 2) 新增的建 desk，已存在的换愿望。
        for wish in wishes {
            let key = DeskHub::key(&wish.id);
            if let Some(existing) = self.desks.get(&key) {
                existing.update_wish(wish.name, wish.fit, wish.behavior);
                existing.set_wanted(true);
                continue;
            }
            let desk = Desk::spawn(
                &self.lane,
                Arc::clone(&self.books),
                Arc::clone(&self.displays),
                Arc::clone(&self.windows),
                Arc::clone(&self.apps),
                &self.paths,
                Arc::clone(&self.timeline),
                self.default_zoom,
                wish.fit,
                wish.id,
                wish.name,
                wish.behavior,
                self.forwarded_events(),
                None,
            );
            desk.set_remote(self.remote);
            desk.set_wanted(true);
            self.desks.insert(key, desk);
        }

        self.notify_state_changed();
    }

    /// 把单个 desk 的事件转发成 set 级事件（C# 在 `SetWishes` 里逐 desk 挂回调）。
    fn forwarded_events(&self) -> DeskEvents {
        let state_callback = self.events.state_changed.clone();
        let state_changed = state_callback.map(|callback| {
            Arc::new(move |_: &DeskState| callback()) as Arc<dyn Fn(&DeskState) + Send + Sync>
        });
        DeskEvents {
            state_changed,
            gone: self.events.gone.clone(),
            topology_changing: self.events.topology_changing.clone(),
            log: self.events.log.clone(),
        }
    }

    fn notify_state_changed(&self) {
        if let Some(callback) = self.events.state_changed.as_ref() {
            callback();
        }
    }

    /// 某块屏的状态（C# `StateOf`）。
    pub fn state_of(&self, screen_id: &str) -> Option<DeskState> {
        self.desks.get(&DeskHub::key(screen_id)).map(Desk::state)
    }

    /// 所有屏的状态（C# `AllStates`），key 是原始屏 id。
    pub fn all_states(&self) -> HashMap<String, DeskState> {
        self.desks.values().map(|desk| (desk.screen_id(), desk.state())).collect()
    }

    pub fn is_any_ready(&self) -> bool {
        self.desks.values().any(|desk| desk.state().screen_phase == PhoneScreenPhase::Ready)
    }

    pub fn is_any_touch_ready(&self) -> bool {
        self.desks.values().any(|desk| {
            desk.behavior() == ScreenBehavior::TouchScreen
                && desk.state().screen_phase == PhoneScreenPhase::Ready
        })
    }

    pub fn has_any_dismissed_touch(&self) -> bool {
        self.desks
            .values()
            .any(|desk| desk.behavior() == ScreenBehavior::TouchScreen && desk.state().dismissed)
    }

    /// 某块显示器上的 desk（C# `DeskOfDisplay`）。
    pub fn desk_of_display(&self, display_id: u32) -> Option<&Desk> {
        self.desks.values().find(|desk| desk.state().display_id == Some(display_id))
    }

    /// 对应 C# `DeskForScreen`：给了 id 就找它，找不到 / 没给就退到第一块触屏，再退到第一块。
    pub fn desk_for_screen(&self, screen_id: Option<&str>) -> Option<&Desk> {
        if let Some(id) = screen_id
            && !id.trim().is_empty()
            && let Some(exact) = self.desks.get(&DeskHub::key(id))
        {
            return Some(exact);
        }
        self.desks
            .values()
            .find(|desk| desk.behavior() == ScreenBehavior::TouchScreen)
            .or_else(|| self.desks.values().next())
    }

    /// 对应 C# `SetRemote`：UU 远程是否连着。
    pub fn set_remote(&mut self, on: bool) {
        self.remote = on;
        for desk in self.desks.values() {
            desk.set_remote(on);
        }
    }

    /// 对应 C# `SetDefaultZoom`。
    pub fn set_default_zoom(&mut self, steps: i32) {
        self.default_zoom = steps;
        for desk in self.desks.values() {
            desk.set_default_zoom(steps);
        }
    }

    pub fn recover(&self) {
        for desk in self.desks.values() {
            desk.recover();
        }
    }

    pub fn verify_placement(&self) {
        for desk in self.desks.values() {
            desk.verify_placement();
        }
    }

    pub fn check_returns(&self) {
        for desk in self.desks.values() {
            desk.check_returns();
        }
    }

    pub fn settle_displays(&self) {
        for desk in self.desks.values() {
            desk.settle_displays();
        }
    }

    pub fn displays_changed(&self) {
        for desk in self.desks.values() {
            desk.displays_changed();
        }
    }

    pub fn show_list(&self, screen_id: Option<&str>) {
        if let Some(desk) = self.desk_for_screen(screen_id) {
            desk.show_list();
        }
    }

    pub fn return_to_placed(&self, screen_id: Option<&str>) {
        if let Some(desk) = self.desk_for_screen(screen_id) {
            desk.return_to_placed();
        }
    }

    pub fn put_back(&self, screen_id: Option<&str>) {
        if let Some(desk) = self.desk_for_screen(screen_id) {
            desk.put_back();
        }
    }

    pub fn zoom(&self, screen_id: &str, delta: i32) {
        if let Some(desk) = self.desk_for_screen(Some(screen_id)) {
            desk.zoom(delta);
        }
    }

    pub fn open(&self, screen_id: &str, app: AppInfo) {
        if let Some(desk) = self.desk_for_screen(Some(screen_id)) {
            desk.open(app);
        }
    }

    pub fn new_window(&self, screen_id: &str, app: RunningApp) {
        if let Some(desk) = self.desk_for_screen(Some(screen_id)) {
            desk.new_window(app);
        }
    }

    pub fn close(&self, screen_id: &str, window: WindowInfo) {
        if let Some(desk) = self.desk_for_screen(Some(screen_id)) {
            desk.close(window);
        }
    }

    pub fn dismiss_screen(&self, screen_id: &str) {
        if let Some(desk) = self.desk_for_screen(Some(screen_id)) {
            desk.dismiss_screen();
        }
    }

    pub fn dismiss_notice(&self, screen_id: &str) {
        if let Some(desk) = self.desk_for_screen(Some(screen_id)) {
            desk.dismiss_notice();
        }
    }

    /// 对应 C# `ReenterAllDismissedTouch`：所有"这次远程里退出过"的触屏都恢复建屏。
    pub fn reenter_all_dismissed_touch(&self) {
        for desk in self.desks.values() {
            if desk.behavior() == ScreenBehavior::TouchScreen && desk.state().dismissed {
                desk.reenter_screen();
            }
        }
    }

    /// 对应 C# `ReenterScreen`：给了 id 就只重进那一块（找不到则按 `DeskForScreen` 的退路）。
    pub fn reenter_screen(&self, screen_id: Option<&str>) {
        if screen_id.is_some() {
            if let Some(desk) = self.desk_for_screen(screen_id) {
                desk.reenter_screen();
            }
            return;
        }
        self.reenter_all_dismissed_touch();
    }

    /// 把一行日志送进 set 级的 log 通道（`DeskSetEvents::log`）。
    fn log(&self, message: impl Into<String>) {
        if let Some(sink) = &self.events.log {
            sink(&message.into());
        }
    }

    /// 对应 C# `Pull`：把窗口放到目标屏；若它正铺在别的屏上，先摘出那条还原记录，
    /// 新屏沿用**原始的还原坐标**（而不是把它放回原位再重新记一次）。
    pub fn pull(&self, target_screen_id: &str, window: WindowInfo) {
        // 静默返回是调试黑洞：真机上出现过"点了 App 什么都不发生、日志只有一行铺开"，
        // 就是因为这里找不到屏或行为不对。两个分支都留一条日志，代价是一行。
        let Some(target) = self.desk_for_screen(Some(target_screen_id)) else {
            self.log(format!(
                "放 {}：找不到屏 {target_screen_id}（现有 {:?}）",
                window.label(),
                self.wishes().into_iter().map(|wish| wish.id.clone()).collect::<Vec<_>>()
            ));
            return;
        };
        // 这两条与 C# `PhoneDesk.Pull` 入队前的判断一致。
        if target.behavior() != ScreenBehavior::TouchScreen {
            self.log(format!(
                "放 {}：屏 {target_screen_id} 不是触屏（{:?}）",
                window.label(),
                target.behavior()
            ));
            return;
        }
        if target.state().quitting.contains(&window.key.pid) {
            let (label, app) = (window.label(), window.app_name.clone());
            target.log(format!("{app} 正在退出，不放 {label}"));
            return;
        }

        let target_key = DeskHub::key(&target.screen_id());
        let window_key = window.key;
        let ready = target.state().screen_phase == PhoneScreenPhase::Ready;
        let lane = self.lane.clone();
        let log = self.events.log.clone();
        let label = window.label();
        let key_label = target_key.clone();
        self.log(format!(
            "放 {label}：入队（屏 {key_label} ready={ready} 队列中还有多少动作看不到）"
        ));
        lane.run(move |hub: &mut DeskHub| {
            if let Some(log) = &log {
                log(&format!("放 {label}：桌面队列开始执行"));
            }
            // 屏没就绪时不要先摘别处的记录：C# 在 `PlaceAsync` 里也是先过 `ReadyDisplay`。
            let transfer = if ready { hub.detach_for_transfer(&target_key, window_key) } else { None };
            match hub.core_mut(&target_key) {
                Some(core) => core.pull_with(window, transfer),
                None => {
                    if let Some(log) = &log {
                        log(&format!("放 {label}：桌面队列里没有屏 {key_label}"));
                    }
                }
            }
        });
    }

    /// 对应 C# `Quit`：先把这个 App 在**别的**屏上铺着的窗口还回去，再让目标屏退出它。
    pub fn quit(&self, screen_id: &str, app: RunningApp) {
        let target_key = match self.desk_for_screen(Some(screen_id)) {
            Some(desk) => DeskHub::key(&desk.screen_id()),
            None => return,
        };
        let pid = app.pid;
        let lane = self.lane.clone();
        lane.run(move |hub: &mut DeskHub| {
            for id in hub.ids() {
                if id == target_key {
                    continue;
                }
                if let Some(core) = hub.core_mut(&id)
                    && core.placed_pid() == Some(pid)
                {
                    core.return_window_if_placed(pid);
                }
            }
            if let Some(core) = hub.core_mut(&target_key) {
                core.quit_app(app);
            }
        });
    }

    /// 对应 C# `ForceQuit`。
    pub fn force_quit(&self, screen_id: &str, pid: i32, app_name: &str) {
        if let Some(desk) = self.desk_for_screen(Some(screen_id)) {
            desk.force_quit(pid, app_name);
        }
    }

    /// 对应 C# `ClosePlaced`：找出哪块屏正铺着这个句柄，交给它关。
    pub fn close_placed(&self, window_handle: u64) {
        for desk in self.desks.values() {
            if desk.placed_window_key().map(|key| key.handle) == Some(window_handle) {
                desk.close_placed(window_handle);
                return;
            }
        }
    }

    /// 对应 C# `ShutdownAsync`：拆掉所有屏再清空。
    pub fn shutdown(&mut self) {
        let desks: Vec<Desk> = self.desks.drain().map(|(_, desk)| desk).collect();
        self.wishes.clear();
        let mut timed_out = Vec::new();
        for desk in &desks {
            if !desk.shutdown() {
                timed_out.push(desk.screen_id());
            }
        }
        if !timed_out.is_empty() {
            self.log(format!(
                "收尾：{timed_out:?} 的撤屏动作 20 秒内没做完，先退出；残留屏由归属记录在下次启动时清理"
            ));
        }
        self.lane.run(|hub: &mut DeskHub| hub.clear());
        self.notify_state_changed();
    }

    /// 测试用：等所有屏的动作、重试与 watch 都静止（对应 C# 的 `IdleAsync`）。
    ///
    /// **只在测试里存在**。生产路径调它会把界面和状态更新一起卡住：它等的
    /// "静止"包含 watch（最长 30s/10s/60s），而这期间 `set_wishes` 进不了队列。
    /// 详见 `docs/desk-threading.md`。
    #[cfg(test)]
    pub fn idle(&self) {
        loop {
            for desk in self.desks.values() {
                desk.idle();
            }
            self.lane.drain();
            if self.desks.values().all(Desk::is_quiet) {
                return;
            }
        }
    }

    /// 测试用：当前每条屏的还原记录文件是否存在（验证"每屏一份"）。
    #[cfg(test)]
    pub(crate) fn record_file_exists(&self, screen_id: &str) -> bool {
        self.paths.record_file_for_screen(Some(screen_id)).exists()
    }

    /// 测试用：直接看某块屏的 fit。
    #[cfg(test)]
    pub(crate) fn fit_of(&self, screen_id: &str) -> Option<crate::screen_fit::ScreenFit> {
        self.desks.get(&DeskHub::key(screen_id)).map(Desk::fit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::ScreenConfig;
    use crate::desk::test_support::DeskSetHarness;
    use crate::geometry::Rect;
    use crate::record::PhoneRecord;
    use crate::screen_fit::ScreenFit;
    use crate::store::RestoreRecordStore;
    use crate::windows::{AppInfo, WindowKey};

    fn window(key: WindowKey, app_id: Option<&str>, title: &str) -> WindowInfo {
        WindowInfo {
            key,
            app_name: "VS Code".into(),
            app_id: app_id.map(str::to_string),
            title: title.into(),
            minimized: false,
        }
    }

    fn running(pid: i32, name: &str, app_id: &str) -> RunningApp {
        RunningApp {
            app: AppInfo {
                app_id: app_id.into(),
                name: name.into(),
                executable_path: None,
                launch_target: None,
                arguments: None,
                working_directory: None,
            },
            pid,
            windows: Vec::new(),
        }
    }

    /// 建一套 A/B 两块触屏并等它们就绪。
    fn ready_two_screens() -> DeskSetHarness {
        let mut harness = DeskSetHarness::new();
        harness.set.set_remote(true);
        let wishes = vec![harness.wish("a"), harness.wish("b")];
        harness.set.set_wishes(wishes);
        harness.set.idle();
        assert!(harness.set.is_any_ready(), "两块屏都该就绪");
        harness
    }

    #[test]
    fn wishes_create_one_desk_per_screen_and_removals_retire_their_displays() {
        let mut harness = DeskSetHarness::new();
        harness.set.set_remote(true);
        let wishes = vec![harness.wish("a"), harness.wish("b"), harness.wish("c")];
        harness.set.set_wishes(wishes);
        harness.set.idle();

        assert_eq!(harness.set.len(), 3);
        assert_eq!(harness.displays.screen_count(), 3);
        for id in ["a", "b", "c"] {
            assert_eq!(
                harness.set.state_of(id).unwrap().screen_phase,
                PhoneScreenPhase::Ready,
                "{id} 应该就绪"
            );
        }

        let c_display = harness.set.state_of("c").unwrap().display_id.expect("c 的屏");
        harness.set.set_wishes(vec![harness.wish("a")]);
        harness.set.idle();

        assert_eq!(harness.set.len(), 1, "只剩 a");
        assert!(harness.set.state_of("b").is_none());
        assert!(harness.set.state_of("c").is_none());
        assert!(!harness.displays.is_online(c_display), "撤掉的 desk 要把屏也撤了");
        assert_eq!(harness.displays.screen_count(), 1);
    }

    #[test]
    fn state_is_isolated_between_screens() {
        let harness = ready_two_screens();
        let b_before = harness.set.state_of("b").unwrap();

        let key = WindowKey::new(50, 0x300);
        harness.windows.insert(key, Rect::new(100.0, 100.0, 900.0, 700.0));
        harness.set.pull("a", window(key, Some("Code.exe"), "a.rs"));
        harness.set.idle();

        assert!(harness.set.state_of("a").unwrap().placed.is_some());
        assert!(harness.set.state_of("b").unwrap().placed.is_none());
        assert_eq!(harness.set.state_of("b").unwrap(), b_before, "往 A 铺窗口不该动 B");

        harness.set.show_list(Some("a"));
        harness.set.idle();
        assert_eq!(harness.set.state_of("b").unwrap(), b_before, "show_list(A) 也不该动 B");
    }

    #[test]
    fn transfer_pull_reuses_the_original_restore_coordinates() {
        let harness = ready_two_screens();
        let original = Rect::new(10.0, 20.0, 900.0, 600.0);
        let key = WindowKey::new(51, 0x301);
        harness.windows.insert(key, original);

        harness.set.pull("a", window(key, Some("Code.exe"), "同一个窗口"));
        harness.set.idle();
        assert!(harness.set.state_of("a").unwrap().placed.is_some());

        // 同一窗口改投 B 屏。
        harness.set.pull("b", window(key, Some("Code.exe"), "同一个窗口"));
        harness.set.idle();

        let a_state = harness.set.state_of("a").unwrap();
        assert!(a_state.placed.is_none(), "A 的摆放要被摘走");
        assert!(a_state.list_visible, "A 要回到 App 资源库");
        assert!(harness.set.state_of("b").unwrap().placed.is_some());

        let store = RestoreRecordStore::<PhoneRecord>::new(
            harness.paths.record_file_for_screen(Some("b")),
        );
        let record = store.pending().expect("B 屏应有还原记录");
        assert_eq!(record.original, original, "跨屏转移要沿用来源屏的原始还原坐标");
    }

    #[test]
    fn each_screen_writes_its_own_restore_record() {
        let mut harness = DeskSetHarness::new();
        harness.set.set_remote(true);
        let legacy = ScreenConfig::LEGACY_PHONE_ID;
        let wishes = vec![harness.wish(legacy), harness.wish("b")];
        harness.set.set_wishes(wishes);
        harness.set.idle();

        let first = WindowKey::new(52, 0x302);
        let second = WindowKey::new(53, 0x303);
        harness.windows.insert(first, Rect::new(10.0, 10.0, 800.0, 600.0));
        harness.windows.insert(second, Rect::new(30.0, 30.0, 800.0, 600.0));
        harness.set.pull(legacy, window(first, Some("Code.exe"), "一"));
        harness.set.idle();
        harness.set.pull("b", window(second, Some("Code.exe"), "二"));
        harness.set.idle();

        assert!(harness.set.record_file_exists(legacy));
        assert!(harness.set.record_file_exists("b"));
        assert_eq!(
            harness.paths.record_file_for_screen(Some(legacy)),
            harness.paths.phone_record_file(),
            "legacy 屏回落到 phone-screen.json"
        );
        assert_ne!(
            harness.paths.record_file_for_screen(Some("b")),
            harness.paths.phone_record_file(),
            "其它屏各用 screen-<id>.json"
        );
    }

    #[test]
    fn quit_returns_windows_placed_on_other_screens_first() {
        let harness = ready_two_screens();
        let original = Rect::new(20.0, 30.0, 900.0, 700.0);
        let key = WindowKey::new(77, 0x304);
        harness.windows.insert(key, original);
        harness.set.pull("a", window(key, Some("Code.exe"), "被别处退出"));
        harness.set.idle();
        assert!(harness.set.state_of("a").unwrap().placed.is_some());

        harness.apps.set_running(77, false);
        harness.set.quit("b", running(77, "VS Code", "Code.exe"));
        harness.set.idle();

        assert_eq!(harness.windows.frame_of(key), Some(original), "先还回原位");
        assert!(harness.set.state_of("a").unwrap().placed.is_none());
    }

    #[test]
    fn computer_screen_evacuates_windows_to_the_main_screen_only_once() {
        let mut harness = DeskSetHarness::new();
        harness.set.set_remote(true);
        harness.set.set_wishes(vec![harness.computer_wish("pc")]);
        harness.set.idle();

        let display = harness.set.state_of("pc").unwrap().display_id.expect("电脑屏");
        let bounds = harness.displays.bounds_of(display).expect("屏矩形");
        let key = WindowKey::new(60, 0x400);
        harness
            .windows
            .insert(key, Rect::new(bounds.x + 20.0, bounds.y + 20.0, 300.0, 200.0));

        let desk = harness.set.desk_of_display(display).expect("desk");
        desk.evacuate_computer_screen_windows();
        harness.set.idle();

        // 主屏 (0,0,1920,1040)，窗口 300×200 居中 → (810, 420)。
        let centered = Rect::new(810.0, 420.0, 300.0, 200.0);
        assert_eq!(harness.windows.frame_of(key), Some(centered));

        // 再搬一次：已经在主屏里，不该再动。
        desk.evacuate_computer_screen_windows();
        harness.set.idle();
        assert_eq!(harness.windows.frame_of(key), Some(centered), "只搬一次");
    }

    #[test]
    fn computer_evacuation_skips_when_the_screen_is_not_ready() {
        let mut harness = DeskSetHarness::new();
        harness.displays.fail_creates(10); // 建屏一直失败，最后放弃重试
        harness.set.set_remote(true);
        harness.set.set_wishes(vec![harness.computer_wish("pc")]);
        harness.set.idle();
        assert_eq!(
            harness.set.state_of("pc").unwrap().screen_phase,
            PhoneScreenPhase::Failed
        );

        let key = WindowKey::new(61, 0x401);
        let original = Rect::new(4000.0, 10.0, 300.0, 200.0);
        harness.windows.insert(key, original);
        harness
            .set
            .desk_for_screen(Some("pc"))
            .unwrap()
            .evacuate_computer_screen_windows();
        harness.set.idle();

        assert_eq!(harness.windows.frame_of(key), Some(original), "屏没就绪不该搬");
    }

    #[test]
    fn screen_creation_is_serialized_across_desks() {
        let mut harness = DeskSetHarness::new();
        harness.displays.slow_creates(3_000); // 每次建屏停 3ms，重叠会被记下来
        harness.set.set_remote(true);
        let wishes = vec![harness.wish("a"), harness.wish("b")];
        harness.set.set_wishes(wishes);
        harness.set.idle();

        assert_eq!(harness.displays.create_calls().len(), 2, "两块屏都要建");
        assert!(
            !harness.displays.overlap_detected(),
            "共享串行队列下两次建屏不能重叠"
        );
        assert_eq!(harness.set.len(), 2);
    }

    #[test]
    fn resetting_wishes_updates_name_fit_and_behavior() {
        let mut harness = DeskSetHarness::new();
        harness.set.set_remote(true);
        harness.set.set_wishes(vec![harness.wish("a")]);
        harness.set.idle();
        let display = harness.set.state_of("a").unwrap().display_id.expect("屏");

        let narrow = ScreenFit::new(1.2);
        let mut updated = harness.wish_with_fit("a", narrow.clone());
        updated.name = "改过的名字".into();
        harness.set.set_wishes(vec![updated]);
        harness.set.idle();

        let desk = harness.set.desk_for_screen(Some("a")).unwrap();
        assert_eq!(desk.screen_name(), "改过的名字");
        assert_eq!(harness.set.fit_of("a"), Some(narrow.clone()));
        assert_eq!(harness.displays.fit_of(display), Some(narrow), "屏也要换比例");
    }

    #[test]
    fn zoom_memory_is_shared_between_screens() {
        let harness = ready_two_screens();

        let first = WindowKey::new(70, 0x500);
        harness.windows.insert(first, Rect::new(10.0, 10.0, 900.0, 700.0));
        harness.set.pull("a", window(first, Some("Code.exe"), "一"));
        harness.set.idle();
        // 默认 2 级。
        assert_eq!(harness.windows.zoom_delta_total(), 2);

        harness.set.zoom("a", 1);
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 3);

        // 同一个 App 放到 B：应带上 A 记住的 3 级，而不是回到默认 2 级。
        let second = WindowKey::new(71, 0x501);
        harness.windows.insert(second, Rect::new(10.0, 10.0, 900.0, 700.0));
        harness.set.pull("b", window(second, Some("Code.exe"), "二"));
        harness.set.idle();
        assert_eq!(
            harness.windows.zoom_delta_total(),
            6,
            "字号记忆是共享的：B 直接按 3 级放"
        );
    }

    #[test]
    fn shutdown_retires_every_screen_and_clears_the_set() {
        let mut harness = ready_two_screens();
        let displays: Vec<u32> = harness
            .set
            .all_states()
            .values()
            .map(|state| state.display_id.expect("屏"))
            .collect();
        assert_eq!(displays.len(), 2);

        harness.set.shutdown();
        harness.set.idle();

        assert!(harness.set.is_empty());
        assert!(harness.set.wishes().is_empty());
        assert_eq!(harness.displays.screen_count(), 0, "所有屏都要撤掉");
    }
}
