//! PhoneDesk 测试用的假平台，对应 C# `PhoneDeskTests` 里的 `FakeDisplays` /
//! `FakeWindows` / `FakeApps` / `FakeTimeline`，行为照抄（包括假时间线的"睡多久就把
//! 时钟拨多久"）。

#![allow(dead_code)] // 测试脚手架：假平台按 C# 接口全实现，分块移植时逐步用到

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::path::PathBuf;
use std::time::Duration;

use crate::desk::engine::{Desk, DeskEvents, DeskOptions};
use crate::desk::model::{DeskGone, DeskState};
use crate::desk::services::{
    AppControl, CancelToken, PhoneDisplays, PhoneScreenErrorKind, PhoneScreenException, Timeline,
    WindowControl, WindowLook,
};
use crate::desk::set::{DeskSet, DeskSetEvents, DeskSetOptions};
use crate::geometry::{Rect, Size};
use crate::paths::Paths;
use crate::screen_fit::{PhoneSpec, ScreenFit};
use crate::screen_plan::{ScreenBehavior, ScreenWish};
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
    width_sets: Mutex<Vec<i32>>,
    fit: Mutex<Option<ScreenFit>>,
    others: Mutex<Vec<OtherDisplay>>,
    fail_creates: AtomicUsize,
    fail_after_online: AtomicBool,
    fail_reshapes: AtomicUsize,
    reshape_calls: AtomicUsize,
    create_calls: AtomicUsize,
    create_names: Mutex<Vec<String>>,
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
            width_sets: Mutex::new(Vec::new()),
            fit: Mutex::new(None),
            others: Mutex::new(Vec::new()),
            fail_creates: AtomicUsize::new(0),
            fail_after_online: AtomicBool::new(false),
            fail_reshapes: AtomicUsize::new(0),
            reshape_calls: AtomicUsize::new(0),
            create_calls: AtomicUsize::new(0),
            create_names: Mutex::new(Vec::new()),
            retire_calls: AtomicUsize::new(0),
            keep_online_after_retire: AtomicBool::new(false),
            mode_sets: Mutex::new(Vec::new()),
            drift_on_create: Mutex::new(None),
        }
    }
}

impl FakeDisplays {
    pub fn width_sets(&self) -> Vec<i32> {
        self.width_sets.lock().expect("width sets").clone()
    }

    pub fn fail_creates(&self, times: usize) {
        self.fail_creates.store(times, Ordering::SeqCst);
    }

    pub fn fail_after_online(&self) {
        self.fail_after_online.store(true, Ordering::SeqCst);
        self.keep_online_after_retire();
    }

    pub fn fail_reshapes(&self, times: usize) {
        self.fail_reshapes.store(times, Ordering::SeqCst);
    }

    pub fn reshape_calls(&self) -> usize {
        self.reshape_calls.load(Ordering::SeqCst)
    }

    pub fn create_calls(&self) -> usize {
        self.create_calls.load(Ordering::SeqCst)
    }

    pub fn create_names(&self) -> Vec<String> {
        self.create_names.lock().expect("create names").clone()
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
    fn create_named(&self, fit: ScreenFit, name: &str) -> Result<u32, PhoneScreenException> {
        self.create_names.lock().expect("create names").push(name.to_string());
        self.create(fit)
    }

    fn create(&self, fit: ScreenFit) -> Result<u32, PhoneScreenException> {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_creates.load(Ordering::SeqCst) > 0 {
            self.fail_creates.fetch_sub(1, Ordering::SeqCst);
            return Err(PhoneScreenException::new(
                PhoneScreenErrorKind::CreateFailed,
            ));
        }
        *self.active.lock().expect("active") = Some(1001);
        *self.fit.lock().expect("fit") = Some(fit.clone());
        *self.width.lock().expect("width") = fit.base_width();

        if self.fail_after_online.swap(false, Ordering::SeqCst) {
            return Err(
                PhoneScreenException::new(PhoneScreenErrorKind::CreateFailed)
                    .with_pending_display(1001),
            );
        }

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
        self.reshape_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_reshapes.load(Ordering::SeqCst) > 0 {
            self.fail_reshapes.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
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
        self.width_sets.lock().expect("width sets").push(width);
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

type CloseButtonHook = Arc<dyn Fn(WindowKey) -> Option<Rect> + Send + Sync>;

/// 假窗口服务，对应 C# `FakeWindows`（并补上 C# 测试里没用到的"列出窗口"能力）。
pub struct FakeWindows {
    frames: Mutex<HashMap<WindowKey, Rect>>,
    infos: Mutex<HashMap<WindowKey, WindowInfo>>,
    asking: Mutex<Option<WindowInfo>>,
    zoom_delta_total: AtomicI32,
    min_logical_width: Mutex<f64>,
    misplaced_moves_remaining: Mutex<usize>,
    move_calls: Mutex<Vec<(WindowKey, Rect)>>,
    zoom_ok: AtomicBool,
    restore_calls: Mutex<Vec<(HashMap<WindowKey, Rect>, Option<WindowKey>)>>,
    full_screen_calls: Mutex<Vec<(WindowKey, bool)>>,
    minimized_calls: Mutex<Vec<(WindowKey, bool)>>,
    full_screen_state: Mutex<HashMap<WindowKey, bool>>,
    minimized_state: Mutex<HashMap<WindowKey, bool>>,
    close_button_hook: Mutex<Option<CloseButtonHook>>,
}

impl Default for FakeWindows {
    fn default() -> Self {
        Self {
            frames: Mutex::new(HashMap::new()),
            infos: Mutex::new(HashMap::new()),
            asking: Mutex::new(None),
            zoom_delta_total: AtomicI32::new(0),
            min_logical_width: Mutex::new(0.0),
            misplaced_moves_remaining: Mutex::new(0),
            move_calls: Mutex::new(Vec::new()),
            zoom_ok: AtomicBool::new(true),
            restore_calls: Mutex::new(Vec::new()),
            full_screen_calls: Mutex::new(Vec::new()),
            minimized_calls: Mutex::new(Vec::new()),
            full_screen_state: Mutex::new(HashMap::new()),
            minimized_state: Mutex::new(HashMap::new()),
            close_button_hook: Mutex::new(None),
        }
    }
}

impl FakeWindows {
    pub fn on_close_button(&self, hook: impl Fn(WindowKey) -> Option<Rect> + Send + Sync + 'static) {
        *self.close_button_hook.lock().expect("close button hook") = Some(Arc::new(hook));
    }

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

    /// 让接下来的若干次窗口移动表面成功、但仍留在主屏。
    pub fn misplace_moves(&self, count: usize) {
        *self.misplaced_moves_remaining.lock().expect("misplaced moves") = count;
    }

    pub fn move_calls(&self) -> Vec<(WindowKey, Rect)> {
        self.move_calls.lock().expect("move calls").clone()
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
        self.move_calls.lock().expect("move calls").push((window, frame));
        let mut remaining = self.misplaced_moves_remaining.lock().expect("misplaced moves");
        if *remaining > 0 {
            *remaining -= 1;
            let misplaced = Rect::new(0.0, 0.0, frame.width, frame.height);
            frames.insert(window, misplaced);
            return Some(misplaced);
        }
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

    fn close_button(&self, window: WindowKey) -> Option<Rect> {
        let hook = self.close_button_hook.lock().expect("close button hook").clone();
        hook.and_then(|hook| hook(window))
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

/// 多屏版假显示器：每块屏一个独立 id，并记录「两次 `create` 有没有重叠」。
///
/// 单屏的 [`FakeDisplays`] 只认一个活动屏（现有单屏测试靠它），多屏测试用这个。
pub struct FakeMultiDisplays {
    next_id: AtomicU32,
    screens: Mutex<HashMap<u32, MultiScreen>>,
    create_calls: Mutex<Vec<u32>>,
    in_create: AtomicBool,
    overlapped: AtomicBool,
    create_delay_micros: AtomicU64,
    fail_creates: AtomicUsize,
    fail_retires: AtomicUsize,
    retire_calls: AtomicUsize,
    main: Rect,
}

struct MultiScreen {
    fit: ScreenFit,
    width: i32,
    bounds: Rect,
}

impl Default for FakeMultiDisplays {
    fn default() -> Self {
        Self {
            next_id: AtomicU32::new(2001),
            screens: Mutex::new(HashMap::new()),
            create_calls: Mutex::new(Vec::new()),
            in_create: AtomicBool::new(false),
            overlapped: AtomicBool::new(false),
            create_delay_micros: AtomicU64::new(0),
            fail_creates: AtomicUsize::new(0),
            fail_retires: AtomicUsize::new(0),
            retire_calls: AtomicUsize::new(0),
            main: Rect::new(0.0, 0.0, 1920.0, 1040.0),
        }
    }
}

impl FakeMultiDisplays {
    pub fn fail_retires(&self, times: usize) {
        self.fail_retires.store(times, Ordering::SeqCst);
    }

    pub fn retire_calls(&self) -> usize {
        self.retire_calls.load(Ordering::SeqCst)
    }

    pub fn fail_creates(&self, times: usize) {
        self.fail_creates.store(times, Ordering::SeqCst);
    }

    /// 让每次建屏在 `create` 里停一会儿，好暴露"两个 desk 同时建屏"。
    pub fn slow_creates(&self, micros: u64) {
        self.create_delay_micros.store(micros, Ordering::SeqCst);
    }

    pub fn create_calls(&self) -> Vec<u32> {
        self.create_calls.lock().expect("create calls").clone()
    }

    /// 是否观察到两次 `create` 重叠（共享队列生效时应当永远为 false）。
    pub fn overlap_detected(&self) -> bool {
        self.overlapped.load(Ordering::SeqCst)
    }

    pub fn screen_count(&self) -> usize {
        self.screens.lock().expect("screens").len()
    }

    fn screen_bounds(&self, fit: &ScreenFit) -> Rect {
        let index = self.screens.lock().expect("screens").len() as f64;
        let width = fit.base_width() as f64;
        let size: Size = fit.size(fit.base_width());
        // 虚拟屏互相不重叠地排在主屏右边。
        Rect::new(2560.0 + index * 1600.0, 0.0, width * 2.0, size.height * 2.0)
    }
}

impl PhoneDisplays for FakeMultiDisplays {
    fn create(&self, fit: ScreenFit) -> Result<u32, PhoneScreenException> {
        if self.in_create.swap(true, Ordering::SeqCst) {
            self.overlapped.store(true, Ordering::SeqCst);
        }
        let delay = self.create_delay_micros.load(Ordering::SeqCst);
        if delay > 0 {
            std::thread::sleep(Duration::from_micros(delay));
        }
        if self.fail_creates.load(Ordering::SeqCst) > 0 {
            self.fail_creates.fetch_sub(1, Ordering::SeqCst);
            self.in_create.store(false, Ordering::SeqCst);
            return Err(PhoneScreenException::new(PhoneScreenErrorKind::CreateFailed));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let width = fit.base_width();
        let bounds = self.screen_bounds(&fit);
        self.screens
            .lock()
            .expect("screens")
            .insert(id, MultiScreen { fit, width, bounds });
        self.create_calls.lock().expect("create calls").push(id);
        self.in_create.store(false, Ordering::SeqCst);
        Ok(id)
    }

    fn reshape(&self, display_id: u32, fit: ScreenFit) -> bool {
        let mut screens = self.screens.lock().expect("screens");
        let Some(screen) = screens.get_mut(&display_id) else {
            return false;
        };
        screen.width = fit.base_width();
        screen.fit = fit;
        true
    }

    fn fit_of(&self, display_id: u32) -> Option<ScreenFit> {
        self.screens.lock().expect("screens").get(&display_id).map(|screen| screen.fit.clone())
    }

    fn retire(&self, display_id: u32) -> bool {
        self.retire_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_retires.load(Ordering::SeqCst) > 0 {
            self.fail_retires.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        self.screens.lock().expect("screens").remove(&display_id).is_some()
    }

    fn is_online(&self, display_id: u32) -> bool {
        self.screens.lock().expect("screens").contains_key(&display_id)
    }

    fn width_of(&self, display_id: u32) -> Option<i32> {
        self.screens.lock().expect("screens").get(&display_id).map(|screen| screen.width)
    }

    fn set_width(&self, display_id: u32, width: i32) {
        if let Some(screen) = self.screens.lock().expect("screens").get_mut(&display_id) {
            screen.width = width;
        }
    }

    fn bounds_of(&self, display_id: u32) -> Option<Rect> {
        self.screens.lock().expect("screens").get(&display_id).map(|screen| screen.bounds)
    }

    fn window_area_of(&self, display_id: u32) -> Option<Rect> {
        let screens = self.screens.lock().expect("screens");
        let screen = screens.get(&display_id)?;
        let bar = ScreenFit::home_bar_height(screen.width as f64) * 2.0;
        Some(Rect::new(
            screen.bounds.x,
            screen.bounds.y,
            screen.bounds.width,
            screen.bounds.height - bar,
        ))
    }

    fn main_area(&self) -> Option<Rect> {
        Some(self.main)
    }

    fn other_displays(&self) -> Vec<OtherDisplay> {
        Vec::new()
    }

    fn set_mode(&self, _display_id: u32, _mode: DisplayModeInfo, _permanently: bool) {}
}

/// 多屏测试脚手架：一套假平台 + 临时目录 + 一个 [`DeskSet`]。
pub struct DeskSetHarness {
    pub set: DeskSet,
    pub displays: Arc<FakeMultiDisplays>,
    pub windows: Arc<FakeWindows>,
    pub apps: Arc<FakeApps>,
    pub timeline: Arc<FakeTimeline>,
    pub paths: Paths,
    pub dir: PathBuf,
    state_changes: Arc<AtomicUsize>,
}

impl DeskSetHarness {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "snug-desk-set-{}-{}",
            std::process::id(),
            NEXT_HARNESS.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::with_data_dir(dir.clone());

        let displays = Arc::new(FakeMultiDisplays::default());
        let windows = Arc::new(FakeWindows::default());
        let apps = Arc::new(FakeApps::default());
        let timeline = Arc::new(FakeTimeline::default());

        let state_changes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&state_changes);
        let events = DeskSetEvents {
            state_changed: Some(Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            })),
            ..DeskSetEvents::default()
        };
        let set = DeskSet::new(
            Arc::clone(&displays) as Arc<dyn PhoneDisplays>,
            Arc::clone(&windows) as Arc<dyn WindowControl>,
            Arc::clone(&apps) as Arc<dyn AppControl>,
            &paths,
            DeskSetOptions {
                timeline: Some(Arc::clone(&timeline) as Arc<dyn Timeline>),
                events,
                ..DeskSetOptions::default()
            },
        );
        Self { set, displays, windows, apps, timeline, paths, dir, state_changes }
    }

    /// 一块触屏愿望（标准手机屏）。
    pub fn wish(&self, id: &str) -> ScreenWish {
        ScreenWish {
            id: id.to_string(),
            name: id.to_string(),
            device: crate::phone_models::PhoneChoice::standard(),
            orientation: crate::phone_models::ScreenOrientation::Portrait,
            fit: ScreenFit::standard(),
            behavior: ScreenBehavior::TouchScreen,
        }
    }

    /// 一块带指定比例的触屏愿望。
    pub fn wish_with_fit(&self, id: &str, fit: ScreenFit) -> ScreenWish {
        ScreenWish { fit, ..self.wish(id) }
    }

    /// 一块电脑副屏愿望（1080p 横屏）。
    pub fn computer_wish(&self, id: &str) -> ScreenWish {
        ScreenWish {
            id: id.to_string(),
            name: id.to_string(),
            device: crate::phone_models::PhoneChoice::standard_computer(),
            orientation: crate::phone_models::ScreenOrientation::Landscape,
            fit: ScreenFit::from_device(
                &PhoneSpec::new(1080.0, 1920.0, None),
                crate::phone_models::DeviceKind::Computer,
                crate::phone_models::ScreenOrientation::Landscape,
            ),
            behavior: ScreenBehavior::Computer,
        }
    }

    pub fn state_changes(&self) -> usize {
        self.state_changes.load(Ordering::SeqCst)
    }
}

impl Drop for DeskSetHarness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
