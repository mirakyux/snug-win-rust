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
use snug_core::windows::{AppInfo, RunningApp, WindowInfo};
use snug_win::WinApps;

/// 多久重新扫一次（新装的 App 最多半分钟后出现）。
const RESCAN_INTERVAL: Duration = Duration::from_secs(30);
/// 网格里一个格子的边长（点）。
pub const TILE: f32 = 76.0;

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

    pub fn installed(&self) -> &[AppInfo] {
        &self.installed
    }

    pub fn running(&self) -> &[RunningApp] {
        &self.running
    }

    /// 所有运行中的窗口，按 Z 序（前面的是最近用过的）。
    pub fn running_windows(&self) -> Vec<WindowInfo> {
        self.running
            .iter()
            .flat_map(|app| app.windows.iter().cloned())
            .collect()
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

    /// 按查询过滤运行中的窗口（标题拼音匹配）。
    pub fn filter_windows(&self, query: &str) -> Vec<WindowInfo> {
        if query.trim().is_empty() {
            return self.running_windows();
        }
        snug_core::apps::search(query, &self.installed, &self.running).windows
    }
}
