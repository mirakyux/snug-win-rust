//! 手势"选后复制/粘贴/删除"的最后一环：把气泡上的动作变成一次真实按键。
//!
//! 移植自 C# 版 `SnugWin.Windows.Windows.WindowService` 的三个方法：
//! `PerformSelectionActionAsync`（点 → 顶层窗口 → 聚焦 → 等 35ms → 按键）、
//! `CopySelectionAtAsync`（它的复制版）、`SendKeystroke`（真正发键）。
//!
//! 快捷键解析**一律**走 `snug-core` 的 [`snug_core::selection::resolve`]
//! （C# `SelectionShortcutResolver.Resolve`，含终端里 `Ctrl+Shift+C` 的特例）；
//! 这一层不再写第二份终端判断，避免两套实现悄悄分叉。
//!
//! 与 C# 有两处**有意差异**，都是为了不在别人的窗口里乱按：
//!
//! 1. **聚焦失败就不发键**。C# 的 `FocusAsync` 不回报成功与否，随后照样
//!    `SendKeystroke`——万一抢前台被系统拒绝，`Ctrl+C`/退格就打到当前前台窗口里去了。
//!    这里在聚焦后（等完 35ms）核对一次前台窗口，不是目标就 `warn` 并返回 `false`。
//!    目标窗口若在 `WindowFromPoint` 之后消失，同样不发键（C# 会打到别处）。
//! 2. **`SendInput` 被 UIPI 挡住要报告**。比如目标是管理员权限窗口时，非提权的
//!    `SendInput` 会静默失败（返回 0）；C# 不看返回值，这里返回 `false` 并 `warn`。
//!
//! 坐标取整用 `round_ties_even`：C# `(int)Math.Round(point.X)` 默认是银行家舍入
//! （同 `display` 模块 A5 的结论），`.round()` 会在正好 `.5` 时差 1 像素。

use std::time::Duration;

use snug_core::selection::{resolve, SelectionBubbleAction, SelectionKeystroke};
use snug_core::{Point, WindowKey};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_CONTROL, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use crate::touch::REPLAY_MARKER;
use crate::window::{ProcessMeta, ShowCommand, WinDesktop, WindowApi, WindowProbe};

/// 聚焦之后等焦点稳一下再发键（C# `Task.Delay(35)`）。测试里注入 0。
pub const SELECTION_SETTLE: Duration = Duration::from_millis(35);

/// 一次按键事件：只记"哪个虚拟键、按下还是抬起"，顺序即发送顺序。
///
/// 单独抽成纯数据，是为了不开真键盘也能测按下/抬起配对（C# `SendKeystroke`
/// 里手写的 `List<INPUT>` 顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub virtual_key: u16,
    pub down: bool,
}

/// 把解析出的组合键展开成按下/抬起序列（C# `SendKeystroke` 的 inputs 列表）。
///
/// 顺序：修饰键按下 → 主键按下 → 主键抬起 → 修饰键抬起（后进先出）。
pub fn key_events(keystroke: SelectionKeystroke) -> Vec<KeyEvent> {
    let mut events = Vec::with_capacity(6);
    if keystroke.with_ctrl {
        events.push(KeyEvent { virtual_key: VK_CONTROL.0, down: true });
    }
    if keystroke.with_shift {
        events.push(KeyEvent { virtual_key: VK_SHIFT.0, down: true });
    }
    events.push(KeyEvent { virtual_key: keystroke.virtual_key, down: true });
    events.push(KeyEvent { virtual_key: keystroke.virtual_key, down: false });
    if keystroke.with_shift {
        events.push(KeyEvent { virtual_key: VK_SHIFT.0, down: false });
    }
    if keystroke.with_ctrl {
        events.push(KeyEvent { virtual_key: VK_CONTROL.0, down: false });
    }
    events
}

// ---------------------------------------------------------------------------
// 平台接口
// ---------------------------------------------------------------------------

/// 选择复制要用的窗口能力（C# `WindowService` 里那几个 `Win32.*` 调用）。
///
/// 没有直接复用 [`WindowApi`] 是因为这里只需要六个方法，而且要多一个
/// "当前前台窗口"查询；单测换假实现也小得多。
pub trait SelectionWindows: Send + Sync {
    /// `WindowFromPoint`；点不在任何窗口上返回 `None`。
    fn window_at_point(&self, point: Point) -> Option<u64>;
    /// `GetAncestor(hwnd, GA_ROOT)`；失败/不是子窗口返回 `None`（调用方退回原句柄）。
    fn root_of(&self, handle: u64) -> Option<u64>;
    fn probe(&self, handle: u64) -> Option<WindowProbe>;
    fn process_meta(&self, pid: i32) -> ProcessMeta;
    /// 先还原最小化，再挂线程抢前台（C# `FocusAsync`）。
    fn focus(&self, handle: u64);
    /// `GetForegroundWindow`。
    fn foreground_window(&self) -> Option<u64>;
}

/// 键盘注入器：真机是 [`WinKeyboard`]，测试里换成记录用的假实现。
///
/// `touch` 的 [`crate::touch::InputInjector`] 只承载鼠标 `Injection`，键盘这套
/// 单列一个 trait；`dwExtraInfo` 复用同一个 [`REPLAY_MARKER`]。
pub trait KeystrokeInjector: Send + Sync {
    /// 发一次组合键，返回系统是否真的接受了整批事件。
    fn send(&self, keystroke: SelectionKeystroke) -> bool;
}

/// 真机键盘注入：`SendInput` + [`REPLAY_MARKER`]（C# `SendKeystroke`）。
#[derive(Debug, Default, Clone, Copy)]
pub struct WinKeyboard;

impl WinKeyboard {
    fn key_input(virtual_key: u16, down: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(virtual_key),
                    wScan: 0,
                    dwFlags: if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP },
                    time: 0,
                    // C# 用的就是 0x534E5547（ASCII "SNUG"），用户自己的钩子能认出
                    // 这是我们注入的按键。
                    dwExtraInfo: REPLAY_MARKER,
                },
            },
        }
    }
}

impl KeystrokeInjector for WinKeyboard {
    fn send(&self, keystroke: SelectionKeystroke) -> bool {
        let inputs: Vec<INPUT> = key_events(keystroke)
            .into_iter()
            .map(|event| Self::key_input(event.virtual_key, event.down))
            .collect();
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        // C# 不看返回值；我们拿它判断是否被 UIPI 挡掉。
        sent as usize == inputs.len()
    }
}

/// 真机选择动作。
pub type WinSelectionActions = SelectionActions<WinDesktop, WinKeyboard>;

impl WinSelectionActions {
    pub fn new() -> Self {
        Self {
            windows: WinDesktop,
            keys: WinKeyboard,
            settle: SELECTION_SETTLE,
        }
    }
}

impl Default for WinSelectionActions {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 选择动作
// ---------------------------------------------------------------------------

/// 选择动作本体（C# `WindowService.PerformSelectionActionAsync`）。
pub struct SelectionActions<W: SelectionWindows, K: KeystrokeInjector> {
    windows: W,
    keys: K,
    settle: Duration,
}

impl<W: SelectionWindows, K: KeystrokeInjector> SelectionActions<W, K> {
    /// 测试/替身用：`settle` 对应 C# 的 35ms，测试里给 [`Duration::ZERO`]。
    pub fn with_parts(windows: W, keys: K, settle: Duration) -> Self {
        Self { windows, keys, settle }
    }

    /// 点 `point` 处的窗口 → 提到顶层 → 聚焦 → 等 `settle` → 按快捷键。
    ///
    /// 返回 `true` 表示按键确实发出去了；找不到窗口、窗口消失、聚焦失败、
    /// 注入被挡都返回 `false`（后三种会 `warn`）。
    pub fn perform_selection_action(
        &self,
        point: Point,
        action: SelectionBubbleAction,
    ) -> bool {
        // C# `(int)Math.Round(...)`：银行家舍入。
        let point = Point::new(point.x.round_ties_even(), point.y.round_ties_even());
        let Some(hit) = self.windows.window_at_point(point) else {
            // C# 这里静默返回；每点一次空白桌面就 warn 会刷屏，保持安静。
            return false;
        };
        // C#：GetAncestor 失败时 root 退回 hit。
        let root = self.windows.root_of(hit).unwrap_or(hit);
        let Some(probe) = self.windows.probe(root) else {
            crate::warn(&format!(
                "selection: 窗口 0x{root:X} 在取到后已消失，未发送 {action:?}"
            ));
            return false;
        };

        let keystroke = self.keystroke_for(probe.pid, &probe.class, action);

        self.windows.focus(root);
        sleep_for(self.settle);

        if !self.is_foreground(root) {
            // 有意偏离 C#：没抢到前台就不发键，宁可这次复制不生效。
            crate::warn(&format!(
                "selection: 聚焦失败，未发送 {action:?}（pid={}, hwnd=0x{root:X}）",
                probe.pid
            ));
            return false;
        }

        if !self.keys.send(keystroke) {
            crate::warn(&format!(
                "selection: SendInput 未送达 {action:?}（pid={}, hwnd=0x{root:X}）",
                probe.pid
            ));
            return false;
        }
        true
    }

    /// 只按一次组合键，不再聚焦（`window` 只用来解析快捷键）。
    ///
    /// 调用方（复制气泡的 UI）自己保证前台已经是对的。窗口不存在、或句柄
    /// 已经被别的进程复用，都返回 `false` 且不发键。
    pub fn send_selection_keystroke(
        &self,
        window: WindowKey,
        action: SelectionBubbleAction,
    ) -> bool {
        let Some(probe) = self.windows.probe(window.handle) else {
            crate::warn(&format!(
                "selection: 窗口 0x{:X} 已不存在，未发送 {action:?}",
                window.handle
            ));
            return false;
        };
        if probe.pid != window.pid {
            // 句柄被系统复用：按 pid 认窗口是 C# `WindowKey` 的约定。
            crate::warn(&format!(
                "selection: 窗口 0x{:X} 的 pid 已变为 {}（期望 {}），未发送 {action:?}",
                window.handle, probe.pid, window.pid
            ));
            return false;
        }

        let keystroke = self.keystroke_for(probe.pid, &probe.class, action);
        self.keys.send(keystroke)
    }

    /// C# `SelectionShortcutResolver.Resolve(action, cls, exe)`。
    ///
    /// `exe` 只在 `pid > 0` 时取（C# 的三元表达式），取不到就是 `None`。
    fn keystroke_for(
        &self,
        pid: i32,
        class: &str,
        action: SelectionBubbleAction,
    ) -> SelectionKeystroke {
        let exe = if pid > 0 {
            self.windows.process_meta(pid).app_id
        } else {
            None
        };
        resolve(action, Some(class), exe.as_deref())
    }

    /// 前台窗口是不是 `root`（或者它的子窗口被提到顶层了）。
    fn is_foreground(&self, root: u64) -> bool {
        let Some(foreground) = self.windows.foreground_window() else {
            return false;
        };
        foreground == root || self.windows.root_of(foreground) == Some(root)
    }
}

fn sleep_for(duration: Duration) {
    if !duration.is_zero() {
        std::thread::sleep(duration);
    }
}

// ---------------------------------------------------------------------------
// 真机窗口实现
// ---------------------------------------------------------------------------

impl SelectionWindows for WinDesktop {
    fn window_at_point(&self, point: Point) -> Option<u64> {
        // `WindowApi::window_at_point` 自己会再取整一次；这里进来的已经是整数，
        // 不会二次舍入。
        <WinDesktop as WindowApi>::window_at_point(self, point)
    }

    fn root_of(&self, handle: u64) -> Option<u64> {
        <WinDesktop as WindowApi>::root_of(self, handle)
    }

    fn probe(&self, handle: u64) -> Option<WindowProbe> {
        <WinDesktop as WindowApi>::probe(self, handle)
    }

    fn process_meta(&self, pid: i32) -> ProcessMeta {
        <WinDesktop as WindowApi>::process_meta(self, pid)
    }

    fn focus(&self, handle: u64) {
        // C# `FocusAsync`：最小化的先还原，再抢前台。
        if let Some(probe) = <WinDesktop as WindowApi>::probe(self, handle)
            && probe.minimized
        {
            <WinDesktop as WindowApi>::show(self, handle, ShowCommand::Restore);
        }
        <WinDesktop as WindowApi>::focus(self, handle);
    }

    fn foreground_window(&self) -> Option<u64> {
        let window = unsafe { GetForegroundWindow() };
        if window.0.is_null() {
            None
        } else {
            Some(window.0 as usize as u64)
        }
    }
}

// ---------------------------------------------------------------------------
// 单测（只走假实现，绝不真的按键）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use snug_core::Rect;
    use snug_core::selection::{VK_BACK, VK_C, VK_V};

    use super::*;

    fn probe_of(handle: u64, pid: i32, class: &str) -> WindowProbe {
        WindowProbe::new(handle, pid, class, "", Rect::ZERO)
    }

    /// 一次调用里假窗口与假注入器共用的记录。
    #[derive(Default)]
    struct FakeEnv {
        hit: Option<u64>,
        points: Vec<Point>,
        roots: HashMap<u64, Option<u64>>,
        probes: HashMap<u64, WindowProbe>,
        metas: HashMap<i32, ProcessMeta>,
        meta_calls: Vec<i32>,
        focus_calls: Vec<u64>,
        focus_works: bool,
        foreground: Option<u64>,
        keystrokes: Vec<SelectionKeystroke>,
        accept_keys: bool,
        /// 调用顺序，用来断言"先聚焦、再发键"。
        log: Vec<String>,
    }

    struct FakeWindows(Arc<Mutex<FakeEnv>>);

    impl SelectionWindows for FakeWindows {
        fn window_at_point(&self, point: Point) -> Option<u64> {
            let mut env = self.0.lock().unwrap();
            env.points.push(point);
            env.hit
        }

        fn root_of(&self, handle: u64) -> Option<u64> {
            self.0.lock().unwrap().roots.get(&handle).copied().flatten()
        }

        fn probe(&self, handle: u64) -> Option<WindowProbe> {
            self.0.lock().unwrap().probes.get(&handle).cloned()
        }

        fn process_meta(&self, pid: i32) -> ProcessMeta {
            let mut env = self.0.lock().unwrap();
            env.meta_calls.push(pid);
            env.metas
                .get(&pid)
                .cloned()
                .unwrap_or(ProcessMeta { name: format!("PID {pid}"), app_id: None })
        }

        fn focus(&self, handle: u64) {
            let mut env = self.0.lock().unwrap();
            env.focus_calls.push(handle);
            env.log.push(format!("focus 0x{handle:X}"));
            if env.focus_works {
                env.foreground = Some(handle);
            }
        }

        fn foreground_window(&self) -> Option<u64> {
            self.0.lock().unwrap().foreground
        }
    }

    struct FakeKeys(Arc<Mutex<FakeEnv>>);

    impl KeystrokeInjector for FakeKeys {
        fn send(&self, keystroke: SelectionKeystroke) -> bool {
            let mut env = self.0.lock().unwrap();
            env.keystrokes.push(keystroke);
            env.log.push(format!(
                "keys vk={} ctrl={} shift={}",
                keystroke.virtual_key, keystroke.with_ctrl, keystroke.with_shift
            ));
            env.accept_keys
        }
    }

    /// 一个普通窗口：`hit` 是子窗口 0x10，顶层是 0x20。
    fn normal_setup() -> (SelectionActions<FakeWindows, FakeKeys>, Arc<Mutex<FakeEnv>>) {
        let env = Arc::new(Mutex::new(FakeEnv::default()));
        {
            let mut state = env.lock().unwrap();
            state.hit = Some(0x10);
            state.roots.insert(0x10, Some(0x20));
            state.probes.insert(0x20, probe_of(0x20, 100, "Chrome_WidgetWin_1"));
            state.metas.insert(
                100,
                ProcessMeta { name: "Chrome".to_string(), app_id: Some("chrome.exe".to_string()) },
            );
            state.focus_works = true;
            state.accept_keys = true;
        }
        let actions = SelectionActions::with_parts(
            FakeWindows(env.clone()),
            FakeKeys(env.clone()),
            Duration::ZERO,
        );
        (actions, env)
    }

    #[test]
    fn copy_focuses_top_level_then_sends_ctrl_c() {
        let (actions, env) = normal_setup();
        assert!(actions.perform_selection_action(Point::new(10.5, 11.5), SelectionBubbleAction::Copy));

        let env = env.lock().unwrap();
        // 银行家舍入：10.5 → 10、11.5 → 12。
        assert_eq!(env.points, vec![Point::new(10.0, 12.0)]);
        // 聚焦的是顶层窗口，不是 WindowFromPoint 命中的子窗口。
        assert_eq!(env.focus_calls, vec![0x20]);
        assert_eq!(env.keystrokes, vec![SelectionKeystroke::new(VK_C, true, false)]);
        // 先聚焦、后发键。
        assert_eq!(env.log, vec!["focus 0x20", "keys vk=67 ctrl=true shift=false"]);
    }

    #[test]
    fn terminal_windows_get_ctrl_shift_c() {
        for (class, exe) in [
            ("CASCADIA_HOSTING_WINDOW_CLASS", "WindowsTerminal.exe"),
            ("ConsoleWindowClass", "cmd.exe"),
            ("Notepad", "pwsh.exe"),
        ] {
            let (actions, env) = normal_setup();
            {
                let mut state = env.lock().unwrap();
                state.probes.insert(0x20, probe_of(0x20, 100, class));
                state.metas.insert(
                    100,
                    ProcessMeta { name: exe.to_string(), app_id: Some(exe.to_string()) },
                );
            }
            assert!(actions.perform_selection_action(Point::ZERO, SelectionBubbleAction::Copy));
            assert_eq!(
                env.lock().unwrap().keystrokes,
                vec![SelectionKeystroke::new(VK_C, true, true)],
                "{class} 应走 Ctrl+Shift+C"
            );
        }
    }

    #[test]
    fn paste_and_delete_resolve_through_core() {
        let (actions, env) = normal_setup();
        assert!(actions.perform_selection_action(Point::ZERO, SelectionBubbleAction::Paste));
        // Delete 是裸退格（无 Ctrl、无 Shift）。
        assert!(actions.perform_selection_action(Point::ZERO, SelectionBubbleAction::Delete));
        assert_eq!(
            env.lock().unwrap().keystrokes,
            vec![
                SelectionKeystroke::new(VK_V, true, false),
                SelectionKeystroke::new(VK_BACK, false, false),
            ]
        );
    }

    #[test]
    fn failed_focus_sends_no_keys() {
        let (actions, env) = normal_setup();
        env.lock().unwrap().focus_works = false;

        assert!(!actions.perform_selection_action(Point::ZERO, SelectionBubbleAction::Copy));
        let env = env.lock().unwrap();
        assert_eq!(env.focus_calls, vec![0x20], "还是尝试过聚焦");
        assert!(env.keystrokes.is_empty(), "聚焦失败绝不能按 Ctrl+C");
    }

    #[test]
    fn no_window_at_point_does_nothing() {
        let (actions, env) = normal_setup();
        env.lock().unwrap().hit = None;

        assert!(!actions.perform_selection_action(Point::new(3.0, 4.0), SelectionBubbleAction::Copy));
        let env = env.lock().unwrap();
        assert_eq!(env.points, vec![Point::new(3.0, 4.0)]);
        assert!(env.focus_calls.is_empty());
        assert!(env.keystrokes.is_empty());
        assert!(env.meta_calls.is_empty());
    }

    #[test]
    fn root_falls_back_to_hit_when_get_ancestor_fails() {
        let (actions, env) = normal_setup();
        {
            let mut state = env.lock().unwrap();
            state.roots.clear();
            state.probes.insert(0x10, probe_of(0x10, 100, "Notepad"));
        }
        assert!(actions.perform_selection_action(Point::ZERO, SelectionBubbleAction::Copy));
        let env = env.lock().unwrap();
        assert_eq!(env.focus_calls, vec![0x10]);
        assert_eq!(env.keystrokes, vec![SelectionKeystroke::new(VK_C, true, false)]);
    }

    #[test]
    fn window_gone_after_hit_sends_no_keys() {
        let (actions, env) = normal_setup();
        env.lock().unwrap().probes.clear();

        assert!(!actions.perform_selection_action(Point::ZERO, SelectionBubbleAction::Paste));
        let env = env.lock().unwrap();
        assert!(env.focus_calls.is_empty());
        assert!(env.keystrokes.is_empty());
    }

    #[test]
    fn explicit_keystroke_needs_no_focus() {
        let (actions, env) = normal_setup();
        assert!(actions.send_selection_keystroke(
            WindowKey::new(100, 0x20),
            SelectionBubbleAction::Copy
        ));
        let env = env.lock().unwrap();
        assert!(env.focus_calls.is_empty(), "send_selection_keystroke 不抢前台");
        assert_eq!(env.keystrokes, vec![SelectionKeystroke::new(VK_C, true, false)]);
    }

    #[test]
    fn explicit_keystroke_rejects_reused_handle() {
        let (actions, env) = normal_setup();
        // 句柄还在，但已经属于别的进程。
        assert!(!actions.send_selection_keystroke(
            WindowKey::new(999, 0x20),
            SelectionBubbleAction::Copy
        ));
        assert!(env.lock().unwrap().keystrokes.is_empty());
    }

    #[test]
    fn explicit_keystroke_rejects_missing_window() {
        let (actions, env) = normal_setup();
        assert!(!actions.send_selection_keystroke(
            WindowKey::new(100, 0x99),
            SelectionBubbleAction::Copy
        ));
        assert!(env.lock().unwrap().keystrokes.is_empty());
    }

    #[test]
    fn blocked_injection_reports_false() {
        let (actions, env) = normal_setup();
        env.lock().unwrap().accept_keys = false;
        assert!(!actions.perform_selection_action(Point::ZERO, SelectionBubbleAction::Copy));
        assert_eq!(env.lock().unwrap().keystrokes.len(), 1, "尝试过发送");
    }

    #[test]
    fn pid_zero_skips_process_lookup() {
        let (actions, env) = normal_setup();
        {
            let mut state = env.lock().unwrap();
            state.probes.insert(0x20, probe_of(0x20, 0, "Notepad"));
        }
        assert!(actions.perform_selection_action(Point::ZERO, SelectionBubbleAction::Copy));
        let env = env.lock().unwrap();
        assert!(env.meta_calls.is_empty(), "pid<=0 不查进程名");
        assert_eq!(env.keystrokes, vec![SelectionKeystroke::new(VK_C, true, false)]);
    }

    #[test]
    fn foreground_child_window_counts_as_focused() {
        let (actions, env) = normal_setup();
        {
            let mut state = env.lock().unwrap();
            // 比如输入法之类把焦点给了顶层窗口的子窗口。
            state.foreground = Some(0x30);
            state.roots.insert(0x30, Some(0x20));
        }
        assert!(actions.perform_selection_action(Point::ZERO, SelectionBubbleAction::Copy));
        assert_eq!(env.lock().unwrap().keystrokes.len(), 1);
    }

    #[test]
    fn key_events_pair_press_and_release_like_the_csharp_list() {
        assert_eq!(
            key_events(SelectionKeystroke::new(VK_C, true, true)),
            vec![
                KeyEvent { virtual_key: VK_CONTROL.0, down: true },
                KeyEvent { virtual_key: VK_SHIFT.0, down: true },
                KeyEvent { virtual_key: VK_C, down: true },
                KeyEvent { virtual_key: VK_C, down: false },
                KeyEvent { virtual_key: VK_SHIFT.0, down: false },
                KeyEvent { virtual_key: VK_CONTROL.0, down: false },
            ]
        );
        assert_eq!(
            key_events(SelectionKeystroke::new(VK_V, true, false)),
            vec![
                KeyEvent { virtual_key: VK_CONTROL.0, down: true },
                KeyEvent { virtual_key: VK_V, down: true },
                KeyEvent { virtual_key: VK_V, down: false },
                KeyEvent { virtual_key: VK_CONTROL.0, down: false },
            ]
        );
        // 裸退格：不夹带任何修饰键。
        assert_eq!(
            key_events(SelectionKeystroke::new(VK_BACK, false, false)),
            vec![
                KeyEvent { virtual_key: VK_BACK, down: true },
                KeyEvent { virtual_key: VK_BACK, down: false },
            ]
        );
    }
}
