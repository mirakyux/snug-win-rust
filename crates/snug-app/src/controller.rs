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
        desk.set_wanted(config.phone_screen);

        let running = Arc::new(AtomicBool::new(true));
        let stop = running.clone();
        let desk_worker = desk.clone();
        let uu = Arc::new(snug_win::UuSensor::new());
        let uu_worker = uu.clone();
        let worker = std::thread::Builder::new()
            .name("snug-desk-idle".to_string())
            .spawn(move || {
                // UU 采样：一路读不到不该影响状态机，`sample` 自己保证只返回"读不到"。
                // 采样节奏照 C# 的 1 秒一次；`idle`/`check_returns` 保持 500ms。
                let mut tracker = snug_core::session::ConnectionTracker::new();
                let started = std::time::Instant::now();
                let mut last_sample = -1.0_f64;
                while stop.load(Ordering::Relaxed) {
                    desk_worker.idle();
                    desk_worker.check_returns();
                    let now = started.elapsed().as_secs_f64();
                    if now - last_sample >= 1.0 {
                        last_sample = now;
                        let signals = uu_worker.sample(now);
                        tracker.update(&signals.to_signals(), now);
                        desk_worker.set_remote(tracker.is_connected());
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            })
            .ok();

        let touch = Arc::new(snug_win::TouchHook::new());

        Self {
            desk,
            displays,
            touch,
            uu,
            touch_region: std::sync::Mutex::new(None),
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

    /// 回到资源库（C# `ShowList`）：把铺开的窗口归位。
    pub fn show_list(&self) {
        self.desk.show_list();
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
