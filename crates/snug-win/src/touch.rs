//! 鼠标钩子与输入重放，移植自 C# 版 `SnugWin.Windows.Input.TouchHookService`。
//!
//! 分工与 C# 一致：
//! - `snug-core` 的 [`TouchTranslator`] 决定"这一下该放行还是吞掉、吞掉后做什么"；
//! - 这里只做三件事：装 `WH_MOUSE_LL` 低层鼠标钩子、把注入事件交给翻译器、把翻译出的
//!   指令重放成 `SendInput`。
//!
//! 重放为什么必须走独立线程：低层钩子回调里直接 `SendInput` 会被自己的钩子再次拦下，
//! 慢一点还会拖住整个系统的鼠标输入。C# 用"队列 + 重放线程"，这里照搬：
//! 回调只把动作塞进有界队列（满了就丢，和 C# `TryAdd` 一致），重放线程负责真正发送。
//! 惯性滚动也挂在同一个重放线程上——用 [`Receiver::recv_timeout`] 的超时代替 C# 的定时器。
//!
//! 自己注入的事件带 [`REPLAY_MARKER`]（ASCII "SNUG"）标记，回调里一眼认出并放行；
//! 另外还有 [`HookEngine::set_require_injected_flag`] 控制"只接管注入事件"。
//!
//! 手动验证（自动测试不会真的装钩子）：
//! 1. 用 `cargo run -p snug-app` 起程序并连上 UU 远程；
//! 2. 在手机端轻点手机屏 → 应该是一次正常的单击；
//! 3. 快速滑动 → 桌面滚动 + 松手后继续滑一小段（惯性）；
//! 4. 长按后松手 → 右键菜单；长按后拖动 → 选中文字；
//! 5. 钩子生效时任务栏图标上不该出现"卡鼠标"的拖影；拔掉手机屏后鼠标立即恢复正常。

use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use snug_core::scrolling::{Momentum, ScrollPhase, ScrollStep};
use snug_core::touch::{
    CloseButton, TouchCommand, TouchInput, TouchInputKind, TouchOptions, TouchRegion,
    TouchTranslator,
};
use snug_core::{Point, Rect};
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, GetSystemMetrics, PostThreadMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, MSG, MSLLHOOKSTRUCT, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WH_MOUSE_LL, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_QUIT, LLMHF_INJECTED,
};

use crate::window::{on_screen_z_order, WinDesktop, WindowApi};

/// 自己注入的事件带这个标记（C# `ReplayMarker`，ASCII "SNUG"）。
pub const REPLAY_MARKER: usize = 0x534E_5547;
/// 滚动像素 → 滚轮档位的换算（C# `WheelScaleFactor`：120/48 点，正好一比一跟手）。
pub const WHEEL_SCALE: f64 = 2.5;
/// 浏览器顶部工具栏的高度（点），随屏幕缩放放大（C# `120.0 * Math.Max(1.0, scale)`）。
pub const BROWSER_CHROME_POINTS: f64 = 120.0;
/// 重放队列上限（C# `BlockingCollection<Action>(256)`）。
pub const REPLAY_QUEUE: usize = 256;

/// 钩子消息 → 输入类型；不关心的事件返回 `None`（C# 里的 `switch` 默认分支）。
pub fn hook_input_kind(message: u32) -> Option<TouchInputKind> {
    match message {
        WM_LBUTTONDOWN => Some(TouchInputKind::Press),
        WM_MOUSEMOVE => Some(TouchInputKind::Drag),
        WM_LBUTTONUP => Some(TouchInputKind::Release),
        _ => None,
    }
}

/// 屏幕坐标 → `MOUSEEVENTF_ABSOLUTE` 要的 0..=65535（C# `ToVirtualDesktopAbsolute`）。
///
/// `desktop` 是整个虚拟桌面的矩形（多屏时可能带负原点）。
pub fn to_virtual_desktop_absolute(point: Point, desktop: Rect) -> (i32, i32) {
    let width = f64::max(1.0, desktop.width);
    let height = f64::max(1.0, desktop.height);
    let absolute_x = ((point.x - desktop.x) * 65535.0 / f64::max(1.0, width - 1.0)).round();
    let absolute_y = ((point.y - desktop.y) * 65535.0 / f64::max(1.0, height - 1.0)).round();
    (absolute_x as i32, absolute_y as i32)
}

/// 是不是常见的浏览器（C# 里那张 exe 名单，忽略大小写）。
pub fn is_browser_exe(exe: &str) -> bool {
    const BROWSERS: [&str; 6] =
        ["msedge.exe", "chrome.exe", "brave.exe", "opera.exe", "vivaldi.exe", "firefox.exe"];
    BROWSERS.iter().any(|name| name.eq_ignore_ascii_case(exe))
}

/// 落点是不是浏览器自己的地址栏/标签栏（那一片要直通，别翻译成手势）。
pub fn browser_chrome_hit(point: Point, window: Rect, scale: f64) -> bool {
    let chrome_height = BROWSER_CHROME_POINTS * f64::max(1.0, scale);
    point.y >= window.y && point.y <= window.y + chrome_height
}

/// 落在"复制气泡"上吗（C# `IsSelectionBubbleAt` 里判自身进程的那支）。
pub fn is_copy_bubble(window_pid: i32, title: &str, self_pid: i32) -> bool {
    window_pid == self_pid && title == "SnugCopyBubble"
}

/// 一次按下的落点判定结果，对应 C# `IsSelectionBubbleAt` 的两个 out 参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BubbleHit {
    /// 点在复制气泡上：直通，不要吞。
    pub on_bubble: bool,
    /// 点在浏览器工具栏上：直通。
    pub browser_chrome: bool,
}

/// 点关闭按钮时的两种走法（C# `HandleCloseTap`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseTap {
    /// 按钮确实在最前面 → 直接请上层关这个窗口。
    RequestClose(u64),
    /// 否则老老实实发一次点击。
    ReplayClick,
}

/// 判断这次点关闭按钮是不是真能点到（C# `CloseButton.IsFrontmost` 的调用点）。
pub fn close_tap_decision(
    button: Option<CloseButton>,
    at: Point,
    z_order: &[(u64, Rect)],
) -> CloseTap {
    match button {
        Some(button) if button.is_frontmost(at, z_order) => CloseTap::RequestClose(button.window_handle),
        _ => CloseTap::ReplayClick,
    }
}

/// 滚动累积器：C# 里的 `_scrollRemX/_scrollRemY`。
///
/// 触控位移是亚像素的，滚轮只能整档发，所以余数要留着下次一起发。
#[derive(Debug, Clone, Copy, Default)]
pub struct ScrollAccumulator {
    remainder_x: f64,
    remainder_y: f64,
}

/// 一次滚动要发出去的东西。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WheelShot {
    /// 竖直滚轮档位（负数向上）。
    pub dy: i32,
    /// 水平滚轮档位。
    pub dx: i32,
    /// 滚动开始时要先把光标挪到手指按下的位置（C# `SetCursorPos`）。
    pub cursor_to: Option<Point>,
}

impl ScrollAccumulator {
    /// 吃一步滚动，吐出这一次要发的滚轮量。
    pub fn take(&mut self, step: &ScrollStep) -> WheelShot {
        let mut cursor_to = None;
        if step.phase == ScrollPhase::Began {
            self.remainder_x = 0.0;
            self.remainder_y = 0.0;
            cursor_to = Some(step.location);
        }

        self.remainder_y += step.delta_y * WHEEL_SCALE;
        // C# 里横向取反：手指往左推 → 内容往左走。
        self.remainder_x += -step.delta_x * WHEEL_SCALE;
        let dy = self.remainder_y.trunc();
        let dx = self.remainder_x.trunc();
        self.remainder_y -= dy;
        self.remainder_x -= dx;

        WheelShot { dy: dy as i32, dx: dx as i32, cursor_to }
    }
}

// ---------------------------------------------------------------------------
// 钩子决策引擎（纯逻辑，不碰 Win32）
// ---------------------------------------------------------------------------

/// 一次钩子事件的判决结果。
#[derive(Debug, Clone, PartialEq)]
pub struct HookOutcome {
    /// 是否把原始事件继续传给后面的程序。
    pub pass_original: bool,
    /// 吞掉事件后要重放的指令。
    pub commands: Vec<TouchCommand>,
    /// 该让界面上那个复制气泡消失。
    pub dismiss_bubble: bool,
    /// 翻译器是否进入惯性滚动。
    pub coasting: bool,
    /// 本次按下（被吞掉的那次）的落点，重放时要用。
    pub press_origin: Point,
}

impl HookOutcome {
    fn pass() -> Self {
        Self {
            pass_original: true,
            commands: Vec::new(),
            dismiss_bubble: false,
            coasting: false,
            press_origin: Point::ZERO,
        }
    }
}

/// C# `HookCallback` 去掉 Win32 之后的部分：注入标记、放行判定、交给翻译器。
///
/// 时间由调用方给（`time`），所以整条判定链可以离线单测。
#[derive(Debug)]
pub struct HookEngine {
    region: Option<TouchRegion>,
    translator: TouchTranslator,
    options: TouchOptions,
    held_press_point: Option<Point>,
    passthrough_browser_chrome: bool,
    require_injected_flag: bool,
    logged_first_flag: bool,
}

impl Default for HookEngine {
    fn default() -> Self {
        Self {
            region: None,
            translator: TouchTranslator::default(),
            options: TouchOptions::default(),
            held_press_point: None,
            passthrough_browser_chrome: false,
            // C# 默认只接管注入事件（UU 远程模拟出来的鼠标）。
            require_injected_flag: true,
            logged_first_flag: false,
        }
    }
}

impl HookEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn region(&self) -> Option<TouchRegion> {
        self.region
    }

    pub fn set_region(&mut self, region: Option<TouchRegion>) {
        self.region = region;
    }

    pub fn options(&self) -> TouchOptions {
        self.options
    }

    pub fn set_options(&mut self, options: TouchOptions) {
        self.options = options;
    }

    pub fn require_injected_flag(&self) -> bool {
        self.require_injected_flag
    }

    /// 只在拿到真机"注入标记"证据前打开，之后可以放宽。
    pub fn set_require_injected_flag(&mut self, require: bool) {
        self.require_injected_flag = require;
    }

    /// 是否已经看到过第一次按下事件（C# 用来只打一条日志）。
    pub fn logged_first_press(&self) -> bool {
        self.logged_first_flag
    }

    pub fn is_coasting(&self) -> bool {
        self.translator.is_coasting()
    }

    /// 惯性滚动取一帧。
    pub fn momentum_frame(&mut self) -> Option<ScrollStep> {
        self.translator.momentum_frame()
    }

    /// 处理一次鼠标事件。
    ///
    /// `injected` 是 `LLMHF_INJECTED`，`replay_marked` 表示这是自己注入的；
    /// `bubble_at` 只在按下时调用，用来判断落点是不是复制气泡/浏览器工具栏。
    pub fn on_event(
        &mut self,
        kind: TouchInputKind,
        point: Point,
        time: f64,
        injected: bool,
        replay_marked: bool,
        bubble_at: impl Fn(Point, f64) -> BubbleHit,
    ) -> HookOutcome {
        // 自己注入的事件一律放行，否则会无限套娃。
        if replay_marked {
            return HookOutcome::pass();
        }
        let Some(region) = self.region else {
            return HookOutcome::pass();
        };

        if !self.logged_first_flag && kind == TouchInputKind::Press && region.frame.contains(point) {
            self.logged_first_flag = true;
        }

        if self.require_injected_flag && !injected {
            return HookOutcome::pass();
        }

        let mut dismiss_bubble = false;
        if kind == TouchInputKind::Press {
            let inside = region.frame.contains(point);
            let hit = if inside { bubble_at(point, region.scale) } else { BubbleHit::default() };
            dismiss_bubble = inside && !hit.on_bubble;
            self.passthrough_browser_chrome = inside && (hit.on_bubble || hit.browser_chrome);
        }
        if self.passthrough_browser_chrome {
            if kind == TouchInputKind::Release {
                self.passthrough_browser_chrome = false;
            }
            return HookOutcome::pass();
        }

        let input = TouchInput::new(kind, point.x, point.y, time);
        let response =
            self.translator
                .handle_with(input, Some(&region), self.options);

        if kind == TouchInputKind::Press && !response.pass_original {
            self.held_press_point = Some(point);
        }
        let press_origin = self.held_press_point.unwrap_or(Point::ZERO);
        if kind == TouchInputKind::Release {
            self.held_press_point = None;
        }

        HookOutcome {
            pass_original: response.pass_original,
            commands: response.commands,
            dismiss_bubble,
            coasting: self.translator.is_coasting(),
            press_origin,
        }
    }
}

// ---------------------------------------------------------------------------
// 输入重放
// ---------------------------------------------------------------------------

/// 一条要注入的鼠标动作。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Injection {
    MoveTo(Point),
    Button { at: Point, down: bool, right: bool },
    Wheel { steps: i32 },
    HScroll { steps: i32 },
    CursorTo(Point),
}

/// 重放线程要执行的一步。
#[derive(Debug, Clone, PartialEq)]
pub enum ReplayAction {
    Injection(Injection),
    /// 滚动要经过累积器，所以保留原始步进。
    Scroll(ScrollStep),
    CloseWindow { at: Point },
    SelectionCompleted { at: Point },
    Wait(Duration),
}

/// 指令 → 重放动作（C# `ExecuteCommand` 的纯逻辑部分）。
///
/// `current_kind` 是触发这批指令的那次事件类型：抬起时要发 up，拖动时发 move。
pub fn plan_replay(
    commands: &[TouchCommand],
    current_kind: TouchInputKind,
    press_origin: Point,
    current_point: Point,
) -> Vec<ReplayAction> {
    let mut actions = Vec::with_capacity(commands.len() + 2);
    let mut has_press = false;

    for command in commands {
        match command {
            TouchCommand::ReplayPress => {
                actions.push(ReplayAction::Injection(Injection::Button {
                    at: press_origin,
                    down: true,
                    right: false,
                }));
                has_press = true;
            }
            TouchCommand::ReplayCurrent => {
                // C#：先按下去再抬/拖，中间要留一点点时间给应用反应。
                if has_press {
                    let pause = if current_kind == TouchInputKind::Release {
                        Duration::from_millis(10)
                    } else {
                        Duration::from_millis(4)
                    };
                    actions.push(ReplayAction::Wait(pause));
                }
                match current_kind {
                    TouchInputKind::Release => actions.push(ReplayAction::Injection(
                        Injection::Button { at: current_point, down: false, right: false },
                    )),
                    TouchInputKind::Drag => {
                        actions.push(ReplayAction::Injection(Injection::MoveTo(current_point)))
                    }
                    TouchInputKind::Press => {}
                }
            }
            TouchCommand::RightClick { at } => {
                actions.push(ReplayAction::Injection(Injection::Button {
                    at: *at,
                    down: true,
                    right: true,
                }));
                actions.push(ReplayAction::Wait(Duration::from_millis(10)));
                actions.push(ReplayAction::Injection(Injection::Button {
                    at: *at,
                    down: false,
                    right: true,
                }));
            }
            TouchCommand::Scroll(step) => actions.push(ReplayAction::Scroll(*step)),
            TouchCommand::CloseWindow { at } => {
                actions.push(ReplayAction::CloseWindow { at: *at })
            }
            TouchCommand::SelectionCompleted { at } => {
                actions.push(ReplayAction::SelectionCompleted { at: *at })
            }
        }
    }
    actions
}

/// 注入器：真机是 [`WinInput`]，测试里换成记录用的假实现。
pub trait InputInjector: Send + Sync {
    fn apply(&self, injection: Injection);
}

/// 一次性注入的回调：把"重放"这件事从服务里解耦出来。
pub trait TouchEvents: Send + Sync {
    /// 用户点了窗口自带关闭按钮，请上层去关（`u64` 是窗口句柄）。
    fn close_window_requested(&self, _handle: u64) {}
    fn selection_completed(&self, _at: Point) {}
    fn selection_dismissed(&self) {}
}

/// 什么都不做的回调，给不关心这些事件的地方用。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEvents;

impl TouchEvents for NoopEvents {}

/// 真机注入器：`SendInput` + `SetCursorPos`。
#[derive(Debug, Default, Clone, Copy)]
pub struct WinInput;

impl WinInput {
    fn mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: mouse_data,
                    dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS(flags),
                    time: 0,
                    dwExtraInfo: REPLAY_MARKER,
                },
            },
        }
    }

    /// 整个虚拟桌面的矩形（多屏时可能带负原点）。
    fn virtual_desktop() -> Rect {
        unsafe {
            let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let width = i32::max(1, GetSystemMetrics(SM_CXVIRTUALSCREEN));
            let height = i32::max(1, GetSystemMetrics(SM_CYVIRTUALSCREEN));
            Rect::new(x as f64, y as f64, width as f64, height as f64)
        }
    }

    fn absolute(point: Point) -> (i32, i32) {
        to_virtual_desktop_absolute(point, Self::virtual_desktop())
    }

    fn send(input: INPUT) {
        unsafe {
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
    }
}

impl InputInjector for WinInput {
    fn apply(&self, injection: Injection) {
        let move_flags =
            MOUSEEVENTF_MOVE.0 | MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0;
        match injection {
            Injection::MoveTo(point) => {
                let (x, y) = Self::absolute(point);
                Self::send(Self::mouse_input(x, y, 0, move_flags));
            }
            Injection::Button { at, down, right } => {
                let button = match (right, down) {
                    (true, true) => MOUSEEVENTF_RIGHTDOWN.0,
                    (true, false) => MOUSEEVENTF_RIGHTUP.0,
                    (false, true) => MOUSEEVENTF_LEFTDOWN.0,
                    (false, false) => MOUSEEVENTF_LEFTUP.0,
                };
                let (x, y) = Self::absolute(at);
                Self::send(Self::mouse_input(x, y, 0, move_flags | button));
            }
            Injection::Wheel { steps } => {
                Self::send(Self::mouse_input(0, 0, steps as u32, MOUSEEVENTF_WHEEL.0));
            }
            Injection::HScroll { steps } => {
                Self::send(Self::mouse_input(0, 0, steps as u32, MOUSEEVENTF_HWHEEL.0));
            }
            Injection::CursorTo(point) => unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SetCursorPos(
                    point.x.round() as i32,
                    point.y.round() as i32,
                );
            },
        }
    }
}

// ---------------------------------------------------------------------------
// 钩子服务
// ---------------------------------------------------------------------------

/// 重放线程收到的一批动作。
struct ReplayJob {
    actions: Vec<ReplayAction>,
}

/// 钩子线程与重放线程共享的东西。
struct HookContext {
    engine: Arc<Mutex<HookEngine>>,
    desktop: WinDesktop,
    replay: SyncSender<ReplayJob>,
    events: Arc<dyn TouchEvents>,
    started: Instant,
}

impl HookContext {
    /// 落点是不是复制气泡 / 浏览器工具栏（C# `IsSelectionBubbleAt`）。
    fn bubble_at(&self, point: Point, scale: f64) -> BubbleHit {
        let Some(hit) = self.desktop.window_at_point(point) else {
            return BubbleHit::default();
        };
        let root = self.desktop.root_of(hit).unwrap_or(hit);
        let Some(probe) = self.desktop.probe(root) else {
            return BubbleHit::default();
        };
        if probe.pid <= 0 {
            return BubbleHit::default();
        }
        if is_copy_bubble(probe.pid, &probe.title, std::process::id() as i32) {
            return BubbleHit { on_bubble: true, browser_chrome: false };
        }
        let meta = self.desktop.process_meta(probe.pid);
        let Some(exe) = meta.app_id else {
            return BubbleHit::default();
        };
        if !is_browser_exe(&exe) {
            return BubbleHit::default();
        }
        BubbleHit { on_bubble: false, browser_chrome: browser_chrome_hit(point, probe.rect, scale) }
    }

    /// C# `HookCallback` 的 Win32 壳子。
    ///
    /// 返回 `Some(swallow)`：`true` = 吞掉这次事件，`false`/`None` = 交给下一个钩子。
    fn dispatch(&self, message: u32, info: *const MSLLHOOKSTRUCT) -> Option<bool> {
        let kind = hook_input_kind(message)?;
        let info = unsafe { &*info };
        let point = Point::new(info.pt.x as f64, info.pt.y as f64);
        let injected = info.flags & LLMHF_INJECTED != 0;
        let replay_marked = info.dwExtraInfo == REPLAY_MARKER;
        let now = self.started.elapsed().as_secs_f64();

        let outcome = {
            let mut engine = match self.engine.lock() {
                Ok(engine) => engine,
                Err(poisoned) => poisoned.into_inner(),
            };
            engine.on_event(kind, point, now, injected, replay_marked, |at, scale| {
                self.bubble_at(at, scale)
            })
        };

        if outcome.dismiss_bubble {
            self.events.selection_dismissed();
        }
        if !outcome.commands.is_empty() {
            let actions = plan_replay(&outcome.commands, kind, outcome.press_origin, point);
            // 队列满了就丢这一批，和 C# `TryAdd` 返回 false 一样，绝不阻塞钩子。
            let _ = self.replay.try_send(ReplayJob { actions });
        }

        Some(!outcome.pass_original)
    }
}

thread_local! {
    /// 钩子回调只能拿到裸指针，所以把上下文挂在线程本地供回调取用。
    static HOOK_CONTEXT: RefCell<Option<Arc<HookContext>>> = const { RefCell::new(None) };
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    HOOK_CONTEXT.with(|cell| {
        let borrowed = cell.borrow();
        match borrowed.as_ref() {
            Some(context) => match context.dispatch(wparam.0 as u32, lparam.0 as *const MSLLHOOKSTRUCT) {
                // 返回非 0 就是吞掉这次事件。
                Some(true) => LRESULT(1),
                _ => unsafe { CallNextHookEx(None, code, wparam, lparam) },
            },
            None => unsafe { CallNextHookEx(None, code, wparam, lparam) },
        }
    })
}

struct Worker {
    thread_id: Arc<AtomicU32>,
    hook_thread: std::thread::JoinHandle<()>,
    replay_thread: std::thread::JoinHandle<()>,
}

/// 手势翻译服务（C# `TouchHookService`）。
pub struct TouchHook {
    engine: Arc<Mutex<HookEngine>>,
    injector: Arc<dyn InputInjector>,
    events: Arc<dyn TouchEvents>,
    desktop: WinDesktop,
    worker: Mutex<Option<Worker>>,
}

impl Default for TouchHook {
    fn default() -> Self {
        Self::new()
    }
}

impl TouchHook {
    pub fn new() -> Self {
        Self::with_parts(Arc::new(WinInput), Arc::new(NoopEvents))
    }

    pub fn with_parts(injector: Arc<dyn InputInjector>, events: Arc<dyn TouchEvents>) -> Self {
        Self {
            engine: Arc::new(Mutex::new(HookEngine::new())),
            injector,
            events,
            desktop: WinDesktop,
            worker: Mutex::new(None),
        }
    }

    pub fn is_running(&self) -> bool {
        self.worker.lock().map(|worker| worker.is_some()).unwrap_or(false)
    }

    /// 设定生效区域：`None` 表示停掉钩子（手机屏撤了）。
    pub fn set_region(&self, region: Option<TouchRegion>) -> bool {
        {
            let mut engine = match self.engine.lock() {
                Ok(engine) => engine,
                Err(poisoned) => poisoned.into_inner(),
            };
            engine.set_region(region);
        }
        match region {
            None => {
                self.stop();
                true
            }
            Some(_) => self.start(),
        }
    }

    pub fn options(&self) -> TouchOptions {
        self.lock_engine().options()
    }

    pub fn set_options(&self, options: TouchOptions) {
        self.lock_engine().set_options(options);
    }

    pub fn require_injected_flag(&self) -> bool {
        self.lock_engine().require_injected_flag()
    }

    pub fn set_require_injected_flag(&self, require: bool) {
        self.lock_engine().set_require_injected_flag(require);
    }

    fn lock_engine(&self) -> std::sync::MutexGuard<'_, HookEngine> {
        match self.engine.lock() {
            Ok(engine) => engine,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 装钩子并起重放线程（C# `Start`）。
    fn start(&self) -> bool {
        {
            let worker = match self.worker.lock() {
                Ok(worker) => worker,
                Err(poisoned) => poisoned.into_inner(),
            };
            if worker.is_some() {
                return true;
            }
        }

        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        let (replay_tx, replay_rx) = mpsc::sync_channel::<ReplayJob>(REPLAY_QUEUE);
        let thread_id = Arc::new(AtomicU32::new(0));

        let context = Arc::new(HookContext {
            engine: self.engine.clone(),
            desktop: self.desktop,
            replay: replay_tx,
            events: self.events.clone(),
            started: Instant::now(),
        });

        let hook_context = context.clone();
        let hook_thread_id = thread_id.clone();
        let hook_thread = std::thread::Builder::new()
            .name("SnugWin.TouchHook".to_string())
            .spawn(move || {
                unsafe {
                    hook_thread_id.store(GetCurrentThreadId(), Ordering::SeqCst);
                }
                HOOK_CONTEXT.with(|slot| {
                    *slot.borrow_mut() = Some(hook_context.clone());
                });

                let module = unsafe { GetModuleHandleW(None) }.ok().map(HINSTANCE::from);
                let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0) };
                let installed = hook.is_ok();
                let _ = ready_tx.send(installed);
                let Ok(hook) = hook else {
                    HOOK_CONTEXT.with(|slot| {
                        *slot.borrow_mut() = None;
                    });
                    return;
                };

                let mut message = MSG::default();
                while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
                    unsafe {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                let _ = unsafe { UnhookWindowsHookEx(hook) };
                HOOK_CONTEXT.with(|slot| {
                    *slot.borrow_mut() = None;
                });
            });

        let Ok(hook_thread) = hook_thread else {
            return false;
        };
        let installed = ready_rx.recv_timeout(Duration::from_secs(2)).unwrap_or(false);
        if !installed {
            let _ = hook_thread.join();
            return false;
        }

        // 重放线程：一边执行队列里的动作，一边按帧推进惯性滚动。
        let replay_engine = self.engine.clone();
        let replay_injector = self.injector.clone();
        let replay_events = self.events.clone();
        let replay_desktop = self.desktop;
        let replay_thread = std::thread::Builder::new()
            .name("SnugWin.TouchReplay".to_string())
            .spawn(move || {
                let mut accumulator = ScrollAccumulator::default();
                loop {
                    let coasting = match replay_engine.lock() {
                        Ok(engine) => engine.is_coasting(),
                        Err(poisoned) => poisoned.into_inner().is_coasting(),
                    };
                    let timeout = if coasting {
                        frame_interval()
                    } else {
                        Duration::from_secs(30)
                    };
                    match replay_rx.recv_timeout(timeout) {
                        Ok(job) => run_actions(
                            &replay_injector,
                            &replay_engine,
                            &replay_desktop,
                            &replay_events,
                            &mut accumulator,
                            &job.actions,
                        ),
                        Err(RecvTimeoutError::Timeout) => {
                            let step = {
                                let mut engine = match replay_engine.lock() {
                                    Ok(engine) => engine,
                                    Err(poisoned) => poisoned.into_inner(),
                                };
                                engine.momentum_frame()
                            };
                            if let Some(step) = step {
                                apply_scroll(&replay_injector, &mut accumulator, &step);
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            });

        let Ok(replay_thread) = replay_thread else {
            unsafe {
                let _ = PostThreadMessageW(
                    thread_id.load(Ordering::SeqCst),
                    WM_QUIT,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
            let _ = hook_thread.join();
            return false;
        };

        let mut worker = match self.worker.lock() {
            Ok(worker) => worker,
            Err(poisoned) => poisoned.into_inner(),
        };
        *worker = Some(Worker { thread_id, hook_thread, replay_thread });
        true
    }

    /// 卸钩子、收线程（C# `Stop`）。
    fn stop(&self) {
        let Some(worker) = ({
            let mut worker = match self.worker.lock() {
                Ok(worker) => worker,
                Err(poisoned) => poisoned.into_inner(),
            };
            worker.take()
        }) else {
            return;
        };

        // 让钩子线程的消息循环退出；它一退出，线程本地的上下文也释放掉，
        // 重放队列的另一端随之断开，重放线程收到 Disconnected 就收工。
        let thread_id = worker.thread_id.load(Ordering::SeqCst);
        if thread_id != 0 {
            unsafe {
                let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        let _ = worker.hook_thread.join();
        let _ = worker.replay_thread.join();
    }
}

impl Drop for TouchHook {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 惯性滚动的一帧间隔（C# `1000.0 / Momentum.FrameRate`）。
fn frame_interval() -> Duration {
    Duration::from_millis((1000.0 / Momentum::FRAME_RATE).round() as u64)
}

/// 执行一批重放动作（重放线程里跑）。
fn run_actions(
    injector: &Arc<dyn InputInjector>,
    engine: &Arc<Mutex<HookEngine>>,
    desktop: &WinDesktop,
    events: &Arc<dyn TouchEvents>,
    accumulator: &mut ScrollAccumulator,
    actions: &[ReplayAction],
) {
    for action in actions {
        match action {
            ReplayAction::Injection(injection) => injector.apply(*injection),
            ReplayAction::Scroll(step) => apply_scroll(injector, accumulator, step),
            ReplayAction::CloseWindow { at } => {
                // 关闭按钮的位置由界面那边塞进 region（C# 从 `region?.CloseButton` 取）。
                let button = {
                    let engine = match engine.lock() {
                        Ok(engine) => engine,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    engine.region().and_then(|region| region.close_button)
                };
                let z_order = on_screen_z_order(&desktop.probes());
                match close_tap_decision(button, *at, &z_order) {
                    CloseTap::RequestClose(handle) => events.close_window_requested(handle),
                    CloseTap::ReplayClick => {
                        injector.apply(Injection::Button { at: *at, down: true, right: false });
                        injector.apply(Injection::Button { at: *at, down: false, right: false });
                    }
                }
            }
            ReplayAction::SelectionCompleted { at } => events.selection_completed(*at),
            ReplayAction::Wait(duration) => {
                if !duration.is_zero() {
                    std::thread::sleep(*duration);
                }
            }
        }
    }
}

fn apply_scroll(
    injector: &Arc<dyn InputInjector>,
    accumulator: &mut ScrollAccumulator,
    step: &ScrollStep,
) {
    let shot = accumulator.take(step);
    if let Some(point) = shot.cursor_to {
        injector.apply(Injection::CursorTo(point));
    }
    if shot.dy != 0 {
        injector.apply(Injection::Wheel { steps: shot.dy });
    }
    if shot.dx != 0 {
        injector.apply(Injection::HScroll { steps: shot.dx });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region() -> TouchRegion {
        TouchRegion {
            frame: Rect::new(0.0, 0.0, 540.0, 950.0),
            scale: 1.0,
            close_button: None,
        }
    }

    fn no_bubble(_point: Point, _scale: f64) -> BubbleHit {
        BubbleHit::default()
    }

    #[test]
    fn hook_kind_maps_mouse_messages() {
        assert_eq!(hook_input_kind(WM_LBUTTONDOWN), Some(TouchInputKind::Press));
        assert_eq!(hook_input_kind(WM_MOUSEMOVE), Some(TouchInputKind::Drag));
        assert_eq!(hook_input_kind(WM_LBUTTONUP), Some(TouchInputKind::Release));
        assert_eq!(hook_input_kind(0x0200 | 0x8000), None, "别的消息不归我们管");
    }

    #[test]
    fn absolute_coordinates_map_desktop_to_65535() {
        let desktop = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        assert_eq!(to_virtual_desktop_absolute(Point::new(0.0, 0.0), desktop), (0, 0));
        assert_eq!(
            to_virtual_desktop_absolute(Point::new(1919.0, 1079.0), desktop),
            (65535, 65535)
        );
        assert_eq!(to_virtual_desktop_absolute(Point::new(960.0, 540.0), desktop), (32785, 32798));

        // 多屏负原点：左侧副屏从 -1920 开始。
        let wide = Rect::new(-1920.0, 0.0, 3840.0, 1080.0);
        assert_eq!(to_virtual_desktop_absolute(Point::new(-1920.0, 0.0), wide), (0, 0));
        assert_eq!(
            to_virtual_desktop_absolute(Point::new(1919.0, 1079.0), wide),
            (65535, 65535)
        );
    }

    #[test]
    fn scroll_accumulator_carries_subpixel_remainder() {
        let mut accumulator = ScrollAccumulator::default();
        let mut step = ScrollStep {
            pixels_x: 0,
            pixels_y: 0,
            delta_x: 0.0,
            delta_y: 1.0,
            phase: ScrollPhase::Began,
            momentum: snug_core::MomentumPhase::None,
            location: Point::new(100.0, 200.0),
        };

        // 1 像素 * 2.5 = 2.5 档 → 发 2 档，留 0.5。
        let first = accumulator.take(&step);
        assert_eq!((first.dy, first.dx), (2, 0));
        assert_eq!(first.cursor_to, Some(Point::new(100.0, 200.0)), "滚动开始要先挪光标");

        // 再来 1 像素 → 0.5 + 2.5 = 3 档。
        step.phase = ScrollPhase::Changed;
        let second = accumulator.take(&step);
        assert_eq!(second.dy, 3);
        assert_eq!(second.cursor_to, None, "只有 Began 才挪光标");

        // 向左推：横向取反。
        let mut horizontal = ScrollStep { delta_x: -2.0, delta_y: 0.0, ..step };
        horizontal.phase = ScrollPhase::Changed;
        let third = accumulator.take(&horizontal);
        assert_eq!((third.dy, third.dx), (0, 5));

        // 余数用不掉的，下一次接着用。
        let mut tiny = ScrollStep { delta_x: 0.0, delta_y: 0.1, ..step };
        tiny.phase = ScrollPhase::Changed;
        let fourth = accumulator.take(&tiny);
        assert_eq!(fourth.dy, 0, "0.25 档发不出去");
    }

    #[test]
    fn browser_list_and_chrome_height() {
        assert!(is_browser_exe("msedge.exe"));
        assert!(is_browser_exe("Chrome.EXE"));
        assert!(is_browser_exe("firefox.exe"));
        assert!(!is_browser_exe("notepad.exe"));

        let window = Rect::new(0.0, 100.0, 1000.0, 800.0);
        assert!(browser_chrome_hit(Point::new(10.0, 120.0), window, 1.0));
        assert!(!browser_chrome_hit(Point::new(10.0, 300.0), window, 1.0));
        // 2 倍缩放时工具栏区域也跟着放大。
        assert!(browser_chrome_hit(Point::new(10.0, 330.0), window, 2.0));
        // 窗口上方的点不算。
        assert!(!browser_chrome_hit(Point::new(10.0, 50.0), window, 1.0));
    }

    #[test]
    fn copy_bubble_is_our_own_window_only() {
        assert!(is_copy_bubble(1234, "SnugCopyBubble", 1234));
        assert!(!is_copy_bubble(4321, "SnugCopyBubble", 1234));
        assert!(!is_copy_bubble(1234, "别的窗口", 1234));
    }

    #[test]
    fn close_tap_requests_close_only_when_frontmost() {
        let window = Rect::new(0.0, 22.0, 540.0, 900.0);
        let button = CloseButton {
            frame: Rect::new(8.0, 30.0, 14.0, 14.0),
            window_handle: 7,
            window_frame: window,
        };
        let at = Point::new(15.0, 37.0);
        assert_eq!(
            close_tap_decision(Some(button), at, &[(7, window)]),
            CloseTap::RequestClose(7)
        );
        // 被别的窗口盖住了 → 只能发点击。
        assert_eq!(
            close_tap_decision(
                Some(button),
                at,
                &[(9, Rect::new(0.0, 22.0, 540.0, 892.0)), (7, window)]
            ),
            CloseTap::ReplayClick
        );
        assert_eq!(close_tap_decision(None, at, &[(7, window)]), CloseTap::ReplayClick);
    }

    #[test]
    fn engine_swallows_press_and_replays_tap() {
        let mut engine = HookEngine::new();
        engine.set_region(Some(region()));

        let press = engine.on_event(
            TouchInputKind::Press,
            Point::new(100.0, 300.0),
            0.0,
            true,
            false,
            no_bubble,
        );
        assert!(!press.pass_original, "区域内按下要吞掉");
        assert!(press.commands.is_empty());
        assert!(press.dismiss_bubble, "按在空白处要收起复制气泡");

        let drag = engine.on_event(
            TouchInputKind::Drag,
            Point::new(102.0, 302.0),
            0.05,
            true,
            false,
            no_bubble,
        );
        assert!(!drag.pass_original);
        assert!(drag.commands.is_empty(), "没超过 slop 不动");

        let release = engine.on_event(
            TouchInputKind::Release,
            Point::new(103.0, 303.0),
            0.1,
            true,
            false,
            no_bubble,
        );
        assert!(!release.pass_original);
        assert_eq!(release.commands.len(), 2);
        assert_eq!(release.commands[0], TouchCommand::ReplayPress);
        assert_eq!(release.commands[1], TouchCommand::ReplayCurrent);
        assert_eq!(release.press_origin, Point::new(100.0, 300.0));
    }

    #[test]
    fn engine_passes_through_uninjected_events_until_allowed() {
        let mut engine = HookEngine::new();
        engine.set_region(Some(region()));
        let press = |engine: &mut HookEngine, injected: bool| {
            engine
                .on_event(TouchInputKind::Press, Point::new(100.0, 300.0), 0.0, injected, false, no_bubble)
                .pass_original
        };

        assert!(press(&mut engine, false), "真实鼠标要放行");
        assert!(engine.logged_first_press(), "第一次按下之后要记一笔");
        engine.set_require_injected_flag(false);
        assert!(!press(&mut engine, false), "放宽之后才接管");
    }

    #[test]
    fn engine_ignores_replay_marked_events() {
        let mut engine = HookEngine::new();
        engine.set_region(Some(region()));
        let outcome = engine.on_event(
            TouchInputKind::Press,
            Point::new(100.0, 300.0),
            0.0,
            true,
            true,
            no_bubble,
        );
        assert!(outcome.pass_original);
        assert!(outcome.commands.is_empty());
    }

    #[test]
    fn engine_passes_events_outside_the_region() {
        let mut engine = HookEngine::new();
        engine.set_region(Some(region()));
        let outcome = engine.on_event(
            TouchInputKind::Press,
            Point::new(2000.0, 300.0),
            0.0,
            true,
            false,
            no_bubble,
        );
        assert!(outcome.pass_original);
        assert!(!outcome.dismiss_bubble);
    }

    #[test]
    fn engine_keeps_bubble_and_browser_chrome_untouched() {
        let mut engine = HookEngine::new();
        engine.set_region(Some(region()));
        let on_bubble = |_point: Point, _scale: f64| BubbleHit { on_bubble: true, browser_chrome: false };

        let press = engine.on_event(
            TouchInputKind::Press,
            Point::new(100.0, 300.0),
            0.0,
            true,
            false,
            on_bubble,
        );
        assert!(press.pass_original, "点在气泡上要直通");
        assert!(!press.dismiss_bubble, "已经在气泡上就别收了");

        // 浏览器工具栏：整整一段按下/拖动/抬起都直通，抬起后恢复接管。
        let mut engine = HookEngine::new();
        engine.set_region(Some(region()));
        let chrome = |_point: Point, _scale: f64| BubbleHit { on_bubble: false, browser_chrome: true };
        assert!(engine.on_event(TouchInputKind::Press, Point::new(100.0, 300.0), 0.0, true, false, chrome).pass_original);
        assert!(engine.on_event(TouchInputKind::Drag, Point::new(100.0, 380.0), 0.1, true, false, chrome).pass_original);
        assert!(engine.on_event(TouchInputKind::Release, Point::new(100.0, 390.0), 0.2, true, false, chrome).pass_original);
        // 抬起后旗标复位，下次按下重新接管。
        assert!(!engine.on_event(TouchInputKind::Press, Point::new(100.0, 300.0), 0.3, true, false, no_bubble).pass_original);
    }

    #[test]
    fn plan_replay_orders_press_then_release_with_pause() {
        let actions = plan_replay(
            &[TouchCommand::ReplayPress, TouchCommand::ReplayCurrent],
            TouchInputKind::Release,
            Point::new(100.0, 300.0),
            Point::new(103.0, 303.0),
        );
        assert_eq!(actions.len(), 3);
        assert_eq!(
            actions[0],
            ReplayAction::Injection(Injection::Button {
                at: Point::new(100.0, 300.0),
                down: true,
                right: false
            })
        );
        assert_eq!(actions[1], ReplayAction::Wait(Duration::from_millis(10)));
        assert_eq!(
            actions[2],
            ReplayAction::Injection(Injection::Button {
                at: Point::new(103.0, 303.0),
                down: false,
                right: false
            })
        );
    }

    #[test]
    fn plan_replay_drag_moves_and_right_click_pauses() {
        let drag = plan_replay(
            &[TouchCommand::ReplayPress, TouchCommand::ReplayCurrent],
            TouchInputKind::Drag,
            Point::new(100.0, 300.0),
            Point::new(130.0, 300.0),
        );
        assert_eq!(drag[1], ReplayAction::Wait(Duration::from_millis(4)));
        assert_eq!(drag[2], ReplayAction::Injection(Injection::MoveTo(Point::new(130.0, 300.0))));

        let right = plan_replay(
            &[TouchCommand::RightClick { at: Point::new(50.0, 60.0) }],
            TouchInputKind::Release,
            Point::ZERO,
            Point::ZERO,
        );
        assert_eq!(
            right[0],
            ReplayAction::Injection(Injection::Button {
                at: Point::new(50.0, 60.0),
                down: true,
                right: true
            })
        );
        assert_eq!(right[1], ReplayAction::Wait(Duration::from_millis(10)));
        assert_eq!(
            right[2],
            ReplayAction::Injection(Injection::Button {
                at: Point::new(50.0, 60.0),
                down: false,
                right: true
            })
        );
    }

    #[test]
    fn plan_replay_passes_through_scroll_close_and_selection() {
        let step = ScrollStep {
            pixels_x: 0,
            pixels_y: 40,
            delta_x: 0.0,
            delta_y: 40.0,
            phase: ScrollPhase::Changed,
            momentum: snug_core::MomentumPhase::None,
            location: Point::new(100.0, 300.0),
        };
        let actions = plan_replay(
            &[
                TouchCommand::Scroll(step),
                TouchCommand::CloseWindow { at: Point::new(10.0, 20.0) },
                TouchCommand::SelectionCompleted { at: Point::new(30.0, 40.0) },
            ],
            TouchInputKind::Release,
            Point::ZERO,
            Point::ZERO,
        );
        assert_eq!(actions[0], ReplayAction::Scroll(step));
        assert_eq!(actions[1], ReplayAction::CloseWindow { at: Point::new(10.0, 20.0) });
        assert_eq!(actions[2], ReplayAction::SelectionCompleted { at: Point::new(30.0, 40.0) });
    }

    #[test]
    fn frame_interval_matches_momentum_rate() {
        assert_eq!(frame_interval(), Duration::from_millis(17));
    }
}
