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
//! 设置分为屏幕、交互、通用三类。每屏编辑进入详情；驱动诊断和更新折叠在通用页。
//! 旧页签索引仍用于外部入口，进入时路由到对应的新分类。
//! 字段全部来自 `snug_core::AppConfig`（对应 C# `SnugWin.Core.Config.AppConfig`），
//! 本模块不新造配置结构。
//!
//! 手动验证（真机）：
//! 1. 屏幕、交互、通用三类可达，旧驱动与更新入口进入对应折叠段；
//! 2. 改“默认字号放大”后关窗再开，值能被控制器写回 config.json；
//! 3. 关掉手势总开关后，四个子项变灰且不可点；
//! 4. 驱动未就绪时可安装修复，通用中的驱动与日志可以展开；
//! 5. 通用切换浅色/深色/跟随系统，界面配色立刻更新。

use eframe::egui;
use snug_core::config::ScreenResolution;
use snug_core::{AppConfig, AppThemeMode, PhoneChoice, PhoneScreenSpec, ScreenConfig};

fn screen_spec_text(screen: &ScreenConfig) -> String {
    let fit = snug_core::ScreenPlan::fit_of(screen);
    let (width, height) = fit.pixel_size(fit.base_width());
    format!("{width} × {height}")
}

/// 字号放大档位（C# `ZoomCombo` 的 0~4），与 core 的 `MIN_ZOOM_STEPS`/`MAX_ZOOM_STEPS` 一致。
pub const ZOOM_LABELS: [&str; 5] = ["0 级（默认）", "+1 级", "+2 级", "+3 级", "+4 级"];

/// 保留旧五种入口及索引；界面只呈现屏幕、交互、通用三类。
/// `Home` 为通用，`Diagnostics` / `About` 进入通用的对应折叠段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Home,
    Gestures,
    Diagnostics,
    About,
    Screens,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 3] = [
        SettingsTab::Screens,
        SettingsTab::Gestures,
        SettingsTab::Home,
    ];

    /// 左侧导航文字。
    pub fn nav_title(self) -> &'static str {
        match self {
            SettingsTab::Home => "通用",
            SettingsTab::Gestures => "交互",
            SettingsTab::Diagnostics => "驱动与日志",
            SettingsTab::About => "关于与更新",
            SettingsTab::Screens => "屏幕",
        }
    }

    /// 页面标题（对应 C# 子页顶部标题；首页是品牌行，用程序名）。
    pub fn page_title(self) -> &'static str {
        match self {
            SettingsTab::Home => "通用",
            SettingsTab::Gestures => "交互",
            SettingsTab::Diagnostics => "驱动状态与实时日志",
            SettingsTab::About => "关于与版本更新",
            SettingsTab::Screens => "屏幕",
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
        match index {
            1 => Self::Gestures,
            2 => Self::Diagnostics,
            3 => Self::About,
            4 => Self::Screens,
            _ => Self::Home,
        }
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
    /// 用户明确确认后收起本次手机屏，不关闭应用或修改自动开屏配置。
    DismissScreen,
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
            UpdateUiState::Available { version, .. } => {
                format!("发现新版本 v{version}（点击可直接下载、校验 SHA-256 并重启升级）")
            }
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
        ChangeImpact::RebuildPhoneScreen => Some("显示与字号设置已保存。"),
        ChangeImpact::Theme => None,
        ChangeImpact::Screens => Some("屏幕设置已保存。切换应用不会更改分辨率。"),
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

/// 仅在用户点应用时写入配置的分辨率草稿。
const RESOLUTION_PRESETS: &[(i32, i32, &str)] = &[
    (720, 1280, "720 × 1280 · 轻量竖屏"),
    (900, 1600, "900 × 1600 · 均衡竖屏"),
    (1080, 1920, "1080 × 1920 · 默认"),
    (1080, 2400, "1080 × 2400 · 长屏"),
    (1440, 2560, "1440 × 2560 · 高清竖屏"),
    (1280, 720, "1280 × 720 · 轻量横屏"),
    (1920, 1080, "1920 × 1080 · 全高清横屏"),
    (1200, 1920, "1200 × 1920 · 平板竖屏"),
    (1536, 2048, "1536 × 2048 · 平板 4:3"),
    (1600, 1200, "1600 × 1200 · 平板横屏"),
];

#[derive(Clone)]
struct ResolutionDraft {
    width: String,
    height: String,
    custom: bool,
}

impl ResolutionDraft {
    fn from_screen(screen: &ScreenConfig) -> Self {
        let fit = snug_core::ScreenPlan::fit_of(screen);
        let (width, height) = fit.pixel_size(fit.base_width());
        Self::from_resolution(ScreenResolution { width, height })
    }

    fn from_resolution(resolution: ScreenResolution) -> Self {
        Self {
            width: resolution.width.to_string(),
            height: resolution.height.to_string(),
            custom: !RESOLUTION_PRESETS
                .iter()
                .any(|&(width, height, _)| resolution == ScreenResolution { width, height }),
        }
    }

    fn select_preset(&mut self, index: usize) {
        let (width, height, _) = RESOLUTION_PRESETS[index];
        *self = Self::from_resolution(ScreenResolution { width, height });
    }

    fn label(&self) -> &'static str {
        if self.custom { return "自定义"; }
        RESOLUTION_PRESETS.iter().find_map(|&(width, height, label)| {
            (self.resolution() == Some(ScreenResolution { width, height })).then_some(label)
        }).unwrap_or("自定义")
    }

    fn resolution(&self) -> Option<ScreenResolution> {
        let resolution = ScreenResolution {
            width: self.width.trim().parse().ok()?,
            height: self.height.trim().parse().ok()?,
        };
        resolution.is_valid().then_some(resolution)
    }

    fn apply(&self, screen: &mut ScreenConfig) -> bool {
        let Some(resolution) = self.resolution() else {
            return false;
        };
        if screen.resolution == Some(resolution) {
            return false;
        }
        screen.resolution = Some(resolution);
        true
    }
}

fn resolution_fields(ui: &mut egui::Ui, draft: &mut ResolutionDraft, id: &str) {
    ui.label("分辨率（物理像素）");
    egui::ComboBox::from_id_salt(("resolution-preset", id))
        .selected_text(draft.label())
        .width(ui.available_width())
        .truncate()
        .show_ui(ui, |ui| {
            for (index, &(_, _, label)) in RESOLUTION_PRESETS.iter().enumerate() {
                if ui
                    .add_sized([ui.available_width(), 48.0], egui::Button::new(label))
                    .clicked()
                {
                    draft.select_preset(index);
                }
            }
            if ui
                .add_sized([ui.available_width(), 48.0], egui::Button::new("自定义"))
                .clicked()
            {
                draft.custom = true;
            }
        });
    if draft.custom {
        let field_width = ((ui.available_width() - ui.spacing().item_spacing.x) / 2.0).max(48.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label("宽");
                ui.add_sized(
                    [field_width, 48.0],
                    egui::TextEdit::singleline(&mut draft.width),
                );
            });
            ui.vertical(|ui| {
                ui.label("高");
                ui.add_sized(
                    [field_width, 48.0],
                    egui::TextEdit::singleline(&mut draft.height),
                );
            });
        });
        if draft.resolution().is_none() {
            ui.label("宽高需为 320–8192 之间的偶数。");
        }
    }
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
    /// 待执行动作队列。
    actions: Vec<SettingsAction>,
    /// 打开窗口时的配置快照，用来算“改了什么”。
    baseline: Option<AppConfig>,
    /// 最近一次改动的提示，换页/关窗后清空。
    last_hint: Option<&'static str>,
    new_screen_kind: snug_core::DeviceKind,
    new_screen_name: String,
    new_screen_resolution: ResolutionDraft,
    resolution_drafts: std::collections::HashMap<String, ResolutionDraft>,
    screen_detail: Option<String>,
    screen_name_draft: String,
    adding_screen: bool,
    confirm_delete: bool,
    show_updates: bool,
    show_diagnostics: bool,
    reset_scroll: bool,
    /// 只在设置页内确认本次收屏，避免与自动开屏总开关混淆。
    confirm_dismiss: bool,
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
            tab: SettingsTab::Screens,
            status: StatusInfo::default(),
            update: UpdateUiState::Idle,
            autostart: false,
            manual_override: false,
            logs: String::new(),
            actions: Vec::new(),
            baseline: None,
            last_hint: None,
            new_screen_kind: snug_core::DeviceKind::Phone,
            new_screen_name: String::new(),
            new_screen_resolution: ResolutionDraft::from_resolution(ScreenResolution::DEFAULT),
            resolution_drafts: std::collections::HashMap::new(),
            screen_detail: None,
            screen_name_draft: String::new(),
            adding_screen: false,
            confirm_delete: false,
            show_updates: false,
            show_diagnostics: false,
            reset_scroll: true,
            confirm_dismiss: false,
        }
    }

    /// 打开窗口（C# 里是 `new SettingsWindow(...)` + `Show()`）。
    pub fn open(&mut self) {
        self.open = true;
        self.baseline = None;
        self.last_hint = None;
        self.confirm_dismiss = false;
        self.confirm_delete = false;
        self.screen_detail = None;
        self.adding_screen = false;
        self.reset_scroll = true;
    }

    /// 用代码切页（C# `SelectTab`）。
    pub fn select_tab(&mut self, tab: SettingsTab) {
        self.tab = match tab {
            SettingsTab::Diagnostics => {
                self.show_diagnostics = true;
                self.show_updates = false;
                SettingsTab::Home
            }
            SettingsTab::About => {
                self.show_updates = true;
                self.show_diagnostics = false;
                SettingsTab::Home
            }
            _ => tab,
        };
        self.screen_detail = None;
        self.adding_screen = false;
        self.confirm_delete = false;
        self.confirm_dismiss = false;
        self.last_hint = None;
        self.reset_scroll = true;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    #[cfg(test)]
    pub fn tab(&self) -> SettingsTab {
        self.tab
    }

    pub fn needs_logs(&self) -> bool {
        self.open && self.tab == SettingsTab::Home && self.show_diagnostics
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

    /// 设置画在当前手机屏窗口里。窄屏占满页面，宽屏保留桌面设置窗。
    /// `cfg` 是本模块唯一会改的外部状态；调用方在设置打开时不再绘制资源库。
    pub fn show(&mut self, ctx: &egui::Context, cfg: &mut AppConfig) -> SettingsOutcome {
        if !self.open {
            return SettingsOutcome::Idle;
        }
        let before = cfg.clone();
        if self.baseline.is_none() {
            self.baseline = Some(before.clone());
        }

        if ctx.screen_rect().width() < 720.0 {
            self.show_mobile(ctx, cfg);
        } else {
            let mut titlebar_open = self.open;
            let mut back_clicked = false;
            egui::Window::new("Snug 设置")
                .open(&mut titlebar_open)
                .collapsible(false)
                .resizable(true)
                .default_size([680.0, 560.0])
                .min_size([560.0, 420.0])
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(self.tab.page_title())
                                .size(19.0)
                                .strong(),
                        );
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
        }

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

    fn show_mobile(&mut self, ctx: &egui::Context, cfg: &mut AppConfig) {
        let palette = crate::theme::palette(ctx.style().visuals.dark_mode);
        egui::TopBottomPanel::top("snug-mobile-settings-header")
            .frame(
                egui::Frame::default()
                    .fill(palette.bg)
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add_sized([96.0, 48.0], egui::Button::new("‹ 应用"))
                        .clicked()
                    {
                        self.open = false;
                    }
                    ui.label(egui::RichText::new("设置").size(22.0).strong());
                });
                self.tab_bar(ui);
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(palette.bg).inner_margin(16.0))
            .show(ctx, |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                ui.spacing_mut().interact_size.y = ui.spacing().interact_size.y.max(48.0);
                self.scroll_page(ui, cfg);
            });
    }

    fn layout(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        self.tab_bar(ui);
        ui.add_space(8.0);
        self.scroll_page(ui, cfg);
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let width = ((ui.available_width() - ui.spacing().item_spacing.x * 2.0) / 3.0).max(48.0);
        ui.horizontal(|ui| {
            for tab in SettingsTab::ALL {
                let text = if tab == SettingsTab::Home && self.update.has_update_dot() {
                    egui::RichText::new(tab.nav_title())
                        .color(crate::theme::palette(ui.visuals().dark_mode).accent)
                } else {
                    egui::RichText::new(tab.nav_title())
                };
                if ui
                    .add_sized(
                        [width, 48.0],
                        egui::Button::new(text).selected(self.tab == tab),
                    )
                    .clicked()
                {
                    self.select_tab(tab);
                }
            }
        });
    }

    fn scroll_page(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt((
                "snug-settings-page",
                self.tab.index(),
                self.screen_detail.clone(),
                self.adding_screen,
            ))
            .auto_shrink([false, false]);
        if std::mem::take(&mut self.reset_scroll) {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        scroll.show(ui, |ui| {
            ui.set_max_width(ui.available_width());
            match self.tab {
                SettingsTab::Screens => self.page_screens(ui, cfg),
                SettingsTab::Gestures => self.page_gestures(ui, cfg),
                SettingsTab::Home | SettingsTab::Diagnostics | SettingsTab::About => {
                    self.page_home(ui, cfg)
                }
            }
        });
    }

    /// 一页的公共收尾：影响提示 + 自动化都关掉的提醒。
    fn page_footer(&mut self, ui: &mut egui::Ui, cfg: &AppConfig) {
        let palette = crate::theme::palette(ui.visuals().dark_mode);
        if let Some(hint) = self.last_hint {
            ui.add_space(10.0);
            ui.label(egui::RichText::new(hint).small().color(palette.accent));
        }
        if let Some(hint) = both_off_hint(cfg, self.autostart) {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(hint).small().color(palette.warn));
        }
    }

    /// 带说明的两行勾选框：标题一行、灰色说明一行。
    fn labeled_checkbox(
        ui: &mut egui::Ui,
        checked: &mut bool,
        title: &str,
        description: &str,
    ) -> bool {
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
    fn card<R>(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
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

    /// 通用：常用设置在前，更新与驱动诊断按需展开。
    fn page_home(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        Self::card(ui, "外观", |ui| {
            ui.horizontal_wrapped(|ui| {
                for (mode, label) in [
                    (AppThemeMode::Light, "浅色"),
                    (AppThemeMode::Dark, "深色"),
                    (AppThemeMode::Auto, "跟随系统"),
                ] {
                    if ui
                        .add(
                            egui::Button::new(label)
                                .selected(cfg.theme == mode)
                                .min_size(egui::vec2(64.0, 48.0)),
                        )
                        .clicked()
                    {
                        cfg.theme = mode;
                    }
                }
            });
        });
        ui.add_space(8.0);
        Self::card(ui, "启动与字号", |ui| {
            if Self::labeled_checkbox(
                ui,
                &mut self.autostart,
                "登录后后台启动",
                "以普通权限常驻托盘",
            ) {
                self.actions
                    .push(SettingsAction::SetAutoStart(self.autostart));
            }
            ui.label("应用字号");
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        cfg.zoom_steps > 0,
                        egui::Button::new("−").min_size(egui::vec2(48.0, 48.0)),
                    )
                    .clicked()
                {
                    cfg.zoom_steps -= 1;
                }
                ui.label(zoom_label(cfg.zoom_steps));
                if ui
                    .add_enabled(
                        cfg.zoom_steps < 4,
                        egui::Button::new("+").min_size(egui::vec2(48.0, 48.0)),
                    )
                    .clicked()
                {
                    cfg.zoom_steps += 1;
                }
            });
            ui.label(
                egui::RichText::new("对支持缩放的手机、平板应用窗口生效")
                    .small()
                    .weak(),
            );
        });
        ui.add_space(8.0);
        if ui
            .add_sized(
                [ui.available_width(), 48.0],
                egui::Button::new("更新与关于").selected(self.show_updates),
            )
            .clicked()
        {
            self.show_updates = !self.show_updates;
        }
        if self.show_updates {
            Self::card(ui, "软件更新", |ui| {
                self.page_about(ui, cfg);
            });
        }
        ui.add_space(8.0);
        if ui
            .add_sized(
                [ui.available_width(), 48.0],
                egui::Button::new("驱动与日志").selected(self.show_diagnostics),
            )
            .clicked()
        {
            self.show_diagnostics = !self.show_diagnostics;
        }
        if self.show_diagnostics {
            Self::card(ui, "驱动诊断", |ui| {
                self.page_diagnostics(ui);
            });
        }
        self.page_footer(ui, cfg);
    }

    /// 屏幕列表只显示摘要，编辑和添加分别进入自己的表单。
    fn page_screens(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        if self.adding_screen {
            self.page_add_screen(ui, cfg);
            return;
        }
        if let Some(id) = self.screen_detail.clone() {
            self.page_screen_detail(ui, cfg, &id);
            return;
        }
        let palette = crate::theme::palette(ui.visuals().dark_mode);
        ui.label(
            egui::RichText::new(&self.status.status_summary)
                .strong()
                .color(palette.accent),
        );
        ui.add_space(8.0);
        if !self.status.pipe_available {
            ui.label("虚拟显示器驱动未就绪");
            if ui.button("安装 / 修复驱动").clicked() {
                self.actions.push(SettingsAction::InstallDriver);
            }
            ui.add_space(8.0);
        }
        let screens = cfg.effective_screens();
        if screens.is_empty() {
            ui.label("还没有屏幕");
        }
        for screen in &screens {
            egui::Frame::group(ui.style())
                .inner_margin(10.0)
                .show(ui, |ui| {
                    let text_width =
                        (ui.available_width() - 72.0 - ui.spacing().item_spacing.x).max(40.0);
                    ui.horizontal(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(text_width, 56.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                ui.add_sized(
                                    [text_width, 24.0],
                                    egui::Label::new(egui::RichText::new(&screen.name).strong())
                                        .truncate(),
                                );
                                let summary = format!(
                                    "{} · {} · {}",
                                    screen.device.kind.display_name(),
                                    screen_spec_text(screen),
                                    if screen.enabled {
                                        "已启用"
                                    } else {
                                        "已关闭"
                                    }
                                );
                                ui.add_sized(
                                    [text_width, 20.0],
                                    egui::Label::new(egui::RichText::new(summary).small().weak())
                                        .truncate(),
                                );
                            },
                        );
                        if ui
                            .add_sized([72.0, 48.0], egui::Button::new("详情"))
                            .clicked()
                        {
                            self.screen_detail = Some(screen.id.clone());
                            self.screen_name_draft = screen.name.clone();
                            self.confirm_delete = false;
                        }
                    });
                });
            ui.add_space(6.0);
        }
        if ui
            .add_enabled(
                screens.len() < ScreenConfig::LIMIT,
                egui::Button::new("添加屏幕").min_size(egui::vec2(ui.available_width(), 48.0)),
            )
            .clicked()
        {
            self.adding_screen = true;
        }
        ui.label(
            egui::RichText::new(format!(
                "最多 {} 块，同一时间使用当前选中的一块。",
                ScreenConfig::LIMIT
            ))
            .small()
            .weak(),
        );
        ui.add_space(8.0);
        if Self::labeled_checkbox(
            ui,
            &mut cfg.phone_screen,
            "连接后自动开屏",
            "断开后还原窗口并撤屏",
        ) {
            self.actions
                .push(SettingsAction::SetPhoneScreen(cfg.phone_screen));
        }
        if ui
            .add_sized([ui.available_width(), 48.0], egui::Button::new("预览界面"))
            .clicked()
        {
            self.actions.push(SettingsAction::OpenPreview);
        }
        self.screen_session_actions(ui);
        self.page_footer(ui, cfg);
    }

    fn page_screen_detail(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig, id: &str) {
        if ui
            .add(egui::Button::new("‹ 屏幕列表").min_size(egui::vec2(120.0, 48.0)))
            .clicked()
        {
            self.screen_detail = None;
            self.confirm_delete = false;
            return;
        }
        let Some(mut screen) = cfg
            .effective_screens()
            .into_iter()
            .find(|screen| screen.id == id)
        else {
            self.screen_detail = None;
            self.confirm_delete = false;
            return;
        };
        ui.add(egui::Label::new(egui::RichText::new(&screen.name).size(20.0).strong()).wrap());
        ui.label(
            egui::RichText::new(format!(
                "{} · {}",
                screen.device.kind.display_name(),
                screen_spec_text(&screen)
            ))
            .small()
            .weak(),
        );
        ui.add_space(12.0);
        let mut changed = false;
        ui.label("名字");
        let name_response = ui.add_sized(
            [ui.available_width(), 48.0],
            egui::TextEdit::singleline(&mut self.screen_name_draft),
        );
        let save_name = ui
            .add_enabled(
                !self.screen_name_draft.trim().is_empty(),
                egui::Button::new("保存名字").min_size(egui::vec2(112.0, 48.0)),
            )
            .clicked()
            || (name_response.lost_focus()
                && ui.input(|input| input.key_pressed(egui::Key::Enter)));
        if save_name && !self.screen_name_draft.trim().is_empty() {
            let name = self.screen_name_draft.trim();
            if screen.name != name {
                screen.name = name.to_string();
                changed = true;
            }
        }
        ui.add_space(8.0);
        changed |= ui.checkbox(&mut screen.enabled, "启用这块屏").changed();
        ui.add_space(12.0);
        let draft = self
            .resolution_drafts
            .entry(screen.id.clone())
            .or_insert_with(|| ResolutionDraft::from_screen(&screen));
        resolution_fields(ui, draft, &screen.id);
        ui.label(
            egui::RichText::new("选择预设或编辑宽高后，点击应用才会改屏。")
                .small()
                .weak(),
        );
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    draft.resolution().is_some(),
                    egui::Button::new("应用分辨率").min_size(egui::vec2(120.0, 48.0)),
                )
                .clicked()
            {
                changed |= draft.apply(&mut screen);
            }
            if ui
                .add(egui::Button::new("恢复默认").min_size(egui::vec2(112.0, 48.0)))
                .clicked()
            {
                let fit = snug_core::ScreenPlan::fit_of(&screen);
                let (width, height) = fit.pixel_size(fit.base_width());
                let orientation = if width > height {
                    snug_core::ScreenOrientation::Landscape
                } else {
                    snug_core::ScreenOrientation::Portrait
                };
                *draft =
                    ResolutionDraft::from_resolution(ScreenResolution::default_for(orientation));
            }
        });
        if changed {
            *cfg = cfg.with_screen_updated(id, |_| screen.clone());
        }
        ui.add_space(16.0);
        if self.confirm_delete {
            ui.label(egui::RichText::new("删除这块屏？").strong());
            ui.label("屏幕会撤除，上面的窗口放回原处；应用保持打开。");
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add(egui::Button::new("确认删除").min_size(egui::vec2(112.0, 48.0)))
                    .clicked()
                {
                    *cfg = cfg.with_screen_removed(id);
                    self.resolution_drafts.remove(id);
                    self.screen_detail = None;
                    self.confirm_delete = false;
                }
                if ui
                    .add(egui::Button::new("取消删除").min_size(egui::vec2(112.0, 48.0)))
                    .clicked()
                {
                    self.confirm_delete = false;
                }
            });
        } else if ui
            .add(
                egui::Button::new(
                    egui::RichText::new("删除这块屏")
                        .color(crate::theme::palette(ui.visuals().dark_mode).warn),
                )
                .min_size(egui::vec2(120.0, 48.0)),
            )
            .clicked()
        {
            self.confirm_delete = true;
        }
        self.page_footer(ui, cfg);
    }

    fn page_add_screen(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        if ui
            .add(egui::Button::new("‹ 屏幕列表").min_size(egui::vec2(120.0, 48.0)))
            .clicked()
        {
            self.adding_screen = false;
            return;
        }
        ui.label(egui::RichText::new("添加屏幕").size(20.0).strong());
        ui.add_space(12.0);
        let previous_kind = self.new_screen_kind;
        ui.label("设备");
        ui.horizontal_wrapped(|ui| {
            for (kind, label) in [
                (snug_core::DeviceKind::Phone, "手机"),
                (snug_core::DeviceKind::Tablet, "平板"),
                (snug_core::DeviceKind::Computer, "电脑"),
            ] {
                if ui
                    .add(
                        egui::Button::new(label)
                            .selected(self.new_screen_kind == kind)
                            .min_size(egui::vec2(64.0, 48.0)),
                    )
                    .clicked()
                {
                    self.new_screen_kind = kind;
                }
            }
        });
        if previous_kind != self.new_screen_kind
            && !self.new_screen_resolution.custom
            && self
                .new_screen_resolution
                .resolution()
                .is_some_and(|resolution| {
                    resolution == ScreenResolution::DEFAULT
                        || resolution
                            == ScreenResolution::default_for(
                                snug_core::ScreenOrientation::Landscape,
                            )
                })
        {
            self.new_screen_resolution = ResolutionDraft::from_resolution(
                ScreenResolution::default_for(self.new_screen_kind.usual_orientation()),
            );
        }
        ui.add_space(8.0);
        resolution_fields(ui, &mut self.new_screen_resolution, "new-screen");
        ui.add_space(8.0);
        ui.label("名字（可选）");
        ui.add_sized(
            [ui.available_width(), 48.0],
            egui::TextEdit::singleline(&mut self.new_screen_name).hint_text("例如：手机工作区"),
        );
        ui.add_space(12.0);
        let can_add = cfg.effective_screens().len() < ScreenConfig::LIMIT
            && self.new_screen_resolution.resolution().is_some();
        if ui
            .add_enabled(
                can_add,
                egui::Button::new("确认添加").min_size(egui::vec2(ui.available_width(), 48.0)),
            )
            .clicked()
            && let Some(resolution) = self.new_screen_resolution.resolution()
        {
            let orientation = if resolution.width > resolution.height {
                snug_core::ScreenOrientation::Landscape
            } else {
                snug_core::ScreenOrientation::Portrait
            };
            let mut screens = cfg.effective_screens();
            let name = self.new_screen_name.trim();
            screens.push(
                ScreenConfig {
                    id: String::new(),
                    orientation,
                    device: PhoneChoice {
                        model: None,
                        screen: PhoneScreenSpec::new(resolution.width, resolution.height, None),
                        kind: self.new_screen_kind,
                    },
                    enabled: true,
                    name: if name.is_empty() {
                        format!("屏幕 {}", screens.len() + 1)
                    } else {
                        name.to_string()
                    },
                    resolution: Some(resolution),
                }
                .sanitize(),
            );
            cfg.screens = Some(screens);
            *cfg = cfg.clone().sanitize();
            self.new_screen_name.clear();
            self.adding_screen = false;
        }
    }

    fn screen_session_actions(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        if self.confirm_dismiss {
            ui.label(egui::RichText::new("收起手机屏？").strong());
            ui.label("窗口还原，应用保持打开，可从胶囊或托盘重新进入。");
            if ui
                .add_sized(
                    [ui.available_width(), 48.0],
                    egui::Button::new("收起并还原窗口"),
                )
                .clicked()
            {
                self.actions.push(SettingsAction::DismissScreen);
                self.confirm_dismiss = false;
                self.open = false;
            }
            if ui
                .add_sized([ui.available_width(), 48.0], egui::Button::new("继续使用"))
                .clicked()
            {
                self.confirm_dismiss = false;
            }
        } else if self.status.phone_screen_ready {
            if ui
                .add_sized(
                    [ui.available_width(), 48.0],
                    egui::Button::new("收起手机屏"),
                )
                .clicked()
            {
                self.confirm_dismiss = true;
            }
        } else if self.status.can_reenter
            && ui
                .add_sized(
                    [ui.available_width(), 48.0],
                    egui::Button::new("重新进入手机屏"),
                )
                .clicked()
        {
            self.actions.push(SettingsAction::ReenterScreen);
        }
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

        ui.separator();
        Self::labeled_checkbox(
            ui,
            &mut cfg.screenshot,
            "底栏显示截图按钮",
            "截取当前窗口，可继续裁剪或复制",
        );
        self.page_footer(ui, cfg);
    }

    fn page_diagnostics(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("virtual-display-rs 0.3.1 驱动通道").strong());
        ui.label(
            egui::RichText::new(pipe_text(self.status.pipe_available))
                .small()
                .weak(),
        );
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("安装 / 修复驱动").clicked() {
                self.actions.push(SettingsAction::InstallDriver);
            }
            if ui.button("打开日志目录").clicked() {
                self.actions.push(SettingsAction::OpenLogDirectory);
            }
        });
        ui.collapsing("高级与权限", |ui| {
            ui.label(elevation_text(self.status.elevated));
            if !self.status.elevated && ui.button("以管理员身份重启").clicked() {
                self.actions.push(SettingsAction::RestartElevated);
            }
            if Self::labeled_checkbox(
                ui,
                &mut self.manual_override,
                "手动强制开启手机屏",
                "跳过自动探测，仅用于连接识别失败时的兜底",
            ) {
                self.actions
                    .push(SettingsAction::SetManualOverride(self.manual_override));
            }
        });
        ui.add_space(10.0);

        // 只读文本框：可以选中复制，但不能编辑。
        ui.add(
            egui::TextEdit::multiline(&mut self.logs)
                .code_editor()
                .desired_rows(8)
                .desired_width(ui.available_width())
                .interactive(false),
        );
    }

    fn page_about(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        ui.label(
            egui::RichText::new(version_line(
                &self.status.current_version,
                self.status.self_contained,
            ))
            .size(15.0)
            .strong(),
        );
        ui.label(
            egui::RichText::new(self.update.status_text(&self.status.current_version))
                .small()
                .weak(),
        );
        if let Some(progress) = self.update.progress() {
            ui.add(egui::ProgressBar::new(progress).desired_width(ui.available_width().min(320.0)));
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(self.update.check_enabled(), egui::Button::new("检查更新"))
                .clicked()
            {
                self.actions.push(SettingsAction::CheckUpdate);
            }
            if ui
                .add_enabled(
                    self.update.install_enabled(),
                    egui::Button::new("下载并更新重启"),
                )
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
                "• 本项目基于 MIT License 开源，参考 nicocart/snug-win 与 zephyr4123/snug",
            )
            .small()
            .weak(),
        );
        ui.label(
            egui::RichText::new("• 虚拟显示器驱动采用 MolotovCherry/virtual-display-rs（0.3.1）")
                .small()
                .weak(),
        );
        ui.collapsing("随附字体许可与版权", |ui| {
            for (name, license) in [
                (
                    "Hack-Regular",
                    include_str!("../assets/fonts/licenses/Hack-Regular.txt"),
                ),
                (
                    "Ubuntu-Light",
                    include_str!("../assets/fonts/licenses/UFL.txt"),
                ),
                (
                    "Google Noto Emoji — copyright and attribution",
                    include_str!("../assets/fonts/licenses/Noto-Emoji-attribution.txt"),
                ),
                (
                    "Google Noto Emoji — SIL Open Font License 1.1",
                    include_str!("../assets/fonts/licenses/OFL.txt"),
                ),
                (
                    "emoji-icon-font",
                    include_str!("../assets/fonts/licenses/emoji-icon-font-mit-license.txt"),
                ),
            ] {
                ui.label(egui::RichText::new(name).strong());
                egui::ScrollArea::vertical()
                    .max_height(140.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(license).small().monospace())
                                .wrap(),
                        );
                    });
            }
        });
        ui.add_space(6.0);
        if ui.button("访问 GitHub 仓库主页").clicked() {
            self.actions.push(SettingsAction::OpenRepository);
        }
    }
}

#[cfg(test)]
mod tests {
    fn settle_settings(
        ctx: &egui::Context,
        window: &mut SettingsWindow,
        cfg: &mut AppConfig,
    ) -> egui::FullOutput {
        let mut output = egui::FullOutput::default();
        for _ in 0..3 {
            output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(400.0, 1800.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    window.show(ctx, cfg);
                },
            );
        }
        output
    }

    #[test]
    fn fused_settings_show_three_classes_and_reach_each_with_real_buttons() {
        assert_eq!(
            SettingsTab::ALL.as_slice(),
            [
                SettingsTab::Screens,
                SettingsTab::Gestures,
                SettingsTab::Home
            ]
            .as_slice()
        );
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut window = SettingsWindow::new();
        window.open();
        assert_eq!(window.tab(), SettingsTab::Screens);
        let mut cfg = standard().sanitize();
        let before = cfg.clone();
        let mut output = settle_settings(&ctx, &mut window, &mut cfg);
        for (label, tab) in [
            ("交互", SettingsTab::Gestures),
            ("通用", SettingsTab::Home),
            ("屏幕", SettingsTab::Screens),
        ] {
            click_settings_label(&ctx, &output, label, &mut window, &mut cfg);
            output = settle_settings(&ctx, &mut window, &mut cfg);
            assert_eq!(window.tab(), tab);
        }
        assert_eq!(
            cfg, before,
            "navigation alone must not rewrite configuration"
        );
    }

    #[test]
    fn fused_screen_detail_requires_confirmation_before_deleting() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut cfg = standard().sanitize();
        let before = cfg.clone();
        let mut window = SettingsWindow::new();
        window.open();
        window.select_tab(SettingsTab::Screens);
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "详情", &mut window, &mut cfg);
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "删除这块屏", &mut window, &mut cfg);
        assert_eq!(
            cfg, before,
            "opening delete confirmation must not remove a screen"
        );
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "取消删除", &mut window, &mut cfg);
        assert_eq!(cfg, before);
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "删除这块屏", &mut window, &mut cfg);
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "确认删除", &mut window, &mut cfg);
        assert!(cfg.effective_screens().is_empty());
        assert!(window.is_open(), "deletion returns to the screen list");
    }

    #[test]
    fn fused_screen_detail_keeps_resolution_drafts_until_apply() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut cfg = standard().sanitize();
        cfg.screens.as_mut().unwrap()[0].resolution = Some(ScreenResolution {
            width: 720,
            height: 1280,
        });
        let before = cfg.clone();
        let mut window = SettingsWindow::new();
        window.open();
        window.select_tab(SettingsTab::Screens);
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "详情", &mut window, &mut cfg);
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "恢复默认", &mut window, &mut cfg);
        assert_eq!(cfg, before);
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "应用分辨率", &mut window, &mut cfg);
        assert_eq!(
            cfg.effective_screens()[0].resolution,
            Some(ScreenResolution::DEFAULT)
        );
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "‹ 屏幕列表", &mut window, &mut cfg);
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "详情")));
    }

    #[test]
    fn fused_legacy_routes_keep_driver_and_update_actions_reachable() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        ctx.style_mut(|style| style.animation_time = 0.0);
        let mut cfg = standard().sanitize();
        let mut window = SettingsWindow::new();
        window.select_tab(SettingsTab::from_index(2));
        window.open();
        assert_eq!(window.tab(), SettingsTab::Home);
        assert!(window.needs_logs());
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "安装 / 修复驱动", &mut window, &mut cfg);
        assert_eq!(window.take_actions(), vec![SettingsAction::InstallDriver]);
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "打开日志目录", &mut window, &mut cfg);
        assert_eq!(
            window.take_actions(),
            vec![SettingsAction::OpenLogDirectory]
        );
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "高级与权限", &mut window, &mut cfg);
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "以管理员身份重启", &mut window, &mut cfg);
        assert_eq!(window.take_actions(), vec![SettingsAction::RestartElevated]);
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "手动强制开启手机屏", &mut window, &mut cfg);
        assert_eq!(
            window.take_actions(),
            vec![SettingsAction::SetManualOverride(true)]
        );

        window.select_tab(SettingsTab::from_index(3));
        assert_eq!(window.tab(), SettingsTab::Home);
        assert!(!window.needs_logs());
        window.set_update_state(UpdateUiState::Available {
            version: "9.0.0".into(),
            downloadable: true,
        });
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "检查更新", &mut window, &mut cfg);
        assert_eq!(window.take_actions(), vec![SettingsAction::CheckUpdate]);
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "下载并更新重启", &mut window, &mut cfg);
        assert_eq!(window.take_actions(), vec![SettingsAction::InstallUpdate]);
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "访问 GitHub 仓库主页", &mut window, &mut cfg);
        assert_eq!(window.take_actions(), vec![SettingsAction::OpenRepository]);
    }

    #[test]
    fn fused_screen_detail_saves_name_and_enabled_state() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut cfg = standard().sanitize();
        let mut window = SettingsWindow::new();
        window.open();
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "详情", &mut window, &mut cfg);
        window.screen_name_draft = "重命名后的手机屏".into();
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "保存名字", &mut window, &mut cfg);
        assert_eq!(cfg.effective_screens()[0].name, "重命名后的手机屏");
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "启用这块屏", &mut window, &mut cfg);
        assert!(!cfg.effective_screens()[0].enabled);
    }

    #[test]
    fn fused_editor_and_expanded_general_fit_narrow_screens_with_long_text() {
        for width in [320.0, 400.0, 540.0] {
            for page in 0..5 {
                let mut cfg = standard().sanitize();
                cfg.screens.as_mut().unwrap()[0].name =
                    "这是很长的屏幕名字用来检查设置列表详情和输入框".repeat(4);
                let mut window = SettingsWindow::new();
                window.open();
                match page {
                    1 => {
                        window.screen_detail = Some(cfg.effective_screens()[0].id.clone());
                        window.screen_name_draft = cfg.effective_screens()[0].name.clone();
                    }
                    2 => {
                        window.adding_screen = true;
                        window.new_screen_resolution.custom = true;
                    }
                    3 => {
                        window.select_tab(SettingsTab::Home);
                        window.show_updates = true;
                        window.show_diagnostics = true;
                        window.set_update_state(UpdateUiState::CheckFailed(
                            "用于测试窄屏换行的长错误说明".repeat(12),
                        ));
                        window.set_logs("a deliberately long diagnostic line ".repeat(60));
                    }
                    4 => window.select_tab(SettingsTab::Gestures),
                    _ => {}
                }
                let output = draw_settings_at(width, &mut window, &mut cfg);
                for shape in &output.shapes {
                    let painted = drawn_bounds(&shape.shape).intersect(shape.clip_rect);
                    assert!(
                        painted.right() <= width + 1.0,
                        "page {page} painted outside {width} points: {painted:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn restoring_default_preserves_applied_orientation_and_waits_for_apply() {
        for (width, height, expected) in [
            (720, 1280, ScreenResolution::DEFAULT),
            (
                1280,
                720,
                ScreenResolution {
                    width: 1920,
                    height: 1080,
                },
            ),
        ] {
            let ctx = egui::Context::default();
            crate::theme::apply(&ctx, false);
            let mut cfg = standard().sanitize();
            let screen = &mut cfg.screens.as_mut().unwrap()[0];
            // Applying a landscape preset can leave the legacy orientation unchanged.
            screen.orientation = snug_core::ScreenOrientation::Portrait;
            screen.resolution = Some(ScreenResolution { width, height });
            let before = cfg.clone();
            let mut window = SettingsWindow::new();
            window.open();
            window.select_tab(SettingsTab::Screens);
            let mut output = egui::FullOutput::default();
            for _ in 0..3 {
                output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(400.0, 1800.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        window.show(ctx, &mut cfg);
                    },
                );
            }
            click_settings_label(&ctx, &output, "详情", &mut window, &mut cfg);
            let output = settle_settings(&ctx, &mut window, &mut cfg);
            let output = click_settings_label(&ctx, &output, "恢复默认", &mut window, &mut cfg);
            assert_eq!(cfg, before, "restoring the draft must not apply it");
            click_settings_label(&ctx, &output, "应用分辨率", &mut window, &mut cfg);
            assert_eq!(cfg.effective_screens()[0].resolution, Some(expected));
        }
    }

    #[test]
    fn resolution_presets_and_default_are_only_applied_by_explicit_action() {
        let mut screen = ScreenConfig::default();
        let before = screen.clone();
        let mut draft = super::ResolutionDraft::from_screen(&screen);
        draft.select_preset(0);
        assert_eq!(screen, before, "choosing a preset is not applying");
        assert!(draft.apply(&mut screen));
        assert_eq!(super::screen_spec_text(&screen), "720 × 1280");
        let applied = screen.clone();
        draft.width = "721".into();
        assert!(!draft.apply(&mut screen));
        assert_eq!(screen, applied, "invalid input must not alter the applied mode");
        draft = super::ResolutionDraft::from_resolution(ScreenResolution::DEFAULT);
        assert_eq!(screen, applied, "resetting draft must not change the current resolution");
        assert!(draft.apply(&mut screen));
        assert_eq!((draft.width.as_str(), draft.height.as_str()), ("1080", "1920"));
    }

    #[test]
    fn landscape_and_existing_custom_dimensions_are_not_normalised_to_a_model() {
        let mut screen = ScreenConfig::default();
        screen.resolution = Some(ScreenResolution {
            width: 1080,
            height: 1900,
        });
        let mut draft = ResolutionDraft::from_screen(&screen);
        assert!(draft.custom);
        assert_eq!(draft.label(), "自定义");
        assert!(!draft.apply(&mut screen));
        draft.select_preset(6);
        assert_eq!(
            draft.resolution(),
            Some(ScreenResolution {
                width: 1920,
                height: 1080
            })
        );
        draft.custom = true;
        draft.width = "1280".into();
        draft.height = "720".into();
        assert!(draft.apply(&mut screen));
        assert_eq!(screen_spec_text(&screen), "1280 × 720");
    }

    #[test]
    fn screen_can_be_added_with_the_default_resolution_without_a_model_catalog() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut cfg = standard();
        cfg.screens = Some(Vec::new());
        let mut window = SettingsWindow::new();
        window.open();
        window.select_tab(SettingsTab::Screens);
        let mut output = egui::FullOutput::default();
        for _ in 0..3 {
            output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(400.0, 1000.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    window.show(ctx, &mut cfg);
                },
            );
        }
        click_settings_label(&ctx, &output, "添加屏幕", &mut window, &mut cfg);
        assert!(
            cfg.effective_screens().is_empty(),
            "opening the add form must not add a screen"
        );
        let output = settle_settings(&ctx, &mut window, &mut cfg);
        click_settings_label(&ctx, &output, "确认添加", &mut window, &mut cfg);
        let screens = cfg.effective_screens();
        assert_eq!(screens.len(), 1);
        assert_eq!(screens[0].resolution, Some(ScreenResolution::DEFAULT));
        assert_eq!(screens[0].device.model, None);
    }

    use super::*;

    #[test]
    fn about_page_bundles_full_font_licenses() {
        assert!(include_str!("../assets/fonts/licenses/Hack-Regular.txt").contains("BITSTREAM VERA LICENSE"));
        assert!(include_str!("../assets/fonts/licenses/UFL.txt").contains("UBUNTU FONT LICENCE Version 1.0"));
        assert!(include_str!("../assets/fonts/licenses/OFL.txt").contains("SIL OPEN FONT LICENSE Version 1.1"));
        assert!(include_str!("../assets/fonts/licenses/Noto-Emoji-attribution.txt").contains("Copyright 2013 Google Inc."));
        assert!(include_str!("../assets/fonts/licenses/emoji-icon-font-mit-license.txt").contains("MIT License"));
    }

    fn standard() -> AppConfig {
        AppConfig::default()
    }

    // Before GPU tessellation a galley's mesh bounds can be empty. Logical text
    // bounds still describe its real layout and provide valid pointer targets.
    fn drawn_bounds(shape: &egui::Shape) -> egui::Rect {
        match shape {
            egui::Shape::Text(text) => text.galley.rect.translate(text.pos.to_vec2()),
            _ => shape.visual_bounding_rect(),
        }
    }

    fn draw_settings_at(
        width: f32,
        window: &mut SettingsWindow,
        cfg: &mut AppConfig,
    ) -> egui::FullOutput {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut result = egui::FullOutput::default();
        for _ in 0..3 {
            result = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 760.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    window.show(ctx, cfg);
                },
            );
        }
        result
    }

    fn click_settings_label(
        ctx: &egui::Context,
        output: &egui::FullOutput,
        label: &str,
        window: &mut SettingsWindow,
        cfg: &mut AppConfig,
    ) -> egui::FullOutput {
        let target = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(drawn_bounds(&shape.shape).center())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing visible control {label}"));
        let mut output = egui::FullOutput::default();
        for pressed in [true, false] {
            output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(400.0, 1800.0),
                    )),
                    events: vec![
                        egui::Event::PointerMoved(target),
                        egui::Event::PointerButton {
                            pos: target,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ctx| {
                    window.show(ctx, cfg);
                },
            );
        }
        output
    }

    #[test]
    fn mobile_settings_home_stays_inside_phone_width() {
        for width in [320.0, 400.0, 540.0] {
            let mut window = SettingsWindow::new();
            window.open();
            let output = draw_settings_at(width, &mut window, &mut standard());
            assert!(
                output
                    .shapes
                    .iter()
                    .all(|shape| drawn_bounds(&shape.shape).right() <= width + 1.0),
                "settings must not draw a desktop-width window at {width} points"
            );
        }
    }

    #[test]
    fn mobile_settings_and_custom_resolution_do_not_overflow_with_long_labels() {
        for width in [320.0, 400.0, 540.0] {
            for tab in SettingsTab::ALL {
                let mut cfg = standard();
                let mut window = SettingsWindow::new();
                window.open();
                window.select_tab(tab);
                window.set_update_state(UpdateUiState::Downloading {
                    version: "1.0.1".into(),
                    progress: 0.42,
                });
                window.new_screen_resolution.custom = true;
                let output = draw_settings_at(width, &mut window, &mut cfg);
                let overflow = output
                    .shapes
                    .iter()
                    .map(|shape| drawn_bounds(&shape.shape).right())
                    .fold(0.0_f32, f32::max);
                assert!(
                    overflow <= width + 1.0,
                    "{tab:?} drew to {overflow} at width {width}"
                );
            }
        }
    }

    #[test]
    fn mobile_settings_back_returns_to_apps_from_each_class() {
        for tab in SettingsTab::ALL {
            let ctx = egui::Context::default();
            crate::theme::apply(&ctx, false);
            let mut cfg = standard();
            let mut window = SettingsWindow::new();
            window.open();
            window.select_tab(tab);
            let mut output = egui::FullOutput::default();
            for _ in 0..3 {
                output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(400.0, 1800.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        window.show(ctx, &mut cfg);
                    },
                );
            }
            click_settings_label(&ctx, &output, "‹ 应用", &mut window, &mut cfg);
            assert!(!window.is_open());
        }
    }

    #[test]
    fn mobile_dismiss_requires_confirmation_and_preserves_automatic_screen_setting() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut cfg = standard();
        let mut window = SettingsWindow::new();
        window.status.phone_screen_ready = true;
        window.open();
        let mut output = egui::FullOutput::default();
        for _ in 0..3 {
            output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(400.0, 1800.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    window.show(ctx, &mut cfg);
                },
            );
        }
        let auto_screen = cfg.phone_screen;
        click_settings_label(&ctx, &output, "收起手机屏", &mut window, &mut cfg);
        assert!(
            window.take_actions().is_empty(),
            "opening confirmation must not dismiss"
        );
        assert!(window.is_open());
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            window.show(ctx, &mut cfg);
        });
        click_settings_label(&ctx, &output, "收起并还原窗口", &mut window, &mut cfg);
        assert_eq!(window.take_actions(), vec![SettingsAction::DismissScreen]);
        assert!(!window.is_open());
        assert_eq!(cfg.phone_screen, auto_screen);
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
        for (index, tab) in [
            SettingsTab::Home,
            SettingsTab::Gestures,
            SettingsTab::Diagnostics,
            SettingsTab::About,
            SettingsTab::Screens,
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(tab.index(), index);
            assert_eq!(SettingsTab::from_index(index), *tab);
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
            |cfg: &mut AppConfig| {
                cfg.gesture_long_press_right_click = !cfg.gesture_long_press_right_click
            },
            |cfg: &mut AppConfig| cfg.gestures_injected_only = !cfg.gestures_injected_only,
        ] {
            let before = standard();
            let mut after = before.clone();
            mutate(&mut after);
            assert_eq!(
                classify_change(&before, &after),
                ChangeImpact::ReloadGestures
            );
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
        assert_eq!(window.tab(), SettingsTab::Home);
        assert!(window.needs_logs());
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
