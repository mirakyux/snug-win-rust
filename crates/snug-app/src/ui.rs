//! 手机屏上的「App 资源库」界面（egui 实现）。
//!
//! 对应 C# 版 `SnugWin.App.PhoneHomeWindow`：顶部搜索、运行中/全部两页、
//! 底部指示条（返回资源库 / 相机键）。当前阶段是骨架 + 真实数据接口占位。

use eframe::egui;

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

pub struct PhoneHomeApp {
    pub config: AppConfig,
    pub paths: Paths,
    /// 虚拟屏驱动管道是否就绪（平台层尚未接通时只用于界面提示）。
    pub pipe_ready: bool,
    pub controller: Controller,
    pub tab: HomeTab,
    pub search: String,
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
    /// 界面窗口当前是不是被挪到屏幕外待命（避免反复 SetWindowPos）。
    pub last_offscreen: Option<bool>,
    /// 收尾是否已经做过（`on_exit` 与 `Drop` 都会调，必须幂等）。
    pub finished: bool,
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
        controller: Controller,
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
            last_offscreen: None,
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
        let state = self.controller.state();
        let ready = state.screen_phase == PhoneScreenPhase::Ready;
        // 有窗口铺在手机屏上时，我们只留底部那条指示条（C# 里 `PhoneHomeWindow` 隐、
        // `HomeBarWindow` 显）；没有窗口铺着时，整块手机屏都是资源库。
        //
        // 注意条件必须是「铺着 **且** 不在看列表」：点了指示条上的「‹ 资源库」之后
        // `list_visible` 变 true，但窗口还在手机屏上（`placed` 仍有值）。如果只看
        // `placed`，界面就永远停在指示条，用户怎么点都回不到资源库——真机反馈的
        // 「打开后没法返回那个列表」就是这条。
        let showing_bar = state.placed.is_some() && !state.list_visible;
        let bounds = if ready {
            if showing_bar {
                self.controller.home_bar_area()
            } else {
                self.controller.phone_bounds()
            }
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
            let request = snug_win::PlacementRequest::new(bounds);
            if snug_win::apply_placement(hwnd, &request) {
                crate::info!(
                    "手机屏界面：{}，搬到 ({}, {}) {}×{}",
                    if showing_bar { "收成底部指示条" } else { "铺满资源库" },
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
            SettingsAction::InstallDriver => {
                // C# `LocateBundledDriverScript`：从程序目录往上找 5 层 `driver/Install-SnugDriver.ps1`。
                match locate_driver_script() {
                    Some(script) => {
                        crate::info!("设置：安装/修复虚拟屏驱动，脚本 {}", script.display());
                        match run_elevated_powershell(&script) {
                            Ok(()) => crate::info!("设置：驱动脚本已拉起（会弹 UAC）"),
                            Err(err) => crate::warn!("设置：拉起驱动脚本失败 {err}"),
                        }
                    }
                    None => crate::warn!(
                        "设置：没找到 driver/Install-SnugDriver.ps1（程序目录及其上 5 层），无法安装驱动"
                    ),
                }
            }
            SettingsAction::OpenPhoneModels => {
                // C# 是弹一个独立的 `PhoneModelWindow`；我们的机型选择在设置窗"常规"页，
                // 直接把人送到那儿，比再开一个只有一份列表的窗口更好用。
                crate::info!("设置：打开机型选择（常规页）");
                self.settings.select_tab(SettingsTab::General);
                self.settings.open();
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
        let placed = self.controller.state().placed.map(|placed| placed.window);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(egui::Color32::from_rgb(24, 26, 30))
                    .inner_margin(egui::Margin::symmetric(8, 2)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    if ui.button("‹ 资源库").clicked() {
                        crate::info!("指示条：回到资源库");
                        self.controller.show_list();
                    }
                    if let Some(window) = &placed {
                        ui.label(egui::RichText::new(window.label()).small().weak());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("⚙").on_hover_text("设置").clicked() {
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
                    let state = self.controller.state();
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
                TrayCommand::SetManualOverride(on) => {
                    self.apply_setting(SettingsAction::SetManualOverride(on));
                }
                TrayCommand::Quit => {
                    crate::info!("托盘：退出（先还原窗口）");
                    // 先收尾再关窗：关窗之后 Rust 侧可能一行都跑不到（真机实测）。
                    self.finish();
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
            // 同样先收尾：关窗之后 Rust 侧可能一行都跑不到。
            self.finish();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // 有窗口铺在手机屏上时，界面只剩底部指示条（C#：PhoneHomeWindow 隐、HomeBarWindow 显）。
        // 和 `sync_window` 同一个条件：点了「‹ 资源库」之后 `list_visible=true`，即使窗口
        // 还铺在手机屏上，也必须回到资源库列表。
        let bar_state = self.controller.state();
        if bar_state.placed.is_some() && !bar_state.list_visible {
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
        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::default().inner_margin(egui::Margin::symmetric(30, 8)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("App")
                            .size(38.0)
                            .strong()
                            .color(egui::Color32::from_gray(238)),
                    );
                    let header_state = self.controller.state();
                    if let Some(placed) = &header_state.placed {
                        let label = format!("· {}", placed.window.label());
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(label)
                                        .size(15.0)
                                        .color(egui::Color32::from_rgb(140, 200, 255)),
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
                        // 原版右上角是「⏏」和「📱」，这里放功能对得上的两个开关。
                        if ui
                            .add(egui::Button::new("⚙").min_size(egui::vec2(56.0, 44.0)))
                            .on_hover_text("设置与诊断")
                            .clicked()
                        {
                            self.settings.open();
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
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("🔍 搜索 App")
                        .desired_width(f32::INFINITY),
                );

                ui.add_space(8.0);
                let half = ((ui.available_width() - 8.0) / 2.0).max(120.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_sized(
                            [half, 44.0],
                            egui::Button::selectable(self.tab == HomeTab::Running, "运行中"),
                        )
                        .clicked()
                    {
                        self.tab = HomeTab::Running;
                    }
                    if ui
                        .add_sized(
                            [half, 44.0],
                            egui::Button::selectable(self.tab == HomeTab::All, "全部"),
                        )
                        .clicked()
                    {
                        self.tab = HomeTab::All;
                    }
                });
            });

        if back_to_placed {
            crate::info!("资源库：回到手机屏上的窗口");
            self.controller.return_to_placed();
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
                        let apps = self.library.running().to_vec();
                        if apps.is_empty() {
                            ui.label(egui::RichText::new("没有正在运行的窗口").size(16.0).weak());
                            ui.label(
                                egui::RichText::new("启动一个 App 后，它的窗口会列在这里，点一下铺满手机屏")
                                    .size(14.0)
                                    .weak(),
                            );
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
                                    let label = format!("❐ {}", window.label());
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
                        for (letter, apps) in &groups {
                            if !letter.is_empty() {
                                ui.add_space(10.0);
                                ui.label(
                                    egui::RichText::new(letter)
                                        .size(13.0)
                                        .strong()
                                        .color(egui::Color32::from_gray(150)),
                                );
                            }
                            for app in apps {
                                let texture = self.library.icon(ctx, app);
                                let count = self
                                    .library
                                    .running()
                                    .iter()
                                    .find(|run| run.app.app_id.eq_ignore_ascii_case(&app.app_id))
                                    .map(|run| run.windows.len());
                                if app_row(ui, &app.name, count, texture.as_ref(), false).0 {
                                    match self.library.action_for(app) {
                                        AppAction::Pull(window) => pull_window = Some(window),
                                        AppAction::Open(app) => open_app = Some(app),
                                    }
                                }
                            }
                        }
                    }
                }
            });
        });

        // 动作在闭包外落地，避免和 `self` 的借用打架。
        if let Some(window) = pull_window {
            crate::info!("资源库：铺开 {}", window.label());
            self.controller.pull(window);
        }
        if let Some(app) = open_app {
            crate::info!("资源库：启动 {}", app.name);
            self.controller.open(app);
        }
        if let Some(app) = new_window {
            crate::info!("资源库：给 {} 新开窗口", app.app.name);
            self.controller.new_window(app);
        }
        if let Some(app) = quit_app {
            crate::info!("资源库：关闭 {} 的全部窗口", app.app.name);
            self.controller.quit(app);
        }
        if let Some(window) = close_window {
            crate::info!("资源库：关闭窗口 {}", window.label());
            self.controller.close_window(window);
        }
    }
}

    /// 从程序目录往上找 `driver/Install-SnugDriver.ps1`（C# `LocateBundledDriverScript`：最多 5 层）。
fn locate_driver_script() -> Option<std::path::PathBuf> {
    let mut dir = std::env::current_exe().ok()?.parent().map(|dir| dir.to_path_buf())?;
    for _ in 0..5 {
        let candidate = dir.join("driver").join("Install-SnugDriver.ps1");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// 用 PowerShell 跑驱动脚本。本进程已经是管理员（清单 requireAdministrator），
/// 所以直接起子进程；不是管理员时 Windows 会由脚本自己报错，我们不假装成功。
fn run_elevated_powershell(script: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &script.to_string_lossy(),
        ])
        .spawn()
        .map(|_| ())
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
