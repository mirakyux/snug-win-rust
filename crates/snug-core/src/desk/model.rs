//! 手机屏桌面的状态与数据模型。
//!
//! 逐条对应 C# 类 `SnugWin.Core.PhoneScreen.PhoneDesk` 文件顶部的这些类型：
//! `PhoneScreenPhase`、`DeskWorkKind`、`DeskWork`、`DeskNotice`、`DeskGone`、
//! `PlacedWindow`、`PhoneDeskState`。

use std::collections::HashSet;
use std::sync::Arc;

use crate::touch::CloseButton;
use crate::windows::{WindowInfo, WindowKey};

/// 手机屏本身的生命周期。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PhoneScreenPhase {
    #[default]
    Off,
    Preparing,
    Ready,
    Failed,
}

/// 桌面上正在做的事，界面据此显示进度文案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskWorkKind {
    Opening,
    Placing,
    PuttingBack,
    Zooming,
    Reshaping,
    Dismissing,
}

/// 一件正在进行的桌面动作，`label` 是窗口/App 的显示名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeskWork {
    pub kind: DeskWorkKind,
    pub label: Option<String>,
}

impl DeskWork {
    pub fn new(kind: DeskWorkKind) -> Self {
        Self { kind, label: None }
    }

    pub fn with_label(kind: DeskWorkKind, label: impl Into<String>) -> Self {
        Self { kind, label: Some(label.into()) }
    }
}

/// 需要提示用户的一次性消息（对应 C# `DeskNotice` 的各个嵌套 record）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeskNotice {
    WindowGone { app: String },
    WindowUnresponsive { app: String },
    RecordUnwritable,
    ScreenNotReady,
    OpenFailed { app: String },
    NoNewWindow { app: String },
    NoWindow { app: String },
    QuitStalled { app: String, pid: i32 },
    Reshaped,
    ReshapeFailed,
    ZoomLeftBehind { app: String, steps: i32 },
}

/// 桌面上除了窗口不见了之外，整个 App 或某个窗口消失的通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeskGone {
    App { pid: i32 },
    Window { key: WindowKey },
}

/// 此刻摆在手机屏上的窗口，以及它当前的放大级数。
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedWindow {
    pub window: WindowInfo,
    pub zoom: i32,
}

impl PlacedWindow {
    /// 这个 App 支持用 `Ctrl +/-` 调字号（对应 C# `ZoomBook.IsZoomable`）。
    pub fn zoomable(&self) -> bool {
        crate::zoom::is_zoomable(self.window.app_id.as_deref())
    }
}

/// 桌面完整状态（对应 C# `PhoneDeskState`）。
///
/// `quitting` / `closing` 用 `Arc<HashSet<_>>` 而不是裸集合，是为了还原 C# 的相等语义：
/// 那两个属性是 `IReadOnlySet`，record 的 `Equals` 对它们做**引用**比较——
/// 内容相同但换了个实例仍算"变了"，`StateChanged` 因此会再触发一次。
/// 这里的 [`PartialEq`] 用 `Arc::ptr_eq` 复刻该行为（见 `state_equality_mirrors_csharp` 测试）。
#[derive(Debug, Clone)]
pub struct DeskState {
    pub screen_phase: PhoneScreenPhase,
    pub display_id: Option<u32>,
    pub placed: Option<PlacedWindow>,
    pub list_visible: bool,
    pub work: Option<DeskWork>,
    pub notice: Option<DeskNotice>,
    pub quitting: Arc<HashSet<i32>>,
    pub closing: Arc<HashSet<WindowKey>>,
    pub dismissed: bool,
    pub close_button: Option<CloseButton>,
}

impl Default for DeskState {
    fn default() -> Self {
        Self {
            screen_phase: PhoneScreenPhase::Off,
            display_id: None,
            placed: None,
            // C# 默认 `ListVisible = true`。
            list_visible: true,
            work: None,
            notice: None,
            // C# 每构造一个状态就 `new HashSet`，两个默认状态彼此不相等。
            quitting: Arc::new(HashSet::new()),
            closing: Arc::new(HashSet::new()),
            dismissed: false,
            close_button: None,
        }
    }
}

impl PartialEq for DeskState {
    fn eq(&self, other: &Self) -> bool {
        self.screen_phase == other.screen_phase
            && self.display_id == other.display_id
            && self.placed == other.placed
            && self.list_visible == other.list_visible
            && self.work == other.work
            && self.notice == other.notice
            // 对应 C# record 对 `IReadOnlySet` 属性的引用相等。
            && Arc::ptr_eq(&self.quitting, &other.quitting)
            && Arc::ptr_eq(&self.closing, &other.closing)
            && self.dismissed == other.dismissed
            && self.close_button == other.close_button
    }
}

/// C# 里 `State = State with { ... }` 的几种常见改法的 Rust 写法。
impl DeskState {
    pub fn with_work(mut self, work: Option<DeskWork>) -> Self {
        self.work = work;
        self
    }

    pub fn with_notice(mut self, notice: Option<DeskNotice>) -> Self {
        self.notice = notice;
        self
    }

    /// 对应 `new HashSet<int>(State.Quitting) { pid }`：总是新建实例。
    pub fn with_quitting_added(mut self, pid: i32) -> Self {
        let mut set: HashSet<i32> = (*self.quitting).clone();
        set.insert(pid);
        self.quitting = Arc::new(set);
        self
    }

    /// 对应 `new HashSet<int>(State.Quitting)` 后 `Remove(pid)`：即使没删掉也会换实例。
    pub fn with_quitting_removed(mut self, pid: i32) -> Self {
        let mut set: HashSet<i32> = (*self.quitting).clone();
        set.remove(&pid);
        self.quitting = Arc::new(set);
        self
    }

    /// 对应 `new HashSet<WindowKey>(State.Closing) { key }`。
    pub fn with_closing_added(mut self, key: WindowKey) -> Self {
        let mut set: HashSet<WindowKey> = (*self.closing).clone();
        set.insert(key);
        self.closing = Arc::new(set);
        self
    }

    /// 对应 `new HashSet<WindowKey>(State.Closing)` 后 `Remove(key)`。
    pub fn with_closing_removed(mut self, key: WindowKey) -> Self {
        let mut set: HashSet<WindowKey> = (*self.closing).clone();
        set.remove(&key);
        self.closing = Arc::new(set);
        self
    }

    pub fn with_closing_cleared(mut self) -> Self {
        self.closing = Arc::new(HashSet::new());
        self
    }

    pub fn with_quitting_cleared(mut self) -> Self {
        self.quitting = Arc::new(HashSet::new());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(app_id: Option<&str>) -> WindowInfo {
        WindowInfo {
            key: WindowKey::new(42, 0x100),
            app_name: "Code".into(),
            app_id: app_id.map(str::to_string),
            title: "README.md".into(),
            minimized: false,
        }
    }

    #[test]
    fn default_state_matches_csharp_initializers() {
        let state = DeskState::default();
        assert_eq!(state.screen_phase, PhoneScreenPhase::Off);
        assert_eq!(state.display_id, None);
        assert_eq!(state.placed, None);
        assert!(state.list_visible, "C# 默认 ListVisible = true");
        assert_eq!(state.work, None);
        assert_eq!(state.notice, None);
        assert!(state.quitting.is_empty());
        assert!(state.closing.is_empty());
        assert!(!state.dismissed);
        assert_eq!(state.close_button, None);
    }

    #[test]
    fn state_equality_mirrors_csharp_record_semantics() {
        let a = DeskState::default();
        // C# 里两个各自 new 出来的集合是不同实例，所以两个默认状态不相等。
        let b = DeskState::default();
        assert_ne!(a, b);
        // clone 共享同一个 Arc / 同一个 HashSet 实例，相等（`State with {}` 的效果）。
        assert_eq!(a.clone(), a);
        assert_eq!(a.clone().with_notice(Some(DeskNotice::Reshaped)), a.clone().with_notice(Some(DeskNotice::Reshaped)));
        // 集合内容相同但换了实例 → 仍算"变了"。
        assert_ne!(a.clone(), a.clone().with_quitting_cleared());
        assert_ne!(a.clone().with_quitting_added(7), a.clone());
        assert_eq!(
            a.clone().with_quitting_added(7).with_quitting_removed(7).quitting.len(),
            0
        );
        // 加进去再删掉，内容回到原样，但因为换过实例，仍不等于初始状态。
        assert_ne!(a.clone(), a.clone().with_quitting_added(7).with_quitting_removed(7));
        // 删一个不存在的 pid 也会换实例（C# RemoveQuitting 每次都 new HashSet）。
        assert_ne!(a.clone(), a.clone().with_quitting_removed(99));
    }

    #[test]
    fn quitting_and_closing_sets_track_membership() {
        let key = WindowKey::new(1, 2);
        let state = DeskState::default()
            .with_quitting_added(10)
            .with_quitting_added(11)
            .with_closing_added(key);
        assert!(state.quitting.contains(&10));
        assert!(state.quitting.contains(&11));
        assert!(state.closing.contains(&key));

        let state = state.with_quitting_removed(10).with_closing_removed(key);
        assert!(!state.quitting.contains(&10));
        assert!(state.quitting.contains(&11));
        assert!(state.closing.is_empty());
    }

    #[test]
    fn placed_window_zoomable_uses_zoom_book_table() {
        let zoomable = PlacedWindow { window: window(Some("Code.exe")), zoom: 2 };
        assert!(zoomable.zoomable(), "Code.exe 在工作区里记字号，可调");
        // 进程名大小写不敏感。
        let upper = PlacedWindow { window: window(Some("WINDOWSTERMINAL.EXE")), zoom: 0 };
        assert!(upper.zoomable());
        let plain = PlacedWindow { window: window(Some("notepad.exe")), zoom: 2 };
        assert!(!plain.zoomable());
        let unknown = PlacedWindow { window: window(None), zoom: 2 };
        assert!(!unknown.zoomable());
    }

    #[test]
    fn desk_work_keeps_optional_label() {
        assert_eq!(DeskWork::new(DeskWorkKind::Zooming).label, None);
        let placing = DeskWork::with_label(DeskWorkKind::Placing, "Code「a.rs」");
        assert_eq!(placing.kind, DeskWorkKind::Placing);
        assert_eq!(placing.label.as_deref(), Some("Code「a.rs」"));
    }

    #[test]
    fn notices_compare_by_fields() {
        assert_eq!(
            DeskNotice::QuitStalled { app: "Code".into(), pid: 7 },
            DeskNotice::QuitStalled { app: "Code".into(), pid: 7 }
        );
        assert_ne!(
            DeskNotice::QuitStalled { app: "Code".into(), pid: 7 },
            DeskNotice::QuitStalled { app: "Code".into(), pid: 8 }
        );
        assert_eq!(DeskGone::App { pid: 7 }, DeskGone::App { pid: 7 });
        assert_ne!(DeskGone::App { pid: 7 }, DeskGone::Window { key: WindowKey::new(7, 1) });
    }
}
