use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, ViewportBuilder, ViewportClass, ViewportCommand, ViewportId};
use snug_core::{AppConfig, Rect};
use snug_win::display::{PhysicalMonitors, WinPhoneDisplays};

use crate::projection::Projection;
use crate::settings::{SettingsAction, SettingsOutcome, SettingsTab, SettingsWindow};

const TITLE: &str = "Snug 桌面设置";
const STATUS_REPAINT: Duration = Duration::from_millis(150);

pub(crate) struct DesktopSettings {
    state: Arc<Mutex<DesktopState>>,
    displays: Arc<WinPhoneDisplays>,
}

struct DesktopState {
    settings: SettingsWindow,
    config: AppConfig,
    dirty: bool,
    actions: Vec<SettingsAction>,
    repaint: bool,
    projection: Option<Projection>,
    window: Option<u64>,
    visible: bool,
    focus: bool,
    display_id: Option<u32>,
}

impl DesktopSettings {
    pub(crate) fn new(config: &AppConfig, displays: Arc<WinPhoneDisplays>) -> Self {
        Self {
            state: Arc::new(Mutex::new(DesktopState {
                settings: SettingsWindow::new(),
                config: config.clone(),
                dirty: false,
                actions: Vec::new(),
                repaint: false,
                projection: None,
                window: None,
                visible: false,
                focus: false,
                display_id: None,
            })),
            displays,
        }
    }

    pub(crate) fn open(&self, tab: SettingsTab) {
        let mut state = self.state.lock().expect("desktop settings");
        state.settings.select_tab(tab);
        if !state.settings.is_open() {
            state.settings.open();
        }
        state.projection = None;
        state.focus = true;
        state.repaint = true;
    }

    pub(crate) fn open_projection(&self) {
        self.open(SettingsTab::Screens);
        let mut state = self.state.lock().expect("desktop settings");
        state.projection = Some(Projection::new(Arc::clone(&self.displays)));
    }

    pub(crate) fn close(&self) {
        self.state.lock().expect("desktop settings").close();
    }

    pub(crate) fn is_open(&self) -> bool {
        self.state
            .lock()
            .expect("desktop settings")
            .settings
            .is_open()
    }

    pub(crate) fn needs_logs(&self) -> bool {
        let state = self.state.lock().expect("desktop settings");
        state.projection.is_none() && state.settings.needs_logs()
    }

    pub(crate) fn sync_runtime_from(&self, source: &SettingsWindow) {
        let mut state = self.state.lock().expect("desktop settings");
        if state.settings.is_open() {
            state.settings.sync_runtime_from(source);
        }
    }

    pub(crate) fn sync_config(&self, config: &AppConfig) {
        let mut state = self.state.lock().expect("desktop settings");
        let DesktopState {
            settings,
            config: local,
            dirty,
            repaint,
            ..
        } = &mut *state;
        if !*dirty && local != config {
            settings.sync_committed_screens(local, config);
            local.clone_from(config);
            *repaint = true;
        }
    }

    pub(crate) fn take_changes(&self) -> (Option<AppConfig>, Vec<SettingsAction>) {
        let mut state = self.state.lock().expect("desktop settings");
        let config = std::mem::take(&mut state.dirty).then(|| state.config.clone());
        (config, std::mem::take(&mut state.actions))
    }

    pub(crate) fn show(
        &self,
        ctx: &egui::Context,
        physical: Option<Arc<PhysicalMonitors>>,
        display_id: Option<u32>,
    ) {
        let repaint = {
            let mut state = self.state.lock().expect("desktop settings");
            if !state.settings.is_open() {
                return;
            }
            if state.display_id != display_id {
                if let Some(projection) = &mut state.projection {
                    projection.suspend(ctx);
                }
                state.display_id = display_id;
                state.repaint = true;
            }
            std::mem::take(&mut state.repaint) || !state.visible
        };
        let viewport = ViewportId::from_hash_of("snug-desktop-settings");
        let builder = ViewportBuilder::default()
            .with_title(TITLE)
            .with_visible(false)
            .with_inner_size([720.0, 620.0])
            .with_min_inner_size([380.0, 300.0]);
        let state = Arc::clone(&self.state);
        let displays = Arc::clone(&self.displays);
        let child_monitors = physical.clone();
        ctx.show_viewport_deferred(viewport, builder, move |child, class| {
            let Ok(mut state) = state.try_lock() else {
                child.request_repaint();
                return;
            };
            if !state.settings.is_open() {
                return;
            }
            if class == ViewportClass::Embedded || child.input(|i| i.viewport().close_requested()) {
                state.close();
                child.request_repaint_of(ViewportId::ROOT);
                return;
            }
            state.draw(child, child_monitors.as_deref(), &displays);
        });
        {
            let mut state = self.state.lock().expect("desktop settings");
            // A hidden deferred viewport may receive no redraw event until it is visible.
            // Once winit has created the HWND, reveal it from this root pass.
            if !state.visible
                && physical.as_ref().is_some_and(|monitors| state.ensure_physical_position(monitors))
            {
                state.set_visibility(ctx, viewport, true);
                if let Some(window) = state.window {
                    snug_win::request_own_window_redraw(window);
                }
            }
            state.focus_if_requested(ctx, viewport);
        }
        if repaint {
            ctx.request_repaint_of(viewport);
        } else {
            ctx.request_repaint_after_for(STATUS_REPAINT, viewport);
        }
    }
}

impl DesktopState {
    fn close(&mut self) {
        self.settings.close();
        self.projection = None;
        self.window = None;
        self.visible = false;
        self.focus = false;
    }

    fn set_visibility(&mut self, ctx: &egui::Context, viewport: ViewportId, visible: bool) {
        if self.visible != visible {
            ctx.send_viewport_cmd_to(viewport, ViewportCommand::Visible(visible));
            self.visible = visible;
            if visible {
                crate::info!("桌面设置：已在实体屏显示");
            }
        }
    }

    fn focus_if_requested(&mut self, ctx: &egui::Context, viewport: ViewportId) {
        if self.visible && std::mem::take(&mut self.focus) {
            ctx.send_viewport_cmd_to(viewport, ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd_to(viewport, ViewportCommand::Focus);
        }
    }

    fn draw(
        &mut self,
        ctx: &egui::Context,
        physical: Option<&PhysicalMonitors>,
        displays: &Arc<WinPhoneDisplays>,
    ) {
        let placed = physical.is_some_and(|monitors| self.ensure_physical_position(monitors));
        self.set_visibility(ctx, ctx.viewport_id(), placed);
        if !placed {
            if let Some(projection) = &mut self.projection {
                projection.suspend(ctx);
            }
            return;
        }
        self.focus_if_requested(ctx, ctx.viewport_id());
        if let Some(projection) = &mut self.projection {
            if projection.show(ctx, self.display_id) {
                self.projection = None;
                ctx.request_repaint();
            }
            return;
        }

        let outcome = self.settings.show_desktop(ctx, &mut self.config);
        self.dirty |= outcome == SettingsOutcome::Changed;
        for action in self.settings.take_actions() {
            if action == SettingsAction::OpenProjection {
                self.projection = Some(Projection::new(Arc::clone(displays)));
                ctx.request_repaint();
            } else {
                self.actions.push(action);
            }
        }
        if outcome == SettingsOutcome::CloseRequested {
            self.close();
        }
        if outcome != SettingsOutcome::Idle || !self.actions.is_empty() {
            ctx.request_repaint_of(ViewportId::ROOT);
        }
    }

    fn ensure_physical_position(&mut self, physical: &PhysicalMonitors) -> bool {
        if !physical.geometry_is_current() {
            return false;
        }
        let Some(window) = self.window.or_else(|| snug_win::find_own_window(TITLE)) else {
            return false;
        };
        let Some(bounds) = snug_win::window::own_window_rect(window) else {
            self.window = None;
            return false;
        };
        self.window = Some(window);
        if self.visible && physical.contains_physical_point(bounds.center()) {
            return true;
        }
        let Some(work_area) = physical.preferred_work_area(bounds.center()) else {
            return false;
        };
        let target = clamp_to_work_area(bounds, work_area);
        bounds.matches(target, 1.0) || snug_win::window::position_own_window(window, target)
    }
}

fn clamp_to_work_area(window: Rect, work: Rect) -> Rect {
    let width = window.width.min(work.width);
    let height = window.height.min(work.height);
    let (x, y) = if work.contains(window.center()) {
        (window.x, window.y)
    } else {
        (
            work.x + (work.width - width) / 2.0,
            work.y + (work.height - height) / 2.0,
        )
    };
    Rect::new(
        x.clamp(work.min_x(), work.max_x() - width),
        y.clamp(work.min_y(), work.max_y() - height),
        width,
        height,
    )
}

#[cfg(test)]
#[test]
fn desktop_settings_moves_from_virtual_coordinates_into_the_physical_work_area() {
    let physical = Rect::new(-1920.0, 0.0, 1920.0, 1040.0);
    let virtual_window = Rect::new(2600.0, 100.0, 780.0, 640.0);
    let moved = clamp_to_work_area(virtual_window, physical);
    assert_eq!(moved, Rect::new(-1350.0, 200.0, 780.0, 640.0));
    assert_eq!(clamp_to_work_area(moved, physical), moved);
}

#[cfg(test)]
#[test]
fn desktop_settings_draws_as_a_deferred_viewport() {
    let ctx = egui::Context::default();
    ctx.set_embed_viewports(false);
    let desktop = DesktopSettings::new(&AppConfig::default(), Arc::new(WinPhoneDisplays::new()));
    desktop.open(SettingsTab::Screens);
    let render_root = || {
        ctx.run(egui::RawInput::default(), |ctx| {
            desktop.show(ctx, None, None)
        })
    };
    let viewport = ViewportId::from_hash_of("snug-desktop-settings");

    let output = render_root();
    let child = &output.viewport_output[&viewport];
    assert!(child.class == ViewportClass::Deferred);
    assert_eq!(child.builder.visible, Some(false));
    assert!(child.viewport_ui_cb.is_some());
    assert!(desktop.is_open());

    desktop.close();
    assert!(!render_root().viewport_output.contains_key(&viewport));
}

#[cfg(test)]
#[test]
fn desktop_settings_retains_uncommitted_changes_until_root_consumes_them() {
    let config = AppConfig::default();
    let desktop = DesktopSettings::new(&config, Arc::new(WinPhoneDisplays::new()));
    desktop.open(SettingsTab::Screens);
    {
        let mut state = desktop.state.lock().unwrap();
        state
            .config
            .set_phone_screen_mode(snug_core::PhoneScreenMode::Always);
        state.dirty = true;
        state.actions.push(SettingsAction::OpenPreview);
    }

    desktop.sync_config(&config);
    desktop.close();
    let (pending, actions) = desktop.take_changes();
    assert_eq!(
        pending.unwrap().phone_screen_mode(),
        snug_core::PhoneScreenMode::Always
    );
    assert_eq!(actions, [SettingsAction::OpenPreview]);

    desktop.sync_config(&config);
    assert_eq!(desktop.state.lock().unwrap().config, config);
    assert_eq!(desktop.take_changes(), (None, Vec::new()));
}
