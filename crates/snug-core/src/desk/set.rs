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
use std::sync::{Arc, Mutex};

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
    restore_files_discovered: Mutex<bool>,
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
            restore_files_discovered: Mutex::new(false),
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
        self.discover_restore_files();

        // 1) 消失的屏：先入队撤屏，再从活跃集合摘掉；未下线的 core 留给定时善后。
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

    /// Recover records left by screens removed from the saved configuration.
    /// These cores keep wanted=false, so recovery never creates their displays.
    fn discover_restore_files(&self) {
        let mut discovered = self.restore_files_discovered.lock().expect("restore discovery");
        if *discovered {
            return;
        }
        let ids = (|| -> std::io::Result<Vec<String>> {
            let entries = match std::fs::read_dir(self.paths.data_dir()) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(error),
            };
            let mut ids = Vec::new();
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue; };
                let lower = name.to_ascii_lowercase();
                let id = if lower == "phone-screen.json" {
                    crate::config::ScreenConfig::LEGACY_PHONE_ID.to_string()
                } else if let Some(id) = lower.strip_prefix("screen-").and_then(|name| name.strip_suffix(".json")) {
                    if id.is_empty() || id.eq_ignore_ascii_case(crate::config::ScreenConfig::LEGACY_PHONE_ID) {
                        continue;
                    }
                    name["screen-".len()..name.len() - ".json".len()].to_string()
                } else {
                    continue;
                };
                // Matching paths must also guard unreadable directories or links.
                // The existing ledger reader decides whether recovery is possible.
                ids.push(id);
            }
            Ok(ids)
        })();
        let ids = match ids {
            Ok(ids) => ids,
            Err(error) => {
                self.log(format!("还原记录目录读取失败，暂不处理共享归还，稍后重试：{error}"));
                self.lane.run(|hub: &mut DeskHub| hub.set_restore_scan_failed(true));
                return;
            }
        };
        for id in ids {
            let key = DeskHub::key(&id);
            if self.desks.contains_key(&key) || self.wishes.iter().any(|wish| DeskHub::key(&wish.id) == key) {
                continue;
            }
            let _desk = Desk::spawn(
                &self.lane, self.books.clone(), self.displays.clone(), self.windows.clone(), self.apps.clone(),
                &self.paths, self.timeline.clone(), self.default_zoom, crate::screen_fit::ScreenFit::standard(),
                id, "残留窗口恢复", ScreenBehavior::TouchScreen, self.forwarded_events(), None,
            );
            self.lane.run(move |hub: &mut DeskHub| hub.remove(&key));
        }
        self.lane.run(|hub: &mut DeskHub| hub.set_restore_scan_failed(false));
        *discovered = true;
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
        self.discover_restore_files();
        for desk in self.desks.values() {
            desk.recover();
        }
        self.lane.run(|hub: &mut DeskHub| hub.settle_retired());
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
        self.discover_restore_files();
        for desk in self.desks.values() {
            desk.settle_displays();
        }
        self.lane.run(|hub: &mut DeskHub| hub.settle_retired());
    }

    pub fn displays_changed(&self) {
        for desk in self.desks.values() {
            desk.displays_changed();
        }
        self.lane.run(|hub: &mut DeskHub| hub.settle_retired());
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

    /// 会话切换时清除临时收起状态，保留已启用的虚拟屏。
    pub fn reset_retained_screen_session(&self, restore_windows: bool) {
        for desk in self.desks.values() {
            desk.reset_retained_screen_session(restore_windows);
        }
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
        // 连点去重：同一个窗口已经在入队/执行的路上，这次点击直接丢掉。
        if !target.claim_pull(window_key) {
            self.log(format!("放 {}：上一次点击还在执行，这次点击忽略", window.label()));
            return;
        }
        let guard = target.pull_guard();
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
            if ready {
                hub.pull_with_transfer(&target_key, window);
            } else {
                match hub.core_mut(&target_key) {
                    Some(core) => { core.pull_with(window, None); }
                    None => {
                        if let Some(log) = &log {
                            log(&format!("放 {label}：桌面队列里没有屏 {key_label}"));
                        }
                    }
                }
            }
            // 动作结束（或失败）都要清标记，否则这个窗口再也点不动。
            *guard.lock().unwrap_or_else(|e| e.into_inner()) = None;
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

    #[cfg(windows)]
    fn startup_shared_read_failure_preserves_debts(unlock_before_handoff: bool) {
        use std::os::windows::fs::OpenOptionsExt;
        use crate::left_behind::{LeftBehindBook, LeftBehindEntry};
        use crate::store::JsonFileStore;

        let mut harness = DeskSetHarness::new();
        let old = closed_record(960, "Code.exe");
        let new = closed_record(961, "notepad.exe");
        let path = harness.paths.left_behind_file();
        let store = JsonFileStore::<LeftBehindBook>::new(&path);
        store.save(&LeftBehindBook {
            entries: vec![LeftBehindEntry::new(old.clone(), 1_800_000_000_000)],
        }).unwrap();
        let original = std::fs::read(&path).unwrap();
        // Deny reads while explicitly allowing replacement of the destination.
        let guard = std::fs::OpenOptions::new()
            .write(true).share_mode(0x2 | 0x4).open(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap_err().raw_os_error(), Some(32));
        restart_set(&mut harness);
        let mut guard = Some(guard);
        if unlock_before_handoff {
            guard.take();
        }
        harness.set.set_wishes(vec![harness.wish("a")]);
        harness.set.idle();
        let source = RestoreRecordStore::<PhoneRecord>::new(harness.paths.record_file_for_screen(Some("a")));
        source.record(&new).unwrap();
        harness.set.recover();
        harness.set.idle();
        drop(guard);

        if !unlock_before_handoff {
            assert_eq!(std::fs::read(&path).unwrap(), original, "unread shared debts must not be overwritten");
            assert_eq!(source.pending(), Some(new.clone()), "the new source must stay recoverable");
            assert_eq!(harness.set.state_of("a").unwrap().notice, Some(crate::desk::model::DeskNotice::RecordUnwritable));
            harness.set.recover();
            harness.set.idle();
        }
        let saved = store.try_load_or_quarantine().unwrap().unwrap();
        assert!(saved.entries.iter().any(|entry| entry.record == old));
        assert!(saved.entries.iter().any(|entry| entry.record == new));
        assert!(source.pending().is_none(), "unlocking must allow the handoff to retry");
    }

    #[cfg(windows)]
    #[test]
    fn startup_shared_read_failure_retries_after_unlock_before_handoff() {
        startup_shared_read_failure_preserves_debts(true);
    }

    #[cfg(windows)]
    #[test]
    fn startup_shared_read_failure_preserves_source_while_writes_are_allowed() {
        startup_shared_read_failure_preserves_debts(false);
    }

    #[test]
    fn automatic_returns_skip_other_screen_active_placement() {
        let harness = ready_two_screens();
        let key = WindowKey::new(901, 0x9011);
        let info = window(key, Some("Code.exe"), "active workspace");
        harness.windows.insert_window(info.clone(), Rect::new(10.0, 20.0, 800.0, 600.0));
        harness.set.pull("a", info);
        harness.set.idle();
        let before = harness.windows.frame_of(key).expect("active frame");
        let active = RestoreRecordStore::<PhoneRecord>::new(harness.paths.record_file_for_screen(Some("a"))).pending().expect("active debt");
        let stale = PhoneRecord {
            window: WindowKey::new(901, 0x9012),
            original: Rect::new(800.0, 600.0, 700.0, 500.0),
            title: Some("closed workspace".into()),
            ..active
        };
        harness.set.books.left_behind.lock().expect("shared book").entries.push(crate::left_behind::LeftBehindEntry::new(stale, 1_800_000_000_000));
        harness.apps.set_pids("Code.exe", vec![901]);
        harness.set.check_returns();
        harness.set.idle();
        let after = harness.windows.frame_of(key).expect("active frame");
        eprintln!("active A before={before:?}, after B checks={after:?}, zoom_delta={}", harness.windows.zoom_delta_total());
        assert_eq!(after, before, "B must not move a window currently owned by A");
        assert_eq!(harness.windows.zoom_delta_total(), 2, "B must not repay A's live zoom debt");
    }

    #[cfg(windows)]
    #[test]
    fn other_screen_cannot_consume_unsettled_shared_handoff() {
        use std::os::windows::fs::OpenOptionsExt;

        let harness = ready_two_screens();
        let old = WindowKey::new(900, 0x9001);
        let other = WindowKey::new(900, 0x9002);
        harness.windows.insert_window(window(other, Some("zed.exe"), "other"), Rect::new(10.0, 20.0, 800.0, 600.0));
        let record = PhoneRecord {
            window: old,
            app: "Zed".into(),
            app_id: Some("zed.exe".into()),
            label: "closed source".into(),
            zoom: 2,
            original: Rect::new(10.0, 20.0, 800.0, 600.0),
            filled: Some(Rect::new(2560.0, 0.0, 1080.0, 1800.0)),
            ..PhoneRecord::default()
        };
        let path = harness.paths.record_file_for_screen(Some("a"));
        let ledger = RestoreRecordStore::<PhoneRecord>::new(&path);
        ledger.record(&record).expect("source ledger");
        let guard = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2).open(&path).expect("deny source settlement");
        harness.set.recover();
        harness.set.check_returns();
        harness.set.idle();
        let while_locked = harness.windows.zoom_delta_total();
        eprintln!("while source A locked: zoom_delta={while_locked}, source_pending={}", ledger.pending().is_some());
        drop(guard);
        harness.set.recover();
        harness.set.idle();
        let after_retry = harness.windows.zoom_delta_total();
        eprintln!("after unlock/recover: zoom_delta={after_retry}, source_pending={}", ledger.pending().is_some());
        assert_eq!(while_locked, 0, "B must not repay shared destination while A still owns source debt");
        assert_eq!(after_retry, -2, "repay the source debt exactly once");
    }

    #[test]
    fn uncertain_app_zoom_keeps_shared_source_until_manual_confirmation() {
        use crate::left_behind::{LeftBehindBook, LeftBehindEntry};
        use crate::store::JsonFileStore;

        let harness = ready_two_screens();
        let record = closed_record(930, "zed.exe");
        let key = WindowKey::new(930, 0x9301);
        let returned = Rect::new(100.0, 100.0, 1080.0, 1800.0);
        harness.windows.insert_window(window(key, Some("zed.exe"), "returned Zed"), returned);
        harness.apps.set_pids("zed.exe", vec![930]);
        let book = LeftBehindBook {
            entries: vec![LeftBehindEntry::new(record.clone(), 1_800_000_000_000)],
        };
        *harness.set.books.left_behind.lock().unwrap() = book.clone();
        let store = JsonFileStore::<LeftBehindBook>::new(harness.paths.left_behind_file());
        store.save(&book).unwrap();
        harness.windows.set_zoom_uncertain(true);

        harness.set.check_returns();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), -1);
        assert_eq!(harness.windows.frame_of(key), Some(returned), "pending zoom must stop the same return scan");
        assert_eq!(store.try_load_strict().unwrap().unwrap().entries.len(), 1,
            "the pending journal must retain its only shared source");
        assert!(harness.paths.data_dir().join("zoom-operation.json").exists());

        harness.windows.set_zoom_uncertain(false);
        harness.set.desk_for_screen(Some("a")).unwrap().confirm_manual_zoom_reset();
        harness.set.idle();
        assert!(!harness.paths.data_dir().join("zoom-operation.json").exists());
        harness.set.check_returns();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), -1, "manual confirmation cannot send more keys");
        assert_eq!(harness.windows.frame_of(key), Some(record.original));
        assert!(store.try_load_strict().unwrap().unwrap().entries.is_empty());
    }

    #[test]
    fn uncertain_app_zoom_stops_clearing_later_shared_debts() {
        use crate::left_behind::{LeftBehindBook, LeftBehindEntry};
        use crate::store::JsonFileStore;

        let harness = ready_two_screens();
        let pending = closed_record(931, "zed.exe");
        let later = closed_record(932, "zed.exe");
        harness.windows.insert_window(window(WindowKey::new(931, 0x9311), Some("zed.exe"), "active Zed"),
            Rect::new(100.0, 100.0, 800.0, 600.0));
        harness.apps.set_running(932, false);
        let book = LeftBehindBook {
            entries: vec![LeftBehindEntry::new(pending, 1_800_000_000_000),
                LeftBehindEntry::new(later.clone(), 1_800_000_000_000)],
        };
        *harness.set.books.left_behind.lock().unwrap() = book.clone();
        let store = JsonFileStore::<LeftBehindBook>::new(harness.paths.left_behind_file());
        store.save(&book).unwrap();
        harness.windows.set_zoom_uncertain(true);

        harness.set.check_returns();
        harness.set.idle();
        let saved = store.try_load_strict().unwrap().unwrap();
        assert_eq!(saved.entries[1].record, later, "a pending zoom must stop later shared debt mutations");
        assert_eq!(harness.windows.zoom_delta_total(), -1);
        assert!(harness.paths.data_dir().join("zoom-operation.json").exists());
    }

    #[test]
    fn rejected_app_zoom_keeps_shared_source_for_retry() {
        use crate::left_behind::{LeftBehindBook, LeftBehindEntry};
        use crate::store::JsonFileStore;

        let harness = ready_two_screens();
        let record = closed_record(933, "zed.exe");
        let key = WindowKey::new(933, 0x9331);
        let returned = Rect::new(100.0, 100.0, 1080.0, 1800.0);
        harness.windows.insert_window(window(key, Some("zed.exe"), "returned Zed"), returned);
        harness.apps.set_pids("zed.exe", vec![933]);
        let book = LeftBehindBook {
            entries: vec![LeftBehindEntry::new(record.clone(), 1_800_000_000_000)],
        };
        *harness.set.books.left_behind.lock().unwrap() = book.clone();
        let store = JsonFileStore::<LeftBehindBook>::new(harness.paths.left_behind_file());
        store.save(&book).unwrap();
        harness.windows.set_zoom_ok(false);

        harness.set.check_returns();
        harness.set.idle();
        assert_eq!(harness.windows.frame_of(key), Some(returned));
        assert_eq!(store.try_load_strict().unwrap().unwrap().entries[0].record, record,
            "rejected app-wide input must preserve its zoom debt");
        assert!(!harness.paths.data_dir().join("zoom-operation.json").exists());

        harness.windows.set_zoom_ok(true);
        harness.set.check_returns();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), -2);
        assert_eq!(harness.windows.frame_of(key), Some(record.original));
        assert!(store.try_load_strict().unwrap().unwrap().entries.is_empty());
    }

    fn closed_record(pid: i32, app_id: &str) -> PhoneRecord {
        PhoneRecord {
            window: WindowKey::new(pid, pid as u64 * 16),
            app: app_id.into(),
            app_id: Some(app_id.into()),
            label: "closed source".into(),
            zoom: 2,
            original: Rect::new(10.0, 20.0, 800.0, 600.0),
            filled: Some(Rect::new(2560.0, 0.0, 1080.0, 1800.0)),
            ..PhoneRecord::default()
        }
    }

    #[cfg(windows)]
    #[test]
    fn explicit_placement_waits_for_other_screen_handoff_settlement() {
        use std::os::windows::fs::OpenOptionsExt;

        let harness = ready_two_screens();
        let record = closed_record(902, "Code.exe");
        let ledger = RestoreRecordStore::<PhoneRecord>::new(harness.paths.record_file_for_screen(Some("a")));
        ledger.record(&record).unwrap();
        let guard = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2)
            .open(harness.paths.record_file_for_screen(Some("a"))).unwrap();
        harness.set.recover();
        harness.set.idle();
        let key = WindowKey::new(902, 0x9021);
        let info = window(key, Some("Code.exe"), "returned workspace");
        harness.windows.insert_window(info.clone(), record.filled.unwrap());
        harness.set.pull("b", info.clone());
        harness.set.idle();
        assert!(harness.set.state_of("b").unwrap().placed.is_none());
        assert!(!harness.set.record_file_exists("b"));
        assert_eq!(harness.windows.zoom_delta_total(), 0);
        assert_eq!(harness.set.books.left_behind.lock().unwrap().entries[0].record, record);

        drop(guard);
        harness.set.recover();
        harness.set.idle();
        harness.set.pull("b", info);
        harness.set.idle();
        assert!(harness.set.state_of("b").unwrap().placed.is_some());
        assert!(ledger.pending().is_none());
        let target = RestoreRecordStore::<PhoneRecord>::new(harness.paths.record_file_for_screen(Some("b"))).pending().unwrap();
        assert_eq!(target.original, record.original);
        assert_eq!(target.zoom, 2);
    }

    #[cfg(windows)]
    #[test]
    fn retired_source_stays_protected_and_retries_after_unlock() {
        use std::os::windows::fs::OpenOptionsExt;

        let mut harness = ready_two_screens();
        let record = closed_record(903, "zed.exe");
        let other = WindowKey::new(903, 0x9031);
        harness.windows.insert_window(window(other, Some("zed.exe"), "other"), record.original);
        let path = harness.paths.record_file_for_screen(Some("a"));
        let ledger = RestoreRecordStore::<PhoneRecord>::new(&path);
        ledger.record(&record).unwrap();
        let guard = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2).open(&path).unwrap();
        harness.set.recover();
        harness.set.idle();
        harness.set.set_wishes(vec![harness.wish("b")]);
        harness.set.check_returns();
        harness.set.settle_displays();
        harness.set.idle();
        assert!(harness.set.state_of("a").is_none());
        assert_eq!(harness.windows.zoom_delta_total(), 0);
        assert_eq!(ledger.pending(), Some(record));

        drop(guard);
        harness.set.settle_displays();
        harness.set.idle();
        harness.set.check_returns();
        harness.set.settle_displays();
        harness.set.idle();
        assert!(ledger.pending().is_none());
        assert_eq!(harness.windows.zoom_delta_total(), -2);
    }

    #[cfg(windows)]
    #[test]
    fn unreadable_source_blocks_shared_consumers_until_unlock() {
        use std::os::windows::fs::OpenOptionsExt;

        let harness = ready_two_screens();
        let record = closed_record(904, "zed.exe");
        let key = WindowKey::new(904, 0x9041);
        let info = window(key, Some("zed.exe"), "returned");
        harness.windows.insert_window(info.clone(), record.filled.unwrap());
        harness.apps.set_pids("zed.exe", vec![904]);
        let path = harness.paths.record_file_for_screen(Some("a"));
        let ledger = RestoreRecordStore::<PhoneRecord>::new(&path);
        ledger.record(&record).unwrap();
        harness.set.books.left_behind.lock().unwrap().entries.push(
            crate::left_behind::LeftBehindEntry::new(record.clone(), 1_800_000_000_000));
        let guard = std::fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
        harness.set.check_returns();
        harness.set.pull("b", info);
        harness.set.idle();
        assert!(harness.set.state_of("b").unwrap().placed.is_none());
        assert_eq!(harness.windows.zoom_delta_total(), 0);
        assert_eq!(harness.set.books.left_behind.lock().unwrap().entries[0].record, record);

        drop(guard);
        harness.set.recover();
        harness.set.idle();
        assert!(ledger.pending().is_none());
        assert_eq!(harness.windows.zoom_delta_total(), -2);
    }

    #[cfg(windows)]
    #[test]
    fn pruning_and_new_handoffs_preserve_other_screen_unsettled_destination() {
        use std::os::windows::fs::OpenOptionsExt;
        use crate::left_behind::{LeftBehindEntry, LIMIT};

        let harness = ready_two_screens();
        let protected = closed_record(905, "Code.exe");
        let path = harness.paths.record_file_for_screen(Some("a"));
        let ledger = RestoreRecordStore::<PhoneRecord>::new(&path);
        ledger.record(&protected).unwrap();
        {
            let mut book = harness.set.books.left_behind.lock().unwrap();
            book.entries.push(LeftBehindEntry::new(protected.clone(), 0));
            for n in 1..LIMIT {
                book.entries.push(LeftBehindEntry::new(closed_record(1000 + n as i32, "other.exe"), 1_800_000_000_000 + n as i64));
            }
        }
        let guard = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2).open(&path).unwrap();
        harness.set.check_returns();
        harness.set.idle();
        assert!(harness.set.books.left_behind.lock().unwrap().entries.iter().any(|e| e.record == protected));
        let arriving = closed_record(906, "Code.exe");
        let target = RestoreRecordStore::<PhoneRecord>::new(harness.paths.record_file_for_screen(Some("b")));
        target.record(&arriving).unwrap();
        harness.set.recover();
        harness.set.idle();
        let book = harness.set.books.left_behind.lock().unwrap();
        assert_eq!(book.entries.len(), LIMIT);
        assert!(book.entries.iter().any(|e| e.record == protected));
        assert!(book.entries.iter().any(|e| e.record == arriving));
        assert!(target.pending().is_none());
        assert_eq!(ledger.pending(), Some(protected));
        drop(guard);
    }

    #[test]
    fn placement_can_reuse_handoff_settled_in_the_same_action() {
        let harness = ready_two_screens();
        let record = closed_record(907, "Code.exe");
        let ledger = RestoreRecordStore::<PhoneRecord>::new(harness.paths.record_file_for_screen(Some("a")));
        ledger.record(&record).unwrap();
        let key = WindowKey::new(907, 0x9071);
        let info = window(key, Some("Code.exe"), "returned workspace");
        harness.windows.insert_window(info.clone(), record.filled.unwrap());
        harness.set.pull("a", info);
        harness.set.idle();
        assert!(harness.set.state_of("a").unwrap().placed.is_some(), "the successful settlement must release its guard before matching");
        assert_eq!(ledger.pending().unwrap().original, record.original);
        assert!(harness.set.books.left_behind.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn other_screen_pending_record_does_not_block_unrelated_repayment() {
        let harness = ready_two_screens();
        let unrelated = closed_record(908, "Code.exe");
        RestoreRecordStore::<PhoneRecord>::new(harness.paths.record_file_for_screen(Some("a"))).record(&unrelated).unwrap();
        let owed = closed_record(909, "zed.exe");
        let key = WindowKey::new(909, 0x9091);
        harness.windows.insert_window(window(key, Some("zed.exe"), "other"), owed.original);
        harness.set.books.left_behind.lock().unwrap().entries.push(
            crate::left_behind::LeftBehindEntry::new(owed, 1_800_000_000_000));
        harness.set.check_returns();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), -2);
    }

    #[cfg(windows)]
    #[test]
    fn startup_recovers_removed_screen_source_without_creating_its_display() {
        use std::os::windows::fs::OpenOptionsExt;
        use crate::left_behind::{LeftBehindBook, LeftBehindEntry};
        use crate::store::JsonFileStore;

        for id in ["removed", ScreenConfig::LEGACY_PHONE_ID] {
            let mut harness = DeskSetHarness::new();
            let record = PhoneRecord {
                window: WindowKey::new(910, 0x9100),
                app: "Zed".into(), app_id: Some("zed.exe".into()), label: "closed source".into(), zoom: 2,
                original: Rect::new(10.0, 20.0, 800.0, 600.0),
                filled: Some(Rect::new(2560.0, 0.0, 1080.0, 1800.0)),
                ..PhoneRecord::default()
            };
            let path = harness.paths.record_file_for_screen(Some(id));
            let ledger = RestoreRecordStore::<PhoneRecord>::new(&path);
            ledger.record(&record).unwrap();
            JsonFileStore::new(harness.paths.left_behind_file()).save(&LeftBehindBook {
                entries: vec![LeftBehindEntry::new(record.clone(), 1_800_000_000_000)],
            }).unwrap();
            let other = WindowKey::new(910, 0x9101);
            harness.windows.insert_window(window(other, Some("zed.exe"), "other"), record.original);
            harness.set = DeskSet::new(
                harness.displays.clone(), harness.windows.clone(), harness.apps.clone(), &harness.paths,
                DeskSetOptions { timeline: Some(harness.timeline.clone()), ..DeskSetOptions::default() });
            let guard = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2).open(&path).unwrap();
            harness.set.set_remote(true);
            harness.set.set_wishes(vec![harness.wish("b")]);
            harness.set.recover();
            harness.set.check_returns();
            harness.set.idle();
            assert_eq!(harness.windows.zoom_delta_total(), 0, "startup must protect removed source {id}");
            assert_eq!(harness.displays.create_calls().len(), 1, "recovery must not create the removed screen");
            assert_eq!(ledger.pending(), Some(record));
            drop(guard);
            harness.set.settle_displays();
            harness.set.check_returns();
            harness.set.idle();
            assert!(ledger.pending().is_none(), "removed source {id} must have a retry path");
            assert_eq!(harness.windows.zoom_delta_total(), -2);
            harness.set.settle_displays();
            harness.set.idle();
            assert_eq!(harness.windows.zoom_delta_total(), -2);
        }
    }

    fn restart_set(harness: &mut DeskSetHarness) {
        harness.set = DeskSet::new(
            harness.displays.clone(), harness.windows.clone(), harness.apps.clone(), &harness.paths,
            DeskSetOptions { timeline: Some(harness.timeline.clone()), ..DeskSetOptions::default() });
    }

    fn seed_startup_debt(harness: &DeskSetHarness, id: &str, record: &PhoneRecord) {
        use crate::left_behind::{LeftBehindBook, LeftBehindEntry};
        use crate::store::JsonFileStore;
        RestoreRecordStore::<PhoneRecord>::new(harness.paths.record_file_for_screen(Some(id))).record(record).unwrap();
        JsonFileStore::new(harness.paths.left_behind_file()).save(&LeftBehindBook {
            entries: vec![LeftBehindEntry::new(record.clone(), 1_800_000_000_000)],
        }).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn startup_keeps_unreadable_removed_source_for_retry() {
        use std::os::windows::fs::OpenOptionsExt;

        let mut harness = DeskSetHarness::new();
        let record = closed_record(911, "zed.exe");
        seed_startup_debt(&harness, "removed", &record);
        restart_set(&mut harness);
        let other = WindowKey::new(911, 0x9111);
        harness.windows.insert_window(window(other, Some("zed.exe"), "other"), record.original);
        let path = harness.paths.record_file_for_screen(Some("removed"));
        let guard = std::fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
        harness.set.set_remote(true);
        harness.set.set_wishes(vec![harness.wish("b")]);
        harness.set.recover();
        harness.set.check_returns();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 0);
        assert_eq!(harness.displays.create_calls().len(), 1);
        drop(guard);
        harness.set.recover();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), -2);
        assert!(!path.exists());
    }

    #[test]
    fn startup_directory_failure_blocks_consumption_and_retries_discovery() {
        let mut harness = DeskSetHarness::new();
        let record = closed_record(912, "zed.exe");
        seed_startup_debt(&harness, "removed", &record);
        restart_set(&mut harness);
        let other = WindowKey::new(912, 0x9121);
        harness.windows.insert_window(window(other, Some("zed.exe"), "other"), record.original);
        let hidden = harness.dir.with_extension("unavailable");
        std::fs::rename(&harness.dir, &hidden).unwrap();
        std::fs::write(&harness.dir, b"directory temporarily unavailable").unwrap();
        harness.set.set_remote(true);
        harness.set.set_wishes(vec![harness.wish("b")]);
        harness.set.check_returns();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 0);
        std::fs::remove_file(&harness.dir).unwrap();
        std::fs::rename(hidden, &harness.dir).unwrap();
        harness.set.recover();
        harness.set.check_returns();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), -2);
        assert!(!harness.paths.record_file_for_screen(Some("removed")).exists());
        harness.set.set_wishes(vec![harness.wish("b")]);
        harness.set.recover();
        harness.set.settle_displays();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), -2, "successful discovery must not register sources twice");
        assert_eq!(harness.displays.create_calls().len(), 1);
    }

    #[test]
    fn startup_current_wish_keeps_one_owner_for_its_restore_file() {
        for id in ["a", ScreenConfig::LEGACY_PHONE_ID] {
            let mut harness = DeskSetHarness::new();
            let record = closed_record(913, "Code.exe");
            seed_startup_debt(&harness, id, &record);
            restart_set(&mut harness);
            harness.set.set_remote(true);
            harness.set.set_wishes(vec![harness.wish(id)]);
            harness.set.recover();
            harness.set.idle();
            let key = WindowKey::new(914, 0x9141);
            let info = window(key, Some("Code.exe"), "new active workspace");
            harness.windows.insert_window(info.clone(), record.original);
            harness.set.pull(id, info);
            harness.set.idle();
            harness.set.recover();
            harness.set.settle_displays();
            harness.set.idle();
            let active = RestoreRecordStore::<PhoneRecord>::new(harness.paths.record_file_for_screen(Some(id))).pending().unwrap();
            assert_eq!(active.window, key);
            assert_eq!(active.zoom, 2);
            assert_eq!(harness.set.state_of(id).unwrap().placed.unwrap().window.key, key);
            assert_eq!(harness.windows.zoom_delta_total(), 2);
            assert_eq!(harness.displays.create_calls().len(), 1);
        }
    }

    #[cfg(windows)]
    #[test]
    fn restoring_source_reclaims_failed_handoff_before_changing_its_debt() {
        use std::os::windows::fs::OpenOptionsExt;
        use crate::left_behind::LeftBehindEntry;

        let harness = ready_two_screens();
        let record = closed_record(915, "zed.exe");
        let path = harness.paths.record_file_for_screen(Some("a"));
        let ledger = RestoreRecordStore::<PhoneRecord>::new(&path);
        ledger.record(&record).unwrap();
        harness.set.books.left_behind.lock().unwrap().entries.push(LeftBehindEntry::new(record.clone(), 1_800_000_000_000));
        // A failed handoff can be retried after the same process reuses the HWND.
        harness.windows.insert_window(window(record.window, Some("zed.exe"), "source returned"), record.filled.unwrap());
        harness.windows.insert_window(window(WindowKey::new(915, 0x9151), Some("zed.exe"), "other"), record.original);
        let guard = Arc::new(Mutex::new(None));
        let held = guard.clone();
        harness.windows.on_zoom(move |_, _| {
            let mut held = held.lock().unwrap();
            if held.is_none() {
                *held = Some(std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2).open(&path).unwrap());
            }
        });
        harness.set.desk_for_screen(Some("a")).unwrap().recover();
        harness.set.idle();
        assert_eq!(ledger.pending().unwrap().zoom, 0);
        assert_eq!(harness.windows.zoom_delta_total(), -2);
        harness.set.check_returns();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), -2, "the original shared copy must not survive source repayment");
        assert!(harness.set.books.left_behind.lock().unwrap().entries.is_empty());
        guard.lock().unwrap().take();
        harness.windows.on_zoom(|_, _| {});
        harness.set.recover();
        harness.set.idle();
        assert!(ledger.pending().is_none());
        assert_eq!(harness.windows.zoom_delta_total(), -2);
    }

    #[test]
    fn startup_keeps_matching_restore_directory_as_an_unreadable_source() {
        let mut harness = DeskSetHarness::new();
        let record = closed_record(916, "zed.exe");
        seed_startup_debt(&harness, "removed", &record);
        let path = harness.paths.record_file_for_screen(Some("removed"));
        let hidden = path.with_extension("held");
        std::fs::rename(&path, &hidden).unwrap();
        std::fs::create_dir(&path).unwrap();
        restart_set(&mut harness);
        harness.windows.insert_window(window(WindowKey::new(916, 0x9161), Some("zed.exe"), "other"), record.original);
        harness.set.set_remote(true);
        harness.set.set_wishes(vec![harness.wish("b")]);
        harness.set.recover();
        harness.set.check_returns();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 0, "matching unreadable source paths must not be ignored");
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(hidden, &path).unwrap();
        harness.set.settle_displays();
        harness.set.idle();
        assert!(!path.exists());
        assert_eq!(harness.windows.zoom_delta_total(), -2);
    }

    #[cfg(windows)]
    #[test]
    fn source_repayment_waits_until_shared_copy_can_be_reclaimed() {
        use std::os::windows::fs::OpenOptionsExt;
        use crate::left_behind::{LeftBehindBook, LeftBehindEntry};
        use crate::store::JsonFileStore;

        let harness = ready_two_screens();
        let record = closed_record(917, "zed.exe");
        let ledger = RestoreRecordStore::<PhoneRecord>::new(harness.paths.record_file_for_screen(Some("a")));
        ledger.record(&record).unwrap();
        let book = LeftBehindBook { entries: vec![LeftBehindEntry::new(record.clone(), 1_800_000_000_000)] };
        *harness.set.books.left_behind.lock().unwrap() = book.clone();
        let path = harness.paths.left_behind_file();
        JsonFileStore::new(&path).save(&book).unwrap();
        let guard = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2).open(&path).unwrap();
        harness.windows.insert_window(window(record.window, Some("zed.exe"), "source returned"), record.filled.unwrap());
        harness.set.desk_for_screen(Some("a")).unwrap().recover();
        harness.set.idle();
        assert_eq!(ledger.pending(), Some(record.clone()));
        assert_eq!(harness.windows.zoom_delta_total(), 0);
        assert_eq!(harness.windows.frame_of(record.window), record.filled);
        assert_eq!(harness.set.books.left_behind.lock().unwrap().entries[0].record, record);
        drop(guard);
        harness.set.desk_for_screen(Some("a")).unwrap().recover();
        harness.set.check_returns();
        harness.set.idle();
        assert!(ledger.pending().is_none());
        assert_eq!(harness.windows.zoom_delta_total(), -2);
        assert!(harness.set.books.left_behind.lock().unwrap().entries.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn zoom_change_reclaims_failed_handoff_before_updating_live_source() {
        use std::os::windows::fs::OpenOptionsExt;

        let harness = ready_two_screens();
        let key = WindowKey::new(918, 0x9181);
        let info = window(key, Some("zed.exe"), "active Zed");
        harness.windows.insert_window(info.clone(), Rect::new(10.0, 20.0, 800.0, 600.0));
        harness.set.pull("a", info.clone());
        harness.set.idle();
        let filled = harness.windows.frame_of(key).unwrap();
        let path = harness.paths.record_file_for_screen(Some("a"));
        let guard = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2).open(&path).unwrap();
        harness.windows.remove(key);
        harness.set.verify_placement();
        harness.set.idle();
        assert!(harness.set.state_of("a").unwrap().placed.is_some());
        assert_eq!(harness.set.books.left_behind.lock().unwrap().entries.len(), 1);
        drop(guard);
        harness.windows.insert_window(info, filled);
        harness.windows.insert_window(window(WindowKey::new(918, 0x9182), Some("zed.exe"), "other Zed"), Rect::new(30.0, 40.0, 800.0, 600.0));
        harness.set.zoom("a", 1);
        harness.set.check_returns();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 3, "the old shared debt must be reclaimed before zoom changes its source");
        assert!(harness.set.books.left_behind.lock().unwrap().entries.is_empty());
        harness.set.put_back(Some("a"));
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn transfer_reclaims_failed_handoff_before_source_settlement() {
        use std::os::windows::fs::OpenOptionsExt;

        for lock_shared in [false, true] {
            let harness = ready_two_screens();
            let key = WindowKey::new(919, 0x9191);
            let info = window(key, Some("zed.exe"), "transferred Zed");
            let original = Rect::new(10.0, 20.0, 800.0, 600.0);
            harness.apps.set_running(key.pid, true);
            harness.windows.insert_window(info.clone(), original);
            harness.set.pull("a", info.clone());
            harness.set.idle();
            let filled = harness.windows.frame_of(key).unwrap();
            let source_path = harness.paths.record_file_for_screen(Some("a"));
            let source = RestoreRecordStore::<PhoneRecord>::new(&source_path);
            let record = source.pending().unwrap();
            let source_guard = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2)
                .open(&source_path).unwrap();
            harness.windows.remove(key);
            harness.set.verify_placement();
            harness.set.idle();
            assert_eq!(source.pending(), Some(record.clone()));
            assert_eq!(harness.set.books.left_behind.lock().unwrap().entries.len(), 1);
            drop(source_guard);
            harness.windows.insert_window(info.clone(), filled);
            harness.windows.insert_window(window(WindowKey::new(919, 0x9192), Some("zed.exe"), "other Zed"), original);
            let shared_guard = lock_shared.then(|| std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2)
                .open(harness.paths.left_behind_file()).unwrap());
            harness.set.pull("b", info.clone());
            harness.set.check_returns();
            harness.set.idle();
            if lock_shared {
                assert_eq!(source.pending(), Some(record.clone()), "failed shared reclaim must retain the source");
                assert!(harness.set.state_of("a").unwrap().placed.is_some());
                assert!(harness.set.state_of("b").unwrap().placed.is_none());
                assert!(!harness.set.record_file_exists("b"));
                assert_eq!(harness.windows.frame_of(key), Some(filled));
                assert_eq!(harness.set.books.left_behind.lock().unwrap().entries[0].record, record);
                assert_eq!(harness.windows.zoom_delta_total(), 2);
                drop(shared_guard);
                harness.set.pull("b", info);
                harness.set.check_returns();
                harness.set.idle();
            }
            assert!(source.pending().is_none());
            assert!(harness.set.state_of("a").unwrap().placed.is_none());
            assert!(harness.set.state_of("b").unwrap().placed.is_some());
            assert!(harness.set.books.left_behind.lock().unwrap().entries.is_empty(), "transfer must withdraw the obsolete shared debt");
            assert_eq!(harness.windows.zoom_delta_total(), 2, "transfer cannot repay the shared copy of an active debt");
            harness.set.put_back(Some("b"));
            harness.set.check_returns();
            harness.set.idle();
            assert_eq!(harness.windows.frame_of(key), Some(original));
            assert_eq!(harness.windows.zoom_delta_total(), 0);
        }
    }

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
    fn removed_desk_retries_failed_retirement_without_an_active_wish() {
        let mut harness = DeskSetHarness::new();
        harness.set.set_remote(true);
        harness.set.set_wishes(vec![harness.wish("a")]);
        harness.set.idle();
        let display = harness.set.state_of("a").unwrap().display_id.unwrap();
        harness.displays.fail_retires(1);

        harness.set.set_wishes(Vec::new());
        harness.set.idle();
        assert!(harness.set.is_empty());
        assert!(harness.displays.is_online(display));
        assert_eq!(harness.displays.retire_calls(), 1);

        // 普通心跳不应无间隔重发删除命令，也不依赖屏幕拓扑产生变化。
        harness.set.settle_displays();
        harness.set.idle();
        assert_eq!(harness.displays.retire_calls(), 1);
        harness.timeline.advance(5.0);
        harness.set.settle_displays();
        harness.set.idle();
        assert!(!harness.displays.is_online(display));
        assert_eq!(harness.displays.retire_calls(), 2);
        harness.timeline.advance(5.0);
        harness.set.settle_displays();
        harness.set.idle();
        assert_eq!(harness.displays.retire_calls(), 2, "成功后释放善后记录");
    }

    #[test]
    fn reconnecting_same_wish_does_not_discard_old_screen_retirement() {
        let mut harness = DeskSetHarness::new();
        harness.set.set_remote(true);
        harness.set.set_wishes(vec![harness.wish("a")]);
        harness.set.idle();
        let old = harness.set.state_of("a").unwrap().display_id.unwrap();
        harness.displays.fail_retires(1);
        harness.set.set_remote(false);
        harness.set.set_wishes(Vec::new());
        harness.set.set_wishes(vec![harness.wish("a")]);
        harness.set.set_remote(true);
        harness.set.idle();
        let new = harness.set.state_of("a").unwrap().display_id.unwrap();
        assert_ne!(old, new);
        harness.timeline.advance(5.0);
        harness.set.settle_displays();
        harness.set.idle();
        assert!(!harness.displays.is_online(old));
        assert!(harness.displays.is_online(new));
    }

    #[test]
    fn retained_screen_returns_the_remote_window_without_rebuilding_the_display() {
        let mut harness = DeskSetHarness::new();
        let wishes = vec![harness.wish("a")];
        harness.set.set_remote(true);
        harness.set.set_wishes(wishes.clone());
        harness.set.idle();
        let display = harness.set.state_of("a").unwrap().display_id;
        let key = WindowKey::new(45, 0x103);
        let original = Rect::new(10.0, 20.0, 900.0, 600.0);
        harness.windows.insert(key, original);
        harness.set.pull("a", window(key, Some("Code.exe"), "README.md"));
        harness.set.idle();
        assert_ne!(harness.windows.frame_of(key), Some(original));

        harness.set.set_remote(false);
        harness.set.set_wishes(wishes);
        harness.set.reset_retained_screen_session(true);
        harness.set.idle();

        let state = harness.set.state_of("a").unwrap();
        assert_eq!(state.display_id, display);
        assert_eq!(state.screen_phase, PhoneScreenPhase::Ready);
        assert!(state.placed.is_none());
        assert_eq!(harness.windows.frame_of(key), Some(original));
        assert_eq!(harness.displays.create_calls().len(), 1);
        assert_eq!(harness.displays.retire_calls(), 0);
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
    fn transfer_never_leaves_both_persistent_ledgers_absent() {
        let harness = ready_two_screens();
        let key = WindowKey::new(51, 0x301);
        harness.windows.insert(key, Rect::new(10.0, 20.0, 900.0, 600.0));
        harness.set.pull("a", window(key, Some("Code.exe"), "continuous debt"));
        harness.set.idle();
        let source = harness.paths.record_file_for_screen(Some("a"));
        let target = harness.paths.record_file_for_screen(Some("b"));
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&snapshots);
        harness.windows.on_look(move |looked_up| {
            if looked_up == key {
                observed.lock().unwrap().push((source.is_file(), target.is_file()));
            }
        });

        harness.set.pull("b", window(key, Some("Code.exe"), "continuous debt"));
        harness.set.idle();

        let snapshots = snapshots.lock().unwrap();
        assert!(!snapshots.is_empty(), "observe target preparation and placement");
        assert!(snapshots.iter().all(|(source, target)| *source || *target),
            "a crash at any observed boundary must retain a persistent debt: {snapshots:?}");
        assert!(!harness.set.record_file_exists("a"));
        assert!(harness.set.record_file_exists("b"));
    }

    #[test]
    fn transfer_rejects_target_record_created_during_preparation() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let harness = ready_two_screens();
        let key = WindowKey::new(51, 0x301);
        harness.windows.insert(key, Rect::new(10.0, 20.0, 900.0, 600.0));
        harness.set.pull("a", window(key, Some("Code.exe"), "source retained"));
        harness.set.idle();
        let source = harness.paths.record_file_for_screen(Some("a"));
        let source_bytes = std::fs::read(&source).unwrap();
        let target = harness.paths.record_file_for_screen(Some("b"));
        let target_bytes = serde_json::to_vec(&closed_record(99, "notepad.exe")).unwrap();
        let target_path = target.clone();
        let expected = target_bytes.clone();
        let observations = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&observations);
        harness.windows.on_look(move |looked_up| {
            if looked_up == key && count.fetch_add(1, Ordering::SeqCst) == 1 {
                std::fs::write(&target_path, &expected).unwrap();
            }
        });

        harness.set.pull("b", window(key, Some("Code.exe"), "source retained"));
        harness.set.idle();

        assert_eq!(observations.load(Ordering::SeqCst), 2, "target appears after leftover check");
        assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
        assert_eq!(std::fs::read(&target).unwrap(), target_bytes, "no target replacement");
        assert!(harness.set.state_of("a").unwrap().placed.is_some());
        assert!(harness.set.state_of("b").unwrap().placed.is_none());
        assert_eq!(harness.windows.zoom_delta_total(), 2, "no premature repayment");
    }

    #[test]
    fn transfer_store_refuses_existing_target_and_cross_directory_moves() {
        let harness = ready_two_screens();
        let source_path = harness.paths.record_file_for_screen(Some("a"));
        let target_path = harness.paths.record_file_for_screen(Some("b"));
        let source = RestoreRecordStore::<PhoneRecord>::new(&source_path);
        let target = RestoreRecordStore::<PhoneRecord>::new(&target_path);
        let original = closed_record(91, "Code.exe");
        let other = closed_record(92, "notepad.exe");
        source.record(&original).unwrap();
        target.record(&other).unwrap();
        assert_eq!(source.try_transfer_to(&target).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(source.pending(), Some(original.clone()));
        assert_eq!(target.pending(), Some(other));
        let outside_path = harness.dir.join("other-directory").join("restore.json");
        std::fs::create_dir(outside_path.parent().unwrap()).unwrap();
        let outside = RestoreRecordStore::<PhoneRecord>::new(&outside_path);
        assert_eq!(source.try_transfer_to(&outside).unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
        assert!(!outside_path.exists());
        assert_eq!(source.pending(), Some(original));
    }

    #[cfg(windows)]
    #[test]
    fn transfer_store_refuses_a_dangling_target_without_losing_source() {
        let harness = ready_two_screens();
        let source = RestoreRecordStore::<PhoneRecord>::new(harness.paths.record_file_for_screen(Some("a")));
        let target_path = harness.paths.record_file_for_screen(Some("b"));
        let backing = harness.dir.join("missing-transfer-target");
        let original = closed_record(91, "Code.exe");
        source.record(&original).unwrap();
        std::fs::create_dir(&backing).unwrap();
        let status = std::process::Command::new("powershell.exe").args([
            "-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:SNUG_TEST_LINK -Target $env:SNUG_TEST_TARGET | Out-Null",
        ]).env("SNUG_TEST_LINK", &target_path).env("SNUG_TEST_TARGET", &backing).status().unwrap();
        assert!(status.success());
        std::fs::remove_dir(&backing).unwrap();
        assert!(!target_path.exists());
        assert!(std::fs::symlink_metadata(&target_path).is_ok());

        let target = RestoreRecordStore::<PhoneRecord>::new(&target_path);
        let result = source.try_transfer_to(&target);
        let target_retained = std::fs::symlink_metadata(&target_path).is_ok();
        std::fs::remove_dir(&target_path).unwrap();
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert!(target_retained, "the dangling target entry must be preserved");
        assert_eq!(source.pending(), Some(original));
    }

    #[test]
    fn transfer_settles_source_before_both_desks_recover() {
        let harness = ready_two_screens();
        let key = WindowKey::new(51, 0x301);
        harness.windows.insert(key, Rect::new(10.0, 20.0, 900.0, 600.0));
        harness.set.pull("a", window(key, Some("Code.exe"), "single debt"));
        harness.set.idle();
        harness.set.pull("b", window(key, Some("Code.exe"), "single debt"));
        harness.set.idle();
        assert!(!harness.set.record_file_exists("a"), "移交后只能留下目标债务");
        harness.set.put_back(Some("b"));
        harness.set.recover();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 0, "跨屏后只能偿还一次");
    }

    #[test]
    fn transfer_with_unwritable_target_keeps_source_for_retry() {
        for list_visible in [false, true] {
            let harness = ready_two_screens();
            let key = WindowKey::new(51, 0x301);
            harness.windows.insert(key, Rect::new(10.0, 20.0, 900.0, 600.0));
            harness.set.pull("a", window(key, Some("Code.exe"), "source retained"));
            harness.set.idle();
            if list_visible {
                harness.set.show_list(Some("a"));
                harness.set.idle();
            }
            assert_eq!(harness.set.state_of("a").expect("source").list_visible, list_visible);
            let target = harness.paths.record_file_for_screen(Some("b"));
            std::fs::create_dir(&target).expect("block target source");
            harness.set.pull("b", window(key, Some("Code.exe"), "source retained"));
            harness.set.idle();
            let source = harness.set.state_of("a").expect("source");
            assert!(source.placed.is_some());
            assert_eq!(source.list_visible, list_visible, "拒收后应恢复来源屏原来的显示状态");
            assert!(harness.set.state_of("b").expect("target").placed.is_none());
            assert_eq!(harness.windows.zoom_delta_total(), 2);
            std::fs::remove_dir(&target).expect("unblock target source");
            harness.set.pull("b", window(key, Some("Code.exe"), "source retained"));
            harness.set.idle();
            assert!(harness.set.state_of("b").expect("target").placed.is_some());
            harness.set.put_back(Some("b"));
            harness.set.recover();
            harness.set.idle();
            assert_eq!(harness.windows.zoom_delta_total(), 0);
        }
    }

    #[test]
    fn transfer_target_fill_failure_does_not_restore_consumed_source() {
        let harness = ready_two_screens();
        let key = WindowKey::new(51, 0x301);
        let original = Rect::new(10.0, 20.0, 900.0, 600.0);
        harness.windows.insert(key, original);
        harness.set.pull("a", window(key, Some("Code.exe"), "failed target fill"));
        harness.set.idle();
        harness.windows.misplace_moves(3);
        harness.set.pull("b", window(key, Some("Code.exe"), "failed target fill"));
        harness.set.idle();
        assert!(harness.set.state_of("a").expect("source").placed.is_none());
        assert!(harness.set.state_of("b").expect("target").placed.is_none());
        assert_eq!(harness.windows.frame_of(key), Some(original));
        harness.set.recover();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 0);
        assert!(!harness.set.record_file_exists("a"));
        assert!(!harness.set.record_file_exists("b"));
    }

    #[cfg(windows)]
    #[test]
    fn locked_source_settlement_blocks_transfer_until_unlock() {
        use std::os::windows::fs::OpenOptionsExt;

        let harness = ready_two_screens();
        let key = WindowKey::new(51, 0x301);
        harness.windows.insert(key, Rect::new(10.0, 20.0, 900.0, 600.0));
        harness.set.pull("a", window(key, Some("Code.exe"), "locked source"));
        harness.set.idle();
        let path = harness.paths.record_file_for_screen(Some("a"));
        let guard = std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2)
            .open(&path).expect("deny source settlement");
        harness.set.pull("b", window(key, Some("Code.exe"), "locked source"));
        harness.set.idle();
        assert!(harness.set.state_of("a").expect("source").placed.is_some());
        assert!(harness.set.state_of("b").expect("target").placed.is_none());
        drop(guard);
        harness.set.pull("b", window(key, Some("Code.exe"), "locked source"));
        harness.set.idle();
        assert!(harness.set.state_of("b").expect("target").placed.is_some());
        harness.set.put_back(Some("b"));
        harness.set.recover();
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn transfer_rejects_locked_zoom_rollback_and_retries_after_unlock() {
        use std::os::windows::fs::OpenOptionsExt;

        let harness = ready_two_screens();
        let key = WindowKey::new(51, 0x301);
        harness.windows.insert(key, Rect::new(10.0, 20.0, 900.0, 600.0));
        harness.set.pull("a", window(key, Some("Code.exe"), "transfer debt"));
        harness.set.idle();
        let path = harness.paths.record_file_for_screen(Some("a"));
        let guard = Arc::new(std::sync::Mutex::new(None));
        let lock = Arc::clone(&guard);
        harness.windows.on_zoom(move |_, _| {
            let mut held = lock.lock().expect("lock slot");
            if held.is_none() {
                *held = Some(std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2)
                    .open(&path).expect("lock after reservation"));
            }
        });
        harness.windows.set_zoom_ok(false);
        harness.set.put_back(Some("a"));
        harness.set.pull("b", window(key, Some("Code.exe"), "transfer debt"));
        harness.set.idle();
        assert!(harness.set.state_of("a").expect("source").placed.is_some());
        assert!(harness.set.state_of("b").expect("target").placed.is_none());

        guard.lock().expect("lock slot").take();
        harness.windows.on_zoom(|_, _| {});
        harness.windows.set_zoom_ok(true);
        harness.set.pull("b", window(key, Some("Code.exe"), "transfer debt"));
        harness.set.idle();
        assert!(harness.set.state_of("a").expect("source").placed.is_none());
        assert!(harness.set.state_of("b").expect("target").placed.is_some());
        harness.set.put_back(Some("b"));
        harness.set.idle();
        assert_eq!(harness.windows.zoom_delta_total(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn readding_screen_inherits_unwritten_zoom_rollback() {
        use std::os::windows::fs::OpenOptionsExt;

        for readd_before_unlock in [false, true] {
            let mut harness = ready_two_screens();
            let key = WindowKey::new(51, 0x301);
            harness.windows.insert(key, Rect::new(10.0, 20.0, 900.0, 600.0));
            harness.set.pull("a", window(key, Some("Code.exe"), "retired debt"));
            harness.set.idle();
            let path = harness.paths.record_file_for_screen(Some("a"));
            let ledger = RestoreRecordStore::<PhoneRecord>::new(&path);
            let guard = Arc::new(std::sync::Mutex::new(None));
            let lock = Arc::clone(&guard);
            harness.windows.on_zoom(move |_, _| {
                let mut held = lock.lock().expect("lock slot");
                if held.is_none() {
                    *held = Some(std::fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2)
                        .open(&path).expect("lock after reservation"));
                }
            });
            harness.windows.set_zoom_ok(false);
            harness.set.put_back(Some("a"));
            harness.set.idle();
            harness.set.set_wishes(vec![harness.wish("b")]);
            harness.set.idle();
            if readd_before_unlock {
                harness.set.set_wishes(vec![harness.wish("a"), harness.wish("b")]);
                harness.set.idle();
            }
            let next = WindowKey::new(52, 0x302);
            harness.windows.insert(next, Rect::new(30.0, 40.0, 800.0, 600.0));
            harness.set.pull("a", window(next, Some("Code.exe"), "next"));
            harness.set.settle_displays();
            harness.set.idle();
            assert_eq!(ledger.pending().expect("reserved source").zoom, 0);
            assert_eq!(harness.windows.zoom_delta_total(), 2);

            guard.lock().expect("lock slot").take();
            harness.windows.on_zoom(|_, _| {});
            harness.windows.set_zoom_ok(true);
            harness.set.check_returns();
            harness.set.settle_displays();
            harness.set.idle();
            if readd_before_unlock {
                assert_eq!(ledger.pending().expect("active desk inherited debt").zoom, 2);
            } else {
                assert!(ledger.pending().is_none(), "retired source should finish without a new wish");
                assert_eq!(harness.windows.zoom_delta_total(), 0);
            }
            if !readd_before_unlock {
                harness.set.set_wishes(vec![harness.wish("a"), harness.wish("b")]);
                harness.set.idle();
            }
            harness.set.pull("a", window(next, Some("Code.exe"), "next"));
            harness.set.idle();
            assert_eq!(harness.windows.zoom_delta_total(), 2, "先还旧债再放大新窗口");
            harness.set.put_back(Some("a"));
            harness.set.recover();
            harness.set.settle_displays();
            harness.set.idle();
            assert_eq!(harness.windows.zoom_delta_total(), 0);
            assert!(ledger.pending().is_none(), "退休core不能重新写回旧影子");
        }
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
