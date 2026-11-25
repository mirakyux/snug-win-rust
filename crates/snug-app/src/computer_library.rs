use eframe::egui;
use snug_core::Rect;
use snug_core::apps::AppAction;
use snug_core::windows::{AppInfo, WindowInfo};

use crate::library::Library;
use crate::mobile_library::LibraryAction;

#[derive(Default)]
pub(crate) struct ComputerLibrary {
    pub(crate) expanded: bool,
    query: String,
    focus_search: bool,
}

pub(crate) enum ComputerAction {
    Focus(WindowInfo),
    Library(LibraryAction),
}

impl ComputerLibrary {
    pub(crate) fn request_focus(&self) -> bool {
        self.focus_search
    }

    pub(crate) fn bounds(&self, screen: Rect, scale: f64) -> Rect {
        let (width, height) = if self.expanded {
            (460.0, 660.0)
        } else {
            (46.0, 116.0)
        };
        let width = screen.width.min(width * scale);
        let height = screen.height.min(height * scale);
        Rect::new(
            screen.x,
            screen.y + (screen.height - height) / 2.0,
            width,
            height,
        )
    }

    pub(crate) fn show(
        &mut self,
        ctx: &egui::Context,
        library: &mut Library,
        screen: Rect,
        busy: bool,
        placed: bool,
        preview: bool,
    ) -> Option<ComputerAction> {
        let mut action = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            crate::theme::compact_controls(ui);
            if !self.expanded {
                if ui
                    .add_sized(ui.available_size(), egui::Button::new("应用\n资源库"))
                    .on_hover_text("打开电脑屏 App 资源库")
                    .clicked()
                {
                    self.expanded = true;
                    self.focus_search = true;
                    if library.needs_scan(true) {
                        library.start_scan();
                    }
                    ctx.request_repaint();
                }
                return;
            }
            let mut close = ctx.input(|input| input.key_pressed(egui::Key::Escape));
            ui.horizontal_wrapped(|ui| {
                ui.heading("App 资源库");
                close |= ui
                    .button(if preview { "关闭预览" } else { "关闭" })
                    .clicked();
                if ui.button("切换屏幕").clicked() {
                    action = Some(ComputerAction::Library(LibraryAction::Screens));
                }
                if placed
                    && ui
                        .add_enabled(!busy, egui::Button::new("还原窗口"))
                        .clicked()
                {
                    action = Some(ComputerAction::Library(LibraryAction::RestorePlaced));
                }
            });
            if close {
                self.expanded = false;
                ctx.request_repaint();
                if preview {
                    action = Some(ComputerAction::Library(LibraryAction::ClosePreview));
                }
                return;
            }
            let search = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("搜索应用或窗口")
                    .desired_width(f32::INFINITY),
            );
            if std::mem::take(&mut self.focus_search) {
                search.request_focus();
            }
            if !library.scanned_once {
                ui.spinner();
                return;
            }
            let rows = rows(library, screen, &self.query);
            if rows.iter().all(Vec::is_empty) {
                ui.label("没有匹配的应用或窗口");
            }
            let submit = !busy
                && (search.has_focus() || search.lost_focus())
                && ui.input(|input| input.key_pressed(egui::Key::Enter))
                && !self.query.trim().is_empty();
            ui.add_enabled_ui(!busy, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (heading, group) in
                            ["这块屏", "其他位置", "没有窗口"].into_iter().zip(rows)
                        {
                            if group.is_empty() {
                                continue;
                            }
                            ui.add_space(8.0);
                            ui.strong(heading);
                            for row in group {
                                let (label, verb) = row.text();
                                let clicked = ui
                                    .horizontal(|ui| {
                                        let clicked = ui.button(verb).clicked();
                                        ui.add(egui::Label::new(label).truncate());
                                        clicked
                                    })
                                    .inner;
                                if clicked || (submit && action.is_none()) {
                                    action = Some(row.into_action());
                                }
                            }
                        }
                    });
            });
            if !preview
                && matches!(
                    action,
                    Some(ComputerAction::Focus(_) | ComputerAction::Library(LibraryAction::App(_)))
                )
            {
                self.expanded = false;
                ctx.request_repaint();
            }
        });
        action
    }
}

enum Row {
    Window(WindowInfo, bool),
    App(AppInfo),
}

impl Row {
    fn text(&self) -> (String, &'static str) {
        match self {
            Self::Window(window, true) => (window.label(), "切换"),
            Self::Window(window, false) => (window.label(), "移至此屏"),
            Self::App(app) => (app.name.clone(), "打开"),
        }
    }

    fn into_action(self) -> ComputerAction {
        match self {
            Self::Window(window, true) => ComputerAction::Focus(window),
            Self::Window(window, false) => {
                ComputerAction::Library(LibraryAction::App(AppAction::Pull(window)))
            }
            Self::App(app) => ComputerAction::Library(LibraryAction::App(AppAction::Open(app))),
        }
    }
}

fn rows(library: &Library, screen: Rect, query: &str) -> [Vec<Row>; 3] {
    let mut groups = [Vec::new(), Vec::new(), Vec::new()];
    for app in library.running() {
        let app_matches = library.matches_search(&app.app, query);
        for window in &app.windows {
            if !app_matches && !snug_core::pinyin::matches(&window.title, query.trim()) {
                continue;
            }
            let here = !window.minimized
                && library
                    .frame(window.key)
                    .is_some_and(|frame| screen.contains(frame.center()));
            groups[usize::from(!here)].push(Row::Window(window.clone(), here));
        }
    }
    groups[2].extend(
        library
            .filter_installed(query)
            .into_iter()
            .filter(|app| {
                !library.running().iter().any(|running| {
                    !running.windows.is_empty()
                        && running.app.app_id.eq_ignore_ascii_case(&app.app_id)
                })
            })
            .map(Row::App),
    );
    groups
}
