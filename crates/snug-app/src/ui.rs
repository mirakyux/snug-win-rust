//! 手机屏上的「App 资源库」界面（egui 实现）。
//!
//! 对应 C# 版 `SnugWin.App.PhoneHomeWindow`：顶部搜索、运行中/全部两页、
//! 底部指示条（返回资源库 / 相机键）。当前阶段是骨架 + 真实数据接口占位。

use eframe::egui;

use snug_core::desk::PhoneScreenPhase;
use snug_core::{AppConfig, Paths, PhoneCatalog};

use crate::controller::Controller;
use crate::settings::{ChangeImpact, SettingsAction, SettingsOutcome, SettingsTab, SettingsWindow, StatusInfo};
use crate::tray::{Tray, TrayCommand};

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
    pub catalog: Option<PhoneCatalog>,
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
    /// 正在跑的更新检查（后台线程结果从这里回来；`None` 表示没有任务）。
    pub update_rx: Option<std::sync::mpsc::Receiver<UpdateCheckOutcome>>,
}

/// 后台检查更新的结果。
pub type UpdateCheckOutcome = Result<snug_win::UpdateCheckResult, snug_win::UpdateError>;

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
            catalog,
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
            update_rx: None,
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
                    version: result.latest_version,
                    downloadable: result.package_download_url.is_some(),
                });
            }
            Ok(result) => {
                crate::info!("设置：已是最新版本 v{}", result.current_version);
                self.settings.set_update_state(UpdateUiState::UpToDate);
            }
            Err(err) => {
                crate::warn!("设置：检查更新失败 {err}");
                self.settings
                    .set_update_state(UpdateUiState::CheckFailed(err.to_string()));
            }
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
            SettingsAction::InstallUpdate => {
                // 下载 + 校验 + 解包 + 换文件是下一步；先把"检查"做成真的，
                // 免得按钮一点就假装在做事。
                crate::warn!("设置：下载并安装更新尚未接线（检查已经可用）");
                self.settings.set_update_state(crate::settings::UpdateUiState::InstallFailed {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    message: "下载与替换尚未接线，请等下一版".to_string(),
                });
            }
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
                    self.settings.select_tab(SettingsTab::from_index(tab));
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
                    if ui
                        .add_sized(
                            [ui.available_width(), 60.0],
                            egui::Button::new(egui::RichText::new("暂无运行中的窗口").size(15.0)),
                        )
                        .clicked()
                    {
                        // 平台层接上后：拉出 App 列表并放置窗口。
                    }
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("窗口会在这里显示，点一下即铺满手机屏").weak());
                }
                HomeTab::All => {
                    ui.add_space(6.0);
                    let models = self
                        .catalog
                        .as_ref()
                        .map(|c| c.brands.len())
                        .unwrap_or(0);
                    if let Some(catalog) = &self.catalog {
                        let query = snug_core::phone_models::normalize(&self.search);
                        let brands: Vec<&snug_core::phone_models::Brand> = if query.is_empty() {
                            catalog.brands.iter().collect()
                        } else {
                            catalog
                                .brands
                                .iter()
                                .filter(|b| snug_core::phone_models::normalize(&b.name).contains(&query))
                                .collect()
                        };
                        if brands.is_empty() {
                            ui.label(egui::RichText::new("没有匹配的机型").weak());
                        }
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for brand in brands {
                                ui.collapsing(format!("{}（{} 款）", brand.name, brand.models.len()), |ui| {
                                    for model in &brand.models {
                                        ui.label(format!("{}  {}×{}", model.name, model.screen.width, model.screen.height));
                                    }
                                });
                            }
                        });
                    } else {
                        ui.label(format!("机型库未加载（{models}）"));
                    }
                }
            }
        });
    }
}
