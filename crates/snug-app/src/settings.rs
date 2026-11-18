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
//! 四个页面与 C# 一致：常规与手机屏 / 手势与交互 / 驱动与日志 / 关于与更新。
//! 字段全部来自 `snug_core::AppConfig`（对应 C# `SnugWin.Core.Config.AppConfig`），
//! 本模块不新造配置结构。
//!
//! 手动验证（真机）：
//! 1. 打开设置窗，四个页签都能切换，底部状态与提权文案正确；
//! 2. 改“默认字号放大”后关窗再开，值能被控制器写回 config.json；
//! 3. 关掉手势总开关后，四个子项变灰且不可点；
//! 4. 断驱动时“常规”页出现黄色横幅，“驱动与日志”页显示未就绪。

#![allow(dead_code)] // main.rs 只加了 `mod settings;`，ui.rs 的接线由 Chief 做；接线后删掉这行

use eframe::egui;
use snug_core::{AppConfig, PhoneCatalog, PhoneChoice};

/// 字号放大档位（C# `ZoomCombo` 的 0~4），与 core 的 `MIN_ZOOM_STEPS`/`MAX_ZOOM_STEPS` 一致。
pub const ZOOM_LABELS: [&str; 5] = ["0 级（默认）", "+1 级", "+2 级", "+3 级", "+4 级"];

/// 页签，顺序与 C# 左侧导航一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Gestures,
    Diagnostics,
    About,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 4] =
        [SettingsTab::General, SettingsTab::Gestures, SettingsTab::Diagnostics, SettingsTab::About];

    /// 左侧导航文字（C# `NavTabRadio.Content`）。
    pub fn nav_title(self) -> &'static str {
        match self {
            SettingsTab::General => "常规与手机屏",
            SettingsTab::Gestures => "手势与交互",
            SettingsTab::Diagnostics => "驱动与日志",
            SettingsTab::About => "关于与更新",
        }
    }

    /// 页面标题（C# 页内 `TextBlock`），与导航文字略有出入。
    pub fn page_title(self) -> &'static str {
        match self {
            SettingsTab::General => "常规与手机屏",
            SettingsTab::Gestures => "手势与交互",
            SettingsTab::Diagnostics => "驱动与实时日志",
            SettingsTab::About => "关于与版本更新",
        }
    }

    pub fn index(self) -> usize {
        match self {
            SettingsTab::General => 0,
            SettingsTab::Gestures => 1,
            SettingsTab::Diagnostics => 2,
            SettingsTab::About => 3,
        }
    }

    /// C# `SelectTab(index)`：越界落回“常规”。
    pub fn from_index(index: usize) -> Self {
        Self::ALL.get(index).copied().unwrap_or(SettingsTab::General)
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
    /// 打开机型库窗口（C# `OnOpenModels` 的 `PhoneModelWindow`）。
    OpenPhoneModels,
    /// 手势配置变了，钩子要按新配置重新接管（C# 里是 `SetGesturesEnabled` 等四个 setter）。
    ApplyGestureSettings,
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
}

/// 对比两份配置，得出影响面。判定顺序从“最重”到“最轻”。
pub fn classify_change(before: &AppConfig, after: &AppConfig) -> ChangeImpact {
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

/// 下拉选项下标 → 字号档位（越界夹回 0~4）。
pub fn zoom_steps(index: usize) -> i32 {
    index.min(ZOOM_LABELS.len() - 1) as i32
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
            tab: SettingsTab::General,
            status: StatusInfo::default(),
            update: UpdateUiState::Idle,
            autostart: false,
            manual_override: false,
            logs: String::new(),
            catalog: None,
            actions: Vec::new(),
            baseline: None,
            last_hint: None,
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
    pub fn show(&mut self, ctx: &egui::Context, cfg: &mut AppConfig) -> SettingsOutcome {
        if !self.open {
            return SettingsOutcome::Idle;
        }
        let before = cfg.clone();
        if self.baseline.is_none() {
            self.baseline = Some(before.clone());
        }

        let mut still_open = self.open;
        egui::Window::new("SnugWin 设置")
            .open(&mut still_open)
            .collapsible(false)
            .resizable(true)
            .default_size([780.0, 600.0])
            .min_size([700.0, 520.0])
            .show(ctx, |ui| self.layout(ui, cfg));
        self.open = still_open;

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
                        SettingsTab::General => self.page_general(ui, cfg),
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

    fn page_general(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig) {
        if !self.status.pipe_available {
            egui::Frame::group(ui.style())
                .fill(egui::Color32::from_rgb(0x2B, 0x1F, 0x0E))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new("虚拟显示器驱动尚未就绪")
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
                        if ui.button("一键安装驱动").clicked() {
                            self.actions.push(SettingsAction::InstallDriver);
                        }
                    });
                });
            ui.add_space(8.0);
        }

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

        ui.separator();
        ui.add_space(6.0);

        ui.label(egui::RichText::new("手机型号与屏幕比例").small().weak());
        ui.label(egui::RichText::new(model_text(&cfg.phone)).size(15.0).strong());
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label("终端 / 编辑器默认字号放大：");
            let mut index = zoom_index(cfg.zoom_steps);
            egui::ComboBox::from_id_salt("snug-zoom")
                .selected_text(zoom_label(cfg.zoom_steps))
                .width(120.0)
                .show_ui(ui, |ui| {
                    for (candidate, label) in ZOOM_LABELS.iter().enumerate() {
                        ui.selectable_value(&mut index, candidate, *label);
                    }
                });
            cfg.zoom_steps = zoom_steps(index);
        });

        ui.add_space(10.0);
        if self.catalog.is_some() {
            let mut selected = cfg.phone.model.clone().unwrap_or_default();
            let current_label = model_text(&cfg.phone);
            egui::ComboBox::from_id_salt("snug-model")
                .selected_text(current_label)
                .width(320.0)
                .show_ui(ui, |ui| {
                    if let Some(catalog) = &self.catalog {
                        for model in catalog.models() {
                            let id = model.id();
                            let label = format!(
                                "{} {}（{} × {}）",
                                model.brand, model.name, model.screen.width, model.screen.height
                            );
                            ui.selectable_value(&mut selected, id, label);
                        }
                    }
                });
            if selected != cfg.phone.model.clone().unwrap_or_default()
                && let Some(catalog) = &self.catalog
                && let Some(model) = catalog.find_model(&selected)
            {
                cfg.phone = PhoneChoice::from_model(model);
            }
        }
        if ui.button("更换机型 / 自定义...").clicked() {
            self.actions.push(SettingsAction::OpenPhoneModels);
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
    fn zoom_entry_roundtrips_and_clamps() {
        for steps in 0..=4 {
            assert_eq!(zoom_steps(zoom_index(steps)), steps);
        }
        assert_eq!(zoom_index(-3), 0);
        assert_eq!(zoom_index(99), 4);
        assert_eq!(zoom_steps(99), 4);
        assert_eq!(zoom_label(0), "0 级（默认）");
        assert_eq!(zoom_label(9), "+4 级");
    }

    #[test]
    fn model_text_falls_back_to_custom() {
        let choice = PhoneChoice { model: None, screen: snug_core::PhoneScreenSpec::new(1080, 2400, None) };
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
        assert_eq!(SettingsTab::from_index(0), SettingsTab::General);
        assert_eq!(SettingsTab::from_index(3), SettingsTab::About);
        assert_eq!(SettingsTab::from_index(99), SettingsTab::General);
        for (index, tab) in SettingsTab::ALL.iter().enumerate() {
            assert_eq!(tab.index(), index);
        }
    }

    #[test]
    fn phone_screen_and_updates_need_no_restart() {
        let before = standard();
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
