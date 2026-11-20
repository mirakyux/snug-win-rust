//! 手机屏上的「App 资源库」界面（egui 实现）。
//!
//! 对应 C# 版 `SnugWin.App.PhoneHomeWindow`：顶部搜索、运行中/全部两页、
//! 底部指示条（返回资源库 / 相机键）。当前阶段是骨架 + 真实数据接口占位。

use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use snug_core::desk::PhoneScreenPhase;
use snug_core::{AppConfig, Paths, PhoneCatalog};

use crate::controller::{Controller, TouchEvent};
use crate::library::Library;
use crate::settings::{ChangeImpact, SettingsAction, SettingsOutcome, SettingsTab, SettingsWindow, StatusInfo};
use crate::tray::{Tray, TrayCommand};

use snug_core::apps::AppAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeTab {
    Running,
    All,
}

/// 手机屏窗口的标题。托盘生命周期里靠它找到自己的 `HWND`（eframe 不对外暴露句柄），
/// 改这里必须和 `main.rs` 里 `ViewportBuilder::with_title` 保持一致。
pub const WINDOW_TITLE: &str = "Snug 手机屏";

/// Periodically wakes eframe so tray and quit polling keeps running for hidden windows.
pub struct UiHeartbeat {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl UiHeartbeat {
    pub fn start(ctx: egui::Context) -> std::io::Result<Self> {
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = running.clone();
        let worker = std::thread::Builder::new()
            .name("snug-ui-tick".to_string())
            .spawn(move || {
                while worker_running.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    if worker_running.load(Ordering::Relaxed) {
                        ctx.request_repaint();
                    }
                }
            })
            .map_err(|error| {
                std::io::Error::new(error.kind(), format!("无法启动界面心跳线程：{error}"))
            })?;
        Ok(Self { running, worker: Some(worker) })
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for UiHeartbeat {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod heartbeat_tests {
    use super::*;

    #[test]
    fn heartbeat_stops_and_releases_its_worker_idempotently() {
        let mut heartbeat = UiHeartbeat::start(egui::Context::default()).unwrap();
        let weak = Arc::downgrade(&heartbeat.running);
        heartbeat.stop();
        heartbeat.stop();
        assert!(heartbeat.worker.is_none());
        assert!(!heartbeat.running.load(Ordering::Relaxed));
        drop(heartbeat);
        assert!(weak.upgrade().is_none(), "the worker must release its state before shutdown returns");
    }
}

pub struct PhoneHomeApp {
    pub config: AppConfig,
    pub paths: Paths,
    /// 虚拟屏驱动管道是否就绪。
    ///
    /// **不是启动那一刻的死快照**：驱动自动安装是在运行过程中完成的，用启动时的值
    /// 会让设置窗在装完驱动后还写着「未检测到」（真机反馈）。所以这个字段是缓存，
    /// 由 [`Self::refresh_settings_status`] 定时刷新。
    pub pipe_ready: bool,
    /// 上次查驱动状态的时间（设置窗开着时每帧都会问一次，按 250ms 节流）。
    pipe_checked: Option<std::time::Instant>,
    pub controller: Controller,
    /// Hidden-window repaint worker; joined during app shutdown.
    ui_heartbeat: UiHeartbeat,
    pub tab: HomeTab,
    pub search: String,
    /// 上一帧这个窗口有没有键盘焦点（只在变化时记一行，排查"输入不进来"）。
    pub window_focused: bool,
    /// 手机屏的逻辑宽度（点），用于把界面元素按屏宽等比放大。
    /// 设置窗（无人操作时它自己返回 `Idle`，不画任何东西）。
    pub settings: SettingsWindow,
    /// 本进程是否管理员（设置窗要显示，重启提权也要用）。
    pub elevated: bool,
    /// 设置里点了"以管理员身份重启"，下一帧收尾退出。
    pub request_exit: bool,
    /// 托盘（这台机器没有托盘时为 `None`，界面照样能用）。
    pub tray: Option<Tray>,
    /// 自己那个窗口的句柄（找到后缓存）。
    pub phone_window: Option<u64>,
    /// 上次把窗口摆到的手机屏矩形，用来避免每帧重复 `SetWindowPos`。
    pub last_phone_rect: Option<snug_core::Rect>,
    /// 日志框已经显示到文件的第几个字节（增量追加用；日志轮换时归零重来）。
    pub log_seen_len: u64,
    /// 正在跑的更新检查（后台线程结果从这里回来；`None` 表示没有任务）。
    pub update_rx: Option<std::sync::mpsc::Receiver<UpdateCheckOutcome>>,
    /// 上一次检查的结果（安装更新要用它拿下载地址）。
    pub last_update: Option<snug_win::UpdateCheckResult>,
    /// 正在跑的下载/校验（进度与结果都从这里回来）。
    pub install_rx: Option<std::sync::mpsc::Receiver<InstallOutcome>>,
    /// App 资源库（后台扫描 + 图标缓存）。
    pub library: Library,
    /// 截图覆盖层（相机键）。
    pub shots: crate::shots::Shots,
    /// "选后复制"的气泡（选区结束才弹）。
    pub bubble: crate::bubble::CopyBubble,
    /// 启动时带了 `--minimized`（开机自启，C# `StartupTaskDefinition.MinimizedArg`）。
    ///
    /// 我们只有一个窗口，没有 C# 那种"启动就弹设置窗"的行为，所以这个标记的全部作用就是
    /// **不闪窗、不抢焦点**：窗口先建在屏幕外，等手机屏就绪再摆上去；升级后重启也沿用
    /// 这个形态。
    pub start_minimized: bool,
    /// 界面窗口当前是不是被挪到屏幕外待命（避免反复 SetWindowPos）。
    pub last_offscreen: Option<bool>,
    /// 收尾是否已经做过（`on_exit` 与 `Drop` 都会调，必须幂等）。
    pub finished: bool,
    /// 桌面预览（对应 C# `OpenScreenPreview` 开的那个只读预览窗）。
    ///
    /// 我们只有一个窗口，所以预览不是"再开一个窗"，而是把**这个**窗口搬到主屏上、
    /// 按机型比例缩放到工作区里，并且只读——C# 预览模式下所有会动手机屏的动作
    /// 都只弹一条"预览模式只读"提示。
    pub preview: Option<PreviewState>,
    /// 当前生效的是不是深色主题（`Auto` 时由系统决定）。
    pub dark: bool,
    /// 界面里手写颜色用的调色板（随主题切换）。
    pub palette: crate::theme::Palette,
    /// 当前界面正在显示哪块屏（`None` = 主触控屏）。
    ///
    /// 多屏时界面顶部有切换：一块屏一个 desk，状态/铺窗/指示条都跟着这块屏走。
    /// **与 C# 的差别**：C# 是"一块屏一个 `PhoneHomeWindow`"，我们只有一块窗口，
    /// 所以同一时刻只画一块屏。理由见 `docs/multi-screen.md`。
    pub active_screen: Option<String>,
    /// 用户按下了重入 pill 上的「×」（C# `_reentryPillHidden`）：这次会话里不再弹。
    pub reentry_pill_hidden: bool,
    /// 点了「⏏ 退出手机屏」，正在等确认（C# `_confirmDismiss`）。
    pub confirm_dismiss: bool,
}

/// 桌面预览的运行态。
#[derive(Debug, Clone)]
pub struct PreviewState {
    /// 界面整体缩放（设计 540 点 → 屏幕点），对应 C# 的 `ScaleTransform`。
    pub scale: f32,
}

impl Drop for PhoneHomeApp {
    fn drop(&mut self) {
        // eframe 在某些路径下不会调 `on_exit`（真机撞到过），Drop 兜底。
        self.finish();
    }
}

/// 后台检查更新的结果。
pub type UpdateCheckOutcome = Result<snug_win::UpdateCheckResult, snug_win::UpdateError>;

/// 下载线程发回的消息：先是一串进度，最后是结果。
#[derive(Debug)]
pub enum InstallOutcome {
    Progress(f64),
    Done(Result<snug_win::PreparedUpdate, snug_win::UpdateError>),
}

impl PhoneHomeApp {
    pub fn new(
        config: AppConfig,
        catalog: Option<PhoneCatalog>,
        paths: Paths,
        pipe_ready: bool,
        start_minimized: bool,
        controller: Controller,
        ui_heartbeat: UiHeartbeat,
    ) -> Self {
        let mut settings = SettingsWindow::new();
        settings.set_catalog(catalog.clone());
        // 托盘必须在主线程建（它建的那个隐藏窗口靠 eframe 的消息泵分发），
        // 而 `new` 就是在 `run_native` 的创建回调里跑的。
        let tray = match Tray::new(false) {
            Some(tray) => {
                crate::info!("托盘已就绪");
                Some(tray)
            }
            None => {
                crate::warn!("托盘创建失败（可能没有 shell 会话），本次没有托盘图标");
                None
            }
        };
        Self {
            config,
            paths,
            pipe_ready,
            pipe_checked: None,
            controller,
            ui_heartbeat,
            tab: HomeTab::Running,
            search: String::new(),
            window_focused: false,
            settings,
            elevated: snug_win::is_elevated(),
            request_exit: false,
            tray,
            phone_window: None,
            last_phone_rect: None,
            log_seen_len: 0,
            update_rx: None,
            last_update: None,
            install_rx: None,
            library: Library::new(),
            shots: crate::shots::Shots::new(std::sync::Arc::new(|| {})),
            bubble: crate::bubble::CopyBubble::new(),
            finished: false,
            start_minimized,
            last_offscreen: None,
            preview: None,
            // 真正的应用发生在 `update()` 第一帧（那里才拿得到 `Context`）。
            dark: false,
            palette: crate::theme::Palette::DARK,
            active_screen: None,
            reentry_pill_hidden: false,
            confirm_dismiss: false,
        }
    }

    /// 现在是不是"重入 pill"形态：手机屏被收起、还能重进、没开预览、用户没按过 ×。
    ///
    /// 对应 C# `PhoneReentryWindow`：屏收起时在**主屏右下角**留一个小胶囊，
    /// 点一下就回到手机屏。C# 那是独立的置顶窗口；我们只有一块窗口，
    /// 收起时就把它本身缩成这个胶囊（不新开视口，见 `docs/multi-screen.md`）。
    fn pill_mode(&self) -> bool {
        self.preview.is_none()
            && !self.reentry_pill_hidden
            && !self.settings.is_open()
            && self.state().screen_phase != PhoneScreenPhase::Ready
            && self.controller.can_reenter()
    }

    /// 胶囊该摆在哪、多大（物理像素）。照 C# `ShowAtBottomRight`：
    /// 主屏工作区右下角、留 18 点边距，尺寸取 260×52 点。
    fn pill_rect(&self, ctx: &egui::Context) -> Option<snug_core::Rect> {
        const WIDTH_POINTS: f64 = 260.0;
        const HEIGHT_POINTS: f64 = 52.0;
        const MARGIN_POINTS: f64 = 18.0;
        let area = self.controller.main_area()?;
        let native = (ctx.pixels_per_point() / ctx.zoom_factor()) as f64;
        let width = WIDTH_POINTS * native;
        let height = HEIGHT_POINTS * native;
        let margin = MARGIN_POINTS * native;
        Some(snug_core::Rect::new(
            (area.x + area.width - width - margin).max(area.x + margin),
            (area.y + area.height - height - margin).max(area.y + margin),
            width,
            height,
        ))
    }

    /// 画重入胶囊（只有 [`Self::pill_mode`] 为真时才走这里）。
    fn draw_pill(&mut self, ctx: &egui::Context) {
        let palette = self.palette;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(palette.card)
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let enter = ui.add(
                        egui::Button::new(
                            egui::RichText::new("📱 进入手机屏")
                                .size(15.0)
                                .color(palette.text),
                        )
                        .frame(false),
                    );
                    if enter.clicked() {
                        crate::info!("重入胶囊：进入手机屏");
                        self.reentry_pill_hidden = false;
                        self.last_phone_rect = None;
                        self.controller.reenter();
                        ctx.request_repaint();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new(egui::RichText::new("×").size(15.0)).frame(false))
                            .on_hover_text("收起这个按钮（仍可从托盘菜单重进）")
                            .clicked()
                        {
                            crate::info!("重入胶囊：用户收起");
                            self.reentry_pill_hidden = true;
                            self.last_offscreen = None;
                            ctx.request_repaint();
                        }
                    });
                });
            });
    }

    /// 当前显示的这块屏的状态（`active_screen` 为空时是主触控屏）。
    fn state(&self) -> snug_core::desk::DeskState {
        self.controller.state_of(self.active_screen.as_deref())
    }

    /// 后台查一次新版（C# `OnCheckUpdate`：`await _updates.CheckAsync()` 之后刷界面）。
    ///
    /// 发布仓库由编译期 `SNUG_RELEASES_REPO` 决定；没配置时更新器直接返回
    /// `NotConfigured`，**一个请求都不发**——这样界面不会拿 C# 版的仓库去查。
    fn start_update_check(&mut self) {
        use crate::settings::UpdateUiState;

        let service = snug_win::update::AppUpdateService::new(
            snug_win::update::CurlHttpClient::default(),
        );
        if !service.is_configured() {
            crate::warn!("设置：更新检查不可用——构建时没有设置 SNUG_RELEASES_REPO");
            self.settings
                .set_update_state(UpdateUiState::CheckFailed("未配置发布仓库".to_string()));
            return;
        }

        crate::info!("设置：开始检查更新");
        self.settings.set_update_state(UpdateUiState::Checking);
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        if let Err(err) = std::thread::Builder::new()
            .name("snug-update-check".to_string())
            .spawn(move || {
                let _ = tx.send(service.check_for_update());
            })
        {
            self.update_rx = None;
            crate::warn!("设置：无法启动更新检查线程 {err}");
            self.settings.set_update_state(UpdateUiState::CheckFailed(err.to_string()));
        }
    }

    /// 每帧收一次后台检查的结果。
    fn poll_update_check(&mut self) {
        use crate::settings::UpdateUiState;
        use std::sync::mpsc::TryRecvError;

        let Some(rx) = &self.update_rx else { return };
        let outcome = match rx.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.update_rx = None;
                crate::warn!("设置：更新检查线程异常结束");
                self.settings.set_update_state(UpdateUiState::CheckFailed(
                    "检查线程异常结束".to_string(),
                ));
                return;
            }
        };
        self.update_rx = None;

        match outcome {
            Ok(result) if result.has_update => {
                crate::info!(
                    "设置：发现新版本 v{}（当前 v{}）",
                    result.latest_version,
                    result.current_version
                );
                self.settings.set_update_state(UpdateUiState::Available {
                    version: result.latest_version.clone(),
                    downloadable: result.is_downloadable(),
                });
                self.last_update = Some(result);
            }
            Ok(result) => {
                crate::info!("设置：已是最新版本 v{}", result.current_version);
                self.settings.set_update_state(UpdateUiState::UpToDate);
                self.last_update = None;
            }
            Err(err) => {
                crate::warn!("设置：检查更新失败 {err}");
                self.settings
                    .set_update_state(UpdateUiState::CheckFailed(err.to_string()));
                self.last_update = None;
            }
        }
    }

    /// 下载 + 校验 + 解包，成功后交给 `apply-update.ps1` 并退出（C# `OnInstallUpdate`）。
    fn start_install_update(&mut self) {
        use crate::settings::UpdateUiState;

        let Some(check) = self.last_update.clone() else {
            crate::warn!("设置：还没有检查结果，无法安装更新");
            return;
        };
        let version = check.latest_version.clone();
        if self.elevated {
            self.settings.set_update_state(UpdateUiState::InstallFailed {
                version,
                message: "自动更新只在普通权限下执行，请退出并普通启动 Snug；受保护安装目录请手动替换 EXE。".into(),
            });
            return;
        }
        crate::info!("设置：开始下载 v{version}");

        let service = snug_win::update::AppUpdateService::new(
            snug_win::update::CurlHttpClient::default(),
        );
        if !service.is_configured() {
            self.settings.set_update_state(UpdateUiState::InstallFailed {
                version,
                message: "未配置发布仓库".to_string(),
            });
            return;
        }

        self.settings.set_update_state(UpdateUiState::Downloading {
            version: version.clone(),
            progress: 0.0,
        });
        let (tx, rx) = std::sync::mpsc::channel();
        self.install_rx = Some(rx);
        let progress_tx = tx.clone();
        if let Err(err) = std::thread::Builder::new()
            .name("snug-update-download".to_string())
            .spawn(move || {
                let mut progress = |fraction: f64| {
                    let _ = progress_tx.send(InstallOutcome::Progress(fraction));
                };
                let outcome = service.download_and_prepare(&check, &mut progress);
                let _ = tx.send(InstallOutcome::Done(outcome));
            })
        {
            self.install_rx = None;
            crate::warn!("设置：无法启动下载线程 {err}");
            self.settings.set_update_state(UpdateUiState::InstallFailed {
                version,
                message: err.to_string(),
            });
        }
    }

    /// 每帧收一次下载进度与结果。
    fn poll_install_update(&mut self) {
        use crate::settings::UpdateUiState;
        use std::sync::mpsc::TryRecvError;

        loop {
            let Some(rx) = &self.install_rx else { return };
            let message = match rx.try_recv() {
                Ok(message) => message,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.install_rx = None;
                    crate::warn!("设置：下载线程异常结束");
                    let version = self
                        .last_update
                        .as_ref()
                        .map(|check| check.latest_version.clone())
                        .unwrap_or_default();
                    self.settings.set_update_state(UpdateUiState::InstallFailed {
                        version,
                        message: "下载线程异常结束".to_string(),
                    });
                    return;
                }
            };
            match message {
                InstallOutcome::Progress(fraction) => {
                    let version = self
                        .last_update
                        .as_ref()
                        .map(|check| check.latest_version.clone())
                        .unwrap_or_default();
                    self.settings.set_update_state(UpdateUiState::Downloading {
                        version,
                        progress: fraction as f32,
                    });
                }
                InstallOutcome::Done(outcome) => {
                    self.install_rx = None;
                    match outcome {
                        Ok(prepared) => {
                            crate::info!(
                                "设置：v{} 已下载并校验通过，交给 apply-update.ps1 后退出",
                                prepared.version
                            );
                            self.settings.set_update_state(UpdateUiState::Ready);
                            // 换完再起来的那个实例要沿用这次的启动形态（C#
                            // `ApplyPreparedUpdateAndRestartAsync` 也是读自己的命令行）：
                            // 开机自启起来的就该还是静默的，不该因为一次升级变成弹窗。
                            let relaunch_minimized = self.start_minimized;
                            match snug_win::update::launch_apply_script(&prepared, relaunch_minimized) {
                                Ok(script) => {
                                    crate::info!("设置：替换脚本已启动 {}，本进程退出", script.display());
                                    self.request_exit = true;
                                }
                                Err(err) => {
                                    crate::warn!("设置：启动替换脚本失败 {err}");
                                    self.settings.set_update_state(UpdateUiState::InstallFailed {
                                        version: prepared.version,
                                        message: err.to_string(),
                                    });
                                }
                            }
                        }
                        Err(err) => {
                            crate::warn!("设置：下载/校验失败 {err}");
                            let version = self
                                .last_update
                                .as_ref()
                                .map(|check| check.latest_version.clone())
                                .unwrap_or_default();
                            self.settings.set_update_state(UpdateUiState::InstallFailed {
                                version,
                                message: err.to_string(),
                            });
                        }
                    }
                    return;
                }
            }
        }
    }

    /// 增量刷新设置窗里的日志框（C# `LogAppended` 事件的效果）。
    ///
    /// 首帧或跨天轮换时整体取末尾 200 行，之后只把新增的字节按行追加上去：
    /// 日志一天能写几万字，每帧 `read_to_string` 整份文件会把界面拖卡。
    fn refresh_logs(&mut self) {
        use std::io::{Read, Seek, SeekFrom};

        let Some(path) = crate::logging::current_file() else { return };
        let len = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        if self.log_seen_len == 0 || len < self.log_seen_len {
            self.settings.set_logs(crate::logging::tail(200));
            self.log_seen_len = len;
            return;
        }
        if len == self.log_seen_len {
            return;
        }
        let Ok(mut file) = std::fs::File::open(&path) else { return };
        if file.seek(SeekFrom::Start(self.log_seen_len)).is_err() {
            return;
        }
        let mut buffer = Vec::new();
        if file.read_to_end(&mut buffer).is_err() {
            return;
        }
        self.log_seen_len = len;
        for line in String::from_utf8_lossy(&buffer).lines() {
            self.settings.append_log(line);
        }
    }

    /// 一次性收尾：撤托盘、撤手机屏、把窗口还回去、存配置。
    ///
    /// 真机上撞到过 **eframe 没有调 `on_exit`**（进程正常退出、日志里一行收尾都没有，
    /// 虚拟屏就留在桌面上）。所以收尾做成幂等，`on_exit` 和 `Drop` 都调它：
    /// 只要它被调用一次，桌面就不会留孤儿屏。
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        crate::info!("收尾：开始（撤托盘 + 撤屏 + 归位窗口 + 存配置）");
        self.ui_heartbeat.stop();
        // 托盘要先撤掉：进程退出后留着图标，explorer 会留一个"幽灵图标"直到鼠标划过。
        self.tray = None;
        // 先让状态机收干净（窗口归位、手机屏撤除），再存配置。
        self.controller.shutdown();
        let file = self.paths.config_file();
        let json = self.config.to_json();
        match std::fs::write(&file, json) {
            Ok(()) => crate::info!("收尾：配置已保存 {}", file.display()),
            Err(err) => crate::warn!("收尾：配置保存失败 {}：{err}", file.display()),
        }
        crate::info!("收尾：完成");
    }

    /// 让界面窗口跟着手机屏走：屏在就把窗口搬上去，屏没了就藏起来。
    ///
    /// 对应 C# 里 `PhoneHomeWindow` 始终待在虚拟屏上（由 `_displays.BoundsOf` 定位），
    /// 没有手机屏时整个程序只剩托盘图标。这条是"Rust 版看起来像不像原版"的关键，
    /// 所以失败时只记 warn，不阻塞界面。
    fn sync_window(&mut self, ctx: &egui::Context) {
        let state = self.state();
        let ready = state.screen_phase == PhoneScreenPhase::Ready;
        // 有窗口铺在手机屏上时，我们只留底部那条指示条（C# 里 `PhoneHomeWindow` 隐、
        // `HomeBarWindow` 显）；没有窗口铺着时，整块手机屏都是资源库。
        //
        // 注意条件必须是「铺着 **且** 不在看列表」：点了指示条上的「‹ 资源库」之后
        // `list_visible` 变 true，但窗口还在手机屏上（`placed` 仍有值）。如果只看
        // `placed`，界面就永远停在指示条，用户怎么点都回不到资源库——真机反馈的
        // 「打开后没法返回那个列表」就是这条。
        // 设置窗打开时必须让窗口恢复整屏：设置窗是画在**当前窗口**里的 `egui::Window`，
        // 而指示条状态下窗口只有 1080×56，把它塞进去只会剩一条灰边（真机反馈）。
        let showing_bar =
            state.placed.is_some() && !state.list_visible && !self.settings.is_open()
                && state.screen_phase == PhoneScreenPhase::Ready;
        // 手机屏起来了就把预览让掉：我们只有一个窗口，C# 那边预览窗和手机屏窗是两个。
        if ready && self.preview.is_some() {
            self.preview = None;
            ctx.set_zoom_factor(1.0);
            crate::info!("预览：手机屏已就绪，预览窗口让位");
        }
        let bounds = if ready {
            if showing_bar {
                self.controller.home_bar_area_of(self.active_screen.as_deref())
            } else {
                self.controller.phone_bounds_of(self.active_screen.as_deref())
            }
        } else if self.preview.is_some() {
            self.preview_rect(ctx)
        } else if self.settings.is_open() {
            // 手机屏收起/没连，但用户从托盘打开了设置：设置是画在窗口里的，
            // 窗口只有胶囊那么大就会被裁掉，所以临时按"主屏上的整块手机界面"显示。
            match self.desktop_layout(ctx) {
                Some((rect, scale)) => {
                    self.apply_ui_scale(ctx, scale, "设置（无手机屏）");
                    Some(rect)
                }
                None => None,
            }
        } else if self.pill_mode() {
            self.pill_rect(ctx)
        } else {
            None
        };

        let Some(bounds) = bounds else {
            // 手机屏不在了：**不能把窗口隐藏**。
            // 真机实测：窗口一旦 `Visible(false)`，eframe/winit 就不再出帧，
            // `update()` 几乎不跑 → 托盘点击、退出请求、后续搬窗全部失效。
            // 做法改成"挪到屏幕外"：Windows 认为它可见（帧照常来），用户看不见。
            self.last_phone_rect = None;
            if self.phone_window.is_none() {
                self.phone_window = snug_win::find_own_window(WINDOW_TITLE);
            }
            if let Some(hwnd) = self.phone_window
                && self.last_offscreen != Some(true)
            {
                let far = snug_core::Rect::new(-30_000.0, -30_000.0, 540.0, 800.0);
                if snug_win::apply_placement(hwnd, &snug_win::PlacementRequest::new(far)) {
                    crate::info!("手机屏未就绪：界面窗口挪到屏幕外待命");
                    self.last_offscreen = Some(true);
                }
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            return;
        };
        self.last_offscreen = Some(false);

        if self.phone_window.is_none() {
            self.phone_window = snug_win::find_own_window(WINDOW_TITLE);
        }
        let Some(hwnd) = self.phone_window else {
            crate::warn!("找不到自己的窗口「{WINDOW_TITLE}」，手机屏界面暂留主屏");
            return;
        };

        // 除了"我们想要的矩形变了"，还要核对"窗口实际还在不在那里"：显示器 DPI 变化
        // （虚拟屏是 116.7%）时 winit 会按新比例自己改窗口尺寸，绕开 `SetWindowPos`，
        // 结果界面比屏小一圈、右边和底部留黑边。每帧比一次，不一致就重新摆。
        let placed_ok = self.last_phone_rect == Some(bounds)
            && snug_win::frame_of(hwnd)
                .map(|got| got.matches(bounds, 2.0))
                .unwrap_or(false);
        if !placed_ok {
            // 尺寸与位置都交给 `SetWindowPos`（物理像素，和虚拟屏分辨率同一套单位）。
            //
            // 这里**不能**再发 `ViewportCommand::InnerSize`：那是 egui 的逻辑点，eframe 会按
            // `pixels_per_point` 换算回物理像素，和 `SetWindowPos` 抢同一个窗口尺寸。
            let mut request = snug_win::PlacementRequest::new(bounds);
            // `WS_EX_NOACTIVATE` 不能一直钉着：钉住以后这个窗口**永远拿不到键盘焦点**，
            // 资源库上的搜索框点了也打不出字（真机反馈）。
            //
            // C# 是按需设的（`PhoneHomeWindow.ShowOnDisplay` 里
            // `noActivate: !AcceptsKeys`）：只有"屏幕上是一个铺好的 App、界面只剩底下那条"
            // 时才不抢焦点。我们照这个口径：整屏资源库、设置、预览这些"要输入"的形态
            // 允许被点击激活；只剩底部指示条/胶囊时仍然不抢。
            //
            // 注意 `focus_if_activated` 保持 false：允许被**点**激活，但绝不主动抢前台，
            // 否则每一帧摆窗口都会把焦点从用户正在用的窗口上抢走。
            request.no_activate = showing_bar || self.pill_mode();
            if snug_win::apply_placement(hwnd, &request) {
                crate::info!(
                    "{}：{}，搬到 ({}, {}) {}×{}",
                    if self.preview.is_some() {
                        "预览窗"
                    } else if self.pill_mode() {
                        "重入胶囊"
                    } else {
                        "手机屏界面"
                    },
                    if showing_bar {
                        "收成底部指示条"
                    } else if self.preview.is_some() {
                        "预览（只读）"
                    } else if self.pill_mode() {
                        "主屏右下角"
                    } else {
                        "铺满资源库"
                    },
                    bounds.x,
                    bounds.y,
                    bounds.width,
                    bounds.height
                );
                self.last_phone_rect = Some(bounds);
            } else {
                crate::warn!("搬动手机屏界面失败（SetWindowPos 返回失败）");
            }
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    }

    /// 预览窗该摆在哪：主屏工作区里按机型比例居中。
    ///
    /// 缩放规则照 C# `ScreenFit.ComputePreviewLayout`：最宽取工作区 72%、最高取 84% 减 48，
    /// 缩放上限 0.84、下限 0.35。C# 用 `ScaleTransform` 缩整个界面，egui 这边用
    /// `zoom_factor` 达到同样效果——窗口物理尺寸 = 设计点 × 缩放 × 屏幕原生 DPI。
    ///
    /// 返回 `None` 表示拿不到主屏区域，调用方按"没有手机屏"处理（窗口挪到屏幕外）。
    fn desktop_layout(&self, ctx: &egui::Context) -> Option<(snug_core::Rect, f32)> {
        let area = self.controller.main_area()?;
        let fit = snug_core::ScreenFit::from_phone(&self.config.phone.screen.to_fit_spec());
        let (design_width, design_height, scale) = snug_core::ScreenFit::compute_preview_layout(
            snug_core::Size::new(area.width, area.height),
            &fit,
        );
        // 原生 DPI = 当前有效 ppp ÷ 我们设过的 zoom（zoom 会改 ppp，除回去才稳定）。
        let native = (ctx.pixels_per_point() / ctx.zoom_factor()) as f64;
        let width = design_width * scale * native;
        let height = design_height * scale * native;
        let rect = snug_core::Rect::new(
            area.x + (area.width - width) / 2.0,
            area.y + (area.height - height) / 2.0,
            width,
            height,
        );
        Some((rect, scale as f32))
    }

    /// 把缩放应用到 egui（窗口尺寸另算）。
    fn apply_ui_scale(&self, ctx: &egui::Context, scale: f32, why: &str) {
        if (ctx.zoom_factor() - scale).abs() > 0.01 {
            ctx.set_zoom_factor(scale);
            crate::info!("{why}：界面缩放 {scale:.2}");
        }
    }

    /// 预览模式的窗口矩形（会把 `preview` 状态置上）。
    fn preview_rect(&mut self, ctx: &egui::Context) -> Option<snug_core::Rect> {
        let (rect, scale) = self.desktop_layout(ctx)?;
        self.apply_ui_scale(ctx, scale, "预览");
        self.preview = Some(PreviewState { scale });
        Some(rect)
    }

    /// 开桌面预览（C# `OpenScreenPreview`）。
    ///
    /// C# 里手机屏已经就绪时不去开预览窗，而是把资源库列表翻出来——这里保持一致。
    fn open_preview(&mut self, ctx: &egui::Context) {
        let state = self.state();
        if state.screen_phase == PhoneScreenPhase::Ready {
            crate::info!("预览：手机屏已就绪，改为回到资源库列表");
            self.controller.show_list_of(self.active_screen.as_deref());
            return;
        }
        if self.preview.is_some() {
            return;
        }
        crate::info!("预览：打开桌面预览（只读）");
        // 先把窗口摆正，下一帧 `sync_window` 会按实际可摆放的矩形再校准一次。
        self.last_phone_rect = None;
        self.last_offscreen = None;
        if self.preview_rect(ctx).is_none() {
            crate::warn!("预览：拿不到主屏工作区，无法摆放预览窗");
            self.preview = None;
        }
        ctx.request_repaint();
    }

    /// 关掉桌面预览，界面窗口回到屏幕外待命。
    fn close_preview(&mut self, ctx: &egui::Context) {
        if self.preview.take().is_some() {
            crate::info!("预览：关闭");
            ctx.set_zoom_factor(1.0);
            self.last_phone_rect = None;
            self.last_offscreen = None;
            ctx.request_repaint();
        }
    }


    /// 开机自启服务（每次都按当前 exe 现取，换目录后也能对上）。
    fn startup_service() -> snug_win::StartupService<snug_win::startup::SystemRunner> {
        snug_win::StartupService::for_current_exe()
    }

    /// 把界面读到的运行态喂给设置窗（对应 C# 每次打开设置窗时的 `Refresh`）。
    fn refresh_settings_status(&mut self) {
        self.refresh_pipe_state();
        let state = self.state();
        let status = StatusInfo {
            pipe_available: self.pipe_ready,
            elevated: self.elevated,
            phone_screen_ready: state.screen_phase == PhoneScreenPhase::Ready,
            can_reenter: self.controller.can_reenter(),
            status_summary: Self::phase_text(state.screen_phase).to_string(),
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            self_contained: true,
        };
        self.settings.set_status(status);
        self.settings.set_autostart(Self::startup_service().is_enabled());
        self.settings.set_manual_override(self.controller.manual_override());
        // 日志框：只在"驱动与日志"页增量刷新，别每帧把整个文件读进文本框。
        if self.settings.tab() == SettingsTab::Diagnostics {
            self.refresh_logs();
        }
    }

    /// 重新查一次驱动管道状态（250ms 节流；只在设置窗开着时被调）。
    ///
    /// 没有这一步，自动装完驱动后设置窗会一直显示「未检测到」——因为它读的是启动
    /// 那一刻的值。状态翻转记一行日志，便于和安装日志对上时间。
    fn refresh_pipe_state(&mut self) {
        const INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
        let now = std::time::Instant::now();
        let stale = self
            .pipe_checked
            .map(|last| now.duration_since(last) >= INTERVAL)
            .unwrap_or(true);
        if !stale {
            return;
        }
        self.pipe_checked = Some(now);
        let live = crate::driver::pipe_available();
        if live != self.pipe_ready {
            crate::info!(
                "驱动：管道状态 {} → {}",
                if self.pipe_ready { "就绪" } else { "未就绪" },
                if live { "就绪" } else { "未就绪" }
            );
            self.pipe_ready = live;
        }
    }

    /// 执行设置窗排出来的动作。副作用都在这里落地，`settings.rs` 本身不做事。
    fn apply_setting(&mut self, action: SettingsAction, ctx: &egui::Context) {
        match action {
            SettingsAction::SetPhoneScreen(on) => {
                crate::info!("设置：手机屏 → {on}");
                self.controller.set_phone_screen(on);
            }
            SettingsAction::ReenterScreen => {
                crate::info!("设置：进入手机屏");
                self.controller.reenter();
            }
            SettingsAction::OpenPreview => {
                crate::info!("设置：预览界面");
                self.open_preview(ctx);
            }
            SettingsAction::SetManualOverride(on) => {
                crate::info!("设置：手动强制手机屏 → {on}");
                self.controller.set_manual_override(on);
            }
            SettingsAction::SetAutoStart(on) => {
                let service = Self::startup_service();
                match service.set_enabled(on) {
                    Ok(()) => {
                        crate::info!("设置：开机自启 → {on}（任务 {}）", service.task_name());
                        self.settings.set_autostart(service.is_enabled());
                    }
                    Err(err) => crate::warn!("设置：开机自启失败（{on}）：{err}"),
                }
            }
            SettingsAction::OpenLogDirectory => {
                let dir = self.paths.log_directory();
                crate::info!("设置：打开日志目录 {}", dir.display());
                let explorer = snug_win::system_paths::explorer();
                if let Err(err) = explorer.and_then(|path| std::process::Command::new(path).arg(&dir).spawn()) {
                    crate::warn!("打开日志目录失败：{err}");
                }
            }
            SettingsAction::OpenRepository => {
                // 仓库地址只在根 Cargo.toml 的 repository 里写一份；没配就明说，不猜 URL。
                const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
                if REPOSITORY.is_empty() {
                    crate::warn!("设置：仓库地址未配置（Cargo.toml 缺 repository），无法打开");
                } else if let Err(err) = snug_win::system_paths::system_tool("cmd.exe")
                    .and_then(|path| std::process::Command::new(path)
                    .args(["/c", "start", "", REPOSITORY])
                    .spawn())
                {
                    crate::warn!("打开仓库主页失败：{err}");
                }
            }
            SettingsAction::RestartElevated => {
                if self.elevated {
                    crate::info!("设置：已经是管理员，无需重启");
                } else {
                    match snug_win::restart_elevated(&["--wait-for-parent".into(), std::process::id().to_string()]) {
                        Ok(()) => {
                            crate::info!("设置：已请求以管理员身份重启，本进程退出");
                            self.request_exit = true;
                        }
                        Err(err) => crate::warn!("提权重启失败（可能是 UAC 被取消）：{err}"),
                    }
                }
            }
            SettingsAction::CheckUpdate => self.start_update_check(),
            SettingsAction::InstallUpdate => self.start_install_update(),
            SettingsAction::InstallDriver => {
                match crate::driver::spawn_install() {
                    Ok(()) => crate::info!("设置：已请求使用内嵌包安装/修复虚拟屏驱动"),
                    Err(err) => crate::warn!("设置：启动驱动安装任务失败 {err}"),
                }
            }
            SettingsAction::ApplyGestureSettings => {
                crate::info!("设置：手势配置已变，重建钩子");
                self.controller.apply_gesture_settings(&self.config);
            }
        }
    }

    /// 铺着窗口时的那一条底部指示条（对应 C# `HomeBarWindow`）。
    ///
    /// 这一条要**一直盖在被铺开的窗口上面**，否则用户没法回到资源库；所以它由我们
    /// 自己的置顶窗口承担（`PlacementRequest` 默认 `topmost`）。
    fn draw_home_bar(&mut self, ctx: &egui::Context) {
        let placed = self.state().placed.map(|placed| placed.window);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(self.palette.home_bar)
                    .inner_margin(egui::Margin::symmetric(8, 2)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    if ui.button("‹ 资源库").clicked() {
                        crate::info!("指示条：回到资源库");
                        self.controller.show_list_of(self.active_screen.as_deref());
                    }
                    if let Some(window) = &placed {
                        ui.label(egui::RichText::new(window.label()).small().weak());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("⚙").on_hover_text("设置").clicked() {
                            crate::info!("指示条：打开设置");
                            self.settings.open();
                        }
                        if let Some(window) = &placed
                            && self.config.screenshot
                            && ui
                                .add_enabled(!self.shots.is_open(), egui::Button::new("📷"))
                                .clicked()
                        {
                            self.shots.take(window);
                        }
                    });
                });
            });
    }


/// 手机屏当前阶段的简短文案。
    fn phase_text(phase: PhoneScreenPhase) -> &'static str {
        match phase {
            PhoneScreenPhase::Off => "手机屏：关",
            PhoneScreenPhase::Preparing => "手机屏：准备中",
            PhoneScreenPhase::Ready => "手机屏：在线",
            PhoneScreenPhase::Failed => "手机屏：失败",
        }
    }
}

impl eframe::App for PhoneHomeApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.finish();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 主题：每帧核对一次（`Auto` 还要看系统设置变没变），变了才重建样式。
        let dark = crate::theme::resolve_dark(self.config.theme);
        if dark != self.dark || self.palette.dark != dark {
            self.dark = dark;
            self.palette = crate::theme::palette(dark);
            crate::theme::apply(ctx, dark);
            crate::info!(
                "主题：{}（{}）",
                self.config.theme.label(),
                if dark { "深色" } else { "浅色" }
            );
        }

        // 关闭请求在自己的代码里拦一次：真机上 eframe 的 `on_exit` 没被调用过，
        // 结果虚拟屏留在了桌面上。这里收尾是可靠的（进程还活着，事件循环还在跑）。
        if ctx.input(|i| i.viewport().close_requested()) {
            crate::info!("界面收到关闭请求");
            self.finish();
        }
        // 数据目录里的 `quit.request`：给脚本/外部程序一个"体面退出"的入口。
        // 真机上 `CloseMainWindow` 会让进程直接消失、不走任何 Rust 收尾，这条才是
        // 能保证"撤屏 + 归位窗口"的路。
        let quit_flag = self.paths.data_dir().join("quit.request");
        if quit_flag.exists() {
            let _ = std::fs::remove_file(&quit_flag);
            crate::info!("收到退出请求文件，开始收尾");
            self.finish();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        // 手势钩子跟着手机屏与配置走（区域没变时什么都不做）。
        self.controller.sync_touch(&self.config);

        // 选区结束 → 在选区上方弹"复制"气泡（C# `ShowCopySelectionBubble`）。
        for event in self.controller.take_selection_events() {
            match event {
                TouchEvent::SelectionCompleted(at) => {
                    let state = self.state();
                    if self.config.copy_after_selection
                        && !state.list_visible
                        && state.screen_phase == PhoneScreenPhase::Ready
                    {
                        crate::info!("手势：选区结束，弹复制气泡");
                        self.bubble.show_at(at);
                    }
                }
                TouchEvent::SelectionDismissed => self.bubble.hide(),
            }
        }
        if let Some(at) = self.bubble.poll(ctx) {
            let copied = self.controller.copy_selection_at(at);
            crate::info!(
                "复制气泡：{}",
                if copied { "已发送复制快捷键" } else { "未能复制（前台窗口不符或注入被挡）" }
            );
        }

        // 托盘事件：菜单点击、双击。先把事件收齐再处理，避免与 `apply_setting` 抢借用。
        let tray_commands = self.tray.as_mut().map(|tray| tray.poll()).unwrap_or_default();
        for command in tray_commands {
            match command {
                TrayCommand::OpenSettings(tab) => {
                    let tab = SettingsTab::from_index(tab);
                    crate::debug!("托盘：打开设置第 {} 页（{}）", tab.index(), tab.nav_title());
                    self.settings.select_tab(tab);
                    self.settings.open();
                    ctx.request_repaint();
                }
                TrayCommand::OpenPreview => {
                    crate::debug!("托盘：预览手机界面");
                    self.open_preview(ctx);
                }
                TrayCommand::Reenter => {
                    crate::info!("托盘：重新进入手机屏");
                    self.controller.reenter();
                    ctx.request_repaint();
                }
                TrayCommand::SetManualOverride(on) => {
                    self.apply_setting(SettingsAction::SetManualOverride(on), ctx);
                }
                TrayCommand::Quit => {
                    crate::info!("托盘：退出（先还原窗口）");
                    // 先收尾再关窗：关窗之后 Rust 侧可能一行都跑不到（真机实测）。
                    self.finish();
                    self.request_exit = true;
                }
            }
        }
        {
            let state = self.state();
            let summary = Self::phase_text(state.screen_phase).to_string();
            let connected = state.screen_phase == PhoneScreenPhase::Ready;
            let manual = self.controller.manual_override();
            let reenter = self.controller.can_reenter();
            if let Some(tray) = &mut self.tray {
                tray.set_status(&summary, connected);
                tray.set_manual_override(manual);
                tray.set_reenter_available(reenter);
            }
        }
        // 托盘事件来自另一个消息窗口，egui 没有输入就不会重绘；靠这个定时器
        // 让 `poll` 有节奏地跑起来（200ms 足够，不是每帧空转）。
        ctx.request_repaint_after(std::time::Duration::from_millis(200));

        // 界面窗口跟着手机屏走（对应 C# 里 PhoneHomeWindow 一直待在虚拟屏上）。
        self.sync_window(ctx);

        // 截图覆盖层：过期检查 + 画在最上层。
        self.shots.poll();
        self.shots.show(ctx);

        // 设置窗：先执行上一帧排出的动作，再画这一帧（动作会影响配置和控制器状态）。
        self.poll_update_check();
        self.poll_install_update();
        // 资源库：收扫描结果；手机屏就绪或太久没扫就重扫一次。
        self.library.poll();
        if self.library.needs_scan(false) {
            self.library.start_scan();
        }
        for action in self.settings.take_actions() {
            self.apply_setting(action, ctx);
        }
        if self.settings.is_open() {
            self.refresh_settings_status();
        }
        // 设置窗画在手机屏这块界面上：用户是在手机（UU 远程）上操作的，独立窗口开在
        // 电脑主屏反而看不见。顶部有「返回」可以关掉，标题栏的 X 也能关。
        let before = self.config.clone();
        match self.settings.show(ctx, &mut self.config) {
            SettingsOutcome::Changed => {
                let impact = crate::settings::classify_change(&before, &self.config);
                if let Some(hint) = crate::settings::change_hint(impact) {
                    crate::info!("设置：{hint}");
                }
                match impact {
                    ChangeImpact::RebuildPhoneScreen => {
                        // 机型/方向变了：屏愿望里的 fit 跟着变，状态机按新比例重建。
                        self.controller.apply_screens(&self.config);
                        self.controller.set_default_zoom(self.config.zoom_steps);
                    }
                    ChangeImpact::ReloadGestures => {
                        self.controller.apply_gesture_settings(&self.config)
                    }
                    // 主题在每帧开头按 `config.theme` 自动对齐，这里不用做事。
                    ChangeImpact::Theme => {}
                    // 屏列表交给状态机（C# `DeskSet.SetWishes`）。
                    ChangeImpact::Screens => {
                        self.controller.apply_screens(&self.config);
                    }
                    ChangeImpact::RefreshHomeBar | ChangeImpact::None => {}
                }
            }
            SettingsOutcome::CloseRequested => {
                crate::info!("设置：已关闭");
            }
            SettingsOutcome::Idle => {}
        }
        if self.request_exit {
            // 同样先收尾：关窗之后 Rust 侧可能一行都跑不到。
            self.finish();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // 手机屏被收起、还能重进：窗口缩成主屏右下角的小胶囊（C# `PhoneReentryWindow`）。
        if self.pill_mode() {
            self.draw_pill(ctx);
            return;
        }

        // 有窗口铺在手机屏上时，界面只剩底部指示条（C#：PhoneHomeWindow 隐、HomeBarWindow 显）。
        // 和 `sync_window` 同一个条件：点了「‹ 资源库」之后 `list_visible=true`，即使窗口
        // 还铺在手机屏上，也必须回到资源库列表。
        let bar_state = self.state();
        if bar_state.placed.is_some() && !bar_state.list_visible && !self.settings.is_open() {
            self.draw_home_bar(ctx);
            self.shots.poll();
            self.shots.show(ctx);
            return;
        }

        // 从列表回到手机屏上铺着的那个窗口（C# `ReturnToPlaced`）。
        // 从列表回到手机屏上铺着的那个窗口（C# `ReturnToPlaced`）。
        let mut back_to_placed = false;
        let mut pull_window: Option<snug_core::windows::WindowInfo> = None;
        let mut open_app: Option<snug_core::windows::AppInfo> = None;
        let mut new_window: Option<snug_core::windows::RunningApp> = None;
        let mut quit_app: Option<snug_core::windows::RunningApp> = None;
        let mut close_window: Option<snug_core::windows::WindowInfo> = None;

        // ---- 顶部：App 大字 + 搜索 + 两个页签（对齐原版 `PhoneHomeWindow`）----
        let mut close_preview = false;
        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::default().inner_margin(egui::Margin::symmetric(30, 8)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("App")
                            .size(38.0)
                            .strong()
                            .color(self.palette.text),
                    );
                    let header_state = self.state();
                    if let Some(placed) = &header_state.placed {
                        let label = format!("· {}", placed.window.label());
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(label)
                                        .size(15.0)
                                        .color(self.palette.accent),
                                )
                                .frame(false),
                            )
                            .on_hover_text("回到手机屏上的这个窗口")
                            .clicked()
                        {
                            back_to_placed = true;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // 原版右上角是「⏏」（退出手机屏）和「📱」。我们多一个 ⚙ 设置。
                        if ui
                            .add(egui::Button::new("⚙").min_size(egui::vec2(56.0, 44.0)))
                            .on_hover_text("设置与诊断")
                            .clicked()
                        {
                            self.settings.open();
                        }
                        if ui
                            .add(egui::Button::new("⏏").min_size(egui::vec2(56.0, 44.0)))
                            .on_hover_text("退出手机屏（窗口还回原位，这次远程不再自动开屏）")
                            .clicked()
                        {
                            self.confirm_dismiss = true;
                        }
                        let mut phone_on = self.config.phone_screen;
                        if ui
                            .add(
                                egui::Button::new(if phone_on { "📱 开" } else { "📱 关" })
                                    .min_size(egui::vec2(82.0, 44.0)),
                            )
                            .on_hover_text("手机屏总开关")
                            .clicked()
                        {
                            phone_on = !phone_on;
                            self.config.phone_screen = phone_on;
                            self.controller.set_phone_screen(phone_on);
                        }
                    });
                });

                ui.add_space(6.0);
                if let Some(preview) = &self.preview {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "预览模式 · 只读（{:.0}%）",
                                preview.scale * 100.0
                            ))
                            .size(14.0)
                            .color(self.palette.warn),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("关闭预览").clicked() {
                                close_preview = true;
                            }
                        });
                    });
                    ui.label(
                        egui::RichText::new("预览只用于查看 App 资源库；要铺窗口请先连上手机")
                            .size(12.0)
                            .weak(),
                    );
                }
                // 诊断：真机反馈"搜索框打不出字"，要区分三种情况——
                // ①窗口根本没拿到键盘焦点；②焦点在但按键没进 egui；③按键进了、过滤没生效。
                let window_focused = ui.ctx().input(|input| input.focused);
                if window_focused != self.window_focused {
                    self.window_focused = window_focused;
                    crate::info!(
                        "搜索框诊断：窗口焦点 {}",
                        if window_focused { "有" } else { "无" }
                    );
                }
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("🔍 搜索 App")
                        .desired_width(f32::INFINITY),
                );
                if response.gained_focus() {
                    crate::info!("搜索框诊断：输入框拿到焦点");
                }
                if response.changed() {
                    crate::info!(
                        "搜索框诊断：内容变成「{}」（{} 字节）",
                        self.search,
                        self.search.len()
                    );
                }

                ui.add_space(8.0);
                // 两个页签平分配对宽度，并且**始终画底**。
                //
                // 原先用 `egui::Button::selectable`，它等价于 `frame_when_inactive(false)`：
                // 没选中、没悬停时连底板都不画，只剩一行字贴在面板底色上——真机上
                // "未选中的那个页签看不见"。这里改成显式给底色与文字色，
                // 不再依赖 `WidgetVisuals` 的默认值。
                let spacing = ui.spacing().item_spacing.x;
                let tab_width = ((ui.available_width() - spacing) / 2.0).max(120.0);
                let palette = self.palette;
                let mut switched_to = None;
                ui.horizontal(|ui| {
                    if tab_button(ui, tab_width, palette, self.tab == HomeTab::Running, "运行中") {
                        switched_to = Some(HomeTab::Running);
                    }
                    if tab_button(ui, tab_width, palette, self.tab == HomeTab::All, "全部") {
                        switched_to = Some(HomeTab::All);
                    }
                });
                if let Some(tab) = switched_to {
                    self.tab = tab;
                }

                // 多屏：一块屏一个 desk，这里选当前显示哪块（C# 是一块屏一个窗口）。
                let screens = self.controller.touch_screens();
                if screens.len() > 1 {
                    ui.add_space(6.0);
                    let current = self
                        .active_screen
                        .clone()
                        .or_else(|| screens.first().map(|(id, _)| id.clone()));
                    ui.horizontal_wrapped(|ui| {
                        for (id, name) in &screens {
                            let selected = current.as_deref() == Some(id.as_str());
                            if ui.selectable_label(selected, name).clicked() {
                                self.active_screen = Some(id.clone());
                                self.controller.set_active_screen(Some(id.clone()));
                                // 换了屏要重新摆窗口。
                                self.last_phone_rect = None;
                                self.last_offscreen = None;
                                ctx.request_repaint();
                            }
                        }
                    });
                }
            });

        if back_to_placed {
            crate::info!("资源库：回到手机屏上的窗口");
            self.controller.return_to_placed_of(self.active_screen.as_deref());
        }
        if close_preview {
            self.close_preview(ctx);
        }

        // ---- 退出手机屏的确认条（C# `DismissBanner`）----
        let mut dismiss_now = false;
        let mut dismiss_later = false;
        if self.confirm_dismiss {
            egui::TopBottomPanel::top("dismiss-banner")
                .frame(
                    egui::Frame::default()
                        .fill(self.palette.card)
                        .inner_margin(egui::Margin::symmetric(18, 12)),
                )
                .show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new("退出手机屏？")
                            .size(18.0)
                            .strong()
                            .color(egui::Color32::from_rgb(0xFF, 0x3B, 0x30)),
                    );
                    ui.label(
                        egui::RichText::new(
                            "铺在手机屏上的窗口会还回原位；这次远程里不再自动开屏，\
                             之后可以点右下角的小胶囊或托盘菜单重进。",
                        )
                        .size(13.0)
                        .color(self.palette.text_weak),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("退出").color(egui::Color32::WHITE),
                                )
                                .fill(egui::Color32::from_rgb(0xFF, 0x3B, 0x30)),
                            )
                            .clicked()
                        {
                            dismiss_now = true;
                        }
                        if ui.button("稍后").clicked() {
                            dismiss_later = true;
                        }
                    });
                });
        }
        if dismiss_later {
            self.confirm_dismiss = false;
        }
        if dismiss_now {
            crate::info!("界面：确认退出手机屏");
            self.confirm_dismiss = false;
            // 收起之后要能看到重入胶囊，所以把"用户已收起胶囊"的标记清掉。
            self.reentry_pill_hidden = false;
            self.controller.dismiss_screen_of(self.active_screen.as_deref());
            ctx.request_repaint();
        }

        // ---- 列表：竖向一行一个 App（原版就是这个结构，不是图标网格）----
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.library.needs_scan(false) {
                self.library.start_scan();
            }
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(8.0);
                match self.tab {
                    HomeTab::Running => {
                        // 搜索对两个页签都生效。原版 C# 里搜索是**独立子页面**
                        // （`SubPage.Search`），一输入就换成搜索结果页，跟当前页签无关；
                        // 我们把它做成了内嵌输入框，就更不能让"运行中"页无视搜索词——
                        // 真机反馈"输了 qq 没反应"就是这条。
                        let apps: Vec<_> = self
                            .library
                            .running()
                            .iter()
                            .filter(|app| self.library.matches_search(&app.app, &self.search))
                            .cloned()
                            .collect();
                        if apps.is_empty() {
                            let (title, hint) = if self.search.trim().is_empty() {
                                (
                                    "没有正在运行的窗口",
                                    "启动一个 App 后，它的窗口会列在这里，点一下铺满手机屏",
                                )
                            } else {
                                ("没有匹配的运行中 App", "换个关键词试试，或切到「全部」")
                            };
                            ui.label(egui::RichText::new(title).size(16.0).weak());
                            ui.label(egui::RichText::new(hint).size(14.0).weak());
                        }
                        for app in apps {
                            let texture = self.library.icon(ctx, &app.app);
                            let count = app.windows.len();
                            let (clicked, plus, cross) =
                                app_row(ui, &app.app.name, Some(count), texture.as_ref(), true);
                            if clicked {
                                pull_window = app.windows.first().cloned();
                            }
                            if plus {
                                new_window = Some(app.clone());
                            }
                            if cross {
                                quit_app = Some(app.clone());
                            }
                            for window in &app.windows {
                                ui.horizontal(|ui| {
                                    ui.add_space(52.0);
                                    let label = format!("◻ {}", window.label());
                                    let width = (ui.available_width() - 48.0).max(80.0);
                                    if ui
                                        .add_sized(
                                            [width, 34.0],
                                            egui::Button::new(egui::RichText::new(label).size(15.0))
                                                .frame(false),
                                        )
                                        .clicked()
                                    {
                                        pull_window = Some(window.clone());
                                    }
                                    if ui
                                        .add(egui::Button::new("×").min_size(egui::vec2(34.0, 34.0)))
                                        .on_hover_text("关闭这个窗口")
                                        .clicked()
                                    {
                                        close_window = Some(window.clone());
                                    }
                                });
                            }
                            ui.add_space(4.0);
                        }
                    }
                    HomeTab::All => {
                        let groups = self.library.grouped(&self.search);
                        if groups.is_empty() {
                            let hint = if !self.library.scanned_once {
                                "正在读取已安装的 App…"
                            } else if self.search.trim().is_empty() {
                                "没有找到已安装的 App"
                            } else {
                                "没有匹配的 App"
                            };
                            ui.label(egui::RichText::new(hint).weak());
                        }
                        // 手机桌面式图标网格：竖屏 4 列、横屏 6 列
                        // （C# `UpdateResponsiveLayout`：`Columns = landscape ? 6 : 4`）。
                        let landscape = ui.available_width() > ui.available_height();
                        let columns = grid_columns(landscape);
                        let gap = 10.0;
                        let cell_w =
                            ((ui.available_width() - gap - gap * columns as f32) / columns as f32)
                                .max(90.0);
                        let icon_side = (cell_w * 0.54).clamp(56.0, 116.0);
                        let cell_h = icon_side + 54.0;
                        for (letter, apps) in &groups {
                            if !letter.is_empty() {
                                ui.add_space(12.0);
                                ui.horizontal(|ui| {
                                    ui.add_space(gap);
                                    ui.label(
                                        egui::RichText::new(letter)
                                            .size(13.0)
                                            .strong()
                                            .color(self.palette.text_weak),
                                    );
                                });
                            }
                            for row in (*apps).chunks(columns) {
                                ui.horizontal(|ui| {
                                    ui.add_space(gap);
                                    for app in row {
                                        let texture = self.library.icon(ctx, app);
                                        let (rect, response) = ui.allocate_exact_size(
                                            egui::vec2(cell_w, cell_h),
                                            egui::Sense::click(),
                                        );
                                        if response.hovered() {
                                            ui.painter().rect_filled(
                                                rect.shrink(2.0),
                                                16.0,
                                                self.palette.card,
                                            );
                                        }
                                        let icon_rect = egui::Rect::from_center_size(
                                            egui::pos2(
                                                rect.center().x,
                                                rect.top() + 8.0 + icon_side / 2.0,
                                            ),
                                            egui::vec2(icon_side, icon_side),
                                        );
                                        match &texture {
                                            Some(texture) => {
                                                ui.painter().image(
                                                    texture.id(),
                                                    icon_rect,
                                                    egui::Rect::from_min_max(
                                                        egui::pos2(0.0, 0.0),
                                                        egui::pos2(1.0, 1.0),
                                                    ),
                                                    egui::Color32::WHITE,
                                                );
                                            }
                                            None => {
                                                ui.painter().rect_filled(
                                                    icon_rect,
                                                    icon_side * 0.22,
                                                    self.palette.card_hover,
                                                );
                                                let initial = app
                                                    .name
                                                    .chars()
                                                    .next()
                                                    .map(|c| c.to_uppercase().to_string())
                                                    .unwrap_or_else(|| "?".to_string());
                                                ui.painter().text(
                                                    icon_rect.center(),
                                                    egui::Align2::CENTER_CENTER,
                                                    initial,
                                                    egui::FontId::proportional(icon_side * 0.42),
                                                    self.palette.text,
                                                );
                                            }
                                        }
                                        let label = if app.name.chars().count() > 10 {
                                            format!(
                                                "{}…",
                                                app.name.chars().take(9).collect::<String>()
                                            )
                                        } else {
                                            app.name.clone()
                                        };
                                        ui.painter().text(
                                            egui::pos2(rect.center().x, rect.top() + icon_side + 16.0),
                                            egui::Align2::CENTER_TOP,
                                            label,
                                            egui::FontId::proportional(14.0),
                                            self.palette.text,
                                        );
                                        if response.clicked() {
                                            match self.library.action_for(app) {
                                                AppAction::Pull(window) => pull_window = Some(window),
                                                AppAction::Open(target) => open_app = Some(target),
                                            }
                                        }
                                    }
                                });
                            }
                        }
                    }
                }
            });
        });

        // 动作在闭包外落地，避免和 `self` 的借用打架。
        // 桌面预览是只读的（C# `ShowPreviewReadOnlyNotice`）：没有虚拟屏，铺窗口/开关 App
        // 全部落不了地，与其静默失败，不如明确不执行。
        if self.preview.is_some()
            && (pull_window.is_some()
                || open_app.is_some()
                || new_window.is_some()
                || quit_app.is_some()
                || close_window.is_some())
        {
            crate::info!("预览：只读模式，忽略这次操作");
            pull_window = None;
            open_app = None;
            new_window = None;
            quit_app = None;
            close_window = None;
        }
        if let Some(window) = pull_window {
            crate::info!("资源库：铺开 {}（{:#x}）", window.label(), window.key.handle);
            self.controller.pull(self.active_screen.as_deref(), window);
        }
        if let Some(app) = open_app {
            crate::info!("资源库：启动 {}", app.name);
            self.controller.open(self.active_screen.as_deref(), app);
        }
        if let Some(app) = new_window {
            crate::info!("资源库：给 {} 新开窗口", app.app.name);
            self.controller.new_window(self.active_screen.as_deref(), app);
        }
        if let Some(app) = quit_app {
            crate::info!("资源库：关闭 {} 的全部窗口", app.app.name);
            self.controller.quit(self.active_screen.as_deref(), app);
        }
        if let Some(window) = close_window {
            crate::info!("资源库：关闭窗口 {}", window.label());
            self.controller.close_window(self.active_screen.as_deref(), window);
        }
    }
}

/// 资源库图标网格的列数：横屏 6 列、竖屏 4 列（C# `UpdateResponsiveLayout` 同）。
fn grid_columns(landscape: bool) -> usize {
    if landscape { 6 } else { 4 }
}

/// 资源库顶部的一个页签（"运行中" / "全部"）。
///
/// 不用 `egui::Button::selectable`：它等价于 `frame_when_inactive(false)`，
/// 没选中也没悬停时连底板都不画，真机上那个页签看起来就是"不见了"。
/// 这里显式给底色与文字色，两种主题下都有足够对比。
fn tab_button(
    ui: &mut egui::Ui,
    width: f32,
    palette: crate::theme::Palette,
    selected: bool,
    label: &str,
) -> bool {
    let (fill, text_color) = if selected {
        (palette.accent, egui::Color32::WHITE)
    } else {
        (palette.card, palette.text)
    };
    let text = egui::RichText::new(label).size(15.0).color(text_color);
    ui.add_sized(
        [width, 44.0],
        egui::Button::new(text)
            .fill(fill)
            .stroke(egui::Stroke::new(1.0_f32, palette.card_hover)),
    )
    .clicked()
}

/// 资源库里的一行 App：图标 + 名字（+ 窗口数），右侧「+」「×」。
///
/// 对应原版 `PhoneHomeWindow` 的 `RunningAppVm` 行：图标 66×66、名字、`N 个窗口`，
/// 右侧「+」（再开一个窗口）和「×」（关掉这个 App 的全部窗口）。
/// 返回 `(点按区, 加号, 叉号)` 是否被点。
fn app_row(
    ui: &mut egui::Ui,
    name: &str,
    count: Option<usize>,
    texture: Option<&egui::TextureHandle>,
    show_actions: bool,
) -> (bool, bool, bool) {
    let mut clicked = false;
    let mut plus = false;
    let mut cross = false;

    ui.horizontal(|ui| {
        let icon = match texture {
            Some(texture) => ui.add(
                egui::Image::new(texture)
                    .fit_to_exact_size(egui::vec2(46.0, 46.0))
                    .sense(egui::Sense::click()),
            ),
            None => {
                let initial = name
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "?".to_string());
                ui.add_sized(
                    [46.0, 46.0],
                    egui::Button::new(egui::RichText::new(initial).size(20.0)),
                )
            }
        };
        if icon.clicked() {
            clicked = true;
        }
        ui.vertical(|ui| {
            if ui
                .add(egui::Button::new(egui::RichText::new(name).size(19.0)).frame(false))
                .clicked()
            {
                clicked = true;
            }
            if let Some(count) = count {
                ui.label(egui::RichText::new(format!("{count} 个窗口")).size(13.0).weak());
            }
        });
        if show_actions {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::new("×").min_size(egui::vec2(40.0, 40.0)))
                    .on_hover_text("关闭这个 App 的全部窗口")
                    .clicked()
                {
                    cross = true;
                }
                if ui
                    .add(egui::Button::new("+").min_size(egui::vec2(40.0, 40.0)))
                    .on_hover_text("再开一个窗口")
                    .clicked()
                {
                    plus = true;
                }
            });
        }
    });

    (clicked, plus, cross)
}

#[cfg(test)]
mod tests {
    use super::grid_columns;

    /// 原版 `UpdateResponsiveLayout`：横屏 6 列、竖屏 4 列。
    #[test]
    fn grid_columns_follow_orientation() {
        assert_eq!(grid_columns(false), 4, "竖屏手机屏应该是 4 列");
        assert_eq!(grid_columns(true), 6, "横屏（平板/电脑）应该是 6 列");
    }
}
