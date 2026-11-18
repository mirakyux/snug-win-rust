//! 应用控制器：把 `snug-core` 的手机屏状态机接到 `snug-win` 的平台实现上。
//!
//! 对应 C# 版 `SnugWin.App.AppController`。区别在于本移植不引 async 运行时：
//! `snug-core::desk::Desk` 的方法只入队，真正的动作在它自己的工作线程里串行执行（对应 C# `SerialLane`）。
//! 这里再起一个轻量线程定时 `idle()`，让重试、超时、watch 这类"时间驱动"的事情有机会跑。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use snug_core::desk::{AppControl, Desk, DeskEvents, DeskOptions, DeskState, PhoneDisplays, PhoneScreenPhase};
use snug_core::touch::{TouchOptions, TouchRegion};
use snug_core::windows::{AppInfo, WindowInfo};
use snug_core::{AppConfig, Paths, Rect, ScreenFit};
use snug_win::{WinPhoneDisplays, WinWindows};

/// `snug-win` 的 `WinApps` 已经实现了 core 的 [`AppControl`]
/// （`apps.rs` 的 `impl<S: AppSystem> AppControl for Apps<S>`），控制器直接用真机实现。
use snug_win::WinApps;

/// 手势钩子抛给**界面**的事件（对应 C# `TouchHookService` 的选区两个事件）。
///
/// 关闭窗口那一路不在这里：它由后台线程直接 `desk.close_placed(handle)` 处理，
/// 不经过界面（界面关掉也没关系）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchEvent {
    /// 选区结束（物理像素坐标）→ 界面在选区上方弹"复制"气泡。
    SelectionCompleted(snug_core::Point),
    /// 选区被取消 → 收起气泡。
    SelectionDismissed,
}

/// 钩子线程把事件塞进队列，应用侧每帧取走（对应 C# 里 `Dispatcher.BeginInvoke`）。
#[derive(Default)]
struct TouchEventQueue {
    close: std::sync::Mutex<Vec<u64>>,
    selection: std::sync::Mutex<Vec<TouchEvent>>,
}

impl TouchEventQueue {
    fn push_close(&self, handle: u64) {
        self.close.lock().unwrap_or_else(|e| e.into_inner()).push(handle);
    }

    fn push(&self, event: TouchEvent) {
        self.selection.lock().unwrap_or_else(|e| e.into_inner()).push(event);
    }

    fn take_close(&self) -> Vec<u64> {
        std::mem::take(&mut *self.close.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn take_selection(&self) -> Vec<TouchEvent> {
        std::mem::take(&mut *self.selection.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// 给 `TouchHook` 用的事件接收端。
struct HookEvents {
    queue: Arc<TouchEventQueue>,
}

impl snug_win::touch::TouchEvents for HookEvents {
    fn close_window_requested(&self, handle: u64) {
        self.queue.push_close(handle);
    }

    fn selection_completed(&self, at: snug_core::Point) {
        self.queue.push(TouchEvent::SelectionCompleted(at));
    }

    fn selection_dismissed(&self) {
        self.queue.push(TouchEvent::SelectionDismissed);
    }
}

/// 控制器：持有状态机与后台线程，界面只读它。
pub struct Controller {
    desk: Desk,
    /// 手机屏（界面窗口要搬到它的位置上，所以留一份）。
    displays: Arc<WinPhoneDisplays>,
    /// 手势钩子（C# `TouchHookService`）。
    touch: Arc<snug_win::TouchHook>,
    /// UU 远程探测（C# `UUSensor`）：由后台线程每秒采样一次。
    uu: Arc<snug_win::UuSensor>,
    /// 上一次真的装到钩子上的区域；`set_region` 会起线程，不能每帧重复装。
    touch_region: std::sync::Mutex<Option<TouchRegion>>,
    /// 钩子抛出的事件队列（关闭窗口 / 选区结束 / 选区取消）。
    touch_events: Arc<TouchEventQueue>,
    /// "选后复制"真正按快捷键的那一层（C# `WindowService.PerformSelectionActionAsync`）。
    selection_actions: Arc<snug_win::WinSelectionActions>,
    #[allow(dead_code)] // 后续设置界面/诊断要用
    paths: Paths,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Controller {
    /// 按配置建起状态机与平台实现，并启动后台线程。
    ///
    /// `on_change` 在状态机每次推送新状态时被调用（界面据此重绘）。
    pub fn start(
        config: &AppConfig,
        paths: &Paths,
        on_change: impl Fn() + Send + Sync + 'static,
        log: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        let fit = ScreenFit::from_phone(&config.phone.screen.to_fit_spec());
        let displays = Arc::new(WinPhoneDisplays::new());
        let windows = Arc::new(WinWindows::new());
        let apps: Arc<dyn AppControl> = Arc::new(WinApps::new());

        let events = DeskEvents {
            state_changed: Some(Arc::new(move |_state| on_change())),
            gone: None,
            topology_changing: None,
            log: Some(Arc::new(log)),
        };
        let options = DeskOptions {
            timeline: None,
            default_zoom: config.zoom_steps,
            fit: Some(fit),
            events,
        };

        let desk = Desk::new(displays.clone(), windows, apps, paths, options);
        // 先把上次没撤干净的虚拟屏清掉（C# `CleanupOrphansAsync`）。真机上撞到过：
        // 进程被强杀后虚拟屏留在桌面上，下次启动必须先收尸。
        displays.cleanup_orphans();
        // 再还上次没还回去的窗口（C# `AppController.Start` 里 `_desk.Recover()` 在
        // 第一条 `SetWanted` 之前），否则崩溃/强杀之后那些窗口会留在虚拟屏坐标上。
        desk.recover();
        desk.set_wanted(config.phone_screen);

        let running = Arc::new(AtomicBool::new(true));
        let stop = running.clone();
        let desk_worker = desk.clone();
        let uu = Arc::new(snug_win::UuSensor::new());
        let uu_worker = uu.clone();
        // 钩子要在起后台线程之前建好：线程要 drain 它抛出的"关闭窗口"事件。
        let touch_events = Arc::new(TouchEventQueue::default());
        let touch = Arc::new(snug_win::TouchHook::with_parts(
            Arc::new(snug_win::touch::WinInput),
            Arc::new(HookEvents { queue: touch_events.clone() }),
        ));
        let touch_events_worker = touch_events.clone();
        let worker = std::thread::Builder::new()
            .name("snug-desk-idle".to_string())
            .spawn(move || {
                // UU 采样：一路读不到不该影响状态机，`sample` 自己保证只返回"读不到"。
                // 采样节奏照 C# 的 1 秒一次；`idle`/`check_returns` 保持 500ms。
                let mut tracker = snug_core::session::ConnectionTracker::new();
                // 显示设置变化：C# 用 `SystemEvents.DisplaySettingsChanged`，
                // 我们轮询显示器拓扑签名（更省事，也不需要消息窗）。
                use snug_win::display::DisplayApi;
                let display = snug_win::WinDisplay::new();
                let mut last_topology: Option<Vec<(u32, i32, i32, i32, i32)>> = None;
                let started = std::time::Instant::now();
                let mut last_sample = -1.0_f64;
                let mut last_topology_check = -1.0_f64;
                let mut last_connected = false;
                let mut last_signals: Option<(bool, bool, Option<bool>, bool)> = None;
                while stop.load(Ordering::Relaxed) {
                    desk_worker.idle();
                    desk_worker.check_returns();
                    // 用户点了窗口自带的关闭按钮（C# `_desk.ClosePlaced(handle)`）。
                    for handle in touch_events_worker.take_close() {
                        crate::info!("手势：点了窗口关闭按钮，关掉 {handle:#x}");
                        desk_worker.close_placed(handle);
                    }
                    let now = started.elapsed().as_secs_f64();
                    if now - last_sample >= 1.0 {
                        last_sample = now;
                        let sampled = uu_worker.sample(now);
                        // 真机联调靠日志判断"UU 到底探测到了什么"，所以原始信号变了就记一行。
                        let summary = (
                            sampled.client_running,
                            sampled.client_visible,
                            sampled.capture_active,
                            sampled.peer_active,
                        );
                        if last_signals != Some(summary) {
                            crate::info!(
                                "UU 探测：客户端={} 覆盖窗={} 推流={:?} 对端={} 控制端={}（{}）",
                                summary.0,
                                summary.1,
                                summary.2,
                                summary.3,
                                sampled.controllers.names.len(),
                                if sampled.controllers.readable { "可读" } else { "读不到" }
                            );
                            last_signals = Some(summary);
                        }
                        tracker.update(&sampled.to_signals(), now);
                        if tracker.is_connected() != last_connected {
                            last_connected = tracker.is_connected();
                            crate::info!(
                                "远程会话：{}（控制端 {} 个）",
                                if last_connected { "已连入" } else { "已断开" },
                                tracker.controllers().len()
                            );
                        }
                        desk_worker.set_remote(last_connected);
                    }
                    if now - last_topology_check >= 2.0 {
                        last_topology_check = now;
                        let topology: Vec<(u32, i32, i32, i32, i32)> = display
                            .active_monitors()
                            .iter()
                            .map(|monitor| {
                                (
                                    monitor.stable_id,
                                    monitor.bounds.x as i32,
                                    monitor.bounds.y as i32,
                                    monitor.bounds.width as i32,
                                    monitor.bounds.height as i32,
                                )
                            })
                            .collect();
                        if let Some(previous) = &last_topology
                            && *previous != topology
                        {
                            crate::info!(
                                "显示器拓扑变化（{} → {} 块屏），通知状态机重新核对",
                                previous.len(),
                                topology.len()
                            );
                            desk_worker.displays_changed();
                        }
                        last_topology = Some(topology);
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            })
            .ok();


        Self {
            desk,
            displays,
            touch,
            uu,
            touch_region: std::sync::Mutex::new(None),
            touch_events,
            selection_actions: Arc::new(snug_win::WinSelectionActions::new()),
            paths: paths.clone(),
            running,
            worker,
        }
    }

    pub fn state(&self) -> DeskState {
        self.desk.state()
    }

    /// UU 远程连入/断开（由 `uu` 探测模块调用）。
    #[allow(dead_code)] // 等 snug-win 的 uu 探测接上后由它调用
    pub fn set_remote(&self, on: bool) {
        self.desk.set_remote(on);
    }

    /// 手机屏开关（设置界面用）。
    pub fn set_phone_screen(&self, on: bool) {
        self.desk.set_wanted(on);
    }

    /// 机型/字号变了：换比例并重新铺窗（C# `SetFit` + `SetDefaultZoom`）。
    pub fn set_fit(&self, fit: ScreenFit) {
        self.desk.set_fit(fit);
    }

    /// 默认字号档位（设置窗的 zoom 滑杆）。
    pub fn set_default_zoom(&self, steps: i32) {
        self.desk.set_default_zoom(steps);
    }

    /// 手动强制（C# `AppController.SetManualOverride` → `UUSensor.ManualOverride`）。
    ///
    /// 语义是"不管 UU 连没连都保持手机屏"。现在是真的：采样线程每秒看一次这个开关，
    /// 打开时 `sample` 直接给"在远程"的信号。
    pub fn set_manual_override(&self, on: bool) {
        crate::info!("手动强制手机屏 = {on}");
        self.uu.set_manual_override(on);
    }

    /// 当前"手动强制"状态（设置窗回显用）。
    pub fn manual_override(&self) -> bool {
        self.uu.manual_override()
    }

    /// 手机屏的屏幕矩形（物理像素）。界面窗口要搬到这块屏上，对应 C#
    /// `PhoneHomeWindow` 跟着 `_displays.BoundsOf(id)` 走。
    pub fn phone_bounds(&self) -> Option<Rect> {
        let id = self.desk.state().display_id?;
        self.displays.bounds_of(id)
    }

    /// 手机屏底部指示条的区域（物理像素）。有窗口铺在屏上时，我们的界面窗口
    /// 要缩成这一条，对应 C# 的 `HomeBarWindow`（它是独立置顶窗口）。
    pub fn home_bar_area(&self) -> Option<Rect> {
        let id = self.desk.state().display_id?;
        self.displays.home_bar_area_of(id)
    }

    /// 回到资源库（C# `ShowList`）：界面切回列表，但铺着的窗口**仍留在手机屏上**
    /// （`state.placed` 不清），所以列表里要留一个"回到它"的入口，见 [`Self::return_to_placed`]。
    pub fn show_list(&self) {
        self.desk.show_list();
    }

    /// 回到手机屏上铺着的那个窗口（C# `ReturnToPlaced`）。
    pub fn return_to_placed(&self) {
        self.desk.return_to_placed();
    }

    /// 取走钩子抛出的选区事件（界面每帧调一次）。
    pub fn take_selection_events(&self) -> Vec<TouchEvent> {
        self.touch_events.take_selection()
    }

    /// 在选区处按一次"复制"（C# `CopySelectionAtAsync`）。
    ///
    /// 走平台层的 `perform_selection_action`：它会先确认前台已经是目标窗口，
    /// 不是就拒发——选区在别人窗口里，乱按 Ctrl+C 是有副作用的。
    pub fn copy_selection_at(&self, point: snug_core::Point) -> bool {
        self.selection_actions
            .perform_selection_action(point, snug_core::selection::SelectionBubbleAction::Copy)
    }

    /// 把某个窗口铺到手机屏上（资源库里点已有窗口）。
    pub fn pull(&self, window: WindowInfo) {
        self.desk.pull(window);
    }

    /// 启动一个 App（资源库里点没在跑的 App）。
    pub fn open(&self, app: AppInfo) {
        self.desk.open(app);
    }

    /// 手势配置变了，钩子要按新配置重建（C# `SetGesturesEnabled` 等四个 setter + `TouchHook.Options`）。
    pub fn apply_gesture_settings(&self, config: &AppConfig) {
        crate::info!(
            "设置：手势配置更新（总开关={} 滚动={} 长按右键={} 选后复制={} 仅注入={}）",
            config.gestures,
            config.gesture_scroll,
            config.gesture_long_press_right_click,
            config.copy_after_selection,
            config.gestures_injected_only
        );
        // 立刻按新配置重算一次：关掉总开关会当场把钩子摘下来。
        self.sync_touch(config);
    }

    /// 让手势钩子跟手机屏与配置保持一致（C# `UpdateTouchHook`）。
    ///
    /// 每帧调用是安全的：区域/选项没变时什么都不做，所以不会反复装钩子。
    pub fn sync_touch(&self, config: &AppConfig) {
        let state = self.desk.state();
        let region = if config.gestures && state.screen_phase == PhoneScreenPhase::Ready {
            state
                .display_id
                .and_then(|id| self.displays.bounds_of(id))
                .map(|bounds| TouchRegion::for_phone_screen(bounds, state.close_button))
        } else {
            None
        };

        let mut applied = match self.touch_region.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if *applied != region {
            let running = self.touch.set_region(region);
            crate::info!(
                "手势钩子：{}（{}）",
                if running { "已接管手机屏" } else { "已停用" },
                match &region {
                    Some(region) => format!("区域 {}×{}", region.frame.width, region.frame.height),
                    None => "手机屏未就绪或手势已关".to_string(),
                }
            );
            *applied = region;
        }
        drop(applied);

        // C#：`AllowScroll = GestureScroll`、`AllowRightClick = GestureLongPressRightClick`、
        // `NotifySelectionEnded = CopyAfterSelection && !ListVisible`。
        self.touch.set_options(TouchOptions {
            allow_scroll: config.gestures && config.gesture_scroll,
            allow_right_click: config.gestures && config.gesture_long_press_right_click,
            notify_selection_ended: config.copy_after_selection && !state.list_visible,
        });
        self.touch
            .set_require_injected_flag(config.gestures_injected_only && !self.manual_override());
    }

    /// 撤屏并停掉后台线程；返回前会把状态机收干净。
    pub fn shutdown(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
        self.desk.shutdown();
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}
