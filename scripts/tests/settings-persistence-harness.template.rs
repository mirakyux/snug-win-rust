//! Exercise the production UI change/save methods without starting native displays,
//! hooks, tray icons or controller threads. Only their external collaborators are fakes.
#![allow(dead_code)]

use std::cell::Cell;
use std::path::PathBuf;

#[macro_export]
macro_rules! info { ($($value:tt)*) => {{ let _ = format_args!($($value)*); }} }
#[macro_export]
macro_rules! warn { ($($value:tt)*) => {{ let _ = format_args!($($value)*); }} }

#[derive(Clone, Debug, Default, PartialEq)]
struct AppConfig {
    mode: u8,
    screen: u8,
    phone: u8,
    zoom_steps: i32,
    gestures: bool,
    screenshot: bool,
    theme: u8,
    auto_check_updates: bool,
    screens: Vec<ScreenConfig>,
}
#[derive(Clone, Debug, PartialEq)]
struct ScreenConfig {
    id: String,
    enabled: bool,
}
impl AppConfig {
    fn effective_screens(&self) -> Vec<ScreenConfig> {
        self.screens.clone()
    }
    fn primary_touch_screen(&self) -> Option<ScreenConfig> {
        self.screens.first().cloned()
    }
    fn phone_screen_mode(&self) -> u8 {
        self.mode
    }
    fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(path, format!("{self:?}"))
    }
}
struct Paths(PathBuf);
impl Paths {
    fn config_file(&self) -> PathBuf {
        self.0.join("config.json")
    }
}
#[derive(Default)]
struct Controller {
    mode: Cell<u8>,
    active_screen: std::cell::RefCell<Option<String>>,
}
impl Controller {
    fn set_active_screen(&self, active: Option<String>) {
        *self.active_screen.borrow_mut() = active;
    }
    fn set_phone_screen_mode(&self, mode: u8) {
        self.mode.set(mode);
    }
    fn apply_screens(&self, _: &AppConfig) {}
    fn set_default_zoom(&self, _: i32) {}
    fn apply_gesture_settings(&self, _: &AppConfig) {}
}
#[derive(Default)]
struct SettingsWindow {
    config_save_error: Option<String>,
}
impl SettingsWindow {
    fn sync_committed_screens(&mut self, _: &AppConfig, _: &AppConfig) {}
    fn set_config_save_error(&mut self, error: Option<String>) {
        self.config_save_error = error;
    }
}
struct DesktopSettings;
impl DesktopSettings {
    fn sync_config(&self, _: &AppConfig) {}
    fn sync_runtime_from(&self, _: &SettingsWindow) {}
}
#[derive(PartialEq)]
enum SettingsOutcome {
    Idle,
    Changed,
    CloseRequested,
}
#[derive(Clone, Copy)]
enum ChangeImpact {
    RebuildPhoneScreen,
    ReloadGestures,
    Screens,
    Theme,
    RefreshHomeBar,
    None,
}
mod settings {
    use super::*;
    pub(super) fn classify_change(_: &AppConfig, _: &AppConfig) -> ChangeImpact {
        ChangeImpact::None
    }
    pub(super) fn change_hint(_: ChangeImpact) -> Option<&'static str> {
        None
    }
}
mod egui {
    pub(super) struct Context;
    pub(super) struct ViewportId;
    impl ViewportId {
        pub(super) const ROOT: Self = Self;
    }
    impl Context {
        pub(super) fn request_repaint_of(&self, _: ViewportId) {}
    }
}
struct PhoneHomeApp {
    config: AppConfig,
    paths: Paths,
    controller: Controller,
    settings: SettingsWindow,
    desktop_settings: DesktopSettings,
    active_screen: Option<String>,
    preview: Option<PreviewState>,
    last_phone_rect: Option<()>,
    last_offscreen: Option<bool>,
}
struct PreviewState {
    active_screen: Option<String>,
}
/* PRODUCTION_SELECTION_HELPER */
impl PhoneHomeApp {
    /* PRODUCTION_METHODS */
}
struct Fixture(PhoneHomeApp);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "snug-settings-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let app = PhoneHomeApp {
            config: AppConfig::default(),
            paths: Paths(dir),
            controller: Controller::default(),
            settings: SettingsWindow::default(),
            desktop_settings: DesktopSettings,
            active_screen: None,
            preview: None,
            last_phone_rect: None,
            last_offscreen: None,
        };
        app.config.save(&app.paths.config_file()).unwrap();
        Self(app)
    }
    fn assert_saved(&self) {
        assert_eq!(
            std::fs::read_to_string(self.0.paths.config_file()).unwrap(),
            format!("{:?}", self.0.config)
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0.paths.0);
    }
}

#[test]
fn every_applied_config_change_is_saved_before_exit() {
    let mut edits = Vec::new();
    for field in 0..8 {
        let mut config = AppConfig::default();
        match field {
            0 => config.screen = 1,
            1 => config.phone = 1,
            2 => config.zoom_steps = 1,
            3 => config.gestures = true,
            4 => config.screenshot = true,
            5 => config.theme = 1,
            6 => config.auto_check_updates = true,
            _ => config.mode = 1,
        }
        edits.push(config);
    }
    for config in edits {
        let mut fixture = Fixture::new();
        let before = fixture.0.config.clone();
        fixture.0.config = config;
        fixture
            .0
            .apply_settings_change(&egui::Context, &before, SettingsOutcome::Changed);
        fixture.assert_saved();
    }
}

#[test]
fn mode_change_still_reaches_the_controller_and_disk() {
    let mut fixture = Fixture::new();
    let before = fixture.0.config.clone();
    fixture.0.config.mode = 2;
    fixture
        .0
        .apply_settings_change(&egui::Context, &before, SettingsOutcome::Changed);
    assert_eq!(fixture.0.controller.mode.get(), 2);
    fixture.assert_saved();
}

#[test]
fn idle_or_close_does_not_persist_drafts() {
    for outcome in [SettingsOutcome::Idle, SettingsOutcome::CloseRequested] {
        let mut fixture = Fixture::new();
        let before = fixture.0.config.clone();
        fixture.0.config.screen = 2;
        fixture
            .0
            .apply_settings_change(&egui::Context, &before, outcome);
        assert_eq!(
            std::fs::read_to_string(fixture.0.paths.config_file()).unwrap(),
            format!("{before:?}")
        );
    }
}

#[test]
fn failed_save_is_reported_and_successful_retry_clears_it() {
    let mut fixture = Fixture::new();
    let before = fixture.0.config.clone();
    fixture.0.config.screen = 2;
    let path = fixture.0.paths.config_file();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    fixture
        .0
        .apply_settings_change(&egui::Context, &before, SettingsOutcome::Changed);
    assert!(
        fixture.0.settings.config_save_error.is_some(),
        "save failure must reach settings UI"
    );
    std::fs::remove_dir(&path).unwrap();
    fixture.0.save_config();
    assert!(fixture.0.settings.config_save_error.is_none());
    fixture.assert_saved();
}

#[test]
fn deletion_repairs_the_real_selection_and_controller_independently_of_preview() {
    let mut fixture = Fixture::new();
    fixture.0.config.screens = vec![
        ScreenConfig {
            id: "first".into(),
            enabled: true,
        },
        ScreenConfig {
            id: "second".into(),
            enabled: true,
        },
    ];
    fixture.0.active_screen = Some("first".into());
    fixture.0.preview = Some(PreviewState {
        active_screen: Some("first".into()),
    });
    let before = fixture.0.config.clone();
    fixture.0.config.screens.remove(0);
    fixture
        .0
        .apply_settings_change(&egui::Context, &before, SettingsOutcome::Changed);
    assert_eq!(fixture.0.active_screen.as_deref(), Some("second"));
    assert_eq!(
        fixture.0.controller.active_screen.borrow().as_deref(),
        Some("second")
    );
    assert_eq!(
        fixture.0.preview.as_ref().unwrap().active_screen.as_deref(),
        Some("second")
    );
    fixture.assert_saved();
}
