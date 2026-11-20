//! UU 远程会话状态机。
//!
//! 移植自 C# 命名空间 `SnugWin.Core.Session` 的五个文件：
//! `UUSignals.cs`（`ControllerKind`/`ControllerClassifier`/`ControllerTable`/`UUSignals`/
//! `ConnectionEvent`）、`ConnectionTracker.cs`、`SessionEpisode.cs`、`RemoteSession.cs`、
//! `ControllerTable.cs`（`UuElement`/`ControllerTableParser`）。
//!
//! 时间参数沿用 core 其它模块的约定：`f64` 秒（C# 用 `DateTimeOffset`）。
//! C# 的 `Log.Info` 诊断输出没有移植，状态与事件保持一致。

use std::collections::{HashMap, HashSet};

// ---- UU 信号（UUSignals.cs） ----

/// 控制端设备类型（对应 C# `ControllerKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerKind {
    Phone,
    Tablet,
    Desktop,
    Unknown,
}

/// 按设备名猜设备类型（对应 C# `ControllerClassifier`）。
pub fn classify(device_name: &str) -> ControllerKind {
    if device_name.trim().is_empty() {
        return ControllerKind::Unknown;
    }
    let lower = device_name.to_lowercase();
    if lower.contains("ipad")
        || lower.contains("pad")
        || lower.contains("tablet")
        || lower.contains("平板")
    {
        return ControllerKind::Tablet;
    }
    if lower.contains("iphone")
        || lower.contains("phone")
        || lower.contains("android")
        || lower.contains("手机")
        || lower.contains("xiaomi")
        || lower.contains("huawei")
        || lower.contains("oppo")
        || lower.contains("vivo")
        || lower.contains("redmi")
        || lower.contains("oneplus")
        || lower.contains("pixel")
        || lower.contains("samsung")
    {
        return ControllerKind::Phone;
    }
    if lower.contains("mac")
        || lower.contains("windows")
        || lower.contains("pc")
        || lower.contains("desktop")
        || lower.contains("laptop")
        || lower.contains("电脑")
        || lower.contains("win")
    {
        return ControllerKind::Desktop;
    }
    ControllerKind::Unknown
}

/// 这次远程要不要开手机屏：没有任何控制端时默认开；有非桌面端就开（对应 C# `WantsPhoneScreen`）。
pub fn wants_phone_screen(controllers: &[String]) -> bool {
    controllers.is_empty() || controllers.iter().any(|name| classify(name) != ControllerKind::Desktop)
}

/// 控制端列表（对应 C# `ControllerTable`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerTable {
    pub names: Vec<String>,
    /// 界面能不能读到这张表；`false` 表示"读不到"（而不是"没有控制端"）。
    pub readable: bool,
}

impl ControllerTable {
    /// 读不到（C# `Unreadable`）。
    pub fn unreadable() -> Self {
        Self { names: Vec::new(), readable: false }
    }

    /// 已读到“本机被控中”控制表，但没有远程控制端（C# `Unrecognized`）。
    pub fn unrecognized() -> Self {
        Self { names: Vec::new(), readable: true }
    }

    pub fn from_names(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { names: names.into_iter().map(Into::into).collect(), readable: true }
    }
}

impl std::fmt::Display for ControllerTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.readable {
            return f.write_str("不可读");
        }
        if self.names.is_empty() {
            return f.write_str("[]");
        }
        write!(f, "[{}]", self.names.join(", "))
    }
}

/// 一次 UU 界面采样（对应 C# `UUSignals`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UUSignals {
    pub overlay_visible: bool,
    pub controllers: ControllerTable,
    /// `None` 表示读不到 capture 状态。
    pub capture_active: Option<bool>,
}

impl UUSignals {
    /// 足以开始或恢复远程连接的证据。
    ///
    /// Server 进程的可见窗口只是尺寸启发式，无法区分覆盖层与其它游戏窗口，
    /// 所以只有 UIA 读到被控端控制表中的远程控制行才可准入。
    pub fn has_remote_evidence(&self) -> bool {
        self.controllers.readable && !self.controllers.names.is_empty()
    }

    /// 采样中包含在场迹象。状态机还会检查日志是否有资格维持已有连接。
    pub fn is_present(&self) -> bool {
        self.has_remote_evidence() || self.capture_active == Some(true)
    }
}

impl std::fmt::Display for UUSignals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let capture = match self.capture_active {
            Some(true) => "是",
            Some(false) => "否",
            None => "未知",
        };
        write!(
            f,
            "overlay={} 控制端={} capture={capture}",
            if self.overlay_visible { "是" } else { "否" },
            self.controllers
        )
    }
}

/// 连接状态变化（对应 C# `ConnectionEvent`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionEvent {
    Connected(Vec<String>),
    ControllersChanged(Vec<String>),
    Disconnected,
}

// ---- 连接跟踪（ConnectionTracker.cs） ----

/// 断开防抖：短暂消失不算断开。
pub const DISCONNECT_DEBOUNCE: f64 = 1.5;
/// 自己动过 UI 之后的静默期，期间不判定断开。
pub const QUIET_WINDOW: f64 = 5.0;

/// 推流日志可能包含本地 GameViewer 活动，不能独自无限延长远程会话。
/// 它只在最近一次控制端证据后短暂维持未知状态；已确认的空控制表不接受日志续期。
const CAPTURE_CONTINUITY_WINDOW: f64 = 15.0;

/// 连接跟踪（对应 C# `ConnectionTracker`）。
#[derive(Debug, Clone)]
pub struct ConnectionTracker {
    is_connected: bool,
    controllers: Vec<String>,
    last_present_at: Option<f64>,
    last_remote_evidence_at: Option<f64>,
    quiet_until: Option<f64>,
}

impl Default for ConnectionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionTracker {
    pub fn new() -> Self {
        Self {
            is_connected: false,
            controllers: Vec::new(),
            last_present_at: None,
            last_remote_evidence_at: None,
            quiet_until: None,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.is_connected
    }

    pub fn controllers(&self) -> &[String] {
        &self.controllers
    }

    /// 自己改了 UU 界面（比如收起了浮层），接下来 5 秒内的"消失"不算断开。
    pub fn note_self_induced_change(&mut self, now: f64) {
        let until = now + QUIET_WINDOW;
        self.quiet_until = match self.quiet_until {
            Some(quiet) if quiet > until => Some(quiet),
            _ => Some(until),
        };
    }

    /// 喂一次采样。日志心跳只能短暂维持已有连接，不能开始或恢复连接。
    pub fn update(&mut self, signals: &UUSignals, now: f64) -> Option<ConnectionEvent> {
        let remote_evidence = signals.has_remote_evidence();
        if remote_evidence {
            self.last_remote_evidence_at = Some(now);
        }

        // 本地 GameViewer 也会刷新推流日志。掉线后必须重新观察到远端证据才能恢复，
        // 否则本地日志会把 interrupted episode 不断复活，让虚拟屏永远留着。
        if !self.is_connected && !remote_evidence {
            return None;
        }

        let capture_is_recent = self
            .last_remote_evidence_at
            .is_some_and(|last| now - last <= CAPTURE_CONTINUITY_WINDOW);
        let present = remote_evidence
            || (!signals.controllers.readable
                && signals.capture_active == Some(true)
                && capture_is_recent);
        let names = signals.controllers.names.clone();
        if present {
            self.last_present_at = Some(now);
        }

        if !self.is_connected {
            self.is_connected = true;
            self.controllers = names.clone();
            return Some(ConnectionEvent::Connected(names));
        }

        if present {
            if names.is_empty() || same_set(&names, &self.controllers) {
                return None;
            }
            self.controllers = names.clone();
            return Some(ConnectionEvent::ControllersChanged(names));
        }

        let Some(last_present) = self.last_present_at else {
            return None;
        };
        if now - last_present <= DISCONNECT_DEBOUNCE || !self.quiet_passed(now) {
            return None;
        }

        self.is_connected = false;
        self.controllers = Vec::new();
        Some(ConnectionEvent::Disconnected)
    }

    fn quiet_passed(&self, now: f64) -> bool {
        match self.quiet_until {
            None => true,
            Some(quiet) => now > quiet,
        }
    }
}

fn same_set(left: &[String], right: &[String]) -> bool {
    let left: HashSet<&str> = left.iter().map(String::as_str).collect();
    let right: HashSet<&str> = right.iter().map(String::as_str).collect();
    left == right
}

// ---- 会话片段（SessionEpisode.cs） ----

/// 重连窗口：断开后这么久以内回来算"同一段会话"。
pub const RECONNECT_WINDOW: f64 = 15.0;

/// 会话片段的阶段变化（对应 C# `EpisodeChange`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeChange {
    Began,
    Resumed,
    Interrupted,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Live,
    Interrupted,
}

/// 一次远程会话（对应 C# `SessionEpisode`）：短暂断开不结束，超时才算结束。
#[derive(Debug, Clone)]
pub struct SessionEpisode {
    phase: Phase,
    interrupted_since: f64,
}

impl Default for SessionEpisode {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionEpisode {
    pub fn new() -> Self {
        Self { phase: Phase::Idle, interrupted_since: 0.0 }
    }

    pub fn is_active(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// 连上了（对应 C# `Connected`）。
    pub fn connected(&mut self, now: f64) -> EpisodeChange {
        if self.advance(now) == Some(EpisodeChange::Ended) {
            return self.begin();
        }
        match self.phase {
            Phase::Idle => self.begin(),
            Phase::Live | Phase::Interrupted => {
                self.phase = Phase::Live;
                EpisodeChange::Resumed
            }
        }
    }

    /// 断开了（对应 C# `Disconnected`）。
    pub fn disconnected(&mut self, now: f64) -> Option<EpisodeChange> {
        if self.phase != Phase::Live {
            return None;
        }
        self.phase = Phase::Interrupted;
        self.interrupted_since = now;
        Some(EpisodeChange::Interrupted)
    }

    /// 推进时间：断开超过重连窗口就结束（对应 C# `Advance`）。
    pub fn advance(&mut self, now: f64) -> Option<EpisodeChange> {
        if self.phase != Phase::Interrupted || now - self.interrupted_since <= RECONNECT_WINDOW {
            return None;
        }
        self.phase = Phase::Idle;
        Some(EpisodeChange::Ended)
    }

    fn begin(&mut self) -> EpisodeChange {
        self.phase = Phase::Live;
        EpisodeChange::Began
    }
}

// ---- 远程会话（RemoteSession.cs） ----

/// 会话事件（对应 C# `SessionTransition`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionTransition {
    Began(Vec<String>),
    Resumed(Vec<String>),
    ControllersChanged(Vec<String>),
    Interrupted,
    Ended,
}

/// 把连接跟踪与会话片段合起来（对应 C# `RemoteSession`）。
#[derive(Debug, Clone)]
pub struct RemoteSession {
    tracker: ConnectionTracker,
    episode: SessionEpisode,
    group: Vec<String>,
    departed: HashMap<String, f64>,
    episode_began_at: Option<f64>,
}

impl Default for RemoteSession {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteSession {
    pub fn new() -> Self {
        Self {
            tracker: ConnectionTracker::new(),
            episode: SessionEpisode::new(),
            group: Vec::new(),
            departed: HashMap::new(),
            episode_began_at: None,
        }
    }

    pub fn tracker(&self) -> &ConnectionTracker {
        &self.tracker
    }

    pub fn episode(&self) -> &SessionEpisode {
        &self.episode
    }

    pub fn is_connected(&self) -> bool {
        self.tracker.is_connected()
    }

    pub fn controllers(&self) -> &[String] {
        self.tracker.controllers()
    }

    pub fn in_episode(&self) -> bool {
        self.episode.is_active()
    }

    pub fn episode_began_at(&self) -> Option<f64> {
        self.episode_began_at
    }

    /// 这次会话里出现过的控制端 + 15 秒内刚离开的（对应 C# `EpisodeControllers`）。
    pub fn episode_controllers(&self, now: f64) -> Vec<String> {
        let mut lingering: Vec<&String> = self
            .departed
            .iter()
            .filter(|(_, departed_at)| now - **departed_at <= RECONNECT_WINDOW)
            .map(|(name, _)| name)
            .collect();
        lingering.sort();
        self.group.iter().chain(lingering.into_iter()).cloned().collect()
    }

    /// 喂一次采样，返回这次发生的事件（对应 C# `Update`）。
    pub fn update(&mut self, signals: &UUSignals, now: f64) -> Vec<SessionTransition> {
        let mut transitions = Vec::new();
        if self.episode.advance(now) == Some(EpisodeChange::Ended) {
            transitions.push(SessionTransition::Ended);
            self.group.clear();
            self.departed.clear();
            self.episode_began_at = None;
        }

        match self.tracker.update(signals, now) {
            Some(ConnectionEvent::Connected(controllers)) => {
                if self.episode.connected(now) == EpisodeChange::Began {
                    self.episode_began_at = Some(now);
                    self.group = controllers.clone();
                    self.departed.clear();
                    transitions.push(SessionTransition::Began(self.group.clone()));
                } else {
                    if !controllers.is_empty() {
                        self.regroup(&controllers, now);
                    }
                    transitions.push(SessionTransition::Resumed(controllers));
                }
            }
            Some(ConnectionEvent::ControllersChanged(controllers)) => {
                self.regroup(&controllers, now);
                transitions.push(SessionTransition::ControllersChanged(controllers));
            }
            Some(ConnectionEvent::Disconnected) => {
                if self.episode.disconnected(now) == Some(EpisodeChange::Interrupted) {
                    transitions.push(SessionTransition::Interrupted);
                }
            }
            None => {}
        }

        transitions
    }

    pub fn note_self_induced_change(&mut self, now: f64) {
        self.tracker.note_self_induced_change(now);
    }

    fn regroup(&mut self, names: &[String], now: f64) {
        let next: HashSet<&String> = names.iter().collect();
        for old in &self.group {
            if !next.contains(old) {
                self.departed.insert(old.clone(), now);
            }
        }
        for name in names {
            self.departed.remove(name);
        }
        self.group = names.to_vec();
    }
}

// ---- UU 界面元素与"本机被控中"表格解析（ControllerTable.cs） ----

/// UIA 读出来的一棵元素树（对应 C# `UuElement`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UuElement {
    Text { text: String },
    Table { children: Vec<UuElement> },
    Row { children: Vec<UuElement> },
    Other { children: Vec<UuElement> },
}

impl UuElement {
    pub fn text_node(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn table_node(children: Vec<UuElement>) -> Self {
        Self::Table { children }
    }

    pub fn row_node(children: Vec<UuElement>) -> Self {
        Self::Row { children }
    }

    pub fn other_node(children: Vec<UuElement>) -> Self {
        Self::Other { children }
    }

    fn children(&self) -> &[UuElement] {
        match self {
            Self::Text { .. } => &[],
            Self::Table { children } | Self::Row { children } | Self::Other { children } => children,
        }
    }

    /// 自己是一行就把自己算上，再递归子节点（对应 C# `Rows`）。
    fn rows(&self) -> Vec<&UuElement> {
        let mut result = Vec::new();
        if matches!(self, Self::Row { .. }) {
            result.push(self);
        }
        for child in self.children() {
            result.extend(child.rows());
        }
        result
    }

    /// 所有非空文本节点（对应 C# `Texts`）。
    fn texts(&self) -> Vec<&str> {
        let mut result = Vec::new();
        if let Self::Text { text } = self
            && !text.is_empty()
        {
            result.push(text.as_str());
        }
        for child in self.children() {
            result.extend(child.texts());
        }
        result
    }
}

/// "本机被控中"页面的标题文字。
pub const PAGE_TITLE: &str = "本机被控中";
/// 行里必须出现这个标记才算远程控制行。
pub const CONTROL_MARK: &str = "远程控制";
/// 时长列的前缀。
pub const DURATION_PREFIX: &str = "连接时长";

/// 从 UIA 树里解析控制端（对应 C# `ControllerTableParser`）。
///
/// 只认"本机被控中"标题下面直接挂着的那张表，且行里必须出现"远程控制"，
/// 避免设置页的文字被当成控制端。
pub struct ControllerTableParser;

impl ControllerTableParser {
    pub fn parse_windows(windows: &[UuElement]) -> ControllerTable {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut found_controlled_table = false;
        for window in windows {
            for table in local_tables(window) {
                found_controlled_table = true;
                for row in table.rows() {
                    rows.push(row.texts().into_iter().map(str::to_string).collect());
                }
            }
        }
        if found_controlled_table {
            Self::parse_rows(rows)
        } else {
            // A readable settings window says nothing about a hidden controlled page.
            ControllerTable::unreadable()
        }
    }

    pub fn parse_rows(rows: impl IntoIterator<Item = Vec<String>>) -> ControllerTable {
        let mut names: Vec<String> = Vec::new();
        for texts in rows {
            if !texts.iter().any(|text| text == CONTROL_MARK) {
                continue;
            }
            // 控制端名字：第一个非空、不是"远程控制"、也不是"连接时长 …"的文本。
            let name = texts.iter().find(|text| {
                !text.trim().is_empty()
                    && *text != CONTROL_MARK
                    && !text.starts_with(DURATION_PREFIX)
            });
            if let Some(name) = name
                && !names.contains(name)
            {
                names.push(name.clone());
            }
        }
        if names.is_empty() {
            ControllerTable::unrecognized()
        } else {
            ControllerTable::from_names(names)
        }
    }
}

fn local_tables(element: &UuElement) -> Vec<&UuElement> {
    let mut result = Vec::new();
    let has_header = element
        .children()
        .iter()
        .any(|child| matches!(child, UuElement::Text { text } if text == PAGE_TITLE));
    if has_header {
        for child in element.children() {
            if matches!(child, UuElement::Table { .. }) {
                result.push(child);
            }
        }
    }
    for child in element.children() {
        result.extend(local_tables(child));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 转写自 C# `SessionTests.ConnectionTracker_DebouncesDisconnectAndRespectsQuietWindow`。
    #[test]
    fn connection_tracker_debounces_disconnect_and_respects_quiet_window() {
        let mut tracker = ConnectionTracker::new();
        let t0 = 0.0;

        let first = tracker.update(&present_signals(), t0);
        assert_eq!(first, Some(ConnectionEvent::Connected(vec!["iPhone 17".into()])));
        assert!(tracker.is_connected());

        // 短暂消失 < 1.5s 不断开。
        assert_eq!(tracker.update(&absent(), t0 + 1.0), None);
        assert!(tracker.is_connected());

        // 自己动过 UI 后的 5 秒静默期也不断开。
        tracker.note_self_induced_change(t0 + 1.0);
        assert_eq!(tracker.update(&absent(), t0 + 4.0), None);
        assert!(tracker.is_connected());

        // 静默期 + 防抖都过去才断开。
        assert_eq!(tracker.update(&absent(), t0 + 7.0), Some(ConnectionEvent::Disconnected));
        assert!(!tracker.is_connected());
    }

    #[test]
    fn visible_server_window_without_a_remote_controller_cannot_start_a_session() {
        let mut tracker = ConnectionTracker::new();
        let local_game_window = UUSignals {
            overlay_visible: true,
            controllers: ControllerTable::unreadable(),
            capture_active: Some(true),
        };

        for now in [0.0, 1.0, 2.0, 20.0] {
            assert_eq!(tracker.update(&local_game_window, now), None);
            assert!(!tracker.is_connected());
        }
    }

    #[test]
    fn connection_tracker_reports_controller_changes() {
        let mut tracker = ConnectionTracker::new();
        let present = |names: &[&str]| UUSignals {
            overlay_visible: true,
            controllers: ControllerTable::from_names(names.iter().copied()),
            capture_active: None,
        };

        assert_eq!(
            tracker.update(&present(&["iPhone 17"]), 0.0),
            Some(ConnectionEvent::Connected(vec!["iPhone 17".into()]))
        );
        // 名字顺序变了不算变化（集合相等）。
        assert_eq!(tracker.update(&present(&["iPhone 17"]), 1.0), None);
        assert_eq!(
            tracker.update(&present(&["iPhone 17", "iPad Air"]), 2.0),
            Some(ConnectionEvent::ControllersChanged(vec![
                "iPhone 17".into(),
                "iPad Air".into()
            ]))
        );
    }

    #[test]
    fn quiet_window_extends_to_the_latest_self_induced_change() {
        let mut tracker = ConnectionTracker::new();
        tracker.update(&present_signals(), 0.0);
        tracker.note_self_induced_change(1.0); // 静默到 6.0
        tracker.note_self_induced_change(3.0); // 延到 8.0
        assert_eq!(tracker.update(&absent(), 7.0), None, "仍在后一次静默期内");
        assert_eq!(tracker.update(&absent(), 8.5), Some(ConnectionEvent::Disconnected));
    }

    /// 转写自 C# `SessionTests.RemoteSession_PreservesEpisodeAcrossBriefDisconnect`。
    #[test]
    fn remote_session_preserves_episode_across_brief_disconnect() {
        let mut session = RemoteSession::new();
        let t0 = 0.0;
        let connected = |names: &[&str]| UUSignals {
            overlay_visible: true,
            controllers: ControllerTable::from_names(names.iter().copied()),
            capture_active: Some(true),
        };

        let began = session.update(&connected(&["iPhone 17"]), t0);
        assert_eq!(began, vec![SessionTransition::Began(vec!["iPhone 17".into()])]);
        assert!(session.in_episode());
        assert_eq!(session.episode_began_at(), Some(t0));

        let interrupted = session.update(&absent(), t0 + 2.0);
        assert_eq!(interrupted, vec![SessionTransition::Interrupted]);
        assert!(session.in_episode());

        // 15 秒内回来 → Resumed。
        let resumed = session.update(&connected(&["iPhone 17"]), t0 + 10.0);
        assert_eq!(resumed, vec![SessionTransition::Resumed(vec!["iPhone 17".into()])]);
        assert!(session.in_episode());

        // 再断开并超过 15 秒 → Ended。
        session.update(&absent(), t0 + 12.0);
        let ended = session.update(&absent(), t0 + 30.0);
        assert_eq!(ended, vec![SessionTransition::Ended]);
        assert!(!session.in_episode());
        assert_eq!(session.episode_began_at(), None);
        assert!(session.episode_controllers(t0 + 30.0).is_empty());
    }

    #[test]
    fn capture_log_cannot_start_a_session_without_remote_evidence() {
        let mut session = RemoteSession::new();
        let local_game_log = UUSignals {
            overlay_visible: false,
            controllers: ControllerTable::unreadable(),
            capture_active: Some(true),
        };

        for now in [0.0, 1.0, 3.0, 20.0] {
            assert!(session.update(&local_game_log, now).is_empty());
            assert!(!session.is_connected());
            assert!(!session.in_episode());
        }
    }

    #[test]
    fn capture_log_cannot_extend_a_live_session_indefinitely() {
        let mut session = RemoteSession::new();
        let admitted = UUSignals {
            overlay_visible: true,
            controllers: ControllerTable::from_names(["iPhone 17"]),
            capture_active: Some(true),
        };
        let capture_only = UUSignals {
            overlay_visible: false,
            controllers: ControllerTable::unreadable(),
            capture_active: Some(true),
        };
        assert_eq!(
            session.update(&admitted, 0.0),
            vec![SessionTransition::Began(vec!["iPhone 17".into()])]
        );
        for tick in 1..=16 {
            assert!(session.update(&capture_only, f64::from(tick)).is_empty());
        }
        assert!(session.is_connected());
        assert_eq!(
            session.update(&capture_only, 17.0),
            vec![SessionTransition::Interrupted]
        );
        assert!(session.in_episode());
        for tick in 18..=32 {
            assert!(session.update(&capture_only, f64::from(tick)).is_empty());
            assert!(!session.is_connected());
        }
        assert_eq!(session.update(&capture_only, 33.0), vec![SessionTransition::Ended]);
        assert!(!session.in_episode());
    }

    #[test]
    fn capture_log_cannot_resume_after_disconnect_even_with_recent_remote_evidence() {
        let mut session = RemoteSession::new();
        let admitted = present_signals();
        let capture_only = UUSignals { capture_active: Some(true), ..absent() };

        session.update(&admitted, 0.0);
        assert_eq!(
            session.update(&absent(), 2.0),
            vec![SessionTransition::Interrupted]
        );
        assert!(session.update(&capture_only, 3.0).is_empty());
        assert!(!session.is_connected());
        assert_eq!(
            session.update(&admitted, 4.0),
            vec![SessionTransition::Resumed(admitted.controllers.names)]
        );
        assert!(session.is_connected());
    }

    #[test]
    fn a_confirmed_empty_controller_table_overrides_a_lingering_capture_log() {
        let mut tracker = ConnectionTracker::new();
        tracker.update(&present_signals(), 0.0);
        let disconnected_page = UUSignals {
            overlay_visible: true,
            controllers: ControllerTable::unrecognized(),
            capture_active: Some(true),
        };

        assert_eq!(tracker.update(&disconnected_page, 1.0), None, "仍按原防抖等待");
        assert_eq!(
            tracker.update(&disconnected_page, 2.0),
            Some(ConnectionEvent::Disconnected),
            "已读到的空控制表优先于本地日志心跳"
        );
        assert_eq!(tracker.update(&disconnected_page, 3.0), None);
        assert!(!tracker.is_connected(), "空表和心跳不能恢复远程会话");
    }

    #[test]
    fn switching_to_the_settings_page_is_unknown_instead_of_an_empty_remote_table() {
        let settings_tree = UuElement::other_node(vec![
            UuElement::text_node("设置"),
            UuElement::table_node(vec![UuElement::row_node(vec![
                UuElement::text_node("隐藏至菜单栏"),
            ])]),
        ]);
        let unknown = ControllerTableParser::parse_windows(&[settings_tree]);
        assert!(!unknown.readable, "未找到被控表，不能宣称已经确认断开");

        let mut tracker = ConnectionTracker::new();
        tracker.update(&present_signals(), 0.0);
        let temporarily_unobservable = UUSignals {
            overlay_visible: false,
            controllers: unknown,
            capture_active: Some(true),
        };
        assert_eq!(tracker.update(&temporarily_unobservable, 10.0), None);
        assert!(tracker.is_connected(), "未知信号可以使用有界心跳连续性");
    }

    #[test]
    fn an_empty_controlled_page_is_readable_and_never_admits_a_session() {
        let empty_controlled_page = UuElement::other_node(vec![
            UuElement::text_node(PAGE_TITLE),
            UuElement::table_node(Vec::new()),
        ]);
        let table = ControllerTableParser::parse_windows(&[empty_controlled_page]);
        assert!(table.readable);
        assert!(table.names.is_empty());
        let mut session = RemoteSession::new();
        for now in [0.0, 1.0, 10.0, 30.0] {
            let signals = UUSignals { overlay_visible: true, controllers: table.clone(), capture_active: Some(true) };
            assert!(session.update(&signals, now).is_empty());
            assert!(!session.in_episode());
        }
    }

    #[test]
    fn readable_remote_controller_table_admits_a_session_without_overlay() {
        let mut session = RemoteSession::new();
        let connected = UUSignals {
            overlay_visible: false,
            controllers: ControllerTable::from_names(["iPhone (Phone)"]),
            capture_active: None,
        };

        assert_eq!(
            session.update(&connected, 0.0),
            vec![SessionTransition::Began(vec!["iPhone (Phone)".into()])]
        );
        assert!(session.in_episode());
    }

    #[test]
    fn capture_log_cannot_restart_a_session_after_its_reconnect_episode_ended() {
        let mut session = RemoteSession::new();
        let admitted = UUSignals {
            overlay_visible: true,
            controllers: ControllerTable::from_names(["iPhone 17"]),
            capture_active: Some(true),
        };
        let quiet = UUSignals {
            overlay_visible: false,
            controllers: ControllerTable::unreadable(),
            capture_active: Some(false),
        };
        let local_game_log = UUSignals {
            capture_active: Some(true),
            ..quiet.clone()
        };

        session.update(&admitted, 0.0);
        session.update(&quiet, 2.0);
        assert_eq!(session.update(&quiet, 18.0), vec![SessionTransition::Ended]);
        assert!(session.update(&local_game_log, 19.0).is_empty());
        assert!(!session.is_connected());
        assert!(!session.in_episode());
    }

    #[test]
    fn episode_uses_a_strict_fifteen_second_window() {
        let mut episode = SessionEpisode::new();
        assert_eq!(episode.connected(0.0), EpisodeChange::Began);
        assert_eq!(episode.disconnected(1.0), Some(EpisodeChange::Interrupted));
        // 正好 15 秒还不算结束（C# 是 `<=`）。
        assert_eq!(episode.advance(16.0), None);
        assert_eq!(episode.advance(16.001), Some(EpisodeChange::Ended));
        // 断开两次只有第一次有效。
        assert_eq!(episode.disconnected(20.0), None);
    }

    #[test]
    fn episode_controllers_keep_recent_departures() {
        let mut session = RemoteSession::new();
        session.update(&present_signals(), 0.0);
        // 控制端从两个变成一个：离开的那个保留 15 秒。
        let two = UUSignals {
            overlay_visible: true,
            controllers: ControllerTable::from_names(["iPhone 17", "iPad Air"]),
            capture_active: None,
        };
        session.update(&two, 1.0);
        let one = UUSignals {
            overlay_visible: true,
            controllers: ControllerTable::from_names(["iPhone 17"]),
            capture_active: None,
        };
        session.update(&one, 2.0);
        assert_eq!(
            session.episode_controllers(10.0),
            vec!["iPhone 17".to_string(), "iPad Air".to_string()]
        );
        assert_eq!(session.episode_controllers(20.0), vec!["iPhone 17".to_string()]);
    }

    /// 转写自 C# `SessionTests.ControllerClassifier_DistinguishesPhoneAndDesktop`。
    #[test]
    fn controller_classifier_distinguishes_phone_and_desktop() {
        assert_eq!(classify("iPhone 16 Pro"), ControllerKind::Phone);
        assert_eq!(classify("iPad Air"), ControllerKind::Tablet);
        assert_eq!(classify("DESKTOP-PC (Windows)"), ControllerKind::Desktop);
        assert_eq!(classify(""), ControllerKind::Unknown);
        assert_eq!(classify("Pixel 9"), ControllerKind::Phone);
        assert_eq!(classify("小米平板"), ControllerKind::Tablet);
        assert_eq!(classify("我的电脑"), ControllerKind::Desktop);

        assert!(!wants_phone_screen(&["My MacBook Pro".to_string()]));
        assert!(wants_phone_screen(&["My MacBook Pro".to_string(), "iPhone 17".to_string()]));
        assert!(wants_phone_screen(&[]), "没有控制端时默认开手机屏");
    }

    /// 转写自 C# `SessionTests.ControllerTableParser_IgnoresSettingsRowsAndRequiresControlledTable`。
    #[test]
    fn controller_table_parser_ignores_settings_rows_and_requires_controlled_table() {
        let settings_tree = UuElement::other_node(vec![
            UuElement::text_node("设置"),
            UuElement::table_node(vec![
                UuElement::row_node(vec![UuElement::text_node("隐藏至菜单栏")]),
                UuElement::row_node(vec![UuElement::text_node("仅使用临时验证码")]),
            ]),
        ]);
        let unrecognized = ControllerTableParser::parse_windows(&[settings_tree]);
        assert!(unrecognized.names.is_empty());
        assert!(!unrecognized.readable, "设置页不是已确认的空控制表");

        let controlled_tree = UuElement::other_node(vec![
            UuElement::text_node("本机被控中"),
            UuElement::table_node(vec![
                UuElement::row_node(vec![
                    UuElement::text_node("iPhone 17 Pro"),
                    UuElement::text_node("远程控制"),
                    UuElement::text_node("连接时长 00:12"),
                ]),
                UuElement::row_node(vec![
                    UuElement::text_node("MacBook"),
                    UuElement::text_node("远程协助"),
                ]),
            ]),
        ]);
        let recognized = ControllerTableParser::parse_windows(&[controlled_tree]);
        assert_eq!(recognized.names, vec!["iPhone 17 Pro".to_string()]);
        assert!(recognized.readable);
    }

    #[test]
    fn controller_table_parser_reads_nested_tables_under_the_header() {
        // 表可能挂在标题的兄弟节点的子树里。
        let tree = UuElement::other_node(vec![UuElement::other_node(vec![
            UuElement::text_node("本机被控中"),
            UuElement::table_node(vec![UuElement::row_node(vec![
                UuElement::text_node("iPad Air"),
                UuElement::text_node("远程控制"),
            ])]),
        ])]);
        assert_eq!(
            ControllerTableParser::parse_windows(&[tree]).names,
            vec!["iPad Air".to_string()]
        );

        // 行里只有"远程控制"和时长时，没有名字 → 读到了但没有控制端。
        let only_marker = UuElement::table_node(vec![UuElement::row_node(vec![
            UuElement::text_node("远程控制"),
            UuElement::text_node("连接时长 00:01"),
        ])]);
        let tree = UuElement::other_node(vec![UuElement::text_node("本机被控中"), only_marker]);
        let table = ControllerTableParser::parse_windows(&[tree]);
        assert!(table.names.is_empty());
        assert!(table.readable, "读得到表，只是没认出控制端");
    }

    #[test]
    fn display_impls_match_csharp() {
        assert_eq!(ControllerTable::unreadable().to_string(), "不可读");
        assert_eq!(ControllerTable::unrecognized().to_string(), "[]");
        assert_eq!(
            ControllerTable::from_names(["iPhone 17", "iPad"]).to_string(),
            "[iPhone 17, iPad]"
        );
        let signals = UUSignals {
            overlay_visible: true,
            controllers: ControllerTable::from_names(["iPhone 17"]),
            capture_active: None,
        };
        assert_eq!(signals.to_string(), "overlay=是 控制端=[iPhone 17] capture=未知");
        assert!(signals.is_present());
    }

    fn absent() -> UUSignals {
        UUSignals {
            overlay_visible: false,
            controllers: ControllerTable::unreadable(),
            capture_active: Some(false),
        }
    }

    fn present_signals() -> UUSignals {
        UUSignals {
            overlay_visible: true,
            controllers: ControllerTable::from_names(["iPhone 17"]),
            capture_active: None,
        }
    }
}
