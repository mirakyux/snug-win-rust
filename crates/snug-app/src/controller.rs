//! 应用控制器：把 `snug-core` 的手机屏状态机接到 `snug-win` 的平台实现上。
//!
//! 对应 C# 版 `SnugWin.App.AppController`。区别在于本移植不引 async 运行时：
//! `snug-core::desk::Desk` 的方法只入队，真正的动作在它自己的工作线程里串行执行（对应 C# `SerialLane`）。
//! 这里再起一个轻量线程定时 `idle()`，让重试、超时、watch 这类"时间驱动"的事情有机会跑。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use snug_core::desk::{AppControl, Desk, DeskEvents, DeskOptions, DeskState};
use snug_core::windows::{AppInfo, WindowInfo};
use snug_core::{AppConfig, Paths, ScreenFit};
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

        let desk = Desk::new(displays, windows, apps, paths, options);
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

        Self { desk, paths: paths.clone(), running, worker }
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
