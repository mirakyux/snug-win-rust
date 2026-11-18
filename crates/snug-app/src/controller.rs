//! 应用控制器：把 `snug-core` 的手机屏状态机接到 `snug-win` 的平台实现上。
//!
//! 对应 C# 版 `SnugWin.App.AppController`。区别在于本移植不引 async 运行时：
//! `snug-core::desk::Desk` 的方法只入队，真正的动作在它自己的工作线程里串行执行（对应 C# `SerialLane`）。
//! 这里再起一个轻量线程定时 `idle()`，让重试、超时、watch 这类"时间驱动"的事情有机会跑。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use snug_core::desk::{AppControl, Desk, DeskEvents, DeskOptions, DeskState, PhoneDisplays};
use snug_core::windows::{AppInfo, WindowInfo};
use snug_core::{AppConfig, Paths, Rect, ScreenFit};
use snug_win::{WinPhoneDisplays, WinWindows};

/// 判空壳：`apps` 模块（应用枚举/启动/关闭）还没接上时使用。
/// 接上之后换成 `snug_win::apps::WinApps`，其余代码不动。
struct PendingApps;

impl AppControl for PendingApps {
    fn pid_of(&self, _app: &AppInfo) -> Option<i32> {
        None
    }
    fn pids(&self, _app_id: &str) -> Vec<i32> {
        Vec::new()
    }
    fn open(&self, _app: &AppInfo) -> Option<i32> {
        None
    }
    fn new_window(&self, _pid: i32) -> bool {
        false
    }
    fn quit(&self, _pid: i32) -> bool {
        false
    }
    fn force_quit(&self, _pid: i32) -> bool {
        false
    }
    fn is_running(&self, _pid: i32) -> bool {
        false
    }
    fn app_windows(&self, _app: &AppInfo, _launched_pid: Option<i32>) -> Vec<WindowInfo> {
        Vec::new()
    }
}

/// 控制器：持有状态机与后台线程，界面只读它。
pub struct Controller {
    desk: Desk,
    /// 手机屏（界面窗口要搬到它的位置上，所以留一份）。
    displays: Arc<WinPhoneDisplays>,
    #[allow(dead_code)] // 后续设置界面/诊断要用
    paths: Paths,
    /// "手动强制手机屏"（C# `UUSensor.ManualOverride`），界面回显用。
    manual_override: Arc<AtomicBool>,
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
        let apps: Arc<dyn AppControl> = Arc::new(PendingApps);

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
        let worker = std::thread::Builder::new()
            .name("snug-desk-idle".to_string())
            .spawn(move || {
                while stop.load(Ordering::Relaxed) {
                    desk_worker.idle();
                    desk_worker.check_returns();
                    std::thread::sleep(Duration::from_millis(500));
                }
            })
            .ok();

        Self { desk, displays, paths: paths.clone(), manual_override: Arc::new(AtomicBool::new(false)), running, worker }
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
    /// 语义是"不管 UU 连没连都保持手机屏"。`uu` 探测模块还没接进来，
    /// 这里先把用户的意图记下来（设置窗要回显），并如实说明本次不会生效。
    pub fn set_manual_override(&self, on: bool) {
        self.manual_override.store(on, Ordering::Relaxed);
        crate::warn!("手动强制手机屏 = {on}：uu 探测模块未接线，本次未生效");
    }

    /// 当前"手动强制"状态（设置窗回显用）。
    pub fn manual_override(&self) -> bool {
        self.manual_override.load(Ordering::Relaxed)
    }

    /// 手机屏的屏幕矩形（物理像素）。界面窗口要搬到这块屏上，对应 C#
    /// `PhoneHomeWindow` 跟着 `_displays.BoundsOf(id)` 走。
    pub fn phone_bounds(&self) -> Option<Rect> {
        let id = self.desk.state().display_id?;
        self.displays.bounds_of(id)
    }

    /// 手势配置变了，钩子要按新配置重建（C# `SetGesturesEnabled` 等四个 setter + `TouchHook.Options`）。
    ///
    /// 触摸钩子还没接进控制器，先记一行，避免设置窗看起来改了却毫无反馈。
    pub fn apply_gesture_settings(&self, config: &AppConfig) {
        crate::warn!(
            "手势配置更新（开关={} 滚动={} 长按右键={} 选后复制={} 仅注入={}）：触摸钩子未接线",
            config.gestures,
            config.gesture_scroll,
            config.gesture_long_press_right_click,
            config.copy_after_selection,
            config.gestures_injected_only
        );
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
