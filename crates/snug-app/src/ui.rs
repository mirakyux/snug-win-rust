//! 手机屏上的「App 资源库」界面（egui 实现）。
//!
//! 对应 C# 版 `SnugWin.App.PhoneHomeWindow`：顶部搜索、运行中/全部两页、
//! 底部指示条（返回资源库 / 相机键）。当前阶段是骨架 + 真实数据接口占位。

use eframe::egui;

use snug_core::desk::PhoneScreenPhase;
use snug_core::{AppConfig, Paths, PhoneCatalog};

use crate::controller::Controller;
use crate::library::{Library, TILE};
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

pub struct PhoneHomeApp {
    pub config: AppConfig,
    pub paths: Paths,
    /// 虚拟屏驱动管道是否就绪（平台层尚未接通时只用于界面提示）。
    pub pipe_ready: bool,
    pub controller: Controller,
    pub tab: HomeTab,
    pub search: String,
    /// 手机屏的逻辑宽度（点），用于把界面元素按屏宽等比放大。
    pub screen_width: f32,
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
        controller: Controller,
        screen_width: f32,
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
            controller,
            tab: HomeTab::Running,
            search: String::new(),
            screen_width,
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
        }
    }

    /// 后台查一次新版（C# `OnCheckUpdate`：`await _updates.CheckAsync()` 之后刷界面）。
    ///
    /// 发布仓库由编译期 `SNUG_RELEASES_REPO` 决定；没配置时更新器直接返回
    /// `NotConfigured`，**一个请求都不发**——这样界面不会拿 C# 版的仓库去查。
    fn start_update_check(&mut self) {
        use crate::settings::UpdateUiState;

        let app_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_default();
        let service = snug_win::update::AppUpdateService::new(
            snug_win::update::CurlHttpClient::default(),
            app_dir,
            std::env::temp_dir(),
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
        std::thread::Builder::new()
            .name("snug-update-check".to_string())
            .spawn(move || {
                let _ = tx.send(service.check_for_update());
            })
            .ok();
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
                    downloadable: result.package_download_url.is_some(),
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
        crate::info!("设置：开始下载 v{version}");

        let app_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_default();
        let service = snug_win::update::AppUpdateService::new(
            snug_win::update::CurlHttpClient::default(),
            app_dir,
            std::env::temp_dir(),
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
        std::thread::Builder::new()
            .name("snug-update-download".to_string())
            .spawn(move || {
                let mut progress = |fraction: f64| {
                    let _ = progress_tx.send(InstallOutcome::Progress(fraction));
                };
                let outcome = service.download_and_prepare(&check, &mut progress);
                let _ = tx.send(InstallOutcome::Done(outcome));
            })
            .ok();
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
                            match snug_win::update::launch_apply_script(&prepared, true) {
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

    /// 让界面窗口跟着手机屏走：屏在就把窗口搬上去，屏没了就藏起来。
    ///
    /// 对应 C# 里 `PhoneHomeWindow` 始终待在虚拟屏上（由 `_displays.BoundsOf` 定位），
    /// 没有手机屏时整个程序只剩托盘图标。这条是"Rust 版看起来像不像原版"的关键，
    /// 所以失败时只记 warn，不阻塞界面。
    fn sync_window(&mut self, ctx: &egui::Context) {
        let ready = self.controller.state().screen_phase == PhoneScreenPhase::Ready;
        let bounds = if ready { self.controller.phone_bounds() } else { None };

        let Some(bounds) = bounds else {
            // 手机屏不在了：窗口回主屏并藏起来，只留托盘。
            self.last_phone_rect = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            return;
        };

        if self.phone_window.is_none() {
            self.phone_window = snug_win::find_own_window(WINDOW_TITLE);
        }
        let Some(hwnd) = self.phone_window else {
            crate::warn!("找不到自己的窗口「{WINDOW_TITLE}」，手机屏界面暂留主屏");
            return;
        };

        if self.last_phone_rect != Some(bounds) {
            let request = snug_win::PlacementRequest::new(bounds);
            if snug_win::apply_placement(hwnd, &request) {
                crate::info!(
                    "手机屏界面已搬到 ({}, {}) {}×{}",
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

    /// 开机自启服务（每次都按当前 exe 现取，换目录后也能对上）。
    fn startup_service() -> snug_win::StartupService<snug_win::startup::SystemRunner> {
        snug_win::StartupService::for_current_exe()
    }

    /// 把界面读到的运行态喂给设置窗（对应 C# 每次打开设置窗时的 `Refresh`）。
    fn refresh_settings_status(&mut self) {
        let state = self.controller.state();
        let status = StatusInfo {
            pipe_available: self.pipe_ready,
            elevated: self.elevated,
            phone_screen_ready: state.screen_phase == PhoneScreenPhase::Ready,
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

    /// 执行设置窗排出来的动作。副作用都在这里落地，`settings.rs` 本身不做事。
    fn apply_setting(&mut self, action: SettingsAction) {
        match action {
            SettingsAction::SetPhoneScreen(on) => {
                crate::info!("设置：手机屏 → {on}");
                self.controller.set_phone_screen(on);
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
                if let Err(err) = std::process::Command::new("explorer").arg(&dir).spawn() {
                    crate::warn!("打开日志目录失败：{err}");
                }
            }
            SettingsAction::OpenRepository => {
                // 仓库地址只在根 Cargo.toml 的 repository 里写一份；没配就明说，不猜 URL。
                const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
                if REPOSITORY.is_empty() {
                    crate::warn!("设置：仓库地址未配置（Cargo.toml 缺 repository），无法打开");
                } else if let Err(err) = std::process::Command::new("cmd")
                    .args(["/c", "start", "", REPOSITORY])
                    .spawn()
                {
                    crate::warn!("打开仓库主页失败：{err}");
                }
            }
            SettingsAction::RestartElevated => {
                if self.elevated {
                    crate::info!("设置：已经是管理员，无需重启");
                } else {
                    match snug_win::restart_elevated(&[]) {
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
            SettingsAction::ApplyGestureSettings => {
                crate::info!("设置：手势配置已变，重建钩子");
                self.controller.apply_gesture_settings(&self.config);
            }
            // 下面这些要等平台层的更新器/驱动/机型窗接上，先如实写日志，不假装成功。
            other => crate::warn!("设置：动作 {other:?} 尚未接线，已忽略"),
        }
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

    /// 底部指示条高度，规则与上游一致：28 点 × (屏宽 / 540)。
    pub fn home_bar_height(&self) -> f32 {
        (28.0 * self.screen_width / 540.0).round()
    }

    /// 当前使用中的机型名（顶部标题栏用）。
    pub fn phone_label(&self) -> String {
        self.config
            .phone
            .model
            .clone()
            .unwrap_or_else(|| "自定义分辨率".to_string())
    }
}

impl eframe::App for PhoneHomeApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 托盘要先撤掉：进程退出后留着图标，explorer 会留一个"幽灵图标"直到鼠标划过。
        self.tray = None;
        // 先让状态机收干净（窗口归位、手机屏撤除），再存配置。
        self.controller.shutdown();
        let file = self.paths.config_file();
        let json = self.config.to_json();
        match std::fs::write(&file, json) {
            Ok(()) => crate::info!("配置已保存 {}", file.display()),
            Err(err) => crate::warn!("配置保存失败 {}：{err}", file.display()),
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
                TrayCommand::SetManualOverride(on) => {
                    self.apply_setting(SettingsAction::SetManualOverride(on));
                }
                TrayCommand::Quit => {
                    crate::info!("托盘：退出（先还原窗口）");
                    self.request_exit = true;
                }
            }
        }
        if let Some(tray) = &mut self.tray {
            let state = self.controller.state();
            let summary = Self::phase_text(state.screen_phase).to_string();
            let connected = state.screen_phase == PhoneScreenPhase::Ready;
            tray.set_status(&summary, connected);
            tray.set_manual_override(self.controller.manual_override());
        }
        // 托盘事件来自另一个消息窗口，egui 没有输入就不会重绘；靠这个定时器
        // 让 `poll` 有节奏地跑起来（200ms 足够，不是每帧空转）。
        ctx.request_repaint_after(std::time::Duration::from_millis(200));

        // 界面窗口跟着手机屏走（对应 C# 里 PhoneHomeWindow 一直待在虚拟屏上）。
        self.sync_window(ctx);

        // 设置窗：先执行上一帧排出的动作，再画这一帧（动作会影响配置和控制器状态）。
        self.poll_update_check();
        self.poll_install_update();
        // 资源库：收扫描结果；手机屏就绪或太久没扫就重扫一次。
        self.library.poll();
        if self.library.needs_scan(false) {
            self.library.start_scan();
        }
        for action in self.settings.take_actions() {
            self.apply_setting(action);
        }
        if self.settings.is_open() {
            self.refresh_settings_status();
        }
        let before = self.config.clone();
        match self.settings.show(ctx, &mut self.config) {
            SettingsOutcome::Changed => {
                let impact = crate::settings::classify_change(&before, &self.config);
                if let Some(hint) = crate::settings::change_hint(impact) {
                    crate::info!("设置：{hint}");
                }
                match impact {
                    ChangeImpact::RebuildPhoneScreen => {
                        self.controller
                            .set_fit(snug_core::ScreenFit::from_phone(&self.config.phone.screen.to_fit_spec()));
                        self.controller.set_default_zoom(self.config.zoom_steps);
                    }
                    ChangeImpact::ReloadGestures => self.controller.apply_gesture_settings(&self.config),
                    ChangeImpact::RefreshHomeBar | ChangeImpact::None => {}
                }
            }
            SettingsOutcome::CloseRequested => {
                crate::info!("设置：已关闭");
            }
            SettingsOutcome::Idle => {}
        }
        if self.request_exit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        egui::TopBottomPanel::top("search")
            .frame(egui::Frame::default().inner_margin(egui::Margin::symmetric(12, 8)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("‹");
                    ui.strong(self.phone_label());
                    let state = self.controller.state();
                    ui.label(egui::RichText::new(Self::phase_text(state.screen_phase)).weak());
                    if let Some(placed) = &state.placed {
                        ui.label(
                            egui::RichText::new(format!("· {}", placed.window.label()))
                                .color(egui::Color32::from_rgb(140, 200, 255)),
                        );
                    }
                    if !self.pipe_ready {
                        ui.label(egui::RichText::new("驱动未就绪").color(egui::Color32::LIGHT_RED));
                    }
                    ui.label(
                        egui::RichText::new(format!(
                            "{} App · {} 窗口",
                            self.library.installed().len(),
                            self.library.running().len()
                        ))
                        .weak()
                        .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.search)
                                .hint_text("搜索 App 与窗口")
                                .desired_width(ui.available_width() - 40.0),
                        );
                    });
                });
            });

        egui::TopBottomPanel::bottom("home_bar")
            .exact_height(self.home_bar_height().max(20.0))
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    if self.config.screenshot {
                        ui.label("📷");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut wanted = self.config.phone_screen;
                        if ui.checkbox(&mut wanted, "手机屏").changed() {
                            self.config.phone_screen = wanted;
                            self.controller.set_phone_screen(wanted);
                        }
                        if ui.small_button("全部").clicked() {
                            self.tab = HomeTab::All;
                        }
                        if ui.small_button("⚙").on_hover_text("设置").clicked() {
                            self.settings.open();
                        }
                        if ui.small_button("运行中").clicked() {
                            self.tab = HomeTab::Running;
                        }
                    });
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            match self.tab {
                HomeTab::Running => {
                    ui.add_space(6.0);
                    let windows = self.library.filter_windows(&self.search);
                    if windows.is_empty() {
                        ui.add_sized(
                            [ui.available_width(), 60.0],
                            egui::Button::new(egui::RichText::new(if self.search.trim().is_empty() {
                                "暂无运行中的窗口"
                            } else {
                                "没有匹配的窗口"
                            })
                            .size(15.0)),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("窗口会在这里显示，点一下即铺满手机屏").weak(),
                        );
                    } else {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            let mut clicked = None;
                            for window in &windows {
                                if ui
                                    .add_sized(
                                        [ui.available_width(), 44.0],
                                        egui::Button::new(window.label()),
                                    )
                                    .clicked()
                                {
                                    clicked = Some(window.clone());
                                }
                            }
                            if let Some(window) = clicked {
                                crate::info!("资源库：铺开 {}", window.label());
                                self.controller.pull(window);
                            }
                        });
                    }
                }
                HomeTab::All => {
                    ui.add_space(6.0);
                    if self.library.needs_scan(false) {
                        self.library.start_scan();
                    }
                    let apps = self.library.filter_installed(&self.search);
                    if apps.is_empty() {
                        let hint = if !self.library.scanned_once {
                            "正在读取已安装的 App…"
                        } else if self.search.trim().is_empty() {
                            "没有找到已安装的 App"
                        } else {
                            "没有匹配的 App"
                        };
                        ui.label(egui::RichText::new(hint).weak());
                    }
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        let mut clicked = None;
                        let columns = ((ui.available_width() / TILE).floor() as usize).max(1);
                        for row in apps.chunks(columns) {
                            ui.horizontal(|ui| {
                                for app in row {
                                    let texture = self.library.icon(ctx, app);
                                    let (rect, response) = ui.allocate_exact_size(
                                        egui::vec2(TILE - 4.0, TILE),
                                        egui::Sense::click(),
                                    );
                                    ui.painter().rect_filled(
                                        rect,
                                        10.0,
                                        if response.hovered() {
                                            egui::Color32::from_rgb(56, 60, 70)
                                        } else {
                                            egui::Color32::from_rgb(40, 43, 50)
                                        },
                                    );
                                    match &texture {
                                        Some(texture) => {
                                            let side = TILE * 0.52;
                                            let icon_rect = egui::Rect::from_center_size(
                                                egui::pos2(rect.center().x, rect.top() + 8.0 + side / 2.0),
                                                egui::vec2(side, side),
                                            );
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
                                            // C# 对取不到图标的 App 画占位图（首字母）。
                                            let initial = app
                                                .name
                                                .chars()
                                                .next()
                                                .map(|c| c.to_uppercase().to_string())
                                                .unwrap_or_else(|| "?".to_string());
                                            ui.painter().text(
                                                egui::pos2(rect.center().x, rect.top() + 8.0 + TILE * 0.26),
                                                egui::Align2::CENTER_CENTER,
                                                initial,
                                                egui::FontId::proportional(24.0),
                                                egui::Color32::from_gray(170),
                                            );
                                        }
                                    }
                                    let text_rect = egui::Rect::from_min_max(
                                        egui::pos2(rect.left() + 2.0, rect.bottom() - 24.0),
                                        egui::pos2(rect.right() - 2.0, rect.bottom() - 2.0),
                                    );
                                    ui.painter().text(
                                        text_rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        &app.name,
                                        egui::FontId::proportional(11.0),
                                        egui::Color32::from_gray(220),
                                    );
                                    if response.clicked() {
                                        clicked = Some(app.clone());
                                    }
                                }
                            });
                        }
                        if let Some(app) = clicked {
                            match self.library.action_for(&app) {
                                AppAction::Pull(window) => {
                                    crate::info!("资源库：铺开 {}", window.label());
                                    self.controller.pull(window);
                                }
                                AppAction::Open(app) => {
                                    crate::info!("资源库：启动 {}", app.name);
                                    self.controller.open(app);
                                }
                            }
                        }
                    });
                }
            }
        });
    }
}
