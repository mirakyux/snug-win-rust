//! App 资源库的排序、分组、搜索与"放上手机屏 / 打开"的选择。
//!
//! 移植自 C# 类 `SnugWin.Core.PhoneScreen.AppList`（`Apps.cs`）：
//! `LetterGroup`、`AppSearchResult`、`AppAction` 三个记录类型，
//! 以及 `Ordered` / `KeepingOrder` / `Sorted` / `GroupedByLetter` / `Search` / `ActionFor`。

use std::collections::{HashMap, HashSet};

use crate::pinyin;
use crate::windows::{AppInfo, RunningApp, WindowInfo, WindowKey};

/// A-Z/# 分组（对应 C# `LetterGroup`）。
#[derive(Debug, Clone, PartialEq)]
pub struct LetterGroup {
    pub letter: String,
    pub apps: Vec<AppInfo>,
}

/// 搜索结果：匹配的 App 与窗口标题（对应 C# `AppSearchResult`）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AppSearchResult {
    pub apps: Vec<AppInfo>,
    pub windows: Vec<WindowInfo>,
}

impl AppSearchResult {
    pub fn is_empty(&self) -> bool {
        self.apps.is_empty() && self.windows.is_empty()
    }
}

/// 点一个 App 时该做什么（对应 C# `AppAction`）。
#[derive(Debug, Clone, PartialEq)]
pub enum AppAction {
    /// 已经有窗口在跑：把它放上手机屏。
    Pull(WindowInfo),
    /// 没在跑：打开它。
    Open(AppInfo),
}

/// 按 Z 序排列运行中的应用（对应 C# `AppList.Ordered`）。
///
/// `front_to_back_handles` 是从最前到最后的窗口句柄；不在里面的窗口排在最后。
pub fn ordered(apps: &[RunningApp], front_to_back_handles: &[u64]) -> Vec<RunningApp> {
    let mut rank: HashMap<u64, usize> = HashMap::with_capacity(front_to_back_handles.len());
    for (index, handle) in front_to_back_handles.iter().enumerate() {
        rank.entry(*handle).or_insert(index);
    }
    let position = |window: &WindowInfo| rank.get(&window.key.handle).copied().unwrap_or(usize::MAX);

    let mut result: Vec<RunningApp> = apps
        .iter()
        .map(|app| {
            let mut windows = app.windows.clone();
            // C# 先按 Pos、再按原始下标排序；Rust 的 sort_by_key 是稳定排序，
            // 只按 Pos 排就得到同样结果。
            windows.sort_by_key(position);
            RunningApp { app: app.app.clone(), pid: app.pid, windows }
        })
        .collect();
    // 应用之间同样按"第一个窗口的位置"排，稳定排序保住原顺序。
    result.sort_by_key(|app| app.windows.first().map(position).unwrap_or(usize::MAX));
    result
}

/// 列表可见期间保持顺序（对应 C# `AppList.KeepingOrder`）：
/// 旧顺序里的 App 还在就用最新数据替换，新出现的追加在后面。
pub fn keeping_order(shown: &[RunningApp], fresh: &[RunningApp]) -> Vec<RunningApp> {
    let mut latest: HashMap<String, &RunningApp> = HashMap::new();
    for app in fresh {
        // C# 用 TryAdd：同一个 AppId 只认第一个。
        latest.entry(identity(app)).or_insert(app);
    }

    let mut known: HashSet<String> = HashSet::new();
    let mut kept: Vec<RunningApp> = Vec::new();
    for old in shown {
        let key = identity(old);
        if let Some(updated) = latest.get(&key) {
            known.insert(key);
            let mut updated = (*updated).clone();
            updated.windows = keeping_window_order(&old.windows, &updated.windows);
            kept.push(updated);
        }
    }
    for item in fresh {
        if !known.contains(&identity(item)) {
            kept.push(item.clone());
        }
    }
    kept
}

/// 窗口层面同样"旧顺序优先、新窗口追加"。
fn keeping_window_order(shown: &[WindowInfo], fresh: &[WindowInfo]) -> Vec<WindowInfo> {
    let mut latest: HashMap<WindowKey, &WindowInfo> = HashMap::new();
    for window in fresh {
        latest.entry(window.key).or_insert(window);
    }
    let mut known: HashSet<WindowKey> = HashSet::new();
    let mut result: Vec<WindowInfo> = Vec::new();
    for old in shown {
        if let Some(current) = latest.get(&old.key) {
            known.insert(old.key);
            result.push((*current).clone());
        }
    }
    for window in fresh {
        if !known.contains(&window.key) {
            result.push(window.clone());
        }
    }
    result
}

fn identity(app: &RunningApp) -> String {
    // C# 用 `StringComparer.OrdinalIgnoreCase` 比较 AppId（进程名，ASCII）。
    app.app.app_id.to_ascii_lowercase()
}

/// 去重并按拼音排序（对应 C# `AppList.Sorted`）。
///
/// 排序键是 [`pinyin::sort_key`]；C# 的第二排序键用 `CurrentCultureIgnoreCase`，
/// 这里退化为小写比较（只在两个名字拼音完全相同时才轮到它，影响可忽略）。
pub fn sorted(apps: &[AppInfo]) -> Vec<AppInfo> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut deduped: Vec<AppInfo> = Vec::new();
    for app in apps {
        if seen.insert(app.app_id.to_ascii_lowercase()) {
            deduped.push(app.clone());
        }
    }
    deduped.sort_by(|a, b| {
        pinyin::sort_key(&a.name)
            .to_lowercase()
            .cmp(&pinyin::sort_key(&b.name).to_lowercase())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    deduped
}

/// 按首字母分组（对应 C# `AppList.GroupedByLetter`）：非字母归到 `#` 且排在最后。
pub fn grouped_by_letter(sorted: &[AppInfo]) -> Vec<LetterGroup> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<AppInfo>> = HashMap::new();
    for app in sorted {
        let key = pinyin::sort_key(&app.name);
        let letter = match key.chars().next() {
            Some(first) if first.is_ascii_alphabetic() => first.to_ascii_uppercase().to_string(),
            _ => "#".to_string(),
        };
        if !groups.contains_key(&letter) {
            order.push(letter.clone());
        }
        groups.entry(letter).or_default().push(app.clone());
    }
    order.sort_by(|a, b| (a == "#").cmp(&(b == "#")).then_with(|| a.cmp(b)));
    order
        .into_iter()
        .map(|letter| LetterGroup { apps: groups.remove(&letter).unwrap_or_default(), letter })
        .collect()
}

/// 搜索（对应 C# `AppList.Search`）。
///
/// App 按 `max(Score(名字), Score(AppId))` 从高到低、同分按原顺序；
/// 窗口按标题的 [`pinyin::matches`] 过滤。
pub fn search(query: &str, installed: &[AppInfo], running: &[RunningApp]) -> AppSearchResult {
    if query.trim().is_empty() {
        return AppSearchResult::default();
    }

    let installed_ids: HashSet<String> =
        installed.iter().map(|app| app.app_id.to_ascii_lowercase()).collect();
    let mut candidates: Vec<&AppInfo> = installed.iter().collect();
    for app in running {
        if !installed_ids.contains(&app.app.app_id.to_ascii_lowercase()) {
            candidates.push(&app.app);
        }
    }

    let mut scored: Vec<(i32, usize, &AppInfo)> = candidates
        .into_iter()
        .enumerate()
        .map(|(index, app)| {
            let score = pinyin::score(&app.name, query).max(pinyin::score(&app.app_id, query));
            (score, index, app)
        })
        .filter(|(score, _, _)| *score > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let windows = running
        .iter()
        .flat_map(|app| app.windows.iter())
        .filter(|window| pinyin::matches(&window.title, query))
        .cloned()
        .collect();

    AppSearchResult {
        apps: scored.into_iter().map(|(_, _, app)| app.clone()).collect(),
        windows,
    }
}

/// 已经有窗口在跑就"放上屏"，否则"打开"（对应 C# `AppList.ActionFor`）。
pub fn action_for(app: &AppInfo, running: &[RunningApp]) -> AppAction {
    match running.iter().find(|item| item.app.app_id.eq_ignore_ascii_case(&app.app_id)) {
        Some(item) if !item.windows.is_empty() => AppAction::Pull(item.windows[0].clone()),
        _ => AppAction::Open(app.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(app_id: &str, name: &str) -> AppInfo {
        AppInfo {
            app_id: app_id.into(),
            name: name.into(),
            executable_path: None,
            launch_target: None,
            arguments: None,
            working_directory: None,
        }
    }

    fn window(pid: i32, handle: u64, app_name: &str, app_id: &str, title: &str) -> WindowInfo {
        WindowInfo {
            key: WindowKey::new(pid, handle),
            app_name: app_name.into(),
            app_id: Some(app_id.into()),
            title: title.into(),
            minimized: false,
        }
    }

    /// 转写自 C# `ExpandedCoreTests.AppList_OrderedKeepingOrderGroupedAndActionWork`。
    #[test]
    fn ordered_keeping_order_grouped_and_action_match_csharp() {
        let w1 = window(1, 11, "Edge", "msedge.exe", "Doc");
        let w2 = window(1, 12, "Edge", "msedge.exe", "Home");
        let w3 = window(2, 21, "Weixin", "Weixin.exe", "Chat");

        let edge = app("msedge.exe", "Edge");
        let wx = app("Weixin.exe", "微信");
        let notes = app("notepad.exe", "记事本");

        let ordered = ordered(
            &[
                RunningApp { app: edge.clone(), pid: 1, windows: vec![w1.clone(), w2.clone()] },
                RunningApp { app: wx.clone(), pid: 2, windows: vec![w3.clone()] },
            ],
            &[21, 12, 11],
        );
        assert_eq!(ordered[0].app.app_id, "Weixin.exe", "最前的窗口所属 App 排最前");
        assert_eq!(ordered[1].windows[0].key.handle, 12, "Edge 的两个窗口按 Z 序排");

        let kept = keeping_order(
            &[
                RunningApp { app: edge.clone(), pid: 1, windows: vec![w1.clone(), w2.clone()] },
                RunningApp { app: wx.clone(), pid: 2, windows: vec![w3.clone()] },
            ],
            &[
                RunningApp { app: wx.clone(), pid: 2, windows: vec![w3.clone()] },
                RunningApp { app: edge.clone(), pid: 1, windows: vec![w2.clone(), w1.clone()] },
            ],
        );
        assert_eq!(kept[0].app.app_id, "msedge.exe", "旧顺序优先");
        assert_eq!(kept[0].windows[0].key.handle, 11, "窗口也按旧顺序");

        let sorted = sorted(&[notes.clone(), wx.clone(), edge.clone(), edge.clone()]);
        assert_eq!(sorted.len(), 3, "同 AppId 去重");

        let groups = grouped_by_letter(&sorted);
        assert!(groups.iter().any(|group| group.letter == "W"
            && group.apps.iter().any(|app| app.app_id == "Weixin.exe")));
        assert!(groups.iter().any(|group| group.letter == "J"
            && group.apps.iter().any(|app| app.app_id == "notepad.exe")));

        let search_result = search("wx", &sorted, &ordered);
        assert_eq!(search_result.apps.len(), 1);
        assert_eq!(search_result.apps[0].app_id, "Weixin.exe");

        assert!(matches!(action_for(&edge, &ordered), AppAction::Pull(_)));
        assert!(matches!(action_for(&notes, &ordered), AppAction::Open(_)));
    }

    #[test]
    fn keeping_order_appends_new_apps_and_window_updates() {
        let edge = app("msedge.exe", "Edge");
        let wx = app("Weixin.exe", "微信");
        let old_edge = RunningApp {
            app: edge.clone(),
            pid: 1,
            windows: vec![window(1, 11, "Edge", "msedge.exe", "Doc")],
        };
        let fresh_edge = RunningApp {
            app: edge.clone(),
            pid: 1,
            windows: vec![
                window(1, 11, "Edge", "msedge.exe", "Doc"),
                window(1, 12, "Edge", "msedge.exe", "New"),
            ],
        };
        let fresh_wx = RunningApp {
            app: wx.clone(),
            pid: 2,
            windows: vec![window(2, 21, "Weixin", "Weixin.exe", "Chat")],
        };

        let kept = keeping_order(&[old_edge], &[fresh_wx.clone(), fresh_edge.clone()]);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].windows.len(), 2, "已有 App 用最新窗口列表");
        assert_eq!(kept[1].pid, 2, "新 App 追加在后面");
    }

    #[test]
    fn sorted_orders_by_pinyin_and_dedupes_case_insensitively() {
        let apps = vec![
            app("Weixin.exe", "微信"),
            app("weixin.exe", "微信（重复）"),
            app("notepad.exe", "记事本"),
            app("edge.exe", "Edge"),
        ];
        let sorted = sorted(&apps);
        assert_eq!(sorted.len(), 3, "AppId 大小写不敏感去重");
        assert_eq!(sorted[0].app_id, "edge.exe", "e 在最前");
        assert_eq!(sorted[1].app_id, "notepad.exe", "jishiben 在 weixin 前");
        assert_eq!(sorted[2].app_id, "Weixin.exe");
    }

    #[test]
    fn grouped_by_letter_puts_hash_groups_last() {
        let apps = vec![app("a.exe", "12306"), app("b.exe", "Edge"), app("c.exe", "微信")];
        let groups = grouped_by_letter(&sorted(&apps));
        let letters: Vec<&str> = groups.iter().map(|group| group.letter.as_str()).collect();
        assert_eq!(letters, vec!["E", "W", "#"]);
    }

    #[test]
    fn search_prefers_prefix_and_matches_app_id() {
        let weixin = app("Weixin.exe", "微信");
        let wecom = app("WXWork.exe", "企业微信");
        let installed = vec![weixin.clone(), wecom.clone()];

        // AppId 也能搜到：两个都是 2 分（微信靠首字母 wx，WXWork.exe 靠 AppId 前缀），
        // 同分时按原顺序，所以先列出的微信在前。
        let result = search("wx", &installed, &[]);
        assert_eq!(result.apps.len(), 2);
        assert_eq!(result.apps[0].app_id, "Weixin.exe");
        assert_eq!(result.apps[1].app_id, "WXWork.exe");

        // 精确的 AppId 前缀只剩一个。
        let result = search("wxwork", &installed, &[]);
        assert_eq!(result.apps.len(), 1);
        assert_eq!(result.apps[0].app_id, "WXWork.exe");

        // 名字前缀分高于子串分（企业微信的 qiyeweixin 只是包含 weixin）。
        let result = search("weixin", &installed, &[]);
        assert_eq!(result.apps[0].app_id, "Weixin.exe");

        assert!(search("  ", &installed, &[]).is_empty(), "空白查询返回空");
    }

    #[test]
    fn search_matches_window_titles() {
        let running = vec![RunningApp {
            app: app("msedge.exe", "Edge"),
            pid: 1,
            windows: vec![window(1, 11, "Edge", "msedge.exe", "GitHub - 微软")],
        }];
        let result = search("github", &[], &running);
        assert_eq!(result.windows.len(), 1);
        assert_eq!(result.windows[0].key.handle, 11);

        let by_pinyin = search("weiruan", &[], &running);
        assert_eq!(by_pinyin.windows.len(), 1, "窗口标题也走拼音");
    }

    #[test]
    fn action_for_prefers_the_first_window_of_a_running_app() {
        let edge = app("msedge.exe", "Edge");
        let running = vec![RunningApp {
            app: edge.clone(),
            pid: 1,
            windows: vec![
                window(1, 11, "Edge", "msedge.exe", "Doc"),
                window(1, 12, "Edge", "msedge.exe", "Home"),
            ],
        }];
        match action_for(&edge, &running) {
            AppAction::Pull(window) => assert_eq!(window.key.handle, 11),
            other => panic!("应选第一个窗口，实际 {other:?}"),
        }

        // 大小写不同的 AppId 也算同一个 App。
        let upper = app("MSEDGE.EXE", "Edge");
        assert!(matches!(action_for(&upper, &running), AppAction::Pull(_)));

        // 在跑但没有窗口 → 打开。
        let empty = vec![RunningApp { app: edge.clone(), pid: 1, windows: vec![] }];
        assert!(matches!(action_for(&edge, &empty), AppAction::Open(_)));
    }

    #[test]
    fn is_empty_checks_both_lists() {
        assert!(AppSearchResult::default().is_empty());
        assert!(!AppSearchResult { apps: vec![app("a.exe", "A")], windows: vec![] }.is_empty());
    }
}
