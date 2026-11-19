//! App 资源库：已安装 App 的图标网格、运行中窗口列表。
//!
//! 对应 C# 版 `PhoneHomeWindow` 的列表部分 + `AppIconCache`。
//!
//! 分工：
//! - **扫描**（`WinApps::scan_installed_apps` / `running_apps`）在后台线程跑——
//!   真机上 124 个 App 要走三轮安装源扫描 + `.lnk` 解析，几百毫秒，不能卡界面；
//! - **图标**按需取、取到就缓存：`IShellItemImageFactory` 每个都是进程外 COM 调用；
//! - **点一下要做什么**（放上屏还是启动）由 core 的 [`snug_core::apps::action_for`] 判定，
//!   这里不另写一份规则。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui;
use snug_core::apps::AppAction;
use snug_core::windows::{AppInfo, RunningApp};
use snug_win::WinApps;

/// 多久重新扫一次（新装的 App 最多半分钟后出现）。
const RESCAN_INTERVAL: Duration = Duration::from_secs(30);

/// 后台扫描的线程句柄与结果通道。
struct Scan {
    rx: std::sync::mpsc::Receiver<(Vec<AppInfo>, Vec<RunningApp>)>,
}

pub struct Library {
    /// 只用来取图标（扫描在后台线程里另建一个，省得跟这里的 COM 抢）。
    icons_service: WinApps,
    installed: Vec<AppInfo>,
    running: Vec<RunningApp>,
    /// AppId → 图标；`None` 表示取过但拿不到（例如未运行的 UWP），别反复重试。
    icons: HashMap<String, Option<egui::TextureHandle>>,
    scan: Option<Scan>,
    last_scan: Option<Instant>,
    /// 从来没扫成功过（界面据此显示"正在扫描…"）。
    pub scanned_once: bool,
}

impl Default for Library {
    fn default() -> Self {
        Self::new()
    }
}

impl Library {
    pub fn new() -> Self {
        Self {
            icons_service: WinApps::new(),
            installed: Vec::new(),
            running: Vec::new(),
            icons: HashMap::new(),
            scan: None,
            last_scan: None,
            scanned_once: false,
        }
    }

    pub fn running(&self) -> &[RunningApp] {
        &self.running
    }

    /// 只给测试：塞一份现成的 App 列表进去（真机构造要走 COM，测试里不要碰）。
    #[cfg(test)]
    pub fn with_data(installed: Vec<AppInfo>, running: Vec<RunningApp>) -> Self {
        let mut library = Self::new();
        library.installed = installed;
        library.running = running;
        library.scanned_once = true;
        library
    }

    /// 该不该重扫。`force` 用于"手动刷新"按钮或手机屏刚就绪时。
    pub fn needs_scan(&self, force: bool) -> bool {
        if self.scan.is_some() {
            return false;
        }
        if force || !self.scanned_once {
            return true;
        }
        self.last_scan
            .map(|at| at.elapsed() >= RESCAN_INTERVAL)
            .unwrap_or(true)
    }

    /// 起一个后台扫描（已经有任务在跑就直接返回）。
    pub fn start_scan(&mut self) {
        if self.scan.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.scan = Some(Scan { rx });
        std::thread::Builder::new()
            .name("snug-app-scan".to_string())
            .spawn(move || {
                let service = WinApps::new();
                let installed = service.scan_installed_apps();
                let running = service.running_apps();
                let _ = tx.send((installed, running));
            })
            .ok();
    }

    /// 每帧收一次扫描结果。
    pub fn poll(&mut self) {
        use std::sync::mpsc::TryRecvError;

        let Some(scan) = &self.scan else { return };
        match scan.rx.try_recv() {
            Ok((installed, running)) => {
                crate::info!(
                    "资源库：{} 个已安装 App、{} 个运行中",
                    installed.len(),
                    running.len()
                );
                self.installed = installed;
                self.running = running;
                self.last_scan = Some(Instant::now());
                self.scanned_once = true;
                self.scan = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                crate::warn!("资源库：扫描线程异常结束");
                self.scan = None;
            }
        }
    }

    /// 取图标（取过就缓存，包括"确实没有"）。
    pub fn icon(&mut self, ctx: &egui::Context, app: &AppInfo) -> Option<egui::TextureHandle> {
        let key = app.app_id.to_ascii_lowercase();
        if let Some(cached) = self.icons.get(&key) {
            return cached.clone();
        }
        let pid = self
            .running
            .iter()
            .find(|item| item.app.app_id.eq_ignore_ascii_case(&app.app_id))
            .map(|item| item.pid);
        let loaded = self.icons_service.icon(app, pid).map(|image| {
            let size = [image.width.max(1) as usize, image.height.max(1) as usize];
            let pixels = image
                .rgba
                .chunks_exact(4)
                .map(|px| egui::Color32::from_rgba_premultiplied(px[0], px[1], px[2], px[3]))
                .collect();
            ctx.load_texture(
                format!("app-icon-{key}"),
                egui::ColorImage { size, pixels, source_size: egui::Vec2::new(size[0] as f32, size[1] as f32) },
                egui::TextureOptions::LINEAR,
            )
        });
        self.icons.insert(key, loaded.clone());
        loaded
    }

    /// 按 core 的规则决定"点这一下做什么"。
    pub fn action_for(&self, app: &AppInfo) -> AppAction {
        snug_core::apps::action_for(app, &self.running)
    }

    /// 按查询过滤已安装 App（走拼音搜索，和 C# 同一套）。
    pub fn filter_installed(&self, query: &str) -> Vec<AppInfo> {
        if query.trim().is_empty() {
            return self.installed.clone();
        }
        snug_core::apps::search(query, &self.installed, &self.running).apps
    }

    /// 某个 App 是否匹配搜索词（与「全部」页同一套拼音搜索）。
    ///
    /// 「运行中」页签也要按搜索词过滤：原版 C# 的搜索是独立子页面，
    /// 一输入就换成搜索结果页，跟当前页签无关；我们做成内嵌输入框后，
    /// 只在「全部」页过滤会让用户在「运行中」页输入后看不到任何变化。
    pub fn matches_search(&self, app: &AppInfo, query: &str) -> bool {
        if query.trim().is_empty() {
            return true;
        }
        // 运行中列表要传空：`search` 会把 `running` 里没装过的 App 也加进候选，
        // 传 `self.running` 会让所有运行中的 App 都"命中"。
        let none: [RunningApp; 0] = [];
        let hits = snug_core::apps::search(query, std::slice::from_ref(app), &none);
        !hits.apps.is_empty()
    }

    /// 首字母分组（C# `AppList.GroupedByLetter`）：搜索时只有一组、不显示字母头。
    ///
    /// 124 个 App 平铺没法用，C# 的列表也是按字母分段的。
    pub fn grouped(&self, query: &str) -> Vec<(String, Vec<AppInfo>)> {
        if !query.trim().is_empty() {
            let apps = self.filter_installed(query);
            return if apps.is_empty() {
                Vec::new()
            } else {
                vec![(String::new(), apps)]
            };
        }
        let sorted = snug_core::apps::sorted(&self.installed);
        snug_core::apps::grouped_by_letter(&sorted)
            .into_iter()
            .map(|group| (group.letter, group.apps))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str) -> AppInfo {
        AppInfo {
            app_id: id.to_string(),
            name: name.to_string(),
            executable_path: None,
            launch_target: None,
            arguments: None,
            working_directory: None,
        }
    }

    fn running(app: AppInfo, pid: i32) -> RunningApp {
        RunningApp { app, pid, windows: Vec::new() }
    }

    /// 「运行中」页签也要按搜索词过滤：真机上用户在那一页输入 `QQ` 什么都没变。
    #[test]
    fn running_tab_honours_the_search_query() {
        let qq = running(app("QQ.exe", "QQ"), 100);
        let code = running(app("Code.exe", "Visual Studio Code"), 200);
        let library = Library::with_data(
            vec![app("QQ.exe", "QQ"), app("Code.exe", "Visual Studio Code")],
            vec![qq, code],
        );

        let hits: Vec<&str> = library
            .running()
            .iter()
            .filter(|app| library.matches_search(&app.app, "qq"))
            .map(|app| app.app.name.as_str())
            .collect();
        assert_eq!(hits, vec!["QQ"], "只有 QQ 命中");

        // 空查询放行全部（没输入时列表不该被过滤掉）。
        assert_eq!(
            library
                .running()
                .iter()
                .filter(|app| library.matches_search(&app.app, "  "))
                .count(),
            2
        );
    }
}
