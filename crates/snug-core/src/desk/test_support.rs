//! PhoneDesk 测试用的假平台，对应 C# `PhoneDeskTests` 里的 `FakeDisplays` /
//! `FakeWindows` / `FakeApps` / `FakeTimeline`，行为照抄（包括假时间线的"睡多久就把
//! 时钟拨多久"）。

#![allow(dead_code)] // 测试脚手架：假平台按 C# 接口全实现，分块移植时逐步用到

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::path::PathBuf;

use crate::desk::engine::{Desk, DeskEvents, DeskOptions};
use crate::desk::model::{DeskGone, DeskState};
use crate::desk::services::{
    AppControl, CancelToken, PhoneDisplays, PhoneScreenErrorKind, PhoneScreenException, Timeline,
    WindowControl, WindowLook,
};
use crate::geometry::{Rect, Size};
use crate::paths::Paths;
use crate::screen_fit::ScreenFit;
use crate::windows::{AppInfo, DisplayModeInfo, OtherDisplay, WindowInfo, WindowKey};

static NEXT_HARNESS: AtomicUsize = AtomicUsize::new(0);

/// 假时间线：`sleep` 直接把 `now` 往前拨（C# FakeTimeline 一模一样，也不看取消令牌）。
#[derive(Debug, Default)]
pub struct FakeTimeline {
    now: Mutex<f64>,
}

impl FakeTimeline {
    pub fn now(&self) -> f64 {
        *self.now.lock().expect("timeline")
    }

    pub fn advance(&self, seconds: f64) {
        *self.now.lock().expect("timeline") += seconds;
    }
}

impl Timeline for FakeTimeline {
    fn now(&self) -> f64 {
        FakeTimeline::now(self)
    }

    fn sleep(&self, seconds: f64, _cancel: &CancelToken) {
        self.advance(seconds.max(0.0));
    }
}

/// 假显示器服务，对应 C# `FakeDisplays`。
pub struct FakeDisplays {
    active: Mutex<Option<u32>>,
    width: Mutex<i32>,
    fit: Mutex<Option<ScreenFit>>,
    others: Mutex<Vec<OtherDisplay>>,
    fail_creates: AtomicUsize,
    create_calls: AtomicUsize,
    retire_calls: AtomicUsize,
    keep_online_after_retire: AtomicBool,
    mode_sets: Mutex<Vec<(u32, DisplayModeInfo, bool)>>,
    drift_on_create: Mutex<Option<(u32, DisplayModeInfo)>>,
}

impl Default for FakeDisplays {
    fn default() -> Self {
        Self {
            active: Mutex::new(None),
            width: Mutex::new(540),
            fit: Mutex::new(None),
            others: Mutex::new(Vec::new()),
            fail_creates: AtomicUsize::new(0),
            create_calls: AtomicUsize::new(0),
            retire_calls: AtomicUsize::new(0),
            keep_online_after_retire: AtomicBool::new(false),
            mode_sets: Mutex::new(Vec::new()),
            drift_on_create: Mutex::new(None),
        }
    }
}

impl FakeDisplays {
    pub fn fail_creates(&self, times: usize) {
        self.fail_creates.store(times, Ordering::SeqCst);
    }

    pub fn create_calls(&self) -> usize {
        self.create_calls.load(Ordering::SeqCst)
    }

    pub fn retire_calls(&self) -> usize {
        self.retire_calls.load(Ordering::SeqCst)
    }

    pub fn keep_online_after_retire(&self) {
        self.keep_online_after_retire.store(true, Ordering::SeqCst);
    }

    pub fn bring_offline(&self) {
        *self.active.lock().expect("active") = None;
        self.keep_online_after_retire.store(false, Ordering::SeqCst);
    }

    pub fn mode_sets(&self) -> Vec<(u32, DisplayModeInfo, bool)> {
        self.mode_sets.lock().expect("mode sets").clone()
    }

    /// 登记一块"别的屏"，返回它原来的模式。
    pub fn push_other_display(&self, display_id: u32, friendly_name: &str) -> DisplayModeInfo {
        let mode =
            DisplayModeInfo { width: 1920, height: 1080, refresh_rate: 60.0, dpi_scale_percent: 100 };
        self.others.lock().expect("others").push(OtherDisplay {
            display_id,
            device_name: format!("\\\\.\\DISPLAY{display_id}"),
            friendly_name: friendly_name.to_string(),
            mode,
            asleep: false,
        });
        mode
    }

    /// 模拟系统在建屏时顺手改了别的屏模式（C# 测试里用真实 CCD 才会发生的行为）。
    pub fn drift_others_on_create(&self, display_id: u32, width: i32, height: i32, refresh: f64) {
        let mode = DisplayModeInfo {
            width,
            height,
            refresh_rate: refresh,
            dpi_scale_percent: 100,
        };
        *self.drift_on_create.lock().expect("drift") = Some((display_id, mode));
    }

    fn phone_bounds(&self) -> Rect {
        let width = *self.width.lock().expect("width");
        let fit = self.fit.lock().expect("fit").clone().unwrap_or_else(ScreenFit::standard);
        let size: Size = fit.size(width);
        Rect::new(2560.0, 0.0, width as f64 * 2.0, size.height * 2.0)
    }
}

impl PhoneDisplays for FakeDisplays {
    fn create(&self, fit: ScreenFit) -> Result<u32, PhoneScreenException> {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_creates.load(Ordering::SeqCst) > 0 {
            self.fail_creates.fetch_sub(1, Ordering::SeqCst);
            return Err(PhoneScreenException::new(PhoneScreenErrorKind::CreateFailed));
        }
        *self.active.lock().expect("active") = Some(1001);
        *self.fit.lock().expect("fit") = Some(fit.clone());
        *self.width.lock().expect("width") = fit.base_width();

        if let Some((id, mode)) = self.drift_on_create.lock().expect("drift").take() {
            if let Some(other) = self
                .others
                .lock()
                .expect("others")
                .iter_mut()
                .find(|other| other.display_id == id)
            {
                other.mode = mode;
            }
        }
        Ok(1001)
    }

    fn reshape(&self, display_id: u32, fit: ScreenFit) -> bool {
        if self.active.lock().expect("active").as_ref() != Some(&display_id) {
            return false;
        }
        *self.width.lock().expect("width") = fit.base_width();
        *self.fit.lock().expect("fit") = Some(fit);
        true
    }

    fn fit_of(&self, display_id: u32) -> Option<ScreenFit> {
        if self.active.lock().expect("active").as_ref() == Some(&display_id) {
            self.fit.lock().expect("fit").clone()
        } else {
            None
        }
    }

    fn retire(&self, display_id: u32) -> bool {
        self.retire_calls.fetch_add(1, Ordering::SeqCst);
        if self.active.lock().expect("active").as_ref() != Some(&display_id) {
            return true;
        }
        if self.keep_online_after_retire.load(Ordering::SeqCst) {
            return false;
        }
        *self.active.lock().expect("active") = None;
        true
    }

    fn is_online(&self, display_id: u32) -> bool {
        self.active.lock().expect("active").as_ref() == Some(&display_id)
    }

    fn width_of(&self, display_id: u32) -> Option<i32> {
        if self.active.lock().expect("active").as_ref() == Some(&display_id) {
            Some(*self.width.lock().expect("width"))
        } else {
            None
        }
    }

    fn set_width(&self, display_id: u32, width: i32) {
        if self.active.lock().expect("active").as_ref() == Some(&display_id) {
            *self.width.lock().expect("width") = width;
        }
    }

    fn bounds_of(&self, display_id: u32) -> Option<Rect> {
        if self.active.lock().expect("active").as_ref() == Some(&display_id) {
            Some(self.phone_bounds())
        } else {
            None
        }
    }

    fn window_area_of(&self, display_id: u32) -> Option<Rect> {
        if self.active.lock().expect("active").as_ref() != Some(&display_id) {
            return None;
        }
        let bounds = self.phone_bounds();
        let width = *self.width.lock().expect("width");
        let bar = ScreenFit::home_bar_height(width as f64) * 2.0;
        Some(Rect::new(bounds.x, bounds.y, bounds.width, bounds.height - bar))
    }

    fn main_area(&self) -> Option<Rect> {
        Some(Rect::new(0.0, 0.0, 1920.0, 1040.0))
    }

    fn other_displays(&self) -> Vec<OtherDisplay> {
        self.others.lock().expect("others").clone()
    }

    fn set_mode(&self, display_id: u32, mode: DisplayModeInfo, permanently: bool) {
        self.mode_sets.lock().expect("mode sets").push((display_id, mode, permanently));
        if let Some(other) = self
            .others
            .lock()
            .expect("others")
            .iter_mut()
            .find(|other| other.display_id == display_id)
        {
            other.mode = mode;
        }
    }
}

/// 假窗口服务，对应 C# `FakeWindows`（并补上 C# 测试里没用到的"列出窗口"能力）。
pub struct FakeWindows {
    frames: Mutex<HashMap<WindowKey, Rect>>,
    infos: Mutex<HashMap<WindowKey, WindowInfo>>,
    asking: Mutex<Option<WindowInfo>>,
    zoom_delta_total: AtomicI32,
    min_logical_width: Mutex<f64>,
    zoom_ok: AtomicBool,
    restore_calls: Mutex<Vec<(HashMap<WindowKey, Rect>, Option<WindowKey>)>>,
    full_screen_calls: Mutex<Vec<(WindowKey, bool)>>,
    minimized_calls: Mutex<Vec<(WindowKey, bool)>>,
    full_screen_state: Mutex<HashMap<WindowKey, bool>>,
    minimized_state: Mutex<HashMap<WindowKey, bool>>,
}

impl Default for FakeWindows {
    fn default() -> Self {
        Self {
            frames: Mutex::new(HashMap::new()),
            infos: Mutex::new(HashMap::new()),
            asking: Mutex::new(None),
            zoom_delta_total: AtomicI32::new(0),
            min_logical_width: Mutex::new(0.0),
            zoom_ok: AtomicBool::new(true),
            restore_calls: Mutex::new(Vec::new()),
            full_screen_calls: Mutex::new(Vec::new()),
            minimized_calls: Mutex::new(Vec::new()),
            full_screen_state: Mutex::new(HashMap::new()),
            minimized_state: Mutex::new(HashMap::new()),
        }
    }
}

impl FakeWindows {
    pub fn insert(&self, key: WindowKey, frame: Rect) {
        self.frames.lock().expect("frames").insert(key, frame);
    }

    /// 连窗口信息一起登记（`shown_windows` 会返回它）。
    pub fn insert_window(&self, info: WindowInfo, frame: Rect) {
        self.frames.lock().expect("frames").insert(info.key, frame);
        self.infos.lock().expect("infos").insert(info.key, info);
    }

    /// 模拟"某个进程正在弹询问窗口"。
    pub fn set_asking(&self, window: Option<WindowInfo>) {
        *self.asking.lock().expect("asking") = window;
    }

    pub fn remove(&self, key: WindowKey) {
        self.frames.lock().expect("frames").remove(&key);
    }

    pub fn frame_of(&self, key: WindowKey) -> Option<Rect> {
        self.frames.lock().expect("frames").get(&key).copied()
    }

    pub fn frame_count(&self) -> usize {
        self.frames.lock().expect("frames").len()
    }

    pub fn zoom_delta_total(&self) -> i32 {
        self.zoom_delta_total.load(Ordering::SeqCst)
    }

    pub fn set_zoom_ok(&self, ok: bool) {
        self.zoom_ok.store(ok, Ordering::SeqCst);
    }

    /// C# 测试里 `FakeWindows.MinLogicalWidth` 用来模拟"窗口最小宽度把 540 撑到 700"。
    pub fn set_min_logical_width(&self, width: f64) {
        *self.min_logical_width.lock().expect("min width") = width;
    }

    pub fn restore_calls(&self) -> Vec<(HashMap<WindowKey, Rect>, Option<WindowKey>)> {
        self.restore_calls.lock().expect("restore calls").clone()
    }

    pub fn full_screen_calls(&self) -> Vec<(WindowKey, bool)> {
        self.full_screen_calls.lock().expect("fullscreen calls").clone()
    }

    pub fn minimized_calls(&self) -> Vec<(WindowKey, bool)> {
        self.minimized_calls.lock().expect("minimized calls").clone()
    }

    /// 让窗口在 look() 里报告为最大化 / 最小化（模拟窗口本来就是这个状态）。
    pub fn mark_full_screen(&self, window: WindowKey) {
        self.full_screen_state.lock().expect("fullscreen state").insert(window, true);
    }

    pub fn mark_minimized(&self, window: WindowKey) {
        self.minimized_state.lock().expect("minimized state").insert(window, true);
    }
}

impl WindowControl for FakeWindows {
    fn exists(&self, window: WindowKey) -> bool {
        self.frames.lock().expect("frames").contains_key(&window)
    }

    fn is_shown(&self, window: WindowKey) -> bool {
        self.exists(window)
    }

    fn look(&self, window: WindowKey) -> Option<WindowLook> {
        let frame = self.frame_of(window)?;
        Some(WindowLook {
            frame,
            full_screen: *self.full_screen_state.lock().expect("fullscreen state").get(&window).unwrap_or(&false),
            minimized: *self.minimized_state.lock().expect("minimized state").get(&window).unwrap_or(&false),
        })
    }

    fn move_window(&self, window: WindowKey, frame: Rect) -> Option<Rect> {
        let mut frames = self.frames.lock().expect("frames");
        frames.get(&window)?;
        let min = *self.min_logical_width.lock().expect("min width");
        let width = frame.width.max(min * 2.0);
        let got = Rect::new(frame.x, frame.y, width, frame.height);
        frames.insert(window, got);
        Some(got)
    }

    fn set_full_screen(&self, window: WindowKey, on: bool) {
        self.full_screen_calls.lock().expect("fullscreen calls").push((window, on));
        self.full_screen_state.lock().expect("fullscreen state").insert(window, on);
    }

    fn set_minimized(&self, window: WindowKey, on: bool) {
        self.minimized_calls.lock().expect("minimized calls").push((window, on));
        self.minimized_state.lock().expect("minimized state").insert(window, on);
    }

    fn focus(&self, _window: WindowKey) {}

    fn zoom(&self, _window: WindowKey, steps: i32) -> bool {
        if !self.zoom_ok.load(Ordering::SeqCst) {
            return false;
        }
        self.zoom_delta_total.fetch_add(steps, Ordering::SeqCst);
        true
    }

    fn shown_windows(&self, pid: i32) -> Vec<WindowInfo> {
        let frames = self.frames.lock().expect("frames");
        self.infos
            .lock()
            .expect("infos")
            .values()
            .filter(|info| info.key.pid == pid && frames.contains_key(&info.key))
            .cloned()
            .collect()
    }

    fn asking_window(&self, pid: i32) -> Option<WindowInfo> {
        self.asking
            .lock()
            .expect("asking")
            .as_ref()
            .filter(|info| info.key.pid == pid)
            .cloned()
    }

    fn close(&self, window: WindowKey) -> bool {
        self.infos.lock().expect("infos").remove(&window);
        self.frames.lock().expect("frames").remove(&window).is_some()
    }

    fn close_button(&self, _window: WindowKey) -> Option<Rect> {
        None
    }

    fn frames(&self, _pid: i32) -> HashMap<WindowKey, Rect> {
        self.frames.lock().expect("frames").clone()
    }

    fn snapshot(&self) -> HashMap<WindowKey, Rect> {
        self.frames.lock().expect("frames").clone()
    }

    fn restore(&self, snapshot: &HashMap<WindowKey, Rect>, except: Option<WindowKey>) {
        self.restore_calls
            .lock()
            .expect("restore calls")
            .push((snapshot.clone(), except));
    }
}

/// 假 App 服务，对应 C# `FakeApps`（补上可配置的打开 / 退出结果）。
pub struct FakeApps {
    /// 没登记的 pid 视为"在运行"（C# FakeApps 恒为 true）。
    running: Mutex<HashMap<i32, bool>>,
    quit_result: AtomicBool,
    /// quit 成功但进程还在（模拟"请它退出了，用户没确认"）。
    quit_keeps_running: AtomicBool,
    force_quit_result: AtomicBool,
    open_result: Mutex<Option<i32>>,
    new_window_result: AtomicBool,
    pids_by_app: Mutex<HashMap<String, Vec<i32>>>,
    windows_by_app: Mutex<HashMap<String, Vec<WindowInfo>>>,
    /// 新建窗口时回调（测试用它模拟"新窗口出现了"）。
    on_new_window: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Default for FakeApps {
    fn default() -> Self {
        Self {
            running: Mutex::new(HashMap::new()),
            quit_result: AtomicBool::new(true),
            quit_keeps_running: AtomicBool::new(false),
            force_quit_result: AtomicBool::new(true),
            open_result: Mutex::new(None),
            new_window_result: AtomicBool::new(false),
            pids_by_app: Mutex::new(HashMap::new()),
            windows_by_app: Mutex::new(HashMap::new()),
            on_new_window: Mutex::new(None),
        }
    }
}

impl FakeApps {
    pub fn set_running(&self, pid: i32, running: bool) {
        self.running.lock().expect("running").insert(pid, running);
    }

    pub fn set_quit_result(&self, ok: bool) {
        self.quit_result.store(ok, Ordering::SeqCst);
    }

    /// quit 返回成功但进程继续跑（模拟用户还没确认退出）。
    pub fn set_quit_keeps_running(&self, keeps_running: bool) {
        self.quit_keeps_running.store(keeps_running, Ordering::SeqCst);
    }

    pub fn set_force_quit_result(&self, ok: bool) {
        self.force_quit_result.store(ok, Ordering::SeqCst);
    }

    pub fn set_open_result(&self, pid: Option<i32>) {
        *self.open_result.lock().expect("open result") = pid;
    }

    pub fn set_new_window_result(&self, ok: bool) {
        self.new_window_result.store(ok, Ordering::SeqCst);
    }

    pub fn set_pids(&self, app_id: &str, pids: Vec<i32>) {
        self.pids_by_app.lock().expect("pids").insert(app_id.to_string(), pids);
    }

    pub fn set_app_windows(&self, app_id: &str, windows: Vec<WindowInfo>) {
        self.windows_by_app.lock().expect("app windows").insert(app_id.to_string(), windows);
    }

    pub fn set_on_new_window(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_new_window.lock().expect("hook") = Some(Arc::new(hook));
    }
}

impl AppControl for FakeApps {
    fn pid_of(&self, _app: &AppInfo) -> Option<i32> {
        None
    }

    fn pids(&self, app_id: &str) -> Vec<i32> {
        self.pids_by_app.lock().expect("pids").get(app_id).cloned().unwrap_or_default()
    }

    fn open(&self, _app: &AppInfo) -> Option<i32> {
        *self.open_result.lock().expect("open result")
    }

    fn new_window(&self, _pid: i32) -> bool {
        let hook = self.on_new_window.lock().expect("hook").clone();
        if let Some(hook) = hook {
            hook();
        }
        self.new_window_result.load(Ordering::SeqCst)
    }

    fn quit(&self, pid: i32) -> bool {
        let ok = self.quit_result.load(Ordering::SeqCst);
        if ok && !self.quit_keeps_running.load(Ordering::SeqCst) {
            self.running.lock().expect("running").insert(pid, false);
        }
        ok
    }

    fn force_quit(&self, pid: i32) -> bool {
        let ok = self.force_quit_result.load(Ordering::SeqCst);
        if ok {
            self.running.lock().expect("running").insert(pid, false);
        }
        ok
    }

    fn is_running(&self, pid: i32) -> bool {
        self.running.lock().expect("running").get(&pid).copied().unwrap_or(true)
    }

    fn app_windows(&self, app: &AppInfo, _launched_pid: Option<i32>) -> Vec<WindowInfo> {
        self.windows_by_app
            .lock()
            .expect("app windows")
            .get(&app.app_id)
            .cloned()
            .unwrap_or_default()
    }
}

/// 一套假平台 + 临时目录 + 桌面实例，供各分块的测试复用。
pub struct Harness {
    pub desk: Desk,
    pub displays: Arc<FakeDisplays>,
    pub windows: Arc<FakeWindows>,
    pub apps: Arc<FakeApps>,
    pub timeline: Arc<FakeTimeline>,
    pub paths: Paths,
    pub dir: PathBuf,
    states: Arc<Mutex<Vec<DeskState>>>,
    gones: Arc<Mutex<Vec<DeskGone>>>,
}

impl Harness {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "snug-desk-{}-{}",
            std::process::id(),
            NEXT_HARNESS.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::with_data_dir(dir.clone());

        let displays = Arc::new(FakeDisplays::default());
        let windows = Arc::new(FakeWindows::default());
        let apps = Arc::new(FakeApps::default());
        let timeline = Arc::new(FakeTimeline::default());

        let states = Arc::new(Mutex::new(Vec::<DeskState>::new()));
        let sink = Arc::clone(&states);
        let gones = Arc::new(Mutex::new(Vec::<DeskGone>::new()));
        let gone_sink = Arc::clone(&gones);
        let events = DeskEvents {
            state_changed: Some(Arc::new(move |state: &DeskState| {
                sink.lock().expect("states").push(state.clone());
            })),
            gone: Some(Arc::new(move |gone: &DeskGone| {
                gone_sink.lock().expect("gones").push(gone.clone());
            })),
            ..DeskEvents::default()
        };

        let desk = Desk::new(
            Arc::clone(&displays) as Arc<dyn PhoneDisplays>,
            Arc::clone(&windows) as Arc<dyn WindowControl>,
            Arc::clone(&apps) as Arc<dyn AppControl>,
            &paths,
            DeskOptions {
                timeline: Some(Arc::clone(&timeline) as Arc<dyn Timeline>),
                events,
                ..DeskOptions::default()
            },
        );

        Self { desk, displays, windows, apps, timeline, paths, dir, states, gones }
    }

    pub fn states(&self) -> Vec<DeskState> {
        self.states.lock().expect("states").clone()
    }

    pub fn gones(&self) -> Vec<DeskGone> {
        self.gones.lock().expect("gones").clone()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
