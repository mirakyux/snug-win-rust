//! Touch library rendering. Desktop effects are returned as actions to the owner.

use eframe::egui;
use snug_core::apps::AppAction;
use snug_core::desk::{DeskNotice, DeskState, DeskWorkKind};
use snug_core::windows::{AppInfo, RunningApp, WindowInfo, WindowKey};

use crate::library::Library;
use crate::theme::Palette;

#[derive(Debug, Clone, PartialEq)]
pub enum LibraryAction {
    App(AppAction),
    NewWindow(RunningApp),
    QuitApp(RunningApp),
    CloseWindow(WindowInfo),
    ReturnPlaced,
    RestorePlaced,
    ZoomIn,
    ZoomOut,
    Settings,
    Screens,
    Dismiss,
    ClosePreview,
    ForceQuit { pid: i32, app: String },
    DismissNotice,
    ConfirmManualZoomReset,
}

const TARGET: f32 = 44.0;

#[derive(Clone, Copy)]
enum Glyph {
    Phone,
    Window,
    NewWindow,
    Quit,
    Close,
    Restore,
}

fn control_id(kind: &str, key: impl std::hash::Hash) -> egui::Id {
    egui::Id::new(("mobile-library", kind, key))
}

fn search_fill(palette: Palette) -> egui::Color32 {
    if palette.dark {
        egui::Color32::from_rgb(43, 46, 54)
    } else {
        egui::Color32::from_rgb(228, 228, 234)
    }
}

fn paint_glyph(painter: &egui::Painter, rect: egui::Rect, glyph: Glyph, color: egui::Color32) {
    let center = rect.center();
    let stroke = egui::Stroke::new(1.5_f32, color);
    let line = |a: egui::Vec2, b: egui::Vec2| {
        painter.line_segment([center + a, center + b], stroke);
    };
    match glyph {
        Glyph::Phone => {
            let body = egui::Rect::from_center_size(center, egui::vec2(9.0, 17.0));
            painter.rect_stroke(body, 1.5, stroke, egui::StrokeKind::Inside);
            line(egui::vec2(-2.0, -6.0), egui::vec2(2.0, -6.0));
            line(egui::vec2(-1.0, 5.5), egui::vec2(1.0, 5.5));
        }
        Glyph::Window | Glyph::NewWindow => {
            let body = egui::Rect::from_center_size(center, egui::vec2(17.0, 13.0));
            painter.rect_stroke(body, 2.0, stroke, egui::StrokeKind::Inside);
            line(egui::vec2(-7.0, -2.5), egui::vec2(7.0, -2.5));
            for x in [-5.0, -2.0] {
                painter.circle_filled(center + egui::vec2(x, -4.5), 0.7, color);
            }
            if matches!(glyph, Glyph::NewWindow) {
                let plus = center + egui::vec2(-7.0, 6.0);
                painter.circle_filled(plus, 4.5, color);
                let ink = if color.r() < 128 {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::BLACK
                };
                painter.line_segment(
                    [plus + egui::vec2(-2.0, 0.0), plus + egui::vec2(2.0, 0.0)],
                    (1.0, ink),
                );
                painter.line_segment(
                    [plus + egui::vec2(0.0, -2.0), plus + egui::vec2(0.0, 2.0)],
                    (1.0, ink),
                );
            }
        }
        Glyph::Quit => {
            painter.rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(13.0, 13.0)),
                2.5,
                stroke,
                egui::StrokeKind::Inside,
            );
            line(egui::vec2(-2.5, -2.5), egui::vec2(2.5, 2.5));
            line(egui::vec2(-2.5, 2.5), egui::vec2(2.5, -2.5));
        }
        Glyph::Close => {
            painter.circle_filled(center, 8.0, color.gamma_multiply(0.65));
            let ink = if color.r() < 128 {
                egui::Color32::WHITE
            } else {
                egui::Color32::BLACK
            };
            let stroke = egui::Stroke::new(1.1_f32, ink);
            painter.line_segment(
                [
                    center + egui::vec2(-2.5, -2.5),
                    center + egui::vec2(2.5, 2.5),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(-2.5, 2.5),
                    center + egui::vec2(2.5, -2.5),
                ],
                stroke,
            );
        }
        Glyph::Restore => {
            line(egui::vec2(-3.0, -6.0), egui::vec2(-7.0, -2.0));
            line(egui::vec2(-7.0, -2.0), egui::vec2(-2.0, 1.0));
            painter.add(egui::Shape::line(
                vec![
                    center + egui::vec2(-6.0, -2.0),
                    center + egui::vec2(2.0, -2.0),
                    center + egui::vec2(5.0, 0.0),
                    center + egui::vec2(5.0, 4.0),
                    center + egui::vec2(2.0, 6.0),
                    center + egui::vec2(-2.0, 6.0),
                ],
                stroke,
            ));
        }
    }
}

fn icon_button(
    ui: &mut egui::Ui,
    id: egui::Id,
    label: &str,
    glyph: Glyph,
    palette: Palette,
    circle: bool,
) -> egui::Response {
    let (_, rect) = ui.allocate_space(egui::vec2(TARGET, TARGET));
    let response = ui.interact(rect, id, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    if circle || response.hovered() || response.has_focus() {
        ui.painter().circle_filled(
            rect.center(),
            19.0,
            if response.hovered() || response.has_focus() {
                palette.card_hover
            } else {
                palette.bg
            },
        );
    }
    paint_glyph(
        ui.painter(),
        rect,
        glyph,
        if matches!(glyph, Glyph::Phone) {
            palette.accent
        } else {
            palette.text_weak
        },
    );
    response.on_hover_text(label)
}

fn paint_magnifier(ui: &egui::Ui, center: egui::Pos2, color: egui::Color32) {
    ui.painter().circle_stroke(center, 4.5, (1.2, color));
    ui.painter().line_segment(
        [center + egui::vec2(3.5, 3.5), center + egui::vec2(7.0, 7.0)],
        (1.2, color),
    );
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Tab {
    #[default]
    Running,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Confirm {
    Quit(i32),
    Close(WindowKey),
    Dismiss,
}

#[derive(Default)]
pub struct MobileLibraryUi {
    tab: Tab,
    searching: bool,
    focus_search: bool,
    query: String,
    selected: Option<i32>,
    confirming: Option<Confirm>,
    previous_view: Option<(Option<u32>, bool, bool)>,
}

fn running_apps(library: &Library, state: &DeskState) -> Vec<RunningApp> {
    let mut apps = library.running().to_vec();
    if let Some(placed) = &state.placed {
        let mut current = apps
            .iter()
            .position(|app| app.pid == placed.window.key.pid)
            .map(|index| apps.remove(index))
            .unwrap_or_else(|| RunningApp {
                pid: placed.window.key.pid,
                app: AppInfo {
                    app_id: placed
                        .window
                        .app_id
                        .clone()
                        .unwrap_or_else(|| format!("pid-{}", placed.window.key.pid)),
                    name: placed.window.app_name.clone(),
                    executable_path: None,
                    launch_target: None,
                    arguments: None,
                    working_directory: None,
                },
                windows: Vec::new(),
            });
        let latest = current
            .windows
            .iter()
            .find(|window| window.key == placed.window.key)
            .cloned()
            .unwrap_or_else(|| placed.window.clone());
        current.windows.retain(|window| window.key != latest.key);
        current.windows.insert(0, latest);
        apps.insert(0, current);
    }
    apps
}

fn title(window: &WindowInfo) -> &str {
    if window.title.trim().is_empty() {
        &window.app_name
    } else {
        &window.title
    }
}

fn app_action(library: &Library, state: &DeskState, app: &AppInfo) -> LibraryAction {
    let action = library.action_for(app);
    if matches!(&action, AppAction::Pull(window) if state.placed.as_ref().is_some_and(|placed| placed.window.key == window.key))
    {
        LibraryAction::ReturnPlaced
    } else {
        LibraryAction::App(action)
    }
}

fn text(ui: &egui::Ui, value: &str, rect: egui::Rect, size: f32, color: egui::Color32) {
    let mut job = egui::text::LayoutJob::simple(
        value.to_string(),
        egui::FontId::proportional(size),
        color,
        rect.width().max(1.0),
    );
    job.wrap.max_rows = 1;
    let galley = ui.painter().layout_job(job);
    ui.painter()
        .with_clip_rect(rect.intersect(ui.clip_rect()))
        .galley(rect.min, galley, color);
}

fn app_button(
    ui: &mut egui::Ui,
    app: &AppInfo,
    subtitle: &str,
    icon: Option<&egui::TextureHandle>,
    width: f32,
    selected: bool,
    palette: Palette,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width.max(0.0), 48.0), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), &app.name)
    });
    if selected || response.hovered() || response.has_focus() {
        ui.painter().rect_filled(rect, 10.0, palette.card_hover);
    }
    let side = 32.0_f32.min(rect.width());
    let icon_rect =
        egui::Rect::from_min_size(rect.min + egui::vec2(3.0, 8.0), egui::vec2(side, side));
    if let Some(icon) = icon {
        ui.painter().image(
            icon.id(),
            icon_rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    } else {
        ui.painter().rect_filled(icon_rect, 10.0, palette.bg);
        ui.painter().text(
            icon_rect.center(),
            egui::Align2::CENTER_CENTER,
            app.name.chars().next().unwrap_or('?'),
            egui::FontId::proportional(20.0),
            palette.accent,
        );
    }
    let left = (rect.left() + side + 13.0).min(rect.right());
    text(
        ui,
        &app.name,
        egui::Rect::from_min_max(
            egui::pos2(left, rect.top() + 4.0),
            egui::pos2(rect.right(), rect.top() + 26.0),
        ),
        17.0,
        palette.text,
    );
    text(
        ui,
        subtitle,
        egui::Rect::from_min_max(
            egui::pos2(left, rect.top() + 27.0),
            egui::pos2(rect.right(), rect.bottom()),
        ),
        13.0,
        palette.text_weak,
    );
    response
}

impl MobileLibraryUi {
    fn validate(&mut self, apps: &[RunningApp], state: &DeskState) {
        let view = (state.display_id, state.list_visible, state.work.is_some());
        if self.previous_view.is_some_and(|previous| previous != view) {
            self.confirming = None;
            if state.list_visible {
                self.selected = None;
            }
        }
        self.previous_view = Some(view);
        if self
            .selected
            .is_some_and(|pid| !apps.iter().any(|app| app.pid == pid))
        {
            self.selected = None;
        }
        let valid = match self.confirming {
            Some(Confirm::Quit(pid)) => apps.iter().any(|app| app.pid == pid),
            Some(Confirm::Close(key)) => apps
                .iter()
                .any(|app| app.windows.iter().any(|window| window.key == key)),
            _ => true,
        };
        if !valid {
            self.confirming = None;
        }
    }

    /// Enter the same inline confirmation used by the screen menu.
    pub fn request_dismiss(&mut self) {
        self.searching = false;
        self.query.clear();
        self.confirming = Some(Confirm::Dismiss);
    }

    /// Select a page for the read-only preview command-line entry point.
    pub fn set_preview_page(&mut self, all: bool, search: Option<String>) {
        self.tab = if all { Tab::All } else { Tab::Running };
        self.searching = search.is_some();
        self.focus_search = self.searching;
        self.query = search.unwrap_or_default();
        self.confirming = None;
    }

    fn segments(&mut self, ui: &mut egui::Ui, palette: Palette) {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), TARGET),
            egui::Sense::hover(),
        );
        let track = rect.shrink2(egui::vec2(0.0, 5.0));
        ui.painter().rect_filled(track, 8.0, search_fill(palette));
        for (index, tab, label) in [(0, Tab::Running, "运行中"), (1, Tab::All, "全部")] {
            let half = egui::Rect::from_min_size(
                rect.min + egui::vec2(index as f32 * rect.width() / 2.0, 0.0),
                egui::vec2(rect.width() / 2.0, TARGET),
            );
            let response = ui.interact(half, control_id("tab", index), egui::Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::SelectableLabel, ui.is_enabled(), label)
            });
            if self.tab == tab {
                ui.painter()
                    .rect_filled(half.shrink2(egui::vec2(2.0, 7.0)), 6.0, palette.card);
            }
            ui.painter().text(
                half.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(13.0),
                palette.text,
            );
            if response.clicked() {
                self.tab = tab;
                self.confirming = None;
            }
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, palette: Palette, compact: bool) {
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(
                egui::RichText::new("App")
                    .size(if compact { 27.0 } else { 30.0 })
                    .strong(),
            );
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), TARGET),
                egui::Sense::click(),
            );
            let field = rect.shrink2(egui::vec2(0.0, 4.0));
            ui.painter().rect_filled(field, 9.0, search_fill(palette));
            paint_magnifier(
                ui,
                egui::pos2(field.left() + 16.0, field.center().y - 1.5),
                palette.text_weak,
            );
            ui.painter().text(
                egui::pos2(field.left() + 32.0, field.center().y),
                egui::Align2::LEFT_CENTER,
                "搜索 App",
                egui::FontId::proportional(14.0),
                palette.text_weak,
            );
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), "搜索 App")
            });
            if response.clicked() {
                self.searching = true;
                self.focus_search = true;
                self.confirming = None;
            }
            self.segments(ui, palette);
            ui.add_space(3.0);
        });
    }

    fn app_actions(
        &mut self,
        ui: &mut egui::Ui,
        app: &RunningApp,
        state: &DeskState,
        palette: Palette,
        action: &mut Option<LibraryAction>,
    ) {
        if state.quitting.contains(&app.pid) {
            ui.spinner();
            ui.label(egui::RichText::new("正在退出").size(13.0));
        } else if self.confirming == Some(Confirm::Quit(app.pid)) {
            if ui
                .add_sized(
                    [44.0, TARGET],
                    egui::Button::new(egui::RichText::new("取消").size(13.0)).frame(false),
                )
                .clicked()
            {
                self.confirming = None;
            }
            if ui
                .add_sized(
                    [72.0, TARGET],
                    egui::Button::new(
                        egui::RichText::new("确认退出")
                            .size(13.0)
                            .color(ui.visuals().error_fg_color),
                    )
                    .frame(false),
                )
                .clicked()
            {
                *action = Some(LibraryAction::QuitApp(app.clone()));
            }
        } else {
            if icon_button(
                ui,
                control_id("new-window", app.pid),
                "新建窗口",
                Glyph::NewWindow,
                palette,
                true,
            )
            .clicked()
            {
                *action = Some(LibraryAction::NewWindow(app.clone()));
            }
            if icon_button(
                ui,
                control_id("quit", app.pid),
                "退出 App",
                Glyph::Quit,
                palette,
                true,
            )
            .clicked()
            {
                self.confirming = Some(Confirm::Quit(app.pid));
            }
        }
    }

    fn app_card(
        &mut self,
        ui: &mut egui::Ui,
        library: &mut Library,
        app: &RunningApp,
        state: &DeskState,
        palette: Palette,
        action: &mut Option<LibraryAction>,
    ) {
        let icon = library.icon(ui.ctx(), &app.app);
        ui.push_id(("app-card", app.pid), |ui| {
            egui::Frame::default()
                .fill(palette.card)
                .corner_radius(10)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.set_max_width(ui.available_width());
                    ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                    let summary = if self.confirming == Some(Confirm::Quit(app.pid)) {
                        "退出这个 App？".to_string()
                    } else if app.windows.is_empty() {
                        "没有窗口".to_string()
                    } else {
                        format!("{} 个窗口", app.windows.len())
                    };
                    let trailing = if self.confirming == Some(Confirm::Quit(app.pid)) {
                        116.0
                    } else {
                        88.0
                    };
                    let width = (ui.available_width() - trailing).max(0.0);
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!state.quitting.contains(&app.pid), |ui| {
                            if app_button(
                                ui,
                                &app.app,
                                &summary,
                                icon.as_ref(),
                                width,
                                false,
                                palette,
                            )
                            .clicked()
                            {
                                *action = Some(app_action(library, state, &app.app));
                            }
                        });
                        self.app_actions(ui, app, state, palette, action);
                    });
                    for window in &app.windows {
                        let y = ui.cursor().top();
                        ui.painter().line_segment(
                            [
                                egui::pos2(ui.min_rect().left(), y),
                                egui::pos2(ui.max_rect().right(), y),
                            ],
                            (0.5, palette.bg),
                        );
                        self.window_row(ui, window, state, palette, action);
                        if state
                            .placed
                            .as_ref()
                            .is_some_and(|placed| placed.window.key == window.key)
                        {
                            self.placed_controls(ui, state, palette, action);
                        }
                    }
                });
        });
        ui.add_space(8.0);
    }

    fn window_row(
        &mut self,
        ui: &mut egui::Ui,
        window: &WindowInfo,
        state: &DeskState,
        palette: Palette,
        action: &mut Option<LibraryAction>,
    ) {
        let current = state
            .placed
            .as_ref()
            .is_some_and(|placed| placed.window.key == window.key);
        let confirming = self.confirming == Some(Confirm::Close(window.key));
        let trailing = if confirming { 116.0 } else { TARGET };
        let badge = if current {
            64.0
        } else if window.minimized {
            54.0
        } else {
            0.0
        };
        let width = (ui.available_width() - trailing).max(0.0);
        ui.push_id(window.key, |ui| {
            ui.add_enabled_ui(
                !state.closing.contains(&window.key) && !state.quitting.contains(&window.key.pid),
                |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let (_, rect) = ui.allocate_space(egui::vec2(width, TARGET));
                        let response = ui.interact(
                            rect,
                            control_id("window", window.key),
                            egui::Sense::click(),
                        );
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                ui.is_enabled(),
                                title(window),
                            )
                        });
                        if response.hovered() || response.has_focus() {
                            ui.painter().rect_filled(rect, 6.0, palette.card_hover);
                        }
                        let leading = egui::Rect::from_center_size(
                            egui::pos2(rect.left() + 17.0, rect.center().y),
                            egui::vec2(22.0, 22.0),
                        );
                        paint_glyph(
                            ui.painter(),
                            leading,
                            if current { Glyph::Phone } else { Glyph::Window },
                            if current {
                                palette.accent
                            } else {
                                palette.text_weak
                            },
                        );
                        text(
                            ui,
                            title(window),
                            egui::Rect::from_min_max(
                                egui::pos2(rect.left() + 38.0, rect.center().y - 9.0),
                                egui::pos2(
                                    (rect.right() - badge).max(rect.left() + 38.0),
                                    rect.bottom(),
                                ),
                            ),
                            14.0,
                            palette.text,
                        );
                        if badge > 0.0 {
                            let label = if current {
                                "在手机屏上"
                            } else {
                                "已最小化"
                            };
                            let color = if current {
                                palette.accent
                            } else {
                                palette.text_weak
                            };
                            let badge_rect = egui::Rect::from_center_size(
                                egui::pos2(rect.right() - badge / 2.0, rect.center().y),
                                egui::vec2(badge - 4.0, 18.0),
                            );
                            ui.painter().rect_filled(
                                badge_rect,
                                8.0,
                                if current {
                                    palette.accent.gamma_multiply(0.12)
                                } else {
                                    palette.bg
                                },
                            );
                            ui.painter().text(
                                badge_rect.center(),
                                egui::Align2::CENTER_CENTER,
                                label,
                                egui::FontId::proportional(10.5),
                                color,
                            );
                        }
                        if response.clicked() {
                            *action = Some(if current {
                                LibraryAction::ReturnPlaced
                            } else {
                                LibraryAction::App(AppAction::Pull(window.clone()))
                            });
                        }
                        if confirming {
                            if ui
                                .add_sized(
                                    [44.0, TARGET],
                                    egui::Button::new(egui::RichText::new("取消").size(13.0))
                                        .frame(false),
                                )
                                .clicked()
                            {
                                self.confirming = None;
                            }
                            if ui
                                .add_sized(
                                    [72.0, TARGET],
                                    egui::Button::new(
                                        egui::RichText::new("确认关闭")
                                            .size(13.0)
                                            .color(ui.visuals().error_fg_color),
                                    )
                                    .frame(false),
                                )
                                .clicked()
                            {
                                *action = Some(LibraryAction::CloseWindow(window.clone()));
                            }
                        } else if icon_button(
                            ui,
                            control_id("close-window", window.key),
                            "关闭窗口",
                            Glyph::Close,
                            palette,
                            false,
                        )
                        .clicked()
                        {
                            self.confirming = Some(Confirm::Close(window.key));
                        }
                    });
                },
            );
        });
    }

    fn placed_controls(
        &self,
        ui: &mut egui::Ui,
        state: &DeskState,
        palette: Palette,
        action: &mut Option<LibraryAction>,
    ) {
        let Some(placed) = &state.placed else { return };
        ui.horizontal(|ui| {
            if placed.zoomable() {
                if ui
                    .add_enabled(
                        placed.zoom > snug_core::config::MIN_ZOOM_STEPS,
                        egui::Button::new(egui::RichText::new("字号 −").size(13.0))
                            .frame(false)
                            .min_size(egui::vec2(52.0, TARGET)),
                    )
                    .clicked()
                {
                    *action = Some(LibraryAction::ZoomOut);
                }
                ui.label(
                    egui::RichText::new(if placed.zoom == 0 {
                        "默认".to_string()
                    } else {
                        format!("+{}", placed.zoom)
                    })
                    .size(12.0)
                    .color(palette.text_weak),
                );
                if ui
                    .add_enabled(
                        placed.zoom < snug_core::config::MAX_ZOOM_STEPS,
                        egui::Button::new(egui::RichText::new("字号 +").size(13.0))
                            .frame(false)
                            .min_size(egui::vec2(52.0, TARGET)),
                    )
                    .clicked()
                {
                    *action = Some(LibraryAction::ZoomIn);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(76.0, TARGET), egui::Sense::click());
                ui.painter()
                    .rect_filled(rect.shrink2(egui::vec2(0.0, 3.0)), 20.0, palette.bg);
                paint_glyph(
                    ui.painter(),
                    egui::Rect::from_center_size(
                        rect.center() - egui::vec2(20.0, 0.0),
                        egui::vec2(18.0, 18.0),
                    ),
                    Glyph::Restore,
                    palette.text_weak,
                );
                ui.painter().text(
                    rect.center() + egui::vec2(9.0, 0.0),
                    egui::Align2::CENTER_CENTER,
                    "还原",
                    egui::FontId::proportional(13.0),
                    palette.text,
                );
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), "还原")
                });
                if response.clicked() {
                    *action = Some(LibraryAction::RestorePlaced);
                }
            });
        });
    }

    fn running_page(
        &mut self,
        ui: &mut egui::Ui,
        library: &mut Library,
        apps: &[RunningApp],
        state: &DeskState,
        palette: Palette,
        action: &mut Option<LibraryAction>,
    ) {
        if apps.is_empty() {
            ui.label(if library.scanned_once {
                "没有正在运行的 App"
            } else {
                "正在读取应用…"
            });
        }
        for (index, app) in apps.iter().enumerate() {
            if state.placed.is_some() {
                if index == 0 {
                    ui.label(egui::RichText::new("当前 App").small().weak());
                }
                if index == 1 {
                    ui.label(egui::RichText::new("其他 App").small().weak());
                }
            }
            self.app_card(ui, library, app, state, palette, action);
        }
    }

    fn all_grid(
        &mut self,
        ui: &mut egui::Ui,
        library: &mut Library,
        state: &DeskState,
        palette: Palette,
        action: &mut Option<LibraryAction>,
    ) {
        let apps: Vec<_> = library
            .grouped("")
            .into_iter()
            .flat_map(|(_, apps)| apps)
            .collect();
        if apps.is_empty() {
            ui.label(if library.scanned_once {
                "没有找到已安装的应用"
            } else {
                "正在读取应用…"
            });
        }
        let columns = grid_columns(ui.available_width());
        let width = ((ui.available_width() - 8.0 * (columns - 1) as f32) / columns as f32).max(0.0);
        for row in apps.chunks(columns) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                for app in row {
                    let icon = library.icon(ui.ctx(), app);
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(width, 90.0), egui::Sense::click());
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            ui.is_enabled(),
                            &app.name,
                        )
                    });
                    if response.hovered() || response.has_focus() {
                        ui.painter().rect_filled(rect, 12.0, palette.card_hover);
                    }
                    let side = (width - 10.0).clamp(32.0, 52.0);
                    let icon_rect = egui::Rect::from_center_size(
                        egui::pos2(rect.center().x, rect.top() + side / 2.0),
                        egui::vec2(side, side),
                    );
                    if let Some(icon) = icon {
                        ui.painter().image(
                            icon.id(),
                            icon_rect,
                            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    } else {
                        ui.painter().rect_filled(icon_rect, 14.0, palette.card);
                        ui.painter().text(
                            icon_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            app.name.chars().next().unwrap_or('?'),
                            egui::FontId::proportional(24.0),
                            palette.accent,
                        );
                    }
                    let mut label = egui::text::LayoutJob::simple(
                        app.name.clone(),
                        egui::FontId::proportional(12.0),
                        palette.text,
                        width,
                    );
                    label.wrap.max_rows = 1;
                    let label = ui.painter().layout_job(label);
                    ui.painter()
                        .with_clip_rect(rect.intersect(ui.clip_rect()))
                        .galley(
                            egui::pos2(
                                rect.center().x - label.size().x / 2.0,
                                icon_rect.bottom() + 7.0,
                            ),
                            label,
                            palette.text,
                        );
                    let running = library
                        .running()
                        .iter()
                        .find(|running| running.app.app_id.eq_ignore_ascii_case(&app.app_id));
                    if running.is_some() {
                        ui.painter().circle_filled(
                            egui::pos2(rect.center().x, rect.bottom() - 7.0),
                            3.0,
                            palette.text_weak,
                        );
                    }
                    if response.clicked()
                        && !running.is_some_and(|app| state.quitting.contains(&app.pid))
                    {
                        *action = Some(app_action(library, state, app));
                    }
                }
            });
            ui.add_space(2.0);
        }
    }

    fn search_results(
        &mut self,
        ui: &mut egui::Ui,
        library: &mut Library,
        apps: &[RunningApp],
        state: &DeskState,
        palette: Palette,
        action: &mut Option<LibraryAction>,
    ) {
        let mut candidates: Vec<_> = library
            .grouped("")
            .into_iter()
            .flat_map(|(_, apps)| apps)
            .collect();
        for app in apps {
            if !candidates
                .iter()
                .any(|item| item.app_id.eq_ignore_ascii_case(&app.app.app_id))
            {
                candidates.push(app.app.clone());
            }
        }
        let query = self.query.trim();
        let matched: Vec<_> = candidates
            .into_iter()
            .filter(|app| library.matches_search(app, query))
            .collect();
        let windows: Vec<_> = if query.is_empty() {
            Vec::new()
        } else {
            let query = query.to_lowercase();
            apps.iter()
                .flat_map(|app| &app.windows)
                .filter(|window| title(window).to_lowercase().contains(&query))
                .cloned()
                .collect()
        };
        if matched.is_empty() && windows.is_empty() {
            ui.label("没有匹配的 App 或窗口");
        }
        if !matched.is_empty() {
            ui.label(
                egui::RichText::new("App")
                    .size(12.0)
                    .color(palette.text_weak),
            );
            for app in matched {
                let running = apps
                    .iter()
                    .find(|running| running.app.app_id.eq_ignore_ascii_case(&app.app_id));
                let subtitle = running
                    .map(|running| format!("{} 个窗口", running.windows.len()))
                    .unwrap_or_else(|| "未打开".into());
                let icon = library.icon(ui.ctx(), &app);
                egui::Frame::default()
                    .fill(palette.card)
                    .corner_radius(10)
                    .inner_margin(8.0)
                    .show(ui, |ui| {
                        ui.add_enabled_ui(
                            !running.is_some_and(|app| state.quitting.contains(&app.pid)),
                            |ui| {
                                if app_button(
                                    ui,
                                    &app,
                                    &subtitle,
                                    icon.as_ref(),
                                    ui.available_width(),
                                    false,
                                    palette,
                                )
                                .clicked()
                                {
                                    *action = Some(app_action(library, state, &app));
                                }
                            },
                        );
                    });
            }
        }
        if !windows.is_empty() {
            ui.add_space(16.0);
            ui.label(egui::RichText::new("窗口").strong());
            for window in windows {
                self.window_row(ui, &window, state, palette, action);
            }
        }
    }

    fn landscape(
        &mut self,
        ui: &mut egui::Ui,
        library: &mut Library,
        apps: &[RunningApp],
        state: &DeskState,
        palette: Palette,
        action: &mut Option<LibraryAction>,
    ) {
        let side = (ui.available_width() * 0.4).min(320.0);
        let height = ui.available_height();
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(side, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_max_width(side);
                    self.header(ui, palette, true);
                    egui::ScrollArea::vertical()
                        .id_salt("library-sidebar")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if self.tab == Tab::Running {
                                if apps.is_empty() {
                                    ui.label("没有正在运行的 App");
                                }
                                for app in apps {
                                    let icon = library.icon(ui.ctx(), &app.app);
                                    if app_button(
                                        ui,
                                        &app.app,
                                        &format!("{} 个窗口", app.windows.len()),
                                        icon.as_ref(),
                                        ui.available_width(),
                                        self.selected.or_else(|| apps.first().map(|app| app.pid))
                                            == Some(app.pid),
                                        palette,
                                    )
                                    .clicked()
                                    {
                                        self.selected = Some(app.pid);
                                        self.confirming = None;
                                    }
                                }
                            } else {
                                for (_, installed) in library.grouped("") {
                                    for app in installed {
                                        let running = apps.iter().find(|running| {
                                            running.app.app_id.eq_ignore_ascii_case(&app.app_id)
                                        });
                                        let icon = library.icon(ui.ctx(), &app);
                                        if app_button(
                                            ui,
                                            &app,
                                            if running.is_some() {
                                                "运行中"
                                            } else {
                                                "未打开"
                                            },
                                            icon.as_ref(),
                                            ui.available_width(),
                                            running
                                                .is_some_and(|app| Some(app.pid) == self.selected),
                                            palette,
                                        )
                                        .clicked()
                                        {
                                            self.confirming = None;
                                            if let Some(running) = running {
                                                self.selected = Some(running.pid);
                                            } else {
                                                *action = Some(app_action(library, state, &app));
                                            }
                                        }
                                    }
                                }
                            }
                        });
                },
            );
            ui.separator();
            ui.vertical(|ui| {
                ui.set_max_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .id_salt("library-window-detail")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Some(app) = self
                            .selected
                            .and_then(|pid| apps.iter().find(|app| app.pid == pid))
                            .or_else(|| apps.first())
                        {
                            self.app_card(ui, library, app, state, palette, action);
                            ui.label(egui::RichText::new("点窗口放到这块屏上").small().weak());
                        } else {
                            ui.label("从左侧选择 App");
                        }
                    });
            });
        });
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        library: &mut Library,
        state: &DeskState,
        palette: Palette,
        preview: bool,
        screen_label: &str,
    ) -> Option<LibraryAction> {
        let apps = running_apps(library, state);
        self.validate(&apps, state);
        let mut action = None;
        let busy = state.work.is_some();
        if !self.searching || busy {
            egui::TopBottomPanel::top("library-navigation")
                .frame(
                    egui::Frame::default()
                        .fill(palette.bg)
                        .inner_margin(egui::Margin::symmetric(16, 3)),
                )
                .show(ctx, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                    if !self.searching {
                        ui.add_enabled_ui(!busy, |ui| {
                            ui.horizontal(|ui| {
                                let preview_width = if preview { 88.0 } else { 0.0 };
                                let width =
                                    (ui.available_width() - TARGET - preview_width).max(0.0);
                                let (rect, response) = ui.allocate_exact_size(
                                    egui::vec2(width, TARGET),
                                    egui::Sense::click(),
                                );
                                if let Some(placed) = &state.placed {
                                    let name = format!("‹ {}", placed.window.app_name);
                                    let galley = ui.painter().layout_no_wrap(
                                        name.clone(),
                                        egui::FontId::proportional(14.0),
                                        palette.accent,
                                    );
                                    let pill = egui::Rect::from_min_size(
                                        rect.min + egui::vec2(0.0, 3.0),
                                        egui::vec2((galley.size().x + 22.0).min(width), 38.0),
                                    );
                                    ui.painter().rect_filled(
                                        pill,
                                        21.0,
                                        palette.accent.gamma_multiply(0.12),
                                    );
                                    text(
                                        ui,
                                        &name,
                                        pill.shrink2(egui::vec2(10.0, 10.0)),
                                        14.0,
                                        palette.accent,
                                    );
                                    response.widget_info(|| {
                                        egui::WidgetInfo::labeled(
                                            egui::WidgetType::Button,
                                            ui.is_enabled(),
                                            &name,
                                        )
                                    });
                                    if response.clicked() {
                                        action = Some(LibraryAction::ReturnPlaced);
                                    }
                                }
                                if preview
                                    && ui
                                        .add_sized(
                                            [preview_width, TARGET],
                                            egui::Button::new(
                                                egui::RichText::new("预览 · 关闭")
                                                    .size(11.0)
                                                    .color(palette.text_weak),
                                            )
                                            .frame(false),
                                        )
                                        .clicked()
                                {
                                    action = Some(LibraryAction::ClosePreview);
                                }
                                if icon_button(
                                    ui,
                                    control_id("screens", ()),
                                    "屏幕",
                                    Glyph::Phone,
                                    palette,
                                    false,
                                )
                                .on_hover_text(screen_label)
                                .clicked()
                                {
                                    action = Some(LibraryAction::Screens);
                                }
                            });
                            if self.confirming == Some(Confirm::Dismiss) {
                                ui.label(
                                    egui::RichText::new("退出这块屏？窗口将还原，下次远程时恢复。")
                                        .size(13.0),
                                );
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_sized(
                                            [56.0, TARGET],
                                            egui::Button::new("取消").frame(false),
                                        )
                                        .clicked()
                                    {
                                        self.confirming = None;
                                    }
                                    if ui
                                        .add_sized(
                                            [112.0, TARGET],
                                            egui::Button::new(
                                                egui::RichText::new("确认退出屏幕")
                                                    .size(13.0)
                                                    .color(ui.visuals().error_fg_color),
                                            )
                                            .frame(false),
                                        )
                                        .clicked()
                                    {
                                        action = Some(LibraryAction::Dismiss);
                                    }
                                });
                            }
                        });
                    }
                    if let Some(work) = &state.work {
                        let kind = match work.kind {
                            DeskWorkKind::Opening => "正在打开",
                            DeskWorkKind::Placing => "正在铺窗",
                            DeskWorkKind::PuttingBack => "正在还原窗口",
                            DeskWorkKind::Zooming => "正在调整字号",
                            DeskWorkKind::Reshaping => "正在应用分辨率",
                            DeskWorkKind::Dismissing => "正在退出屏幕",
                        };
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!(
                                "{kind}{}",
                                work.label
                                    .as_ref()
                                    .map(|label| format!(" · {label}"))
                                    .unwrap_or_default()
                            ));
                        });
                    }
                });
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(palette.bg)
                    .inner_margin(egui::Margin {
                        left: 16,
                        right: 16,
                        top: if self.searching { 10 } else { 0 },
                        bottom: 12,
                    }),
            )
            .show(ctx, |ui| {
                ui.set_max_width(ui.available_width());
                ui.add_enabled_ui(!busy, |ui| {
                    if let Some(notice) = &state.notice {
                        notice_row(ui, notice, &mut action);
                    }
                    if self.searching {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            let width = (ui.available_width() - 52.0).max(0.0);
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(width, TARGET),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(
                                rect.shrink2(egui::vec2(0.0, 4.0)),
                                9.0,
                                search_fill(palette),
                            );
                            paint_magnifier(
                                ui,
                                egui::pos2(rect.left() + 15.0, rect.center().y - 1.5),
                                palette.text_weak,
                            );
                            let edit = egui::Rect::from_min_max(
                                rect.min + egui::vec2(30.0, 10.0),
                                rect.max - egui::vec2(TARGET, 6.0),
                            );
                            let response = ui.place(
                                edit,
                                egui::TextEdit::singleline(&mut self.query)
                                    .desired_width(edit.width())
                                    .font(egui::FontId::proportional(14.0))
                                    .hint_text("搜索 App 或窗口")
                                    .frame(false)
                                    .min_size(egui::vec2(0.0, 24.0)),
                            );
                            if !self.query.is_empty() {
                                let clear = egui::Rect::from_min_size(
                                    egui::pos2(rect.right() - TARGET, rect.top()),
                                    egui::vec2(TARGET, TARGET),
                                );
                                paint_glyph(ui.painter(), clear, Glyph::Close, palette.text_weak);
                                let response = ui.interact(
                                    clear,
                                    control_id("clear-search", ()),
                                    egui::Sense::click(),
                                );
                                response.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::WidgetType::Button,
                                        ui.is_enabled(),
                                        "清除搜索",
                                    )
                                });
                                if response.clicked() {
                                    self.query.clear();
                                    self.confirming = None;
                                }
                            }
                            if self.focus_search {
                                response.request_focus();
                                self.focus_search = false;
                            }
                            if response.changed() {
                                self.confirming = None;
                            }
                            if ui
                                .add_sized(
                                    [48.0, TARGET],
                                    egui::Button::new(
                                        egui::RichText::new("取消")
                                            .size(14.0)
                                            .color(palette.accent),
                                    )
                                    .frame(false),
                                )
                                .clicked()
                                || ui.input(|input| input.key_pressed(egui::Key::Escape))
                            {
                                self.searching = false;
                                self.query.clear();
                                self.confirming = None;
                            }
                        });
                        egui::ScrollArea::vertical()
                            .id_salt("library-search")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                self.search_results(
                                    ui,
                                    library,
                                    &apps,
                                    state,
                                    palette,
                                    &mut action,
                                );
                            });
                    } else if ctx.screen_rect().width() > ctx.screen_rect().height() {
                        self.landscape(ui, library, &apps, state, palette, &mut action);
                    } else {
                        self.header(ui, palette, false);
                        egui::ScrollArea::vertical()
                            .id_salt(("library-content", self.tab == Tab::All))
                            .auto_shrink([false, false])
                            .show(ui, |ui| match self.tab {
                                Tab::Running => self.running_page(
                                    ui,
                                    library,
                                    &apps,
                                    state,
                                    palette,
                                    &mut action,
                                ),
                                Tab::All => self.all_grid(ui, library, state, palette, &mut action),
                            });
                    }
                });
            });
        if action.is_some() {
            self.confirming = None;
            self.searching = false;
            self.query.clear();
        }
        action
    }
}

fn grid_columns(width: f32) -> usize {
    if width < 330.0 { 3 } else { 4 }
}

fn notice_row(ui: &mut egui::Ui, notice: &DeskNotice, action: &mut Option<LibraryAction>) {
    let message = match notice {
        DeskNotice::WindowGone { app } => format!("{app} 的窗口已关闭"),
        DeskNotice::WindowUnresponsive { app } => format!("{app} 暂无响应，请稍后再试"),
        DeskNotice::WindowTooLarge { app } => {
            format!("{app} 的最小宽度超出屏幕。分辨率保持固定，可在应用内调整布局或字号。")
        }
        DeskNotice::RecordUnwritable => "无法保存窗口还原记录，请检查数据目录权限".into(),
        DeskNotice::ScreenNotReady => "屏幕尚未就绪，请稍后再试".into(),
        DeskNotice::OpenFailed { app } => format!("无法打开 {app}"),
        DeskNotice::NoNewWindow { app } => format!("{app} 没有打开新窗口"),
        DeskNotice::NoWindow { app } => format!("未找到 {app} 的窗口"),
        DeskNotice::QuitStalled { app, .. } => {
            format!("{app} 尚未退出。请先处理保存对话框，或选择强制退出。")
        }
        DeskNotice::Reshaped => "屏幕分辨率已应用".into(),
        DeskNotice::ReshapeFailed => "未能应用分辨率，请检查驱动状态".into(),
        DeskNotice::ZoomLeftBehind { app, steps } => {
            format!("{app} 的字号保留了 {steps:+} 级，可在应用中手动还原")
        }
        DeskNotice::ZoomUncertain { app } => {
            format!("{app} 的字号操作已中断，结果不确定。请先在该应用中手动还原字号，再点击“已手动还原字号”以继续。")
        }
    };
    ui.label(message);
    ui.horizontal_wrapped(|ui| {
        if matches!(notice, DeskNotice::ZoomUncertain { .. }) {
            if ui
                .add(egui::Button::new("已手动还原字号").min_size(egui::vec2(TARGET, TARGET)))
                .clicked()
            {
                *action = Some(LibraryAction::ConfirmManualZoomReset);
            }
        } else if ui.button("稍后").clicked() {
            *action = Some(LibraryAction::DismissNotice);
        }
        if let DeskNotice::QuitStalled { app, pid } = notice
            && ui.button("强制退出").clicked()
        {
            *action = Some(LibraryAction::ForceQuit {
                pid: *pid,
                app: app.clone(),
            });
        }
    });
    ui.add_space(8.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use snug_core::windows::{AppInfo, WindowKey};

    fn fixture() -> Library {
        let app = AppInfo {
            app_id: "snug-fixture.exe".into(),
            name: "Fixture editor".into(),
            executable_path: None,
            launch_target: None,
            arguments: None,
            working_directory: None,
        };
        let window = |handle, title: &str| WindowInfo {
            key: WindowKey::new(991234, handle),
            app_name: app.name.clone(),
            app_id: Some(app.app_id.clone()),
            title: title.into(),
            minimized: false,
        };
        Library::with_data(
            vec![app.clone()],
            vec![RunningApp {
                app: app.clone(),
                pid: 991234,
                windows: vec![window(1, "First document"), window(2, "Second document")],
            }],
        )
    }

    fn frame(
        ctx: &egui::Context,
        view: &mut MobileLibraryUi,
        library: &mut Library,
        state: &DeskState,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, Option<LibraryAction>) {
        let mut action = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ctx| {
                action = view.show(
                    ctx,
                    library,
                    state,
                    crate::theme::palette(ctx.style().visuals.dark_mode),
                    false,
                    "Test screen",
                );
            },
        );
        (output, action)
    }

    fn text_rect(shape: &egui::epaint::Shape, text: &str) -> Option<egui::Rect> {
        match shape {
            egui::epaint::Shape::Text(shape) if shape.galley.text() == text => {
                Some(egui::Rect::from_min_size(shape.pos, shape.galley.size()))
            }
            egui::epaint::Shape::Vec(shapes) => {
                shapes.iter().find_map(|shape| text_rect(shape, text))
            }
            _ => None,
        }
    }

    fn click(
        text: &str,
        ctx: &egui::Context,
        view: &mut MobileLibraryUi,
        library: &mut Library,
        state: &DeskState,
        size: egui::Vec2,
    ) -> Option<LibraryAction> {
        frame(ctx, view, library, state, size, vec![]);
        let (output, _) = frame(ctx, view, library, state, size, vec![]);
        let rect = output
            .shapes
            .iter()
            .find_map(|shape| text_rect(&shape.shape, text))
            .unwrap_or_else(|| panic!("missing visible control {text}"));
        click_at(rect.center(), ctx, view, library, state, size)
    }

    fn click_id(
        id: egui::Id,
        ctx: &egui::Context,
        view: &mut MobileLibraryUi,
        library: &mut Library,
        state: &DeskState,
        size: egui::Vec2,
    ) -> Option<LibraryAction> {
        frame(ctx, view, library, state, size, vec![]);
        let response = ctx.read_response(id).expect("missing semantic control");
        assert!(response.rect.width() >= TARGET && response.rect.height() >= TARGET);
        click_at(response.rect.center(), ctx, view, library, state, size)
    }

    fn click_at(
        at: egui::Pos2,
        ctx: &egui::Context,
        view: &mut MobileLibraryUi,
        library: &mut Library,
        state: &DeskState,
        size: egui::Vec2,
    ) -> Option<LibraryAction> {
        let _ = frame(
            ctx,
            view,
            library,
            state,
            size,
            vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        frame(
            ctx,
            view,
            library,
            state,
            size,
            vec![egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        )
        .1
    }

    #[test]
    fn uncertain_zoom_notice_requires_explicit_manual_reset() {
        for dark in [false, true] {
            for size in [egui::vec2(375.0, 812.0), egui::vec2(812.0, 375.0)] {
                let ctx = egui::Context::default();
                crate::theme::apply(&ctx, dark);
                let mut view = MobileLibraryUi::default();
                let mut library = fixture();
                let state = DeskState {
                    notice: Some(DeskNotice::ZoomUncertain { app: "Fixture editor".into() }),
                    ..Default::default()
                };
                frame(&ctx, &mut view, &mut library, &state, size, vec![]);
                let (output, action) = frame(&ctx, &mut view, &mut library, &state, size, vec![]);
                assert!(action.is_none());
                assert!(output.shapes.iter().all(|shape| text_rect(&shape.shape, "稍后").is_none()));
                assert!(output.shapes.iter().any(|shape| {
                    text_rect(
                        &shape.shape,
                        "Fixture editor 的字号操作已中断，结果不确定。请先在该应用中手动还原字号，再点击“已手动还原字号”以继续。",
                    )
                    .is_some()
                }));
                assert_eq!(
                    click("已手动还原字号", &ctx, &mut view, &mut library, &state, size),
                    Some(LibraryAction::ConfirmManualZoomReset)
                );
            }
        }
    }

    #[test]
    fn ordinary_notice_keeps_the_later_action() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut view = MobileLibraryUi::default();
        let mut library = fixture();
        let state = DeskState {
            notice: Some(DeskNotice::WindowGone { app: "Fixture editor".into() }),
            ..Default::default()
        };
        assert_eq!(
            click("稍后", &ctx, &mut view, &mut library, &state, egui::vec2(375.0, 812.0)),
            Some(LibraryAction::DismissNotice)
        );
    }

    #[test]
    fn compact_icon_controls_keep_screen_and_new_window_actions() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut view = MobileLibraryUi::default();
        let mut library = fixture();
        let state = DeskState::default();
        let size = egui::vec2(320.0, 800.0);
        let (output, _) = frame(&ctx, &mut view, &mut library, &state, size, vec![]);
        for label in ["新建", "退出", "关闭", "设置", "退出屏幕"] {
            assert!(
                output
                    .shapes
                    .iter()
                    .all(|shape| text_rect(&shape.shape, label).is_none())
            );
        }
        assert_eq!(
            click_id(
                control_id("new-window", 991234),
                &ctx,
                &mut view,
                &mut library,
                &state,
                size
            ),
            Some(LibraryAction::NewWindow(library.running()[0].clone()))
        );
        assert_eq!(
            click_id(
                control_id("screens", ()),
                &ctx,
                &mut view,
                &mut library,
                &state,
                size
            ),
            Some(LibraryAction::Screens)
        );
    }

    #[test]
    fn running_windows_are_directly_selectable_without_a_sheet() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut view = MobileLibraryUi::default();
        let mut library = fixture();
        let state = DeskState::default();
        let size = egui::vec2(400.0, 900.0);
        assert_eq!(
            click(
                "Second document",
                &ctx,
                &mut view,
                &mut library,
                &state,
                size
            ),
            Some(LibraryAction::App(AppAction::Pull(
                library.running()[0].windows[1].clone()
            )))
        );
    }

    #[test]
    fn closing_a_window_requires_the_second_inline_confirmation() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut view = MobileLibraryUi::default();
        let mut library = fixture();
        let state = DeskState::default();
        let size = egui::vec2(400.0, 900.0);
        assert_eq!(
            click_id(
                control_id("close-window", WindowKey::new(991234, 1)),
                &ctx,
                &mut view,
                &mut library,
                &state,
                size
            ),
            None
        );
        assert_eq!(
            click("确认关闭", &ctx, &mut view, &mut library, &state, size),
            Some(LibraryAction::CloseWindow(
                library.running()[0].windows[0].clone()
            ))
        );
    }

    #[test]
    fn landscape_uses_an_app_sidebar_and_direct_window_detail() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut view = MobileLibraryUi::default();
        let mut library = fixture();
        let (output, _) = frame(
            &ctx,
            &mut view,
            &mut library,
            &DeskState::default(),
            egui::vec2(940.0, 525.0),
            vec![],
        );
        let locate = |text| {
            output
                .shapes
                .iter()
                .find_map(|shape| text_rect(&shape.shape, text))
                .unwrap()
        };
        assert!(
            locate("First document").left() > locate("运行中").right(),
            "landscape detail must be beside the app selector"
        );
    }

    #[test]
    fn quitting_and_dismissing_require_inline_confirmation_and_can_be_cancelled() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut view = MobileLibraryUi::default();
        let mut library = fixture();
        let state = DeskState::default();
        let size = egui::vec2(400.0, 900.0);
        assert_eq!(
            click_id(
                control_id("quit", 991234),
                &ctx,
                &mut view,
                &mut library,
                &state,
                size
            ),
            None
        );
        assert_eq!(
            click("取消", &ctx, &mut view, &mut library, &state, size),
            None
        );
        assert!(view.confirming.is_none());
        assert_eq!(
            click_id(
                control_id("quit", 991234),
                &ctx,
                &mut view,
                &mut library,
                &state,
                size
            ),
            None
        );
        assert_eq!(
            click("确认退出", &ctx, &mut view, &mut library, &state, size),
            Some(LibraryAction::QuitApp(library.running()[0].clone()))
        );
        view.request_dismiss();
        assert_eq!(view.confirming, Some(Confirm::Dismiss));
        assert_eq!(
            click("确认退出屏幕", &ctx, &mut view, &mut library, &state, size),
            Some(LibraryAction::Dismiss)
        );
    }

    #[test]
    fn window_title_search_selects_the_exact_window_instead_of_its_first_sibling() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut view = MobileLibraryUi {
            searching: true,
            query: "second".into(),
            ..Default::default()
        };
        let mut library = fixture();
        let state = DeskState::default();
        assert_eq!(
            click(
                "Second document",
                &ctx,
                &mut view,
                &mut library,
                &state,
                egui::vec2(400.0, 900.0)
            ),
            Some(LibraryAction::App(AppAction::Pull(
                library.running()[0].windows[1].clone()
            )))
        );
        assert!(!view.searching);
    }

    #[test]
    fn search_clear_and_cancel_remain_separate_at_narrow_widths() {
        for width in [280.0, 320.0, 400.0] {
            let ctx = egui::Context::default();
            crate::theme::apply(&ctx, false);
            let mut view = MobileLibraryUi::default();
            view.set_preview_page(false, Some("Fixture".into()));
            let mut library = fixture();
            let state = DeskState::default();
            let size = egui::vec2(width, 800.0);
            frame(&ctx, &mut view, &mut library, &state, size, vec![]);
            let (output, _) = frame(&ctx, &mut view, &mut library, &state, size, vec![]);
            let clear = ctx
                .read_response(control_id("clear-search", ()))
                .unwrap()
                .rect;
            let cancel = output
                .shapes
                .iter()
                .find_map(|shape| text_rect(&shape.shape, "取消"))
                .unwrap();
            assert!(
                clear.right() < cancel.left(),
                "clear and cancel overlap at {width}"
            );
            click_id(
                control_id("clear-search", ()),
                &ctx,
                &mut view,
                &mut library,
                &state,
                size,
            );
            assert!(view.searching && view.query.is_empty());
            view.query = "Fixture".into();
            click("取消", &ctx, &mut view, &mut library, &state, size);
            assert!(!view.searching && view.query.is_empty());
        }
    }

    #[test]
    fn busy_work_disables_window_actions_and_names_the_operation() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut view = MobileLibraryUi::default();
        let mut library = fixture();
        let state = DeskState::default().with_work(Some(snug_core::desk::DeskWork::with_label(
            DeskWorkKind::Opening,
            "Fixture editor",
        )));
        let size = egui::vec2(400.0, 900.0);
        assert_eq!(
            click(
                "Second document",
                &ctx,
                &mut view,
                &mut library,
                &state,
                size
            ),
            None
        );
        let (output, _) = frame(&ctx, &mut view, &mut library, &state, size, vec![]);
        assert!(
            output
                .shapes
                .iter()
                .any(|shape| text_rect(&shape.shape, "正在打开 · Fixture editor").is_some())
        );
    }

    #[test]
    fn stale_window_confirmation_and_app_selection_are_removed_before_rendering() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut view = MobileLibraryUi {
            selected: Some(991234),
            confirming: Some(Confirm::Close(WindowKey::new(991234, 2))),
            ..Default::default()
        };
        let mut library = Library::with_data(Vec::new(), Vec::new());
        frame(
            &ctx,
            &mut view,
            &mut library,
            &DeskState::default(),
            egui::vec2(400.0, 900.0),
            vec![],
        );
        assert!(view.confirming.is_none());
        assert!(view.selected.is_none());
    }

    #[test]
    fn cards_search_confirmation_and_grid_fit_small_portrait_and_landscape() {
        for dark in [false, true] {
            for size in [
                egui::vec2(280.0, 800.0),
                egui::vec2(320.0, 800.0),
                egui::vec2(400.0, 900.0),
                egui::vec2(560.0, 320.0),
                egui::vec2(940.0, 525.0),
            ] {
                for mode in 0..4 {
                    let ctx = egui::Context::default();
                    crate::theme::apply(&ctx, dark);
                    let mut view = MobileLibraryUi::default();
                    let mut library = fixture();
                    let mut apps = library.running().to_vec();
                    apps[0].app.name = "A very long application name that must never push controls outside the display".into();
                    apps[0].windows[0].title = "A very long document title that must be truncated before the close control".into();
                    let installed = (0..9)
                        .map(|index| AppInfo {
                            app_id: format!("fixture-grid-{index}.exe"),
                            ..apps[0].app.clone()
                        })
                        .collect();
                    library = Library::with_data(installed, apps);
                    match mode {
                        1 => view.tab = Tab::All,
                        2 => {
                            view.searching = true;
                            view.query = "document".into();
                        }
                        3 => view.confirming = Some(Confirm::Close(WindowKey::new(991234, 1))),
                        _ => {}
                    }
                    let mut output = egui::FullOutput::default();
                    for _ in 0..3 {
                        output = frame(
                            &ctx,
                            &mut view,
                            &mut library,
                            &DeskState::default(),
                            size,
                            vec![],
                        )
                        .0;
                    }
                    let right = output
                        .shapes
                        .iter()
                        .map(|shape| shape.shape.visual_bounding_rect().right())
                        .fold(0.0_f32, f32::max);
                    assert!(
                        right <= size.x + 1.0,
                        "content reaches {right} at {size:?} mode={mode} dark={dark}"
                    );
                }
            }
        }
    }

    #[test]
    fn current_window_returns_without_repulling_and_keeps_restore_and_zoom_controls() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, false);
        let mut view = MobileLibraryUi::default();
        let mut library = fixture();
        let mut window = library.running()[0].windows[1].clone();
        window.app_id = Some("Code.exe".into());
        let mut state = DeskState::default()
            .with_placed(Some(snug_core::desk::PlacedWindow { window, zoom: 0 }));
        let size = egui::vec2(400.0, 1000.0);
        assert_eq!(
            click(
                "Second document",
                &ctx,
                &mut view,
                &mut library,
                &state,
                size
            ),
            Some(LibraryAction::ReturnPlaced)
        );
        assert_eq!(
            click("还原", &ctx, &mut view, &mut library, &state, size),
            Some(LibraryAction::RestorePlaced)
        );
        assert_eq!(
            click("字号 −", &ctx, &mut view, &mut library, &state, size),
            None
        );
        assert_eq!(
            click("字号 +", &ctx, &mut view, &mut library, &state, size),
            Some(LibraryAction::ZoomIn)
        );
        state.placed.as_mut().unwrap().zoom = snug_core::config::MAX_ZOOM_STEPS;
        assert_eq!(
            click("字号 +", &ctx, &mut view, &mut library, &state, size),
            None
        );
    }
}
