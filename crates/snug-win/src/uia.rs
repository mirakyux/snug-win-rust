//! UI Automation，移植自 C# 版 `SnugWin.Windows.Windows.UiaHelper`。
//!
//! 三件事：
//! 1. 找窗口自带的关闭按钮位置（触摸那边据此把"点关闭"翻译成"先归位再关"）；
//! 2. 点"新建窗口"菜单（C# `TryInvokeNewWindowMenu`）；
//! 3. 读 UU 远程"本机被控中"那张控制端表——读成 `snug_core::session::UuElement`
//!    树，再交给 core 的 `ControllerTableParser` 解析。
//!
//! 设计上把 COM 挡在 [`UiaNode`] 后面：树的遍历、深度/节点上限、运行时 ID 防环、
//! 按钮与菜单的匹配规则都是纯逻辑，可以拿假树单测；真机部分只是把
//! `IUIAutomationElement` 包成 [`UiaNode`]。
//!
//! UIA 读的是别的进程，窗口关闭或应用卡死时 COM 会抛异常——C# 是整段 `try/catch`
//! 吞掉返回"读不到"，这里每一步都 `Result` 化，失败就当作没有。
//!
//! 手动验证（自动测试不碰真实 UIA）：
//! 1. 打开记事本，程序里看窗口的关闭按钮位置是否与屏幕上一致（触摸点关闭能直接关窗）；
//! 2. 打开"计算器"，多窗口菜单里有"新建窗口"时应能点中；
//! 3. 手机端连上 UU 远程，设置页停在"本机被控中"，日志里应出现控制端名字。

use std::collections::HashSet;

use snug_core::session::{ControllerTable, ControllerTableParser, UuElement};
use snug_core::Rect;
use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationTreeWalker, UIA_InvokePatternId,
};

/// UIA 控件类型：按钮。
pub const UIA_BUTTON: i32 = 50000;
/// UIA 控件类型：列表项。
pub const UIA_LIST_ITEM: i32 = 50007;
/// UIA 控件类型：列表。
pub const UIA_LIST: i32 = 50008;
/// UIA 控件类型：菜单项。
pub const UIA_MENU_ITEM: i32 = 50011;
/// UIA 控件类型：文本。
pub const UIA_TEXT: i32 = 50020;
/// UIA 控件类型：数据网格。
pub const UIA_DATA_GRID: i32 = 50028;
/// UIA 控件类型：数据项。
pub const UIA_DATA_ITEM: i32 = 50029;
/// UIA 控件类型：表格。
pub const UIA_TABLE: i32 = 50036;

/// 遍历深度上限（C# `MaxUiaDepth`）。
pub const MAX_UIA_DEPTH: usize = 25;
/// 一次读取的节点数上限（C# `MaxUiaNodes`）。
pub const MAX_UIA_NODES: usize = 2048;

/// 节点在 `UuElement` 树里的位置（C# `SnapshotElement` 的三个分支）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// 有文字的文本节点。
    Text,
    /// 表 / 数据网格 / 列表。
    Table,
    /// 表里的一行。
    Row,
    /// 其余容器（只保留子节点）。
    Other,
}

/// 把 UIA 控件类型归到这四类（对应 C# 里三次 `ControlType` 比较）。
pub fn node_kind(control_type: i32) -> NodeKind {
    match control_type {
        UIA_TEXT => NodeKind::Text,
        UIA_TABLE | UIA_DATA_GRID | UIA_LIST => NodeKind::Table,
        UIA_DATA_ITEM | UIA_LIST_ITEM => NodeKind::Row,
        _ => NodeKind::Other,
    }
}

/// 这个按钮是不是"关闭"（C# `FindCloseButtonRect` 的判定）。
///
/// 自动 ID 认 `Close`/`PART_Close`（忽略大小写），名字认中文"关闭"（区分大小写）
/// 和英文 `Close`（忽略大小写）。
pub fn is_close_button(automation_id: &str, name: &str) -> bool {
    automation_id.eq_ignore_ascii_case("Close")
        || automation_id.eq_ignore_ascii_case("PART_Close")
        || name == "关闭"
        || name.eq_ignore_ascii_case("Close")
}

/// 这个菜单项是不是"新建窗口"（C# `TryInvokeNewWindowMenu`）。
pub fn is_new_window_menu(name: &str) -> bool {
    name.contains("新建窗口") || name.to_lowercase().contains("new window")
}

/// UIA 元素的只读视图。真机是 [`ComElement`]，测试里是假树。
pub trait UiaNode {
    fn control_type(&self) -> i32;
    fn name(&self) -> String;
    fn automation_id(&self) -> String;
    fn bounding_rect(&self) -> Option<Rect>;
    /// 运行时 ID，用来防环；拿不到就返回空（C# 里 `GetRuntimeId()` 返回 null 时同样处理）。
    fn runtime_id(&self) -> Vec<i32>;
    fn children(&self) -> Vec<Box<dyn UiaNode>>;
    /// 触发这个元素（只有实现了 InvokePattern 的菜单项会返回 `true`）。
    fn invoke(&self) -> bool {
        false
    }
}

/// 读树的预算：节点数上限 + 是否中途放弃（C# 的 `ref int nodeCount` / `ref bool aborted`）。
#[derive(Debug, Clone, Copy)]
pub struct SnapshotBudget {
    pub nodes: usize,
    pub aborted: bool,
}

impl Default for SnapshotBudget {
    fn default() -> Self {
        Self { nodes: 0, aborted: false }
    }
}

impl SnapshotBudget {
    /// 超限就标成"放弃读取"，上层据此返回"读不到"而不是"没有控制端"。
    fn take(&mut self) -> bool {
        self.nodes += 1;
        if self.nodes > MAX_UIA_NODES {
            self.aborted = true;
            return false;
        }
        true
    }
}

/// 递归快照（C# `SnapshotElement`）。
///
/// `path` 是当前这条链上的运行时 ID 集合，撞到重复的就剪掉（防环）。
pub fn snapshot(
    root: &dyn UiaNode,
    depth: usize,
    budget: &mut SnapshotBudget,
    path: &mut HashSet<String>,
) -> Option<UuElement> {
    if depth >= MAX_UIA_DEPTH || !budget.take() {
        budget.aborted = true;
        return None;
    }

    let runtime_key = root
        .runtime_id()
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    if !runtime_key.is_empty() && !path.insert(runtime_key.clone()) {
        return None;
    }

    let mut children = Vec::new();
    let mut aborted = false;
    for child in root.children() {
        match snapshot(child.as_ref(), depth + 1, budget, path) {
            Some(snapshot) => children.push(snapshot),
            None => {
                if budget.aborted {
                    aborted = true;
                    break;
                }
            }
        }
    }
    if !runtime_key.is_empty() {
        path.remove(&runtime_key);
    }
    if aborted {
        return None;
    }

    match node_kind(root.control_type()) {
        NodeKind::Text => {
            let name = root.name();
            if name.trim().is_empty() {
                return None;
            }
            Some(UuElement::text_node(name.trim()))
        }
        NodeKind::Table => Some(UuElement::table_node(children)),
        NodeKind::Row => Some(UuElement::row_node(children)),
        NodeKind::Other => Some(UuElement::other_node(children)),
    }
}

/// 读若干窗口，拼出控制端表（C# `ReadGameViewerControllers`）。
///
/// 一个窗口都读不到、或者中途因为超限放弃，都返回"不可读"。
pub fn read_controllers(windows: &[Box<dyn UiaNode>]) -> ControllerTable {
    let mut trees = Vec::new();
    let mut budget = SnapshotBudget::default();

    for window in windows {
        let mut path = HashSet::new();
        match snapshot(window.as_ref(), 0, &mut budget, &mut path) {
            Some(tree) => trees.push(tree),
            None => {
                if budget.aborted {
                    return ControllerTable::unreadable();
                }
            }
        }
    }

    if trees.is_empty() {
        return ControllerTable::unreadable();
    }
    ControllerTableParser::parse_windows(&trees)
}

/// 深度优先找关闭按钮的矩形（C# `FindCloseButtonRect`）。
pub fn find_close_button_in(root: &dyn UiaNode) -> Option<Rect> {
    find_close_button_at(root, 0)
}

fn find_close_button_at(node: &dyn UiaNode, depth: usize) -> Option<Rect> {
    if depth >= MAX_UIA_DEPTH {
        return None;
    }
    if node.control_type() == UIA_BUTTON
        && is_close_button(&node.automation_id(), &node.name())
        && let Some(rect) = node.bounding_rect()
        && rect.width > 0.0
        && rect.height > 0.0
    {
        return Some(rect);
    }
    for child in node.children() {
        if let Some(rect) = find_close_button_at(child.as_ref(), depth + 1) {
            return Some(rect);
        }
    }
    None
}

/// 找到"新建窗口"菜单项并点它（C# `TryInvokeNewWindowMenu`）。
pub fn invoke_new_window(root: &dyn UiaNode) -> bool {
    invoke_new_window_at(root, 0)
}

fn invoke_new_window_at(node: &dyn UiaNode, depth: usize) -> bool {
    if depth >= MAX_UIA_DEPTH {
        return false;
    }
    if node.control_type() == UIA_MENU_ITEM && is_new_window_menu(&node.name()) && node.invoke() {
        return true;
    }
    for child in node.children() {
        if invoke_new_window_at(child.as_ref(), depth + 1) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// 真机 COM 实现
// ---------------------------------------------------------------------------

/// 真机 UIA 元素：`IUIAutomationElement` + 走树的 walker。
struct ComElement {
    element: IUIAutomationElement,
    walker: IUIAutomationTreeWalker,
}

impl UiaNode for ComElement {
    fn control_type(&self) -> i32 {
        unsafe { self.element.CurrentControlType() }.map(|id| id.0).unwrap_or(0)
    }

    fn name(&self) -> String {
        unsafe { self.element.CurrentName() }
            .map(|name| name.to_string())
            .unwrap_or_default()
    }

    fn automation_id(&self) -> String {
        unsafe { self.element.CurrentAutomationId() }
            .map(|id| id.to_string())
            .unwrap_or_default()
    }

    fn bounding_rect(&self) -> Option<Rect> {
        let rect = unsafe { self.element.CurrentBoundingRectangle() }.ok()?;
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 0 || height <= 0 {
            return None;
        }
        Some(Rect::new(
            rect.left as f64,
            rect.top as f64,
            width as f64,
            height as f64,
        ))
    }

    fn runtime_id(&self) -> Vec<i32> {
        // SAFEARRAY 要手动拆，收益只有"防环"一项；深度/节点上限已经兜住了，
        // 这里直接当作拿不到（C# 里也有拿不到 runtime id 的分支）。
        Vec::new()
    }

    fn children(&self) -> Vec<Box<dyn UiaNode>> {
        let mut children: Vec<Box<dyn UiaNode>> = Vec::new();
        let Ok(mut current) = (unsafe { self.walker.GetFirstChildElement(&self.element) }) else {
            return children;
        };
        loop {
            children.push(Box::new(ComElement {
                element: current.clone(),
                walker: self.walker.clone(),
            }));
            match unsafe { self.walker.GetNextSiblingElement(&current) } {
                Ok(next) => current = next,
                Err(_) => break,
            }
        }
        children
    }

    fn invoke(&self) -> bool {
        unsafe {
            self.element
                .GetCurrentPattern(UIA_InvokePatternId)
                .ok()
                .and_then(|unknown| unknown.cast::<IUIAutomationInvokePattern>().ok())
                .map(|pattern| pattern.Invoke().is_ok())
                .unwrap_or(false)
        }
    }
}

/// UIA 会话（C# 里 `AutomationElement` 的静态入口）。
pub struct Uia {
    automation: IUIAutomation,
    walker: IUIAutomationTreeWalker,
    _com: Option<ComInitialization>,
}

struct ComInitialization;

impl Drop for ComInitialization {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

impl Uia {
    /// 建一个 UIA 会话。每次成功的 CoInitializeEx（包括 S_FALSE）都由会话析构配对释放；
    /// 如果线程已用另一种 apartment 初始化，则不取得也不释放 COM 引用。
    pub fn new() -> windows::core::Result<Self> {
        unsafe {
            let status = CoInitializeEx(None, COINIT_MULTITHREADED);
            // RPC_E_CHANGED_MODE only means this thread already chose a different apartment.
            // CoCreateInstance may still work there; preserve the old behavior, but never
            // balance an initialization reference that this call did not acquire.
            let com = status.is_ok().then_some(ComInitialization);
            let created = (|| {
                let automation: IUIAutomation =
                    CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
                let walker = automation.RawViewWalker()?;
                Ok((automation, walker))
            })();
            match created {
                Ok((automation, walker)) => Ok(Self { automation, walker, _com: com }),
                Err(error) => {
                    drop(com);
                    Err(error)
                }
            }
        }
    }

    fn root(&self, handle: u64) -> Option<ComElement> {
        let element = unsafe {
            self.automation
                .ElementFromHandle(HWND(handle as usize as *mut core::ffi::c_void))
        }
        .ok()?;
        Some(ComElement { element, walker: self.walker.clone() })
    }

    /// 关窗按钮的矩形（C# `UiaHelper.FindCloseButtonRect`）。
    pub fn find_close_button(&self, handle: u64) -> Option<Rect> {
        let root = self.root(handle)?;
        find_close_button_in(&root)
    }

    /// 点"新建窗口"菜单（C# `UiaHelper.TryInvokeNewWindowMenu`）。
    pub fn invoke_new_window(&self, handle: u64) -> bool {
        match self.root(handle) {
            Some(root) => invoke_new_window(&root),
            None => false,
        }
    }

    /// 读控制端表（C# `UiaHelper.ReadGameViewerControllers`）。
    pub fn read_controllers(&self, handles: &[u64]) -> ControllerTable {
        let mut trees: Vec<Box<dyn UiaNode>> = Vec::new();
        for handle in handles {
            if let Some(root) = self.root(*handle) {
                trees.push(Box::new(root));
            }
        }
        read_controllers(&trees)
    }
}

/// 给 `window` 模块用的关闭按钮查找器。
///
/// **每个线程复用一个 UIA 会话**：`Uia::new()` 要 `CoCreateInstance(CUIAutomation)`，
/// 真机实测单次 500–1500ms；而一次铺满会连查好几轮（换档阶梯每档一轮），
/// 原先每轮都新建，光这一步就要 1–3 秒，全算在"点了 App 多久才有反应"上。
/// 复用之后同一窗口的查找是 50–170ms。
///
/// 用 thread_local 而不是全局缓存：COM 接口是套间绑定的，跨线程共享会出错；
/// 桌面动作和 UU 传感器分别在固定线程上运行，各自保留一份。
#[derive(Debug, Default, Clone, Copy)]
pub struct UiaCloseButtons;

thread_local! {
    /// 本线程的 UIA 会话（第一次用时建，之后复用）。
    static UIA_CACHE: std::cell::RefCell<Option<Uia>> = const { std::cell::RefCell::new(None) };
}

/// 在当前线程复用 UIA 会话。UIA/COM 接口不能在线程间共享；每条固定用途线程各有一份。
pub fn with_thread_uia<T>(body: impl FnOnce(&Uia) -> T) -> Option<T> {
    UIA_CACHE.with(|cache| {
        let mut slot = cache.borrow_mut();
        if slot.is_none() {
            // 初始化失败不缓存，下次探测仍可重试。
            *slot = Uia::new().ok();
        }
        slot.as_ref().map(body)
    })
}

impl crate::window::CloseButtonFinder for UiaCloseButtons {
    fn find_close_button(&self, handle: u64) -> Option<Rect> {
        with_thread_uia(|uia| uia.find_close_button(handle)).flatten()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    #[derive(Clone)]
    struct FakeNode {
        control_type: i32,
        name: String,
        automation_id: String,
        rect: Option<Rect>,
        runtime_id: Vec<i32>,
        children: Vec<FakeNode>,
        invoked: Rc<Cell<bool>>,
    }

    impl FakeNode {
        fn new(control_type: i32, name: &str) -> Self {
            Self {
                control_type,
                name: name.to_string(),
                automation_id: String::new(),
                rect: None,
                runtime_id: Vec::new(),
                children: Vec::new(),
                invoked: Rc::new(Cell::new(false)),
            }
        }

        fn text(name: &str) -> Self {
            Self::new(UIA_TEXT, name)
        }

        fn with_children(mut self, children: Vec<FakeNode>) -> Self {
            self.children = children;
            self
        }

        fn with_id(mut self, id: &str) -> Self {
            self.automation_id = id.to_string();
            self
        }

        fn with_rect(mut self, rect: Rect) -> Self {
            self.rect = Some(rect);
            self
        }

        fn with_runtime_id(mut self, id: i32) -> Self {
            self.runtime_id = vec![id];
            self
        }
    }

    impl UiaNode for FakeNode {
        fn control_type(&self) -> i32 {
            self.control_type
        }

        fn name(&self) -> String {
            self.name.clone()
        }

        fn automation_id(&self) -> String {
            self.automation_id.clone()
        }

        fn bounding_rect(&self) -> Option<Rect> {
            self.rect
        }

        fn runtime_id(&self) -> Vec<i32> {
            self.runtime_id.clone()
        }

        fn children(&self) -> Vec<Box<dyn UiaNode>> {
            self.children
                .iter()
                .map(|child| {
                    Box::new(FakeNode {
                        control_type: child.control_type,
                        name: child.name.clone(),
                        automation_id: child.automation_id.clone(),
                        rect: child.rect,
                        runtime_id: child.runtime_id.clone(),
                        children: child.children.clone(),
                        invoked: child.invoked.clone(),
                    }) as Box<dyn UiaNode>
                })
                .collect()
        }

        fn invoke(&self) -> bool {
            self.invoked.set(true);
            true
        }
    }

    #[test]
    fn node_kind_maps_control_types() {
        assert_eq!(node_kind(UIA_TEXT), NodeKind::Text);
        assert_eq!(node_kind(UIA_TABLE), NodeKind::Table);
        assert_eq!(node_kind(UIA_DATA_GRID), NodeKind::Table);
        assert_eq!(node_kind(UIA_LIST), NodeKind::Table);
        assert_eq!(node_kind(UIA_DATA_ITEM), NodeKind::Row);
        assert_eq!(node_kind(UIA_LIST_ITEM), NodeKind::Row);
        assert_eq!(node_kind(UIA_BUTTON), NodeKind::Other);
        assert_eq!(node_kind(0), NodeKind::Other);
    }

    #[test]
    fn close_button_matches_like_csharp() {
        assert!(is_close_button("Close", ""));
        assert!(is_close_button("PART_Close", ""));
        assert!(is_close_button("close", ""));
        assert!(is_close_button("", "关闭"));
        assert!(is_close_button("", "Close"));
        assert!(is_close_button("", "close"));
        assert!(!is_close_button("", "关闭窗口"), "C# 用的是等于不是包含");
        assert!(!is_close_button("Minimize", "最小化"));
        assert!(!is_close_button("", ""));
    }

    #[test]
    fn new_window_menu_matches_chinese_and_english() {
        assert!(is_new_window_menu("新建窗口"));
        assert!(is_new_window_menu("New Window"));
        assert!(is_new_window_menu("new window"));
        assert!(is_new_window_menu("新建窗口 (Ctrl+N)"));
        assert!(!is_new_window_menu("新建标签页"));
    }

    fn controlled_window() -> FakeNode {
        FakeNode::new(0, "UU").with_children(vec![
            FakeNode::text("本机被控中"),
            FakeNode::new(UIA_TABLE, "").with_children(vec![
                FakeNode::new(UIA_DATA_ITEM, "").with_children(vec![
                    FakeNode::text("iPhone 17 Pro"),
                    FakeNode::text("远程控制"),
                    FakeNode::text("连接时长 00:12"),
                ]),
                FakeNode::new(UIA_DATA_ITEM, "").with_children(vec![
                    FakeNode::text("MacBook"),
                    FakeNode::text("远程协助"),
                ]),
            ]),
        ])
    }

    #[test]
    fn snapshot_feeds_the_core_parser() {
        let window = controlled_window();
        let table = read_controllers(&[Box::new(window)]);
        assert_eq!(table.names, vec!["iPhone 17 Pro".to_string()]);
        assert!(table.readable);
    }

    #[test]
    fn snapshot_drops_blank_text_nodes() {
        let mut budget = SnapshotBudget::default();
        let mut path = HashSet::new();
        assert!(
            snapshot(&FakeNode::text("   "), 0, &mut budget, &mut path).is_none(),
            "空白文本节点整个丢掉"
        );
        let mut budget = SnapshotBudget::default();
        let mut path = HashSet::new();
        let tree = snapshot(&FakeNode::text("  标题  "), 0, &mut budget, &mut path).expect("节点");
        assert_eq!(tree, UuElement::text_node("标题"));
    }

    #[test]
    fn snapshot_aborts_past_the_depth_limit() {
        // 一层套一层的 30 层容器，超过 MAX_UIA_DEPTH。
        let mut node = FakeNode::new(0, "leaf");
        for _ in 0..30 {
            node = FakeNode::new(0, "wrap").with_children(vec![node]);
        }
        let mut budget = SnapshotBudget::default();
        let mut path = HashSet::new();
        assert!(snapshot(&node, 0, &mut budget, &mut path).is_none());
        assert!(budget.aborted);
    }

    #[test]
    fn snapshot_aborts_past_the_node_budget() {
        let wide = FakeNode::new(0, "root").with_children(
            (0..MAX_UIA_NODES + 8).map(|index| FakeNode::text(&format!("节点 {index}"))).collect(),
        );
        let mut budget = SnapshotBudget::default();
        let mut path = HashSet::new();
        assert!(snapshot(&wide, 0, &mut budget, &mut path).is_none());
        assert!(budget.aborted);
        assert_eq!(
            read_controllers(&[Box::new(wide)]),
            ControllerTable::unreadable(),
            "超限必须报不可读，不能报没有控制端"
        );
    }

    #[test]
    fn snapshot_cuts_runtime_id_cycles() {
        let parent = FakeNode::new(0, "root").with_runtime_id(7);
        let child = FakeNode::new(UIA_TEXT, "子节点").with_runtime_id(7);
        let tree = parent.with_children(vec![child]);
        let mut budget = SnapshotBudget::default();
        let mut path = HashSet::new();
        let snapshot = snapshot(&tree, 0, &mut budget, &mut path).expect("根节点");
        assert_eq!(snapshot, UuElement::other_node(Vec::new()), "撞上同一个运行时 ID 就剪掉");
        assert!(!budget.aborted);
    }

    #[test]
    fn read_controllers_without_windows_is_unreadable() {
        assert_eq!(read_controllers(&[]), ControllerTable::unreadable());
    }

    #[test]
    fn read_controllers_of_settings_page_is_readable_but_empty() {
        let settings = FakeNode::new(0, "UU").with_children(vec![
            FakeNode::text("设置"),
            FakeNode::new(UIA_TABLE, "").with_children(vec![
                FakeNode::new(UIA_DATA_ITEM, "").with_children(vec![FakeNode::text("隐藏至菜单栏")]),
            ]),
        ]);
        let table = read_controllers(&[Box::new(settings)]);
        assert!(table.readable);
        assert!(table.names.is_empty());
    }

    #[test]
    fn close_button_is_found_by_id_or_name() {
        let window = Rect::new(0.0, 0.0, 800.0, 600.0);
        let by_id = FakeNode::new(0, "窗口").with_children(vec![
            FakeNode::new(UIA_BUTTON, "最小化").with_id("Minimize"),
            FakeNode::new(UIA_BUTTON, "关闭").with_id("Close").with_rect(Rect::new(770.0, 0.0, 30.0, 30.0)),
        ]);
        assert_eq!(
            find_close_button_in(&by_id),
            Some(Rect::new(770.0, 0.0, 30.0, 30.0))
        );

        // 名字是"关闭"但控件类型不是按钮 → 不算。
        let not_button = FakeNode::new(0, "窗口")
            .with_children(vec![FakeNode::text("关闭").with_rect(window)]);
        assert_eq!(find_close_button_in(&not_button), None);

        // 按钮的矩形是空的（还没布局）→ 不算。
        let empty = FakeNode::new(0, "窗口").with_children(vec![
            FakeNode::new(UIA_BUTTON, "关闭").with_id("Close").with_rect(Rect::ZERO),
        ]);
        assert_eq!(find_close_button_in(&empty), None);
    }

    #[test]
    fn new_window_menu_is_invoked_once() {
        let invoked = Rc::new(Cell::new(false));
        let mut item = FakeNode::new(UIA_MENU_ITEM, "新建窗口 (Ctrl+N)");
        item.invoked = invoked.clone();
        let window = FakeNode::new(0, "UU").with_children(vec![
            FakeNode::new(UIA_MENU_ITEM, "退出"),
            item,
        ]);

        assert!(invoke_new_window(&window));
        assert!(invoked.get(), "应该真的去点那个菜单项");

        let none = FakeNode::new(0, "UU").with_children(vec![FakeNode::new(UIA_MENU_ITEM, "退出")]);
        assert!(!invoke_new_window(&none));
    }
}
