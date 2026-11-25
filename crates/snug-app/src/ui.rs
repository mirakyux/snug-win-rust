//! 手机屏上的「App 资源库」界面（egui 实现）。
//!
//! 对应 C# 版 `SnugWin.App.PhoneHomeWindow`：顶部搜索、运行中/全部两页、
//! 底部指示条（返回资源库 / 相机键）。当前阶段是骨架 + 真实数据接口占位。

use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use snug_core::desk::PhoneScreenPhase;
use snug_core::{AppConfig, Paths, PhoneScreenMode};

use crate::controller::{Controller, TouchEvent};
use crate::desktop_settings::DesktopSettings;
use crate::library::Library;
use crate::mobile_library::{LibraryAction, MobileLibraryUi};
use crate::settings::{ChangeImpact, SettingsAction, SettingsOutcome, SettingsTab, SettingsWindow, StatusInfo};
use crate::tray::{Tray, TrayCommand};

use snug_core::apps::AppAction;

#[derive(Clone)]
enum MobileSheet {
    Screens,
}

/// 手机屏窗口的标题。托盘生命周期里靠它找到自己的 `HWND`（eframe 不对外暴露句柄），
/// 改这里必须和 `main.rs` 里 `ViewportBuilder::with_title` 保持一致。
pub const WINDOW_TITLE: &str = "Snug 手机屏";

/// Keeps native redraws arriving even when egui has merged its pending repaint requests.
pub struct UiHeartbeat {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl UiHeartbeat {
    pub fn start(ctx: egui::Context) -> std::io::Result<Self> {
        // Resolve on the owning GUI thread. Reading our title from the worker would
        // synchronously send WM_GETTEXT and could deadlock shutdown's worker join.
        let window = snug_win::find_own_window(WINDOW_TITLE).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "无法找到界面心跳的主窗口")
        })?;
        Self::start_with_wake(ctx, move || {
            let _ = snug_win::request_own_window_redraw(window);
        })
    }

    fn start_with_wake(
        ctx: egui::Context,
        mut wake: impl FnMut() + Send + 'static,
    ) -> std::io::Result<Self> {
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = running.clone();
        let worker = std::thread::Builder::new()
            .name("snug-ui-tick".to_string())
            .spawn(move || {
                while worker_running.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    if worker_running.load(Ordering::Relaxed) {
                        ctx.request_repaint();
                        // egui only invokes its callback once until a new frame consumes it.
                        // WM_PAINT provides an independent wake without waiting for the UI thread.
                        wake();
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
    fn heartbeat_keeps_native_wakes_running_without_an_egui_frame() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let callbacks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callbacks_for_ctx = callbacks.clone();
        ctx.set_request_repaint_callback(move |_| {
            callbacks_for_ctx.fetch_add(1, Ordering::Relaxed);
        });
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut heartbeat = UiHeartbeat::start_with_wake(ctx, move || {
            let _ = sender.send(());
        }).unwrap();
        let first_wake = receiver.recv_timeout(std::time::Duration::from_secs(2));
        let second_wake = receiver.recv_timeout(std::time::Duration::from_secs(2));
        heartbeat.stop();
        assert!(first_wake.is_ok() && second_wake.is_ok(),
            "native wake must keep ticking when no egui frame consumes the repaint request");
        assert!(callbacks.load(Ordering::Relaxed) <= 1,
            "egui merges pending repaint callbacks, so two native wakes must not depend on two callbacks");
    }

    #[test]
    fn heartbeat_stops_and_releases_its_worker_idempotently() {
        let mut heartbeat = UiHeartbeat::start_with_wake(egui::Context::default(), || {}).unwrap();
        let weak = Arc::downgrade(&heartbeat.running);
        heartbeat.stop();
        heartbeat.stop();
        assert!(heartbeat.worker.is_none());
        assert!(!heartbeat.running.load(Ordering::Relaxed));
        drop(heartbeat);
        assert!(weak.upgrade().is_none(), "the worker must release its state before shutdown returns");
    }
}

struct AutostartProbe {
    enabled: bool,
    error: Option<String>,
}

struct AutostartJob {
    changing: bool,
    receiver: std::sync::mpsc::Receiver<AutostartProbe>,
}

struct DriverProbe {
    pipe_ready: bool,
    health: Option<snug_win::driver_health::DriverHealth>,
}

fn start_settings_probe<T: Send + 'static>(
    ctx: &egui::Context,
    name: &str,
    probe: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<std::sync::mpsc::Receiver<T>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let repaint = ctx.clone();
    std::thread::Builder::new().name(name.into()).spawn(move || {
        let result = probe();
        if sender.send(result).is_ok() {
            repaint.request_repaint();
        }
    })?;
    Ok(receiver)
}

#[test]
fn settings_status_probe_does_not_wait_for_windows() {
    use std::sync::mpsc::{self, TryRecvError};
    use std::time::Duration;

    let (release, waiting) = mpsc::channel();
    let result = start_settings_probe(&egui::Context::default(), "test-settings-probe", move || {
        waiting.recv_timeout(Duration::from_secs(2)) == Ok(())
    })
    .unwrap();
    assert_eq!(result.try_recv(), Err(TryRecvError::Empty));
    release.send(()).unwrap();
    assert_eq!(result.recv_timeout(Duration::from_secs(2)), Ok(true));
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
    driver_probe_checked: Option<std::time::Instant>,
    driver_probe: Option<std::sync::mpsc::Receiver<DriverProbe>>,
    driver_health: Option<snug_win::driver_health::DriverHealth>,
    driver_health_checked: Option<std::time::Instant>,
    driver_retry: Option<std::sync::mpsc::Receiver<Result<snug_win::VddProtocol, String>>>,
    driver_connection_result: Option<String>,
    autostart: Option<bool>,
    autostart_checked: Option<std::time::Instant>,
    autostart_job: Option<AutostartJob>,
    autostart_requested: Option<bool>,
    autostart_error: Option<String>,
    pub controller: Controller,
    /// Hidden-window repaint worker; joined during app shutdown.
    ui_heartbeat: UiHeartbeat,
    library_ui: MobileLibraryUi,
    /// 手机屏的逻辑宽度（点），用于把界面元素按屏宽等比放大。
    /// 设置窗（无人操作时它自己返回 `Idle`，不画任何东西）。
    pub settings: SettingsWindow,
    desktop_settings: DesktopSettings,
    display_layout: crate::display_layout::DisplayLayout,
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
    /// 从资源库打开的屏幕选择浮层。
    mobile_sheet: Option<MobileSheet>,
}

/// 桌面预览的运行态。
#[derive(Debug, Clone)]
pub struct PreviewState {
    active_screen: Option<String>,
    capture: Option<PreviewCapture>,
}

#[derive(Debug, Clone)]
struct PreviewCapture {
    path: std::path::PathBuf,
    started: std::time::Instant,
    requested: bool,
}

fn displayed_screen<'a>(
    active: &'a Option<String>,
    preview: &'a Option<PreviewState>,
) -> Option<&'a str> {
    preview
        .as_ref()
        .map_or(active, |state| &state.active_screen)
        .as_deref()
}

/// Preview navigation must never change the real controller's selected screen.
fn select_displayed_screen(
    active: &mut Option<String>,
    preview: &mut Option<PreviewState>,
    id: String,
) -> Option<String> {
    if let Some(preview) = preview {
        preview.active_screen = Some(id);
        None
    } else {
        *active = Some(id.clone());
        Some(id)
    }
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
        paths: Paths,
        pipe_ready: bool,
        start_minimized: bool,
        controller: Controller,
        ui_heartbeat: UiHeartbeat,
        display_layout: crate::display_layout::DisplayLayout,
    ) -> Self {
        let settings = SettingsWindow::new();
        let desktop_settings = DesktopSettings::new(&config, controller.display_service());
        // 托盘必须在主线程建（它建的那个隐藏窗口靠 eframe 的消息泵分发），
        // 而 `new` 就是在 `run_native` 的创建回调里跑的。
        let tray = match Tray::new(false, config.phone_screen_mode()) {
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
            driver_probe_checked: None,
            driver_probe: None,
            driver_health: None,
            driver_health_checked: None,
            driver_retry: None,
            driver_connection_result: None,
            autostart: None,
            autostart_checked: None,
            autostart_job: None,
            autostart_requested: None,
            autostart_error: None,
            controller,
            ui_heartbeat,
            library_ui: MobileLibraryUi::default(),
            settings,
            desktop_settings,
            display_layout,
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
            mobile_sheet: None,
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
    /// 默认检查 Rust 版的 GitHub Releases，定制构建可覆盖更新源。
    fn start_update_check(&mut self) {
        use crate::settings::UpdateUiState;

        let service = snug_win::update::AppUpdateService::new(
            snug_win::update::CurlHttpClient::default(),
        );
        if !service.is_configured() {
            crate::warn!("设置：更新检查不可用，构建时指定的发布仓库无效");
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
        let (desktop_config, _) = self.desktop_settings.take_changes();
        if let Some(config) = desktop_config {
            self.config = config;
        }
        self.desktop_settings.close();
        crate::info!("收尾：开始（撤托盘 + 撤屏 + 归位窗口 + 存配置）");
        self.ui_heartbeat.stop();
        // 托盘要先撤掉：进程退出后留着图标，explorer 会留一个"幽灵图标"直到鼠标划过。
        self.tray = None;
        // 先让状态机收干净（窗口归位、手机屏撤除），再存配置。
        self.controller.shutdown();
        self.save_config();
        crate::info!("收尾：完成");
    }

    fn save_config(&self) {
        let file = self.paths.config_file();
        match self.config.save(&file) {
            Ok(()) => crate::info!("配置已保存 {}", file.display()),
            Err(err) => crate::warn!("配置保存失败 {}：{err}", file.display()),
        }
    }

    /// 让界面窗口跟着手机屏走：屏在就把窗口搬上去，屏没了就藏起来。
    ///
    /// 对应 C# 里 `PhoneHomeWindow` 始终待在虚拟屏上（由 `_displays.BoundsOf` 定位），
    /// 没有手机屏时整个程序只剩托盘图标。这条是"Rust 版看起来像不像原版"的关键，
    /// 所以失败时只记 warn，不阻塞界面。
    fn sync_window(
        &mut self,
        ctx: &egui::Context,
        phone_layout: Option<(snug_core::Rect, snug_core::Rect)>,
    ) {
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
                && self.mobile_sheet.is_none()
                && state.screen_phase == PhoneScreenPhase::Ready;
        // 手机屏起来了就把预览让掉：我们只有一个窗口，C# 那边预览窗和手机屏窗是两个。
        if ready && self.preview.is_some() {
            self.preview = None;
            self.mobile_sheet = None;
            crate::info!("预览：手机屏已就绪，预览窗口让位");
        }
        if ready && let Some((full_screen, _)) = phone_layout {
            let native = (ctx.pixels_per_point() / ctx.zoom_factor()).max(0.5);
            let zoom = touch_zoom(full_screen.width as f32, full_screen.height as f32, native);
            self.apply_ui_scale(ctx, zoom, "手机触控");
        }
        let bounds = if ready {
            if showing_bar {
                phone_layout.map(|(_, bar)| bar)
            } else {
                phone_layout.map(|(full, _)| full)
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
            if self.last_offscreen != Some(true) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
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
        let mut request = snug_win::PlacementRequest::new(bounds);
        request.no_activate = showing_bar || self.pill_mode();
        let placed_ok = self.last_phone_rect == Some(bounds)
            && snug_win::placement_matches(hwnd, &request, 2.0);
        if !placed_ok {
            // 尺寸与位置都交给 `SetWindowPos`（物理像素，和虚拟屏分辨率同一套单位）。
            //
            // 这里**不能**再发 `ViewportCommand::InnerSize`：那是 egui 的逻辑点，eframe 会按
            // `pixels_per_point` 换算回物理像素，和 `SetWindowPos` 抢同一个窗口尺寸。
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
        let screen = displayed_screen(&self.active_screen, &self.preview)
            .and_then(|id| {
                self.config
                    .effective_screens()
                    .into_iter()
                    .find(|screen| screen.id == id)
            })
            .or_else(|| self.config.primary_touch_screen())
            .or_else(|| self.config.effective_screens().into_iter().next())
            .unwrap_or_default();
        let fit = snug_core::ScreenPlan::fit_of(&screen);
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
        Some((rect, touch_zoom(width as f32, height as f32, native as f32)))
    }

    /// 把缩放应用到 egui（窗口尺寸另算）。
    fn apply_ui_scale(&self, ctx: &egui::Context, scale: f32, why: &str) {
        if (ctx.zoom_factor() - scale).abs() > 0.01 {
            ctx.set_zoom_factor(scale);
            crate::info!("{why}：界面缩放 {scale:.2}");
        }
    }

    /// 预览模式的窗口矩形；保留预览临时选中的屏幕。
    fn preview_rect(&self, ctx: &egui::Context) -> Option<snug_core::Rect> {
        let (rect, scale) = self.desktop_layout(ctx)?;
        self.apply_ui_scale(ctx, scale, "预览");
        Some(rect)
    }

    /// 开桌面预览（C# `OpenScreenPreview`）。
    ///
    /// C# 里手机屏已经就绪时不去开预览窗，而是把资源库列表翻出来——这里保持一致。
    pub(crate) fn open_preview(&mut self, ctx: &egui::Context) {
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
        self.preview = Some(PreviewState {
            active_screen: self.active_screen.clone(),
            capture: None,
        });
        if self.preview_rect(ctx).is_none() {
            crate::warn!("预览：拿不到主屏工作区，无法摆放预览窗");
            self.preview = None;
        }
        ctx.request_repaint();
    }

    /// 命令行预览可直接打开全部应用或搜索页，便于检查实际渲染。
    pub(crate) fn set_preview_library(&mut self, all: bool, search: Option<String>) {
        if self.preview.is_some() {
            self.library_ui.set_preview_page(all, search);
        }
    }

    /// Capture this preview's rendered framebuffer, including occluded OpenGL windows.
    pub(crate) fn capture_preview_to(&mut self, path: std::path::PathBuf) {
        if let Some(preview) = &mut self.preview {
            preview.capture = Some(PreviewCapture {
                path,
                started: std::time::Instant::now(),
                requested: false,
            });
        }
    }

    fn poll_preview_capture(&mut self, ctx: &egui::Context) {
        let Some(preview) = &mut self.preview else {
            return;
        };
        let Some(capture) = &mut preview.capture else {
            return;
        };
        let image = ctx.input(|input| {
            input.events.iter().find_map(|event| {
                if let egui::Event::Screenshot { image, .. } = event {
                    Some(image.clone())
                } else {
                    None
                }
            })
        });
        if let Some(image) = image {
            let path = capture.path.clone();
            preview.capture = None;
            let rgba = image
                .pixels
                .iter()
                .flat_map(|pixel| pixel.to_array())
                .collect();
            if let Some(image) = snug_win::screenshot::PixelImage::new(
                image.size[0] as i32,
                image.size[1] as i32,
                rgba,
            ) {
                match std::fs::write(&path, snug_win::screenshot::encode_png(&image)) {
                    Ok(()) => crate::info!("预览截图已保存 {}", path.display()),
                    Err(error) => crate::warn!("预览截图写入失败：{error}"),
                }
            }
        } else if !capture.requested
            && capture.started.elapsed() >= std::time::Duration::from_secs(3)
        {
            capture.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// 关掉桌面预览，界面窗口回到屏幕外待命。
    fn close_preview(&mut self, ctx: &egui::Context) {
        if self.preview.take().is_some() {
            self.mobile_sheet = None;
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

    fn autostart_change_pending(&self) -> bool {
        self.autostart_requested.is_some()
            || self.autostart_job.as_ref().is_some_and(|job| job.changing)
    }

    fn poll_autostart(&mut self, ctx: &egui::Context, settings_open: bool) {
        const QUERY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
        if let Some(job) = &self.autostart_job {
            let mut connect_driver = false;
            match job.receiver.try_recv() {
                Ok(sample) => {
                    self.autostart = Some(sample.enabled);
                    connect_driver = job.changing && sample.enabled && sample.error.is_none();
                    if job.changing || sample.error.is_some() {
                        self.autostart_error = sample.error;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.autostart_error = Some("后台启动状态读取失败".into());
                }
            }
            self.autostart_job = None;
            self.autostart_checked = settings_open.then(std::time::Instant::now);
            if connect_driver {
                self.start_driver_task(ctx, snug_win::driver_health::retry_connection_without_prompt);
            }
        }
        if !settings_open {
            self.autostart_checked = None;
        }
        if self.autostart_requested == Some(true) && self.driver_retry.is_some() {
            return;
        }
        let requested = self.autostart_requested.take();
        if requested.is_none()
            && (!settings_open || self.autostart_checked.is_some_and(|last| last.elapsed() < QUERY_INTERVAL))
        {
            return;
        }
        match start_settings_probe(ctx, "snug-autostart", move || {
            let service = Self::startup_service();
            let change = requested.map(|enabled| {
                if enabled {
                    crate::driver::prepare_for_login()?;
                }
                service.set_enabled(enabled).map_err(|error| error.to_string())
            });
            let enabled = service.is_enabled();
            let error = match change {
                Some(Err(error)) => Some(error),
                Some(Ok(())) if requested != Some(enabled) => Some("未能确认后台启动设置已生效".into()),
                _ => None,
            };
            if let Some(error) = &error {
                crate::warn!("设置：后台启动设置失败：{error}");
            } else if requested.is_some() {
                crate::info!("设置：开机自启 → {enabled}（任务 {}）", service.task_name());
            }
            AutostartProbe { enabled, error }
        }) {
            Ok(receiver) => self.autostart_job = Some(AutostartJob { changing: requested.is_some(), receiver }),
            Err(error) => {
                self.autostart_checked = Some(std::time::Instant::now());
                self.autostart_error = Some(format!("后台启动状态读取失败：{error}"));
            }
        }
    }

    /// 把界面读到的运行态喂给设置窗（对应 C# 每次打开设置窗时的 `Refresh`）。
    fn refresh_settings_status(&mut self, ctx: &egui::Context) {
        let needs_logs = self.settings.needs_logs() || self.desktop_settings.needs_logs();
        self.poll_driver_probe(ctx, needs_logs);
        let state = self.state();
        let failed_resolution_screen = (state.notice
            == Some(snug_core::desk::DeskNotice::ReshapeFailed))
        .then(|| {
            self.active_screen
                .clone()
                .or_else(|| self.config.primary_touch_screen().map(|screen| screen.id))
        })
        .flatten();
        let broker_status = snug_win::driver_health::broker_status();
        let driver_broker_connected = matches!(broker_status, snug_win::vdd_broker::BrokerStatus::Connected);
        let driver_connection_result = match broker_status {
            snug_win::vdd_broker::BrokerStatus::Failed(failure) if self.driver_retry.is_none() => {
                Some(failure.to_string())
            }
            snug_win::vdd_broker::BrokerStatus::Connected => self.driver_connection_result
                .clone().or_else(|| Some("驱动权限代理已连接".into())),
            _ => self.driver_connection_result.clone(),
        };
        let status = StatusInfo {
            autostart: self.autostart,
            autostart_changing: self.autostart_change_pending(),
            autostart_error: self.autostart_error.clone(),
            pipe_available: self.pipe_ready,
            driver_health: self.driver_health.clone(),
            driver_broker_connected,
            driver_reconnecting: self.driver_retry.is_some(),
            driver_connection_result,
            failed_resolution_screen,
            elevated: self.elevated,
            phone_screen_ready: state.screen_phase == PhoneScreenPhase::Ready,
            can_reenter: self.controller.can_reenter(),
            status_summary: Self::phase_text(state.screen_phase).to_string(),
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            self_contained: true,
        };
        self.settings.set_status(status);
        // 日志框：只在"驱动与日志"页增量刷新，别每帧把整个文件读进文本框。
        if needs_logs {
            self.refresh_logs();
        }
        if self.desktop_settings.is_open() {
            self.desktop_settings.sync_runtime_from(&self.settings);
        }
    }

    pub fn open_desktop_settings(&mut self, tab: SettingsTab) {
        crate::info!("桌面设置：收到打开请求");
        self.desktop_settings.open(tab);
    }

    fn apply_desktop_settings(&mut self, ctx: &egui::Context) {
        let (config, actions) = self.desktop_settings.take_changes();
        if let Some(config) = config {
            let before = std::mem::replace(&mut self.config, config);
            self.apply_settings_change(ctx, &before, SettingsOutcome::Changed);
        }
        for action in actions {
            self.apply_setting(action, ctx);
        }
    }

    fn apply_settings_change(&mut self, ctx: &egui::Context, before: &AppConfig, outcome: SettingsOutcome) {
        if outcome != SettingsOutcome::Changed {
            return;
        }
        let mode = self.config.phone_screen_mode();
        if mode != before.phone_screen_mode() {
            crate::info!("设置：手机屏显示方式 → {mode:?}");
            self.controller.set_phone_screen_mode(mode);
            self.save_config();
        }
        self.settings.sync_committed_screens(before, &self.config);
        self.desktop_settings.sync_config(&self.config);
        let impact = crate::settings::classify_change(before, &self.config);
        if let Some(hint) = crate::settings::change_hint(impact) {
            crate::info!("设置：{hint}");
        }
        match impact {
            ChangeImpact::RebuildPhoneScreen => {
                self.controller.apply_screens(&self.config);
                self.controller.set_default_zoom(self.config.zoom_steps);
            }
            ChangeImpact::ReloadGestures => self.controller.apply_gesture_settings(&self.config),
            ChangeImpact::Screens => self.controller.apply_screens(&self.config),
            ChangeImpact::Theme | ChangeImpact::RefreshHomeBar | ChangeImpact::None => {}
        }
        ctx.request_repaint_of(egui::ViewportId::ROOT);
    }

    fn poll_driver_retry(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let Some(retry) = &self.driver_retry else {
            return;
        };
        let message = match retry.try_recv() {
            Ok(Ok(protocol)) => {
                if self.preview.is_none() {
                    self.controller
                        .retry_failed_screen(self.active_screen.as_deref());
                }
                match protocol {
                    snug_win::VddProtocol::V2 => "连接成功 · 双向协议".into(),
                    snug_win::VddProtocol::Legacy => "连接成功 · 旧版协议".into(),
                }
            }
            Ok(Err(error)) => error,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => "驱动连接任务意外停止".into(),
        };
        crate::info!("驱动：{message}");
        self.driver_retry = None;
        self.driver_health_checked = None;
        self.driver_connection_result = Some(message);
        self.controller.complete_driver_startup();
    }

    pub(crate) fn prepare_driver_at_startup(&mut self, ctx: &egui::Context, started_at_login: bool) {
        self.start_driver_task(ctx, move || crate::driver::prepare_at_startup(started_at_login));
        if self.driver_retry.is_none() {
            self.controller.complete_driver_startup();
        }
    }

    fn start_driver_task(
        &mut self,
        ctx: &egui::Context,
        task: impl FnOnce() -> Result<snug_win::VddProtocol, String> + Send + 'static,
    ) {
        if self.driver_retry.is_some() {
            return;
        }
        match start_settings_probe(ctx, "snug-driver-connect", task) {
            Ok(rx) => {
                self.driver_retry = Some(rx);
                self.driver_connection_result = None;
            }
            Err(error) => {
                self.driver_connection_result = Some(format!("无法启动连接任务：{error}"))
            }
        }
    }

    fn poll_driver_probe(&mut self, ctx: &egui::Context, diagnostics_open: bool) {
        const PIPE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
        const HEALTH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);
        if let Some(receiver) = &self.driver_probe {
            match receiver.try_recv() {
                Ok(probe) => {
                    if probe.pipe_ready != self.pipe_ready {
                        let status = if probe.pipe_ready { "就绪" } else { "未就绪" };
                        crate::info!("驱动：管道状态 → {status}");
                        self.pipe_ready = probe.pipe_ready;
                    }
                    if let Some(health) = probe.health {
                        self.driver_health = Some(health);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    crate::warn!("驱动：诊断任务未返回结果");
                }
            }
            self.driver_probe = None;
        }
        let probe_health = diagnostics_open
            && self.driver_health_checked.is_none_or(|last| last.elapsed() >= HEALTH_INTERVAL);
        if !probe_health && self.driver_probe_checked.is_some_and(|last| last.elapsed() < PIPE_INTERVAL) {
            return;
        }
        let now = std::time::Instant::now();
        self.driver_probe_checked = Some(now);
        if probe_health {
            self.driver_health_checked = Some(now);
        }
        match start_settings_probe(ctx, "snug-driver-status", move || DriverProbe {
            pipe_ready: crate::driver::pipe_available(),
            health: probe_health.then(snug_win::driver_health::DriverHealth::probe),
        }) {
            Ok(receiver) => self.driver_probe = Some(receiver),
            Err(error) => crate::warn!("驱动：无法启动诊断任务：{error}"),
        }
    }

    /// 执行设置窗排出来的动作。副作用都在这里落地，`settings.rs` 本身不做事。
    fn apply_setting(&mut self, action: SettingsAction, ctx: &egui::Context) {
        match action {
            SettingsAction::ReenterScreen => {
                crate::info!("设置：进入手机屏");
                self.controller.reenter();
            }
            SettingsAction::OpenPreview => {
                crate::info!("设置：预览界面");
                self.open_preview(ctx);
            }
            SettingsAction::OpenProjection => {} // Consumed by the desktop viewport.
            SettingsAction::SetAutoStart(on) => {
                self.autostart_requested = Some(on);
                self.autostart_error = None;
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
                const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
                let repository = if REPOSITORY.is_empty() {
                    snug_win::update::ReleaseRepo::from_default()
                        .map(|repo| format!("https://github.com/{}", repo.slug()))
                } else {
                    Some(REPOSITORY.to_owned())
                };
                let result = repository
                    .ok_or_else(|| std::io::Error::other("未配置有效的仓库地址"))
                    .and_then(|url| snug_win::apps::open_web_url(&url));
                if let Err(err) = result {
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
                if !self.autostart_change_pending() {
                    self.start_driver_task(ctx, crate::driver::install_and_connect);
                }
            }
            SettingsAction::RetryDriverConnection => {
                if !self.autostart_change_pending() {
                    self.start_driver_task(ctx, crate::driver::retry_connection);
                }
            }
            SettingsAction::RetryResolution(id) => self.controller.retry_resolution(id),
            SettingsAction::DismissScreen => self.dismiss_active_screen(ctx),
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
        self.draw_mobile_bar(ctx);
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
        self.apply_desktop_settings(ctx);
        self.poll_preview_capture(ctx);
        // 手势钩子跟着手机屏与配置走（区域没变时什么都不做）。
        // 缩放、摆窗和触控共享本帧的显示器快照，避免每帧重复全量枚举。
        let state = self.state();
        let phone_id = if state.screen_phase == PhoneScreenPhase::Ready { state.display_id } else { None };
        let phone_layout = self.display_layout.refresh(self.desktop_settings.is_open(), phone_id);

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
                    let tab = if tab == 0 { SettingsTab::Screens } else { SettingsTab::from_index(tab) };
                    self.open_desktop_settings(tab);
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
                TrayCommand::SetPhoneScreenMode(mode) => {
                    let before = self.config.clone();
                    self.config.set_phone_screen_mode(mode);
                    self.apply_settings_change(ctx, &before, SettingsOutcome::Changed);
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
            let ready = state.screen_phase == PhoneScreenPhase::Ready;
            let reenter = self.controller.can_reenter();
            if let Some(tray) = &mut self.tray {
                tray.set_status(&summary, ready);
                tray.set_phone_screen_mode(self.config.phone_screen_mode());
                tray.set_reenter_available(reenter);
            }
        }
        // 托盘事件来自另一个消息窗口，egui 没有输入就不会重绘；靠这个定时器
        // 让 `poll` 有节奏地跑起来（200ms 足够，不是每帧空转）。
        ctx.request_repaint_after(std::time::Duration::from_millis(200));

        // 界面窗口跟着手机屏走（对应 C# 里 PhoneHomeWindow 一直待在虚拟屏上）。
        self.sync_window(ctx, phone_layout);
        self.controller.sync_touch(
            &self.config,
            self.active_screen.as_deref(),
            phone_layout.map(|(full, _)| full),
            self.last_phone_rect,
            self.bubble.screen_bounds(ctx.pixels_per_point() as f64),
        );

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
        self.poll_driver_retry();
        let settings_open = self.settings.is_open() || self.desktop_settings.is_open();
        self.poll_autostart(ctx, settings_open);
        if settings_open {
            self.refresh_settings_status(ctx);
        }
        if !self.request_exit && self.desktop_settings.is_open() {
            let state = self.state();
            let display_id = state.display_id.filter(|_| {
                state.screen_phase == PhoneScreenPhase::Ready
                    && self.config.phone_screen_mode() != PhoneScreenMode::Off
            });
            self.desktop_settings.show(ctx, self.display_layout.desktop_monitors(), display_id);
        }
        // 设置窗画在手机屏这块界面上：用户是在手机（UU 远程）上操作的，独立窗口开在
        // 电脑主屏反而看不见。顶部有「返回」可以关掉，标题栏的 X 也能关。
        let showing_settings = self.settings.is_open();
        let before = self.config.clone();
        let outcome = self.settings.show(ctx, &mut self.config);
        self.apply_settings_change(ctx, &before, outcome);
        if self.request_exit {
            // 同样先收尾：关窗之后 Rust 侧可能一行都跑不到。
            self.finish();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if showing_settings || self.settings.is_open() {
            if !self.settings.is_open() { ctx.request_repaint(); }
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
        if bar_state.placed.is_some() && !bar_state.list_visible && !self.settings.is_open()
            && self.mobile_sheet.is_none() {
            self.draw_home_bar(ctx);
            self.shots.poll();
            self.shots.show(ctx);
            return;
        }

        self.draw_mobile_home(ctx);
    }
}

impl PhoneHomeApp {
    fn dismiss_active_screen(&mut self, ctx: &egui::Context) {
        self.mobile_sheet = None;
        self.reentry_pill_hidden = false;
        self.controller
            .dismiss_screen_of(self.active_screen.as_deref());
        ctx.request_repaint();
    }

    fn draw_mobile_bar(&mut self, ctx: &egui::Context) {
        let state = self.state();
        let constrained = matches!(
            state.notice,
            Some(snug_core::desk::DeskNotice::WindowTooLarge { .. })
        );
        let mut action = None;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(self.palette.home_bar)
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .show(ctx, |ui| {
                ui.add_enabled_ui(state.work.is_none(), |ui| {
                    action = return_bar(
                        ui,
                        self.config.screenshot && state.placed.is_some() && !self.shots.is_open(),
                        constrained,
                        self.palette,
                    );
                });
            });
        match action {
            Some(ReturnBarAction::Library) => {
                self.controller.show_list_of(self.active_screen.as_deref())
            }
            Some(ReturnBarAction::Capture) if self.preview.is_none() => {
                if let Some(placed) = state.placed {
                    self.shots.take(&placed.window);
                }
            }
            _ => {}
        }
    }

    fn draw_mobile_home(&mut self, ctx: &egui::Context) {
        let state = self.state();
        let screens = self.controller.touch_screens();
        let selected_screen = displayed_screen(&self.active_screen, &self.preview)
            .map(str::to_owned)
            .or_else(|| self.config.primary_touch_screen().map(|screen| screen.id));
        let name = screens
            .iter()
            .find(|(id, _)| selected_screen.as_ref() == Some(id))
            .or_else(|| screens.first())
            .map(|(_, name)| name.as_str())
            .unwrap_or("手机屏");
        let action = self.library_ui.show(
            ctx,
            &mut self.library,
            &state,
            self.palette,
            self.preview.is_some(),
            name,
        );
        if let Some(action) = action {
            self.apply_library_action(action, ctx);
        }
        self.draw_screen_picker(ctx);
    }

    fn draw_screen_picker(&mut self, ctx: &egui::Context) {
        if self.mobile_sheet.is_none() {
            return;
        }
        let mut close = false;
        let mut chosen = None;
        let mut settings = false;
        let mut dismiss = false;
        let response = egui::Modal::new(egui::Id::new("screen-picker")).show(ctx, |ui| {
            ui.set_width((ctx.screen_rect().width() - 56.0).clamp(180.0, 440.0));
            ui.heading("切换屏幕");
            ui.add_space(12.0);
            for (id, name) in self.controller.touch_screens() {
                let selected = displayed_screen(&self.active_screen, &self.preview)
                    .map(|active| active == id)
                    .unwrap_or_else(|| {
                        self.config
                            .primary_touch_screen()
                            .is_some_and(|screen| screen.id == id)
                    });
                if touch_button(
                    ui,
                    &format!("{}{}", if selected { "✓  " } else { "" }, name),
                )
                .clicked()
                {
                    chosen = Some(id);
                    close = true;
                }
            }
            if touch_button(ui, "屏幕与分辨率设置").clicked() {
                settings = true;
                close = true;
            }
            if self.preview.is_none() && touch_button(ui, "退出这块屏…").clicked() {
                dismiss = true;
                close = true;
            }
            if touch_button(ui, "取消").clicked() {
                close = true;
            }
        });
        if close || response.should_close() {
            self.mobile_sheet = None;
        }
        if let Some(id) = chosen {
            if let Some(id) =
                select_displayed_screen(&mut self.active_screen, &mut self.preview, id)
            {
                self.controller.set_active_screen(Some(id));
            }
            self.last_phone_rect = None;
            self.last_offscreen = None;
        }
        if settings {
            self.apply_library_action(LibraryAction::Settings, ctx);
        }
        if dismiss {
            self.library_ui.request_dismiss();
            ctx.request_repaint();
        }
    }

    fn apply_library_action(&mut self, action: LibraryAction, ctx: &egui::Context) {
        if self.preview.is_some() && library_action_changes_desktop(&action) {
            crate::info!("预览：只读模式，未执行应用或显示器操作");
            return;
        }
        let screen = self.active_screen.as_deref();
        match action {
            LibraryAction::App(AppAction::Pull(window)) => self.controller.pull(screen, window),
            LibraryAction::App(AppAction::Open(app)) => self.controller.open(screen, app),
            LibraryAction::NewWindow(app) => self.controller.new_window(screen, app),
            LibraryAction::QuitApp(app) => self.controller.quit(screen, app),
            LibraryAction::CloseWindow(window) => self.controller.close_window(screen, window),
            LibraryAction::ReturnPlaced => self.controller.return_to_placed_of(screen),
            LibraryAction::RestorePlaced => self.controller.restore_placed(screen),
            LibraryAction::ZoomIn => self.controller.adjust_zoom(screen, 1),
            LibraryAction::ZoomOut => self.controller.adjust_zoom(screen, -1),
            LibraryAction::ForceQuit { pid, app } => self.controller.force_quit(screen, pid, app),
            LibraryAction::DismissNotice => self.controller.dismiss_notice(screen),
            LibraryAction::Settings => {
                self.settings.select_tab(SettingsTab::Screens);
                self.settings.open();
            }
            LibraryAction::Screens => {
                self.mobile_sheet = Some(MobileSheet::Screens);
                self.last_phone_rect = None;
            }
            LibraryAction::Dismiss => self.dismiss_active_screen(ctx),
            LibraryAction::ClosePreview => self.close_preview(ctx),
        }
        ctx.request_repaint();
    }
}

fn touch_zoom(width: f32, height: f32, native_dpi: f32) -> f32 {
    (width.min(height) / (native_dpi.max(0.5) * snug_core::screen_fit::TOUCH_DESIGN_WIDTH as f32))
        .clamp(0.35, 8.0)
}

fn touch_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), 52.0],
        egui::Button::new(label).truncate(),
    )
}

fn library_action_changes_desktop(action: &LibraryAction) -> bool {
    !matches!(
        action,
        LibraryAction::Settings | LibraryAction::Screens | LibraryAction::ClosePreview
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnBarAction {
    Library,
    Capture,
}

fn return_bar(
    ui: &mut egui::Ui,
    screenshot: bool,
    constrained: bool,
    palette: crate::theme::Palette,
) -> Option<ReturnBarAction> {
    let mut action = None;
    let height = ui.available_height().max(1.0);
    ui.spacing_mut().item_spacing.x = 0.0;
    ui.horizontal(|ui| {
        if screenshot {
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(48.0, height), egui::Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), "截图当前窗口")
            });
            let camera = egui::Rect::from_center_size(rect.center(), egui::vec2(23.0, 16.0));
            ui.painter().rect_stroke(
                camera,
                3.0,
                egui::Stroke::new(1.8_f32, palette.text_weak),
                egui::StrokeKind::Inside,
            );
            ui.painter().circle_stroke(
                camera.center(),
                4.0,
                egui::Stroke::new(1.8_f32, palette.text_weak),
            );
            if response.on_hover_text("截图当前窗口").clicked() {
                action = Some(ReturnBarAction::Capture);
            }
        }
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width().max(1.0), height),
            egui::Sense::click_and_drag(),
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), "返回 App 资源库")
        });
        let center = rect.center();
        ui.painter().line_segment(
            [
                center + egui::vec2(-9.0, 3.0),
                center + egui::vec2(0.0, -3.0),
            ],
            egui::Stroke::new(2.5_f32, palette.text_weak),
        );
        ui.painter().line_segment(
            [
                center + egui::vec2(0.0, -3.0),
                center + egui::vec2(9.0, 3.0),
            ],
            egui::Stroke::new(2.5_f32, palette.text_weak),
        );
        if constrained {
            ui.painter().text(
                rect.right_center() - egui::vec2(18.0, 0.0),
                egui::Align2::CENTER_CENTER,
                "!",
                egui::FontId::proportional(18.0),
                palette.warn,
            );
        }
        let swipe = response.hovered() && ui.input(|input| input.raw_scroll_delta.y >= 12.0);
        let activated =
            response.clicked() || swipe || (response.dragged() && response.drag_delta().y <= -12.0);
        response.on_hover_text(if constrained {
            "尺寸受限；返回资源库查看详情"
        } else {
            "返回 App 资源库"
        });
        if activated {
            action = Some(ReturnBarAction::Library);
        }
    });
    action
}

#[cfg(test)]
mod mobile_layout_tests {
    use super::*;

    #[test]
    fn temporary_preview_selection_does_not_leak_after_close_or_phone_takeover() {
        for original in [None, Some("A".to_string())] {
            let mut active = original.clone();
            let mut preview = Some(PreviewState {
                active_screen: active.clone(),
                capture: None,
            });
            assert_eq!(
                select_displayed_screen(&mut active, &mut preview, "B".into()),
                None
            );
            assert_eq!(displayed_screen(&active, &preview), Some("B"));
            assert_eq!(active, original);
            preview = None;
            assert_eq!(displayed_screen(&active, &preview), original.as_deref());
            assert_eq!(
                select_displayed_screen(&mut active, &mut preview, "C".into()),
                Some("C".into())
            );
            assert_eq!(active.as_deref(), Some("C"));
        }
    }

    #[test]
    fn return_bar_keeps_capture_separate_from_returning_to_the_library() {
        for (screenshot, x, expected) in [
            (true, 24.0, ReturnBarAction::Capture),
            (true, 200.0, ReturnBarAction::Library),
            (false, 24.0, ReturnBarAction::Library),
        ] {
            let ctx = egui::Context::default();
            crate::theme::apply(&ctx, false);
            let draw = |events: Vec<egui::Event>| {
                let mut action = None;
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(400.0, 60.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default()
                            .frame(egui::Frame::NONE)
                            .show(ctx, |ui| {
                                action =
                                    return_bar(ui, screenshot, false, crate::theme::Palette::LIGHT);
                            });
                    },
                );
                action
            };
            for _ in 0..3 {
                draw(Vec::new());
            }
            let mut result = None;
            for pressed in [true, false] {
                result = draw(vec![
                    egui::Event::PointerMoved(egui::pos2(x, 30.0)),
                    egui::Event::PointerButton {
                        pos: egui::pos2(x, 30.0),
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ]);
            }
            assert_eq!(result, Some(expected));
        }
    }

    #[test]
    fn preview_desktop_action_guard_covers_the_new_window_controls() {
        for action in [
            LibraryAction::ReturnPlaced,
            LibraryAction::RestorePlaced,
            LibraryAction::ZoomIn,
            LibraryAction::ZoomOut,
            LibraryAction::Dismiss,
            LibraryAction::ForceQuit {
                pid: 42,
                app: "test".into(),
            },
            LibraryAction::DismissNotice,
        ] {
            assert!(library_action_changes_desktop(&action));
        }
        for action in [
            LibraryAction::Settings,
            LibraryAction::Screens,
            LibraryAction::ClosePreview,
        ] {
            assert!(!library_action_changes_desktop(&action));
        }
    }

    #[test]
    fn touch_scale_has_equal_short_side_across_dpi_and_orientation() {
        for dpi in [1.0_f32, 1.25, 2.0] {
            let portrait = touch_zoom(1080.0, 1900.0, dpi);
            let landscape = touch_zoom(1900.0, 1080.0, dpi);
            assert!((portrait - landscape).abs() < 0.001);
            assert!((1080.0 / (dpi * portrait) - 400.0).abs() < 0.01);
            let bar =
                snug_core::ScreenFit::home_bar_height_of(snug_core::Size::new(1080.0, 1900.0));
            assert!(bar as f32 / (dpi * portrait) >= 60.0 - 0.01);
        }
    }
}
