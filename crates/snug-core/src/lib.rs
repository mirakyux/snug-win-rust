//! SnugWin 的纯逻辑层（Rust 重写）。
//!
//! 这一层刻意不依赖任何 Win32 API，所有平台副作用都在 `snug-win`。
//! 移植自 `nicocart/snug-win`（MIT）的 `SnugWin.Core`，行为以 C# 版为准。

pub mod apps;
pub mod config;
pub mod crop;
pub mod desk;
pub mod geometry;
pub mod left_behind;
pub mod paths;
pub mod phone_models;
pub mod pinyin;
pub mod record;
pub mod screen_fit;
pub mod screenshots;
pub mod selection;
pub mod session;
pub mod scrolling;
pub mod store;
pub mod touch;
pub mod update;
pub mod windows;
pub mod zoom;

pub use apps::{AppAction, AppSearchResult, LetterGroup};
pub use config::AppConfig;
pub use geometry::{Point, Rect, Size};
pub use left_behind::{LeftBehindBook, LeftBehindEntry};
pub use paths::Paths;
pub use phone_models::{PhoneCatalog, PhoneChoice, PhoneModel, PhoneScreenSpec};
pub use record::PhoneRecord;
pub use screen_fit::{PhoneSpec, ScreenFit};
pub use screenshots::{
    CaptureError, CaptureFailure, CapturedImage, ScreenshotControl, ScreenshotNotice,
    ScreenshotOverlay, Screenshots,
};
pub use scrolling::{Momentum, MomentumPhase, ScrollPhase, ScrollStep};
pub use selection::{
    SelectionBubbleAction, SelectionKeystroke, is_modern_terminal, is_terminal,
    resolve as resolve_selection_shortcut,
};
pub use session::{
    ConnectionEvent, ConnectionTracker, ControllerKind, ControllerTable, ControllerTableParser,
    EpisodeChange, RemoteSession, SessionEpisode, SessionTransition, UUSignals, UuElement,
    classify, wants_phone_screen,
};
pub use store::{JsonFileStore, RestoreRecordStore};
pub use touch::{
    CloseButton, TouchCommand, TouchInput, TouchInputKind, TouchOptions, TouchRegion,
    TouchResponse, TouchTranslator,
};
pub use update::{
    ReleasePackageKind, UpdateAssetInfo, UpdateReleaseInfo, build_task_xml, is_newer, normalize,
    parse_latest_release, preferred_asset_name, try_find_sha256, try_parse_semver,
};
pub use windows::{AppInfo, DisplayModeInfo, OtherDisplay, RunningApp, WindowInfo, WindowKey};
pub use zoom::{ZoomBook, ZoomScope};
