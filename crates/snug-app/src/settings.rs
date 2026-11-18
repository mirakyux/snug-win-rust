//! 设置窗口（egui），移植自 C# 版 `SnugWin.App.Views.SettingsWindow`。
//!
//! C# 那边是 WPF 窗口 + `AppController` 直连：勾选框直接调控制器改状态、改配置。
//! Rust 这边按职责切开：
//! - 本模块**只改传入的 [`AppConfig`]**，不读写文件、不碰全局状态、不调平台层；
//! - 需要动作（自启、检查更新、开日志目录……）时把 [`SettingsAction`] 排进队列，
//!   由 app 层用 [`SettingsWindow::take_actions`] 取走执行；
//! - “改了要不要重来 / 两个开关是不是互相矛盾”这类判定抽成纯函数
//!   （[`classify_change`]、[`change_hint`]、[`both_off_hint`]），`show()` 只摆控件。
//!
//! 五个页面与 C# v1.3.0 一致：控制中心（bento 首页）/ 手势与交互 / 驱动与日志 /
//! 关于与更新 / 多屏与设备。首页把原来"常规与手机屏"那一页的开关与机型信息
//! 收进了卡片里（C# 1.3.0 就是这么拆的）。
//! 字段全部来自 `snug_core::AppConfig`（对应 C# `SnugWin.Core.Config.AppConfig`），
//! 本模块不新造配置结构。
//!
//! 手动验证（真机）：
//! 1. 打开设置窗，五个页签都能切换，底部状态与提权文案正确；
//! 2. 改“默认字号放大”后关窗再开，值能被控制器写回 config.json；
//! 3. 关掉手势总开关后，四个子项变灰且不可点；
//! 4. 断驱动时首页出现黄色横幅，“驱动与日志”页显示未就绪；
//! 5. 首页切浅色/暗黑/自动，界面配色立刻变（自动跟系统）。

use eframe::egui;
use snug_core::{AppConfig, AppThemeMode, PhoneCatalog, PhoneChoice, ScreenConfig, ScreenFit};

/// 字号放大档位（C# `ZoomCombo` 的 0~4），与 core 的 `MIN_ZOOM_STEPS`/`MAX_ZOOM_STEPS` 一致。
pub const ZOOM_LABELS: [&str; 5] = ["0 级（默认）", "+1 级", "+2 级", "+3 级", "+4 级"];

/// 页签，顺序与 C# v1.3.0 的 `SelectTab(index)` 一致：
/// 0 控制中心（bento 首页）/ 1 手势与交互 / 2 驱动与日志 / 3 关于与更新 / 4 多屏与设备。
///
/// "常规与手机屏"那一页在 1.3.0 里被拆掉了：开关与机型信息都挪进首页卡片。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Home,
    Gestures,
    Diagnostics,
    About,
    Screens,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 5] = [
        SettingsTab::Home,
        SettingsTab::Gestures,
        SettingsTab::Diagnostics,
        SettingsTab::About,
        SettingsTab::Screens,
    ];

    /// 左侧导航文字。
    pub fn nav_title(self) -> &'static str {
        match self {
            SettingsTab::Home => "控制中心",
            SettingsTab::Gestures => "手势与交互",
            SettingsTab::Diagnostics => "驱动与日志",
            SettingsTab::About => "关于与更新",
            SettingsTab::Screens => "多屏与设备",
        }
    }

    /// 页面标题（对应 C# 子页顶部标题；首页是品牌行，用程序名）。
    pub fn page_title(self) -> &'static str {
        match self {
            SettingsTab::Home => "SnugWin",
            SettingsTab::Gestures => "手势与交互细项",
            SettingsTab::Diagnostics => "驱动状态与实时日志",
            SettingsTab::About => "关于与版本更新",
            SettingsTab::Screens => "多屏与设备管理",
        }
    }

    pub fn index(self) -> usize {
        match self {
            SettingsTab::Home => 0,
            SettingsTab::Gestures => 1,
            SettingsTab::Diagnostics => 2,
            SettingsTab::About => 3,
            SettingsTab::Screens => 4,
        }
    }

    /// C# `SelectTab(index)`：越界落回首页。
    pub fn from_index(index: usize) -> Self {
        Self::ALL.get(index).copied().unwrap_or(SettingsTab::Home)
    }
}

/// `show()` 的返回值。`Changed` 表示这一帧用户改了配置，app 层应当保存。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsOutcome {
    Idle,
    Changed,
    CloseRequested,
}

/// 需要 app 层去执行的动作（本模块不做任何副作用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsAction {
    /// 手机屏总开关（C# `SetPhoneScreenEnabled`），控制器可实时响应。
    SetPhoneScreen(bool),
    /// 手动强制开关（C# `SetManualOverride`），属于控制器运行态、不写配置。
    SetManualOverride(bool),
    /// 开机自启（C# `SetAutoStartEnabled` → `StartupTaskService`），会真实建计划任务。
    SetAutoStart(bool),
    /// 检查更新（C# `OnCheckUpdate`）。
    CheckUpdate,
    /// 下载并重启升级（C# `OnInstallUpdate`）。
    InstallUpdate,
    /// 打开日志目录（C# `OnOpenLogDir`，资源管理器）。
    OpenLogDirectory,
    /// 安装 / 修复虚拟显示器驱动（C# `OnInstallDriver`）。
    InstallDriver,
    /// 打开 GitHub 仓库主页（C# `OnOpenRepo`）。
    OpenRepository,
    /// 以管理员身份重启（C# `OnRestartElevated`）。
    RestartElevated,
    /// 手势配置变了，钩子要按新配置重新接管（C# 里是 `SetGesturesEnabled` 等四个 setter）。
    ApplyGestureSettings,
    /// 首页的「进入手机屏」（C# `HeroReentryBtn` → `ReenterScreen`）。
    ReenterScreen,
    /// 首页的「预览界面」（C# `OpenScreenPreview`）。
    OpenPreview,
}

/// 运行态快照：由 app 层喂进来，界面只读（对应 C# 里从 `_controller` 读的那些字段）。
#[derive(Debug, Clone)]
pub struct StatusInfo {
    /// `snug_win::vdd::is_driver_pipe_available`：`\\.\pipe\virtualdisplaydriver` 是否在。
    pub pipe_available: bool,
    /// 当前进程是否管理员。
    pub elevated: bool,
    /// 手机屏是否已就绪（决定状态灯颜色）。
    pub phone_screen_ready: bool,
    /// 能不能"重新进入手机屏"（C# `CanReenterPhoneScreen`：会话中且被收起过）。
    pub can_reenter: bool,
    /// 一句话状态（C# `GetStatusSummary()`）。
    pub status_summary: String,
    /// 当前版本（C# `CurrentVersion`）。
    pub current_version: String,
    /// 包形态（C# `PackageKind`）：Rust 版恒为自包含。
    pub self_contained: bool,
}

impl Default for StatusInfo {
    fn default() -> Self {
        Self {
            pipe_available: false,
            elevated: false,
            phone_screen_ready: false,
            can_reenter: false,
            status_summary: "待机中".to_string(),
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            self_contained: true,
        }
    }
}

/// 更新检查/下载的界面状态（C# `RefreshUpdateUi` + 下载进度回调）。
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateUiState {
    /// 还没查过（C# `upd is null`）。
    Idle,
    /// 正在查 GitHub。
    Checking,
    /// 已是最新。
    UpToDate,
    /// 有新版本；`downloadable` 对应 C# 的“资产里有下载地址”。
    Available { version: String, downloadable: bool },
    /// 正在下载。
    Downloading { version: String, progress: f32 },
    /// 校验通过，准备重启升级。
    Ready,
    /// 检查失败。
    CheckFailed(String),
    /// 下载/校验/替换失败（可以重试）。
    InstallFailed { version: String, message: String },
}

impl UpdateUiState {
    /// 页面上的状态文案（逐字对齐 C# `RefreshUpdateUi` / `OnCheckUpdate` / `OnInstallUpdate`）。
    pub fn status_text(&self, current_version: &str) -> String {
        match self {
            UpdateUiState::Idle => "点击右侧按钮检查 GitHub 最新正式版".to_string(),
            UpdateUiState::Checking => "正在向 GitHub 查询最新版本...".to_string(),
            UpdateUiState::UpToDate => format!("已是最新版本（v{current_version}）"),
            UpdateUiState::Available { version, .. } => format!(
                "发现新版本 v{version}（点击可直接下载、校验 SHA-256 并重启升级）"
            ),
            UpdateUiState::Downloading { version, progress } => {
                format!("正在下载 v{version}... {:.0}%", progress * 100.0)
            }
            UpdateUiState::Ready => "校验通过，正在还原窗口并重启更新...".to_string(),
            UpdateUiState::CheckFailed(message) => format!("检查更新失败：{message}"),
            UpdateUiState::InstallFailed { message, .. } => format!("更新失败：{message}"),
        }
    }

    /// “下载并更新重启”按钮是否可用（C#：有更新且资产带下载地址，且不在下载/重启中）。
    pub fn install_enabled(&self) -> bool {
        match self {
            UpdateUiState::Available { downloadable, .. } => *downloadable,
            UpdateUiState::InstallFailed { .. } => true,
            _ => false,
        }
    }

    /// “检查更新”按钮是否可用（C# 在查询/下载期间禁用）。
    pub fn check_enabled(&self) -> bool {
        !matches!(
            self,
            UpdateUiState::Checking
                | UpdateUiState::Downloading { .. }
                | UpdateUiState::Ready
        )
    }

    /// “关于与更新”导航上的小绿点（C# `UpdateDot`）。
    pub fn has_update_dot(&self) -> bool {
        matches!(self, UpdateUiState::Available { .. })
    }

    /// 下载进度条是否可见。
    pub fn progress(&self) -> Option<f32> {
        match self {
            UpdateUiState::Downloading { progress, .. } => Some(progress.clamp(0.0, 1.0)),
            _ => None,
        }
    }
}

/// 改动的影响面：决定要不要提示“重来一次”。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeImpact {
    None,
    /// 底部条/相机键：下一帧重画即可。
    RefreshHomeBar,
    /// 手势钩子要按新配置刷新（C# 是 `SetGesturesEnabled` 等实时生效）。
    ReloadGestures,
    /// 机型或字号变了：手机屏要以新比例重建并重新铺窗。
    RebuildPhoneScreen,
    /// 明暗主题变了：只换配色，不重建屏。
    Theme,
    /// 多屏配置增删改：屏列表发给状态机，由它建/撤屏。
    Screens,
}

/// 对比两份配置，得出影响面。判定顺序从“最重”到“最轻”。
pub fn classify_change(before: &AppConfig, after: &AppConfig) -> ChangeImpact {
    if before.screens != after.screens {
        return ChangeImpact::Screens;
    }
    if before.phone != after.phone || before.zoom_steps != after.zoom_steps {
        return ChangeImpact::RebuildPhoneScreen;
    }
    if before.gestures != after.gestures
        || before.gesture_scroll != after.gesture_scroll
        || before.gesture_long_press_right_click != after.gesture_long_press_right_click
        || before.copy_after_selection != after.copy_after_selection
        || before.gestures_injected_only != after.gestures_injected_only
    {
        return ChangeImpact::ReloadGestures;
    }
    if before.screenshot != after.screenshot {
        return ChangeImpact::RefreshHomeBar;
    }
    if before.theme != after.theme {
        return ChangeImpact::Theme;
    }
    // phone_screen / auto_check_updates / welcomed / last_version 都不需要“重来”：
    // 前两个由控制器实时响应，后两个不是设置界面能改的。
    ChangeImpact::None
}

/// 供界面显示的“你需要做什么”提示；`None` 表示无需打扰用户。
pub fn change_hint(impact: ChangeImpact) -> Option<&'static str> {
    match impact {
        ChangeImpact::None => None,
        ChangeImpact::RefreshHomeBar => Some("相机键会在手机屏底部条上即时刷新。"),
        ChangeImpact::ReloadGestures => Some("手势设置会在下一次接管触控时生效。"),
        ChangeImpact::RebuildPhoneScreen => {
            Some("机型或字号已改变：下次连接会按新比例重建手机屏，已铺开的窗口会重新归位。")
        }
        ChangeImpact::Theme => None,
        ChangeImpact::Screens => Some("屏配置已变化：下一次连接会按新配置建屏。"),
    }
}

/// 两个“自动化”都关掉时的提醒：程序既不随登录启动、也不主动发现新版本。
///
/// C# 允许这么配（没有拦截），这里同样不阻止，只提示——否则用户会以为更新坏了。
pub fn both_off_hint(cfg: &AppConfig, autostart: bool) -> Option<&'static str> {
    if !autostart && !cfg.auto_check_updates {
        Some("自启与自动检查更新都已关闭：程序不会随登录启动，也不会主动发现新版本。")
    } else {
        None
    }
}

/// 机型下拉里显示的当前选择（C# `$"{model ?? "自定义"}（{w} × {h}）"`）。
pub fn model_text(choice: &PhoneChoice) -> String {
    let model = choice.model.clone().unwrap_or_else(|| "自定义".to_string());
    format!("{model}（{} × {}）", choice.screen.width, choice.screen.height)
}

/// 首页「当前手机屏」卡的第二行（C# `PhoneSpecText`：`1206 × 2622 · 200%`）。
///
/// 尺寸取设备本身的物理像素；百分比取 `ScreenFit` 给虚拟屏定的 DPI
/// （电脑屏且 2 倍会超 4K/2400 时退回 100%，见 C# `TargetDpiPercent`）。
pub fn spec_text(choice: &PhoneChoice) -> String {
    let fit = ScreenFit::from_device(
        &choice.screen.to_fit_spec(),
        choice.kind,
        choice.kind.usual_orientation(),
    );
    format!(
        "{} × {} · {}%",
        choice.screen.width as i64,
        choice.screen.height as i64,
        fit.target_dpi_percent(fit.base_width())
    )
}

/// 关于页的版本行（C# `AboutVersionText`）。
pub fn version_line(current_version: &str, self_contained: bool) -> String {
    let kind = if self_contained { "自包含版" } else { "框架依赖版" };
    format!("当前版本：v{current_version}（{kind}）")
}

/// 驱动页的管道文案（C# `DiagPipeText`）。
pub fn pipe_text(available: bool) -> String {
    if available {
        r"已连接命名管道 \\.\pipe\virtualdisplaydriver".to_string()
    } else {
        r"未检测到命名管道 \\.\pipe\virtualdisplaydriver".to_string()
    }
}

/// 提权文案（C# `ElevationText`）。
pub fn elevation_text(elevated: bool) -> &'static str {
    if elevated {
        "管理员权限运行中"
    } else {
        "当前权限：普通用户"
    }
}

/// 字号档位 → 下拉选项下标（越界夹回 0~4）。
pub fn zoom_index(steps: i32) -> usize {
    steps.clamp(0, ZOOM_LABELS.len() as i32 - 1) as usize
}

/// 档位 → 下拉文字。
pub fn zoom_label(steps: i32) -> &'static str {
    ZOOM_LABELS[zoom_index(steps)]
}

/// 设置窗口本体。字段都是界面自己的状态，不持有控制器/平台层。
pub struct SettingsWindow {
    open: bool,
    tab: SettingsTab,
    status: StatusInfo,
    update: UpdateUiState,
    /// 计划任务状态（由 `StartupService` 查询后喂进来，不在 `AppConfig` 里）。
    autostart: bool,
    /// 手动强制手机屏（控制器运行态，不在 `AppConfig` 里）。
    manual_override: bool,
    /// 诊断页的日志文本（app 层追加）。
    logs: String,
    /// 机型库（可有可无：找不到 phone-models.json 时退化成只显示当前机型）。
    catalog: Option<PhoneCatalog>,
    /// 待执行动作队列。
    actions: Vec<SettingsAction>,
    /// 打开窗口时的配置快照，用来算“改了什么”。
    baseline: Option<AppConfig>,
    /// 最近一次改动的提示，换页/关窗后清空。
    last_hint: Option<&'static str>,
    /// 多屏页「添加一块屏」的三个选择（只属于界面状态，不进配置）。
    new_screen_kind: snug_core::DeviceKind,
    new_screen_orientation: snug_core::ScreenOrientation,
    new_screen_model: Option<String>,
    /// 正在给第几块屏挑机型（`None` = 没在挑）。对应 C# 的 `PhoneModelWindow`，
    /// 我们把它做成多屏页里的内嵌选择器，不再单开一个窗口。
    model_picker: Option<usize>,
    /// 机型选择器的搜索词。
    model_search: String,
}

impl Default for SettingsWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsWindow {
    pub fn new() -> Self {
        Self {
            open: false,
            tab: SettingsTab::Home,
            status: StatusInfo::default(),
            update: UpdateUiState::Idle,
            autostart: false,
            manual_override: false,
            logs: String::new(),
            catalog: None,
            actions: Vec::new(),
            baseline: None,
            last_hint: None,
            new_screen_kind: snug_core::DeviceKind::Phone,
            new_screen_orientation: snug_core::ScreenOrientation::Portrait,
            new_screen_model: None,
            model_picker: None,
            model_search: String::new(),
        }
    }

    /// 打开窗口（C# 里是 `new SettingsWindow(...)` + `Show()`）。
    pub fn open(&mut self) {
        self.open = true;
        self.baseline = None;
        self.last_hint = None;
    }

    /// 用代码切页（C# `SelectTab`）。
    pub fn select_tab(&mut self, tab: SettingsTab) {
        self.tab = tab;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn tab(&self) -> SettingsTab {
        self.tab
    }

    /// app 层喂运行态（每帧或每次状态变化时调用都行）。
    pub fn set_status(&mut self, status: StatusInfo) {
        self.status = status;
    }

    pub fn set_update_state(&mut self, update: UpdateUiState) {
        self.update = update;
    }

    pub fn set_autostart(&mut self, enabled: bool) {
        self.autostart = enabled;
    }

    pub fn set_manual_override(&mut self, enabled: bool) {
        self.manual_override = enabled;
    }

    pub fn set_catalog(&mut self, catalog: Option<PhoneCatalog>) {
        self.catalog = catalog;
    }

    pub fn set_logs(&mut self, logs: impl Into<String>) {
        self.logs = logs.into();
    }

    /// 追加一行日志（对应 C# `LogAppended` 事件）。
    pub fn append_log(&mut self, line: &str) {
        if !self.logs.is_empty() && !self.logs.ends_with('\n') {
            self.logs.push('\n');
        }
        self.logs.push_str(line);
        self.logs.push('\n');
    }

    /// 取走队列里的动作。app 层每帧调一次，按需分发。
    pub fn take_actions(&mut self) -> Vec<SettingsAction> {
        std::mem::take(&mut self.actions)
    }

    /// 画一帧。`cfg` 是本模块唯一会改的外部状态。
    /// 画一帧设置窗。调用方把它放进**独立的 OS 窗口**里（`show_viewport_immediate`），
    /// 所以这里是整块面板而不是主窗口里的浮动 `egui::Window`——原版 C# 的设置窗也是
    /// 独立的 WPF 窗口，出现在主屏上，不是塞在手机屏那块虚拟屏里。
    pub fn show(&mut self, ctx: &egui::Context, cfg: &mut AppConfig) -> SettingsOutcome {
        if !self.open {
            return SettingsOutcome::Idle;
        }
        let before = cfg.clone();
        if self.baseline.is_none() {
            self.baseline = Some(before.clone());
        }

        let mut titlebar_open = self.open;
        let mut back_clicked = false;
        egui::Window::new("SnugWin 设置")
            .open(&mut titlebar_open)
            .collapsible(false)
            .resizable(true)
            .default_size([820.0, 640.0])
            .min_size([700.0, 520.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(self.tab.page_title()).size(19.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new("返回").min_size(egui::vec2(76.0, 32.0)))
                            .on_hover_text("关闭设置窗（不退出程序）")
                            .clicked()
                        {
                            back_clicked = true;
                        }
                    });
                });
                ui.separator();
                ui.add_space(2.0);
                self.layout(ui, cfg);
            });
        self.open = titlebar_open && !back_clicked;

        if !self.open {
            self.baseline = None;
            self.last_hint = None;
            return SettingsOutcome::CloseRequested;
        }

        if *cfg != before {
            let impact = self
                .baseline
                .as_ref()
                .map(|baseline| classify_change(baseline, cfg))
                .unwrap_or(ChangeImpact::None);
            self.last_hint = change_hint(impact);
            return SettingsOutcome::Changed;
        }
        SettingsOutcome::Idle
    }

    // -----------------------------------------------------------------------
    // 布局
    // -----------------------------------------------------------------------

    fn layout(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_min_width(188.0);
                ui.set_max_width(188.0);
                self.sidebar(ui);
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("snug-settings-page")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_min_width(520.0);
                    ui.heading(self.tab.page_title());
                    ui.add_space(4.0);
                    match self.tab {
                        SettingsTab::Home => self.page_home(ui, cfg),
                        SettingsTab::Screens => self.page_screens(ui, cfg),
                        SettingsTab::Gestures => self.page_gestures(ui, cfg),
                        SettingsTab::Diagnostics => self.page_diagnostics(ui, cfg),
                        SettingsTab::About => self.page_about(ui, cfg),
                    }
                });
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.heading("SnugWin");
        ui.label(
            egui::RichText::new(format!("v{}", self.status.current_version))
                .small()
                .weak(),
        );
        ui.label(egui::RichText::new("UU远程 手机竖屏伴侣").small().weak());
        ui.add_space(10.0);
        ui.separator();

        for tab in SettingsTab::ALL {
            let mut text = egui::RichText::new(tab.nav_title());
            if tab == SettingsTab::About && self.update.has_update_dot() {
                text = text.color(egui::Color32::from_rgb(0x34, 0xD3, 0x99));
            }
            if ui.selectable_label(self.tab == tab, text).clicked() {
                self.tab = tab;
            }
        }

        ui.separator();
        ui.add_space(6.0);
        let dot = if self.status.phone_screen_ready {
            egui::Color32::from_rgb(0x34, 0xD3, 0x99)
        } else {
            egui::Color32::from_rgb(0x60, 0xA5, 0xFA)
        };
        ui.horizontal(|ui| {
            ui.colored_label(dot, "●");
            ui.label(egui::RichText::new(&self.status.status_summary).strong());
        });
        ui.label(egui::RichText::new(elevation_text(self.status.elevated)).small().weak());
        if !self.status.elevated && ui.small_button("以管理员重启").clicked() {
            self.actions.push(SettingsAction::RestartElevated);
        }
    }

    /// 一页的公共收尾：影响提示 + 自动化都关掉的提醒。
    fn page_footer(&mut self, ui: &mut egui::Ui, cfg: &AppConfig) {
        if let Some(hint) = self.last_hint {
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(hint)
                    .small()
                    .color(egui::Color32::from_rgb(0x93, 0xC5, 0xFD)),
            );
        }
        if let Some(hint) = both_off_hint(cfg, self.autostart) {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(hint)
                    .small()
                    .color(egui::Color32::from_rgb(0xFC, 0xD3, 0x4D)),
            );
        }
    }

    /// 带说明的两行勾选框：标题一行、灰色说明一行。
    fn labeled_checkbox(ui: &mut egui::Ui, checked: &mut bool, title: &str, description: &str) -> bool {
        let changed = ui
            .checkbox(checked, egui::RichText::new(title).strong())
            .changed();
        ui.label(egui::RichText::new(description).small().weak());
        ui.add_space(10.0);
        changed
    }

    /// 同上，但整个控件可以被禁用（C# 里总开关关掉后四个子项变灰）。
    fn labeled_checkbox_enabled(
        ui: &mut egui::Ui,
        enabled: bool,
        checked: &mut bool,
        title: &str,
        description: &str,
    ) -> bool {
        let changed = ui
            .add_enabled(
                enabled,
                egui::Checkbox::new(checked, egui::RichText::new(title).strong()),
            )
            .changed();
        ui.label(egui::RichText::new(description).small().weak());
        ui.add_space(10.0);
        changed
    }

    /// 一张卡片（C# bento 首页的 `Border` 卡片）：标题小字 + 内容。
    fn card<R>(
        ui: &mut egui::Ui,
        title: &str,
        add: impl FnOnce(&mut egui::Ui) -> R,
    ) -> R {
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new(title).size(12.0).weak());
                ui.add_space(2.0);
                add(ui)
            })
            .inner
    }

    /// 首页：C# 1.3.0 的"控制中心"（状态 / 当前手机屏 / 外观 / 细项 四张卡）。
    fn page_home(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        if !self.status.pipe_available {
            egui::Frame::group(ui.style())
                .fill(egui::Color32::from_rgb(0x2B, 0x1F, 0x0E))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new("虚拟显示器驱动未就绪")
                                    .strong()
                                    .color(egui::Color32::from_rgb(0xFC, 0xD3, 0x4D)),
                            );
                            ui.label(
                                egui::RichText::new(
                                    r"未检测到 \\.\pipe\virtualdisplaydriver，请安装或重启 virtual-display-rs 驱动。",
                                )
                                .small()
                                .color(egui::Color32::from_rgb(0xFD, 0xE6, 0x8A)),
                            );
                        });
                        if ui.button("一键安装").clicked() {
                            self.actions.push(SettingsAction::InstallDriver);
                        }
                    });
                });
            ui.add_space(8.0);
        }

        // ---- 状态卡 ----
        let ready = self.status.phone_screen_ready;
        let can_reenter = self.status.can_reenter;
        let summary = self.status.status_summary.clone();
        let elevated = self.status.elevated;
        let pipe_ok = self.status.pipe_available;
        Self::card(ui, "状态", |ui| {
            ui.horizontal(|ui| {
                let dot = if ready {
                    egui::Color32::from_rgb(0x34, 0xD3, 0x99)
                } else {
                    egui::Color32::from_rgb(0x60, 0xA5, 0xFA)
                };
                ui.label(egui::RichText::new("●").size(12.0).color(dot));
                ui.label(egui::RichText::new(&summary).size(15.0).strong());
            });
            ui.label(
                egui::RichText::new(format!(
                    "{} · {}",
                    elevation_text(elevated),
                    if pipe_ok { "管道已连接" } else { "管道未就绪" }
                ))
                .small()
                .weak(),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ready {
                    egui::Frame::group(ui.style())
                        .fill(egui::Color32::from_rgb(0x12, 0x2B, 0x22))
                        .inner_margin(egui::Margin::symmetric(10, 4))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new("已就绪")
                                    .size(12.0)
                                    .strong()
                                    .color(egui::Color32::from_rgb(0x34, 0xD3, 0x99)),
                            );
                        });
                } else if can_reenter && ui.button("进入手机屏").clicked() {
                    self.actions.push(SettingsAction::ReenterScreen);
                }
                if !elevated && ui.button("提权重启").clicked() {
                    self.actions.push(SettingsAction::RestartElevated);
                }
            });
        });
        ui.add_space(8.0);

        // ---- 当前手机屏卡 ----
        let model_line = model_text(&cfg.phone);
        let spec_line = spec_text(&cfg.phone);
        let zoom_steps_now = cfg.zoom_steps;
        let zoom_lbl = zoom_label(zoom_steps_now);
        let enabled_screens = cfg.effective_screens().iter().filter(|s| s.enabled).count();
        let mut go_screens = false;
        let mut zoom_click: Option<i32> = None;
        Self::card(ui, "当前手机屏", |ui| {
            ui.label(egui::RichText::new(&model_line).size(15.0).strong());
            ui.label(egui::RichText::new(&spec_line).small().weak());
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("−").on_hover_text("字号小一级").clicked() {
                    zoom_click = Some(zoom_steps_now - 1);
                }
                ui.label(egui::RichText::new(zoom_lbl).size(13.0));
                if ui.button("+").on_hover_text("字号大一级").clicked() {
                    zoom_click = Some(zoom_steps_now + 1);
                }
            });
            ui.add_space(4.0);
            let mut pick_model = false;
            ui.horizontal(|ui| {
                if ui.button("预览界面").clicked() {
                    self.actions.push(SettingsAction::OpenPreview);
                }
                if ui.button("换机型...").clicked() {
                    pick_model = true;
                }
            });
            if pick_model {
                // 机型选择器画在「多屏与设备」页里，所以要跟着跳过去。
                self.model_picker = Some(0);
                self.model_search.clear();
                self.tab = SettingsTab::Screens;
            }
            ui.add_space(4.0);
            if ui
                .add(egui::Button::new(format!("多屏 {enabled_screens}/4 ›")).frame(false))
                .on_hover_text("多屏与设备管理")
                .clicked()
            {
                go_screens = true;
            }
        });
        if let Some(steps) = zoom_click {
            cfg.zoom_steps = steps.clamp(0, ZOOM_LABELS.len() as i32 - 1);
        }
        if go_screens {
            self.tab = SettingsTab::Screens;
        }
        ui.add_space(8.0);

        // ---- 外观卡 ----
        Self::card(ui, "外观", |ui| {
            ui.horizontal(|ui| {
                for (mode, label) in [
                    (AppThemeMode::Light, "浅色"),
                    (AppThemeMode::Dark, "暗黑"),
                    (AppThemeMode::Auto, "自动"),
                ] {
                    ui.selectable_value(&mut cfg.theme, mode, label);
                }
            });
            ui.label(
                egui::RichText::new("自动：跟随 Windows「设置 → 个性化 → 颜色」里的应用主题")
                    .small()
                    .weak(),
            );
        });
        ui.add_space(8.0);

        // ---- 细项卡 ----
        Self::card(ui, "细项", |ui| {
            if Self::labeled_checkbox(
                ui,
                &mut cfg.phone_screen,
                "手机连入时自动创建虚拟竖屏",
                "检测到 UU远程连入即生成手机屏，断开后自动还原所有窗口",
            ) {
                self.actions.push(SettingsAction::SetPhoneScreen(cfg.phone_screen));
            }

            if Self::labeled_checkbox(
                ui,
                &mut self.autostart,
                "开机静默自启动（后台常驻托盘）",
                "通过 Windows 任务计划以最高权限启动，登录桌面不再弹 UAC 提示",
            ) {
                self.actions.push(SettingsAction::SetAutoStart(self.autostart));
            }

            Self::labeled_checkbox(
                ui,
                &mut cfg.screenshot,
                "在底部小横条左侧显示相机键",
                "一键截取手机屏窗口到剪贴板，点击缩略图可 8 把手裁剪",
            );

            if Self::labeled_checkbox(
                ui,
                &mut self.manual_override,
                "手动强制开启手机屏（测试 / 自动探测兜底）",
                "跳过 UU远程 信号检测，直接在本地桌面旁模拟启用竖屏手机屏",
            ) {
                self.actions.push(SettingsAction::SetManualOverride(self.manual_override));
            }
        });

        self.page_footer(ui, cfg);
    }

    /// 多屏与设备管理（C# 1.3.0 `PageScreens`）。
    fn page_screens(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        // 老配置还没有 `screens` 字段：第一次进来先迁移出一块主手机屏。
        if cfg.screens.is_none() {
            cfg.screens = Some(cfg.effective_screens());
        }
        let mut screens = cfg.screens.clone().unwrap_or_default();
        let mut changed = false;
        let mut remove: Option<usize> = None;

        ui.label(
            egui::RichText::new(format!("最多 {} 块屏，每块对应一台设备", ScreenConfig::LIMIT))
                .small()
                .weak(),
        );
        ui.add_space(6.0);

        if screens.is_empty() {
            ui.label(egui::RichText::new("还没有配置任何屏。").weak());
        }

        for (index, screen) in screens.iter_mut().enumerate() {
            let title = screen.name.clone();
            let badge = format!(
                "{} · {}",
                screen.device.kind.display_name(),
                screen.orientation.display_name()
            );
            let spec = spec_text(&screen.device);
            let mut remove_this = false;
            let mut pick_this = false;
            Self::card(ui, &title, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&badge)
                            .size(13.0)
                            .color(egui::Color32::from_rgb(0x8A, 0xB4, 0xF8)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button("删除")
                            .on_hover_text("移除这块屏（已建的虚拟屏会撤掉）")
                            .clicked()
                        {
                            remove_this = true;
                        }
                    });
                });
                ui.label(egui::RichText::new(&spec).small().weak());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui
                        .checkbox(&mut screen.enabled, "启用")
                        .on_hover_text("关掉后这台设备不建虚拟屏")
                        .changed()
                    {
                        changed = true;
                    }
                    if ui.button("预览界面").clicked() {
                        self.actions.push(SettingsAction::OpenPreview);
                    }
                    if ui.button("换机型...").clicked() {
                        pick_this = true;
                    }
                });
            });
            if pick_this {
                self.model_picker = Some(index);
                self.model_search.clear();
            }
            if remove_this {
                remove = Some(index);
            }
            ui.add_space(6.0);
        }

        if let Some(index) = remove {
            screens.remove(index);
            changed = true;
        }

        ui.separator();
        ui.add_space(6.0);
        ui.label(egui::RichText::new("添加一块屏").strong());
        ui.horizontal(|ui| {
            ui.label("类型");
            for (kind, label) in [
                (snug_core::DeviceKind::Phone, "手机"),
                (snug_core::DeviceKind::Tablet, "平板"),
                (snug_core::DeviceKind::Computer, "电脑"),
            ] {
                ui.selectable_value(&mut self.new_screen_kind, kind, label);
            }
        });
        ui.horizontal(|ui| {
            ui.label("方向");
            for (orientation, label) in [
                (snug_core::ScreenOrientation::Portrait, "竖屏"),
                (snug_core::ScreenOrientation::Landscape, "横屏"),
            ] {
                ui.selectable_value(&mut self.new_screen_orientation, orientation, label);
            }
        });
        // 电脑屏固定横屏（C# `ScreenConfig.Sanitize` 也是这么兜的）。
        if self.new_screen_kind == snug_core::DeviceKind::Computer {
            self.new_screen_orientation = snug_core::ScreenOrientation::Landscape;
        }

        if self.catalog.is_some() {
            let mut selected = self.new_screen_model.clone();
            let selected_label = selected
                .as_ref()
                .map(|id| {
                    self.catalog
                        .as_ref()
                        .and_then(|catalog| catalog.find_model(id))
                        .map(|model| format!("{} {}", model.brand, model.name))
                        .unwrap_or_else(|| id.clone())
                })
                .unwrap_or_else(|| "（选择机型）".to_string());
            egui::ComboBox::from_id_salt("snug-new-screen-model")
                .selected_text(selected_label)
                .width(280.0)
                .show_ui(ui, |ui| {
                    if let Some(catalog) = &self.catalog {
                        for model in catalog.models() {
                            let id = model.id();
                            let label = format!(
                                "{} {}（{} × {}）",
                                model.brand, model.name, model.screen.width, model.screen.height
                            );
                            ui.selectable_value(&mut selected, Some(id), label);
                        }
                    }
                });
            self.new_screen_model = selected;
        }

        let can_add = screens.len() < ScreenConfig::LIMIT && self.new_screen_model.is_some();
        if ui
            .add_enabled(can_add, egui::Button::new("选型号并添加"))
            .on_disabled_hover_text(if screens.len() >= ScreenConfig::LIMIT {
                "已经到上限 4 块"
            } else {
                "先选一个机型"
            })
            .clicked()
        {
            if let (Some(model_id), Some(catalog)) = (&self.new_screen_model, &self.catalog)
                && let Some(model) = catalog.find_model(model_id)
            {
                let mut device = PhoneChoice::from_model(model);
                device.kind = self.new_screen_kind;
                let screen = ScreenConfig {
                    // id 留空，`sanitize()` 会补一个 GUID（C# `ScreenConfig.Sanitize` 同）。
                    id: String::new(),
                    orientation: self.new_screen_orientation,
                    device: device.clone(),
                    enabled: true,
                    name: ScreenConfig::suggested_name(&device, self.new_screen_orientation),
                }
                .sanitize();
                screens.push(screen);
                changed = true;
            }
        }

        // ---- 机型选择器（C# `PhoneModelWindow` 的内嵌版）----
        if let Some(index) = self.model_picker {
            if index >= screens.len() {
                self.model_picker = None;
            } else {
                let kind = screens[index].device.kind;
                let mut chosen: Option<PhoneChoice> = None;
                let mut cancel = false;
                Self::card(ui, "选择机型", |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.model_search)
                                .hint_text("🔍 搜索品牌 / 型号")
                                .desired_width(f32::INFINITY),
                        );
                        if ui.button("取消").clicked() {
                            cancel = true;
                        }
                    });
                    let needle = self.model_search.trim().to_lowercase();
                    egui::ScrollArea::vertical()
                        .id_salt("snug-model-picker")
                        .max_height(260.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            let Some(catalog) = self.catalog.as_ref() else {
                                ui.label(egui::RichText::new("没有加载到机型库").weak());
                                return;
                            };
                            let mut shown = 0usize;
                            for model in catalog.models() {
                                if model.kind != kind {
                                    continue;
                                }
                                let haystack =
                                    format!("{} {}", model.brand, model.name).to_lowercase();
                                if !needle.is_empty() && !haystack.contains(&needle) {
                                    continue;
                                }
                                shown += 1;
                                let label = format!(
                                    "{} {}（{} × {}）",
                                    model.brand, model.name, model.screen.width, model.screen.height
                                );
                                if ui.selectable_label(false, label).clicked() {
                                    chosen = Some(PhoneChoice::from_model(model));
                                }
                            }
                            if shown == 0 {
                                ui.label(egui::RichText::new("没有匹配的机型").weak());
                            }
                        });
                });
                if let Some(mut device) = chosen {
                    device.kind = kind;
                    let orientation = screens[index].orientation;
                    screens[index].name =
                        ScreenConfig::suggested_name(&device, orientation);
                    screens[index].device = device;
                    changed = true;
                    self.model_picker = None;
                }
                if cancel {
                    self.model_picker = None;
                }
                ui.add_space(6.0);
            }
        }

        if changed {
            cfg.screens = Some(screens.clone());
            // 旧字段跟随主手机屏，保持 `effective_screens()` 与保存出来的两套一致。
            if let Some(primary) = screens.iter().find(|s| s.id == ScreenConfig::LEGACY_PHONE_ID)
                .or_else(|| screens.first())
            {
                cfg.phone = primary.device.clone();
                cfg.phone_screen = primary.enabled;
            }
        }

        self.page_footer(ui, cfg);
    }

    fn page_gestures(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        if Self::labeled_checkbox(
            ui,
            &mut cfg.gestures,
            "在手机屏内开启原生触控手势翻译",
            "将手机端划动、长按自动翻译为桌面滚轮、选字拖拽与右键交互",
        ) {
            self.actions.push(SettingsAction::ApplyGestureSettings);
        }
        ui.separator();
        ui.add_space(8.0);

        // C# 在总开关关闭时把这四项 IsEnabled=false，这里用 add_enabled 等价处理。
        let enabled = cfg.gestures;
        if Self::labeled_checkbox_enabled(
            ui,
            enabled,
            &mut cfg.gesture_scroll,
            "单指滑动转为平滑滚动与 60fps 惯性",
            "长按 0.5 秒后拖动进入划选模式，浏览器标签栏保持原生点击直通",
        ) {
            self.actions.push(SettingsAction::ApplyGestureSettings);
        }
        if Self::labeled_checkbox_enabled(
            ui,
            enabled,
            &mut cfg.copy_after_selection,
            "划选松手后弹出悬浮操作条（复制 / 粘贴 / 删除）",
            "终端自动安全发送 Ctrl+Shift+C / V，删除发送 Backspace",
        ) {
            self.actions.push(SettingsAction::ApplyGestureSettings);
        }
        if Self::labeled_checkbox_enabled(
            ui,
            enabled,
            &mut cfg.gesture_long_press_right_click,
            "原地长按松开触发鼠标右键",
            "如果在 PowerShell / 终端内习惯避免右键粘贴，可单独关闭此项",
        ) {
            self.actions.push(SettingsAction::ApplyGestureSettings);
        }
        if Self::labeled_checkbox_enabled(
            ui,
            enabled,
            &mut cfg.gestures_injected_only,
            "仅接管 UU 注入触控事件",
            "保留本机物理鼠标跨入手机屏时的原生点击与拖拽操作",
        ) {
            self.actions.push(SettingsAction::ApplyGestureSettings);
        }

        self.page_footer(ui, cfg);
    }

    fn page_diagnostics(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        ui.label(egui::RichText::new("virtual-display-rs 0.3.1 驱动通道").strong());
        ui.label(egui::RichText::new(pipe_text(self.status.pipe_available)).small().weak());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("安装 / 修复驱动").clicked() {
                self.actions.push(SettingsAction::InstallDriver);
            }
            if ui.button("打开日志目录").clicked() {
                self.actions.push(SettingsAction::OpenLogDirectory);
            }
        });
        ui.add_space(10.0);

        // 只读文本框：可以选中复制，但不能编辑。
        ui.add(
            egui::TextEdit::multiline(&mut self.logs)
                .code_editor()
                .desired_rows(22)
                .desired_width(f32::INFINITY)
                .interactive(false),
        );

        self.page_footer(ui, cfg);
    }

    fn page_about(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        ui.label(
            egui::RichText::new(version_line(&self.status.current_version, self.status.self_contained))
                .size(15.0)
                .strong(),
        );
        ui.label(
            egui::RichText::new(self.update.status_text(&self.status.current_version))
                .small()
                .weak(),
        );
        if let Some(progress) = self.update.progress() {
            ui.add(egui::ProgressBar::new(progress).desired_width(320.0));
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.update.check_enabled(), egui::Button::new("检查更新"))
                .clicked()
            {
                self.actions.push(SettingsAction::CheckUpdate);
            }
            if ui
                .add_enabled(self.update.install_enabled(), egui::Button::new("下载并更新重启"))
                .clicked()
            {
                self.actions.push(SettingsAction::InstallUpdate);
            }
        });

        ui.add_space(12.0);
        if Self::labeled_checkbox(
            ui,
            &mut cfg.auto_check_updates,
            "启动时自动检查新版本",
            "自动下载前会先校对 SHA-256 校验和，确保更新包完整安全",
        ) {
            ui.ctx().request_repaint();
        }

        ui.separator();
        ui.label(egui::RichText::new("开源信息").strong());
        ui.label(
            egui::RichText::new(
                "• 本项目基于 MIT License 开源（nicocart/snug-win），1:1 承接上游 zephyr4123/snug（v2.5.0）交互设计",
            )
            .small()
            .weak(),
        );
        ui.label(
            egui::RichText::new("• 虚拟显示器驱动采用 MolotovCherry/virtual-display-rs（0.3.1）")
                .small()
                .weak(),
        );
        ui.add_space(6.0);
        if ui.button("访问 GitHub 仓库主页").clicked() {
            self.actions.push(SettingsAction::OpenRepository);
        }

        self.page_footer(ui, cfg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standard() -> AppConfig {
        AppConfig::default()
    }

    #[test]
    fn zoom_index_and_label_clamp() {
        for steps in 0..=4 {
            assert_eq!(zoom_index(steps) as i32, steps);
        }
        assert_eq!(zoom_index(-3), 0);
        assert_eq!(zoom_index(99), 4);
        assert_eq!(zoom_label(0), "0 级（默认）");
        assert_eq!(zoom_label(9), "+4 级");
    }

    #[test]
    fn model_text_falls_back_to_custom() {
        let choice = PhoneChoice {
            model: None,
            screen: snug_core::PhoneScreenSpec::new(1080, 2400, None),
            kind: snug_core::DeviceKind::Phone,
        };
        assert_eq!(model_text(&choice), "自定义（1080 × 2400）");
        assert!(model_text(&PhoneChoice::standard()).starts_with("Apple iPhone 17 Pro Max（1320 × 2868"));
    }

    #[test]
    fn version_line_mentions_package_kind() {
        assert_eq!(version_line("1.2.0", true), "当前版本：v1.2.0（自包含版）");
        assert_eq!(version_line("1.2.0", false), "当前版本：v1.2.0（框架依赖版）");
    }

    #[test]
    fn pipe_and_elevation_text_match_csharp() {
        assert_eq!(pipe_text(true), r"已连接命名管道 \\.\pipe\virtualdisplaydriver");
        assert_eq!(pipe_text(false), r"未检测到命名管道 \\.\pipe\virtualdisplaydriver");
        assert_eq!(elevation_text(true), "管理员权限运行中");
        assert_eq!(elevation_text(false), "当前权限：普通用户");
    }

    #[test]
    fn tabs_map_to_csharp_order() {
        assert_eq!(SettingsTab::from_index(0), SettingsTab::Home);
        assert_eq!(SettingsTab::from_index(3), SettingsTab::About);
        assert_eq!(SettingsTab::from_index(4), SettingsTab::Screens);
        assert_eq!(SettingsTab::from_index(99), SettingsTab::Home);
        for (index, tab) in SettingsTab::ALL.iter().enumerate() {
            assert_eq!(tab.index(), index);
        }
    }

    #[test]
    fn theme_change_only_recolors() {
        let before = standard();
        let mut after = before.clone();
        after.theme = AppThemeMode::Dark;
        assert_eq!(classify_change(&before, &after), ChangeImpact::Theme);
        // 换主题不该弹"需要重来"的提示。
        assert_eq!(change_hint(ChangeImpact::Theme), None);
    }

    #[test]
    fn screen_list_change_is_reported() {
        let before = standard();
        let mut after = before.clone();
        let mut screens = after.effective_screens();
        screens[0].enabled = false;
        after.screens = Some(screens);
        assert_eq!(classify_change(&before, &after), ChangeImpact::Screens);
        assert!(change_hint(ChangeImpact::Screens).is_some());
    }

    #[test]
    fn spec_line_follows_screen_fit_dpi() {
        // 标准 iPhone：200%（虚拟屏按 2 倍下发）。
        let standard_phone = PhoneChoice::standard();
        assert!(spec_text(&standard_phone).ends_with("200%"));
    }

    #[test]
    fn phone_screen_and_updates_need_no_restart() {        let before = standard();
        let mut after = before.clone();
        after.phone_screen = false;
        after.auto_check_updates = false;
        assert_eq!(classify_change(&before, &after), ChangeImpact::None);
        assert_eq!(change_hint(ChangeImpact::None), None);
    }

    #[test]
    fn screenshot_only_refreshes_the_home_bar() {
        let before = standard();
        let mut after = before.clone();
        after.screenshot = true;
        assert_eq!(classify_change(&before, &after), ChangeImpact::RefreshHomeBar);
        assert!(change_hint(ChangeImpact::RefreshHomeBar).is_some());
    }

    #[test]
    fn gesture_changes_reload_the_hook() {
        for mutate in [
            (|cfg: &mut AppConfig| cfg.gestures = !cfg.gestures) as fn(&mut AppConfig),
            |cfg: &mut AppConfig| cfg.gesture_scroll = !cfg.gesture_scroll,
            |cfg: &mut AppConfig| cfg.copy_after_selection = !cfg.copy_after_selection,
            |cfg: &mut AppConfig| cfg.gesture_long_press_right_click = !cfg.gesture_long_press_right_click,
            |cfg: &mut AppConfig| cfg.gestures_injected_only = !cfg.gestures_injected_only,
        ] {
            let before = standard();
            let mut after = before.clone();
            mutate(&mut after);
            assert_eq!(classify_change(&before, &after), ChangeImpact::ReloadGestures);
        }
    }

    #[test]
    fn model_or_zoom_rebuilds_the_phone_screen() {
        let before = standard();

        let mut by_model = before.clone();
        by_model.phone = PhoneChoice {
            model: Some("Xiaomi 15".to_string()),
            screen: snug_core::PhoneScreenSpec::new(1200, 2400, Some(400)),
            kind: snug_core::DeviceKind::Phone,
        };
        assert_eq!(classify_change(&before, &by_model), ChangeImpact::RebuildPhoneScreen);

        let mut by_zoom = before.clone();
        by_zoom.zoom_steps = 4;
        assert_eq!(classify_change(&before, &by_zoom), ChangeImpact::RebuildPhoneScreen);
    }

    #[test]
    fn rebuild_outranks_gestures_which_outranks_home_bar() {
        let before = standard();
        let mut after = before.clone();
        after.screenshot = true;
        after.gestures = false;
        after.zoom_steps = 3;
        assert_eq!(classify_change(&before, &after), ChangeImpact::RebuildPhoneScreen);
    }

    #[test]
    fn both_automations_off_is_only_a_hint() {
        let mut cfg = standard();
        cfg.auto_check_updates = false;
        assert!(both_off_hint(&cfg, false).is_some());
        assert!(both_off_hint(&cfg, true).is_none());
        cfg.auto_check_updates = true;
        assert!(both_off_hint(&cfg, false).is_none());
    }

    #[test]
    fn update_status_text_matches_csharp_wording() {
        assert_eq!(
            UpdateUiState::Idle.status_text("1.2.0"),
            "点击右侧按钮检查 GitHub 最新正式版"
        );
        assert_eq!(
            UpdateUiState::UpToDate.status_text("1.2.0"),
            "已是最新版本（v1.2.0）"
        );
        assert_eq!(
            UpdateUiState::Available { version: "1.3.0".into(), downloadable: true }
                .status_text("1.2.0"),
            "发现新版本 v1.3.0（点击可直接下载、校验 SHA-256 并重启升级）"
        );
        assert_eq!(
            UpdateUiState::Downloading { version: "1.3.0".into(), progress: 0.42 }
                .status_text("1.2.0"),
            "正在下载 v1.3.0... 42%"
        );
        assert_eq!(
            UpdateUiState::CheckFailed("network".into()).status_text("1.2.0"),
            "检查更新失败：network"
        );
        assert_eq!(
            UpdateUiState::InstallFailed { version: "1.3.0".into(), message: "checksum".into() }
                .status_text("1.2.0"),
            "更新失败：checksum"
        );
    }

    #[test]
    fn update_buttons_follow_the_state() {
        assert!(UpdateUiState::Available { version: "1.3.0".into(), downloadable: true }.install_enabled());
        assert!(!UpdateUiState::Available { version: "1.3.0".into(), downloadable: false }.install_enabled());
        assert!(UpdateUiState::InstallFailed { version: "1.3.0".into(), message: "x".into() }.install_enabled());
        assert!(!UpdateUiState::Idle.install_enabled());

        assert!(!UpdateUiState::Checking.check_enabled());
        assert!(!UpdateUiState::Downloading { version: "1.3.0".into(), progress: 0.5 }.check_enabled());
        assert!(!UpdateUiState::Ready.check_enabled());
        assert!(UpdateUiState::Idle.check_enabled());
        assert!(UpdateUiState::UpToDate.check_enabled());
    }

    #[test]
    fn update_dot_only_for_available_release() {
        assert!(UpdateUiState::Available { version: "1.3.0".into(), downloadable: true }.has_update_dot());
        assert!(!UpdateUiState::UpToDate.has_update_dot());
        assert_eq!(
            UpdateUiState::Downloading { version: "1.3.0".into(), progress: 1.5 }.progress(),
            Some(1.0)
        );
        assert_eq!(UpdateUiState::Idle.progress(), None);
    }

    #[test]
    fn actions_are_drained_once() {
        let mut window = SettingsWindow::new();
        window.actions.push(SettingsAction::CheckUpdate);
        window.actions.push(SettingsAction::OpenLogDirectory);
        assert_eq!(
            window.take_actions(),
            vec![SettingsAction::CheckUpdate, SettingsAction::OpenLogDirectory]
        );
        assert!(window.take_actions().is_empty());
    }

    #[test]
    fn open_close_state_machine() {
        let mut window = SettingsWindow::new();
        assert!(!window.is_open());
        window.open();
        assert!(window.is_open());
        window.select_tab(SettingsTab::Diagnostics);
        assert_eq!(window.tab(), SettingsTab::Diagnostics);
    }

    #[test]
    fn append_log_keeps_one_line_per_entry() {
        let mut window = SettingsWindow::new();
        window.append_log("a");
        window.append_log("b");
        assert_eq!(window.logs, "a\nb\n");
        window.set_logs("全部换掉");
        assert_eq!(window.logs, "全部换掉");
    }

    #[test]
    fn renders_one_frame_and_leaves_config_untouched() {
        // egui 不需要真窗口：默认 Context 跑一帧就够了。界面在这一帧里只画控件，
        // 没有鼠标交互，所以配置不会被改动、也不该排队任何动作。
        let ctx = egui::Context::default();
        let mut window = SettingsWindow::new();
        window.open();
        let mut cfg = standard();
        let before = cfg.clone();
        let mut outcome = SettingsOutcome::Idle;
        let mut actions = Vec::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            outcome = window.show(ctx, &mut cfg);
            actions = window.take_actions();
        });
        assert_eq!(outcome, SettingsOutcome::Idle);
        assert_eq!(cfg, before);
        assert!(actions.is_empty(), "无交互时不该有动作：{actions:?}");
        assert!(window.is_open());
    }
}
