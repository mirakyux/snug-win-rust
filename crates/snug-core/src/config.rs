//! 配置，移植自 C# v1.3.0 `SnugWin.Core.Config.AppConfig` 与 `ScreenConfig`。
//!
//! v1.3 从「一块手机屏」变成「最多四块屏」：`phone` 是旧版单屏字段，
//! `screens` 是新版的屏列表，`theme` 是新增的明暗主题。三者的关系按 C# 原样：
//! - 老配置只有 `phone`，读进来时迁移成 `screens[0]`（id 固定为 [`ScreenConfig::LEGACY_PHONE_ID`]）；
//! - `phone` 字段**保留**，由迁移逻辑保持一致（应用层还在读它）；
//! - 写回磁盘时按新版格式写 `Screens` + `Theme`，同时把 `Phone` 一起写上，方便 C# 版读。
//!
//! 与 C# 版一致：字段名大小写不敏感（老配置里是 PascalCase），
//! 读不出或数值越界时回落到默认值，绝不因为一份坏配置起不来。

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::phone_models::{
    DeviceKind, PHONE_SCREEN_NAME, PhoneCatalog, PhoneChoice, PhoneScreenSpec, ScreenOrientation,
};

pub const MIN_ZOOM_STEPS: i32 = 0;
pub const MAX_ZOOM_STEPS: i32 = 4;

/// 明暗主题，对应 C# `AppThemeMode`（JSON 里写 `Light` / `Dark` / `Auto`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppThemeMode {
    #[default]
    Light,
    Dark,
    Auto,
}

impl AppThemeMode {
    /// 写配置/日志用的名字，与 C# 枚举一致。
    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::Auto => "Auto",
        }
    }

    /// 按 C# `JsonStringEnumConverter` 的口径解析：字符串大小写不敏感，数字按枚举值。
    pub fn parse(value: &Value) -> Option<Self> {
        match value {
            Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
                "light" => Some(Self::Light),
                "dark" => Some(Self::Dark),
                "auto" => Some(Self::Auto),
                _ => None,
            },
            Value::Number(number) => match number.as_i64()? {
                0 => Some(Self::Light),
                1 => Some(Self::Dark),
                2 => Some(Self::Auto),
                _ => None,
            },
            _ => None,
        }
    }
}

/// 由 Snug 管理的虚拟屏何时启用。单块屏幕仍有独立的启用状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PhoneScreenMode {
    Off,
    #[default]
    WhileConnected,
    Always,
}

impl PhoneScreenMode {
    fn config_value(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::WhileConnected => "WhileConnected",
            Self::Always => "Always",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "whileconnected" => Some(Self::WhileConnected),
            "always" => Some(Self::Always),
            _ => None,
        }
    }
}

/// 用户明确应用的虚拟屏物理像素；与机型本身的规格分别保存。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScreenResolution {
    pub width: i32,
    pub height: i32,
}

impl ScreenResolution {
    pub const DEFAULT: Self = Self { width: 1080, height: 1920 };

    pub const fn default_for(orientation: ScreenOrientation) -> Self {
        match orientation {
            ScreenOrientation::Portrait => Self::DEFAULT,
            ScreenOrientation::Landscape => Self { width: 1920, height: 1080 },
        }
    }

    pub fn is_valid(self) -> bool {
        (320..=8192).contains(&self.width) && (320..=8192).contains(&self.height)
            && self.width % 2 == 0 && self.height % 2 == 0
    }
}

/// 一块（虚拟）屏的配置，对应 C# `ScreenConfig`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenConfig {
    pub id: String,
    pub orientation: ScreenOrientation,
    pub device: PhoneChoice,
    pub enabled: bool,
    pub name: String,
    pub resolution: Option<ScreenResolution>,
}

impl ScreenConfig {
    /// 最多几块屏（C# `ScreenConfig.Limit`）。
    pub const LIMIT: usize = 4;
    /// 旧版单屏配置迁移过来时固定用的 id（C# `ScreenConfig.LegacyPhoneId`）。
    pub const LEGACY_PHONE_ID: &'static str = "00000000-0000-0000-0000-000000000001";

    /// 兼容旧字段的主手机屏：竖屏、固定 id；旧文件像素在读取边界单独迁移。
    pub fn legacy_phone(phone: PhoneChoice, enabled: bool) -> Self {
        Self {
            id: Self::LEGACY_PHONE_ID.to_string(),
            orientation: ScreenOrientation::Portrait,
            device: phone.with_kind(DeviceKind::Phone),
            enabled,
            name: PHONE_SCREEN_NAME.to_string(),
            resolution: Some(ScreenResolution::DEFAULT),
        }
    }

    /// 没填名字时的建议名（C# `SuggestedName`）：
    /// 常用方向直接用机型名，不常用方向追加「横屏」/「竖屏」。
    pub fn suggested_name(device: &PhoneChoice, orientation: ScreenOrientation) -> String {
        let base = match &device.model {
            Some(model) => model.clone(),
            None => device.kind.display_name().to_string(),
        };
        if orientation == device.kind.usual_orientation() {
            base
        } else {
            format!("{base} {}", orientation.display_name())
        }
    }

    /// 兜底：空 id 生成新的、无效规格回落标准屏、电脑强制横屏、空名字用建议名。
    pub fn sanitize(self) -> Self {
        let Self { id, orientation, device, enabled, name, resolution } = self;
        let id = if id.trim().is_empty() { new_guid() } else { id.trim().to_string() };
        let kind = device.kind;
        let screen = if device.screen.is_valid() {
            device.screen
        } else {
            PhoneChoice::standard().screen
        };
        // 电脑只能横屏；其余保留用户选择（枚举在 Rust 里不可能越界，坏值在解析时已回落）。
        let orientation = if kind == DeviceKind::Computer {
            ScreenOrientation::Landscape
        } else {
            orientation
        };
        let device = PhoneChoice { screen, kind, ..device };
        let name = if name.trim().is_empty() {
            Self::suggested_name(&device, orientation)
        } else {
            name.trim().to_string()
        };
        Self { id, orientation, device, enabled, name, resolution: resolution.filter(|value| value.is_valid()) }
    }
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            id: new_guid(),
            orientation: ScreenOrientation::Portrait,
            device: PhoneChoice::standard(),
            enabled: true,
            name: PHONE_SCREEN_NAME.to_string(),
            resolution: Some(ScreenResolution::DEFAULT),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppConfig {
    /// 旧版单屏字段，与屏列表中主手机屏的启用状态保持一致。
    pub phone_screen: bool,
    phone_screen_mode: PhoneScreenMode,
    pub crossdesk_enabled: bool,
    pub gestures: bool,
    pub gesture_scroll: bool,
    pub gesture_long_press_right_click: bool,
    pub copy_after_selection: bool,
    pub gestures_injected_only: bool,
    pub screenshot: bool,
    pub auto_check_updates: bool,
    pub zoom_steps: i32,
    pub theme: AppThemeMode,
    /// 旧版单屏字段：保留给应用层，迁移/兜底后与 `screens` 里的主手机屏保持一致。
    pub phone: PhoneChoice,
    /// 新版屏列表；`None` 表示老配置还没有这个字段（用 `phone` 迁移）。
    pub screens: Option<Vec<ScreenConfig>>,
    pub welcomed: bool,
    pub last_version: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            phone_screen: true,
            phone_screen_mode: PhoneScreenMode::default(),
            crossdesk_enabled: false,
            gestures: true,
            gesture_scroll: true,
            gesture_long_press_right_click: true,
            copy_after_selection: true,
            gestures_injected_only: true,
            screenshot: false,
            auto_check_updates: true,
            zoom_steps: 2,
            theme: AppThemeMode::Light,
            phone: PhoneChoice::standard(),
            screens: None,
            welcomed: false,
            last_version: None,
        }
    }
}

impl AppConfig {
    pub fn phone_screen_mode(&self) -> PhoneScreenMode {
        self.phone_screen_mode
    }

    pub fn set_phone_screen_mode(&mut self, mode: PhoneScreenMode) {
        self.phone_screen_mode = mode;
    }

    /// 兜底与规范化（C# `Sanitize`）：夹取缩放、迁移旧配置、清理屏列表、同步 `phone`。
    pub fn sanitize(mut self) -> Self {
        let fallback_phone = if self.phone.screen.is_valid() {
            self.phone.clone()
        } else {
            PhoneChoice::standard()
        };

        let screens = match self.screens.take() {
            Some(raw) => {
                let mut seen: HashSet<String> = HashSet::new();
                let mut cleaned = Vec::new();
                for entry in raw {
                    // 分辨率坏掉的屏直接丢掉（C# 里是 `!raw.Device.Screen.IsValid → continue`）。
                    if !entry.device.screen.is_valid() {
                        continue;
                    }
                    let clean = entry.sanitize();
                    if !seen.insert(clean.id.to_lowercase()) {
                        continue;
                    }
                    cleaned.push(clean);
                    if cleaned.len() >= ScreenConfig::LIMIT {
                        break;
                    }
                }
                cleaned
            }
            None => vec![ScreenConfig::legacy_phone(fallback_phone.clone(), self.phone_screen)],
        };

        // `phone` / `phone_screen` 与「主手机屏」保持一致（C# 用第一块竖屏手机）。
        let legacy_phone = screens
            .iter()
            .find(|s| {
                s.orientation == ScreenOrientation::Portrait && s.device.kind == DeviceKind::Phone
            })
            .cloned();

        self.zoom_steps = self.zoom_steps.clamp(MIN_ZOOM_STEPS, MAX_ZOOM_STEPS);
        self.phone = legacy_phone
            .as_ref()
            .map(|s| s.device.clone())
            .unwrap_or(fallback_phone);
        self.phone_screen = match &legacy_phone {
            Some(screen) => screen.enabled,
            None => screens.iter().any(|s| s.enabled),
        };
        self.screens = Some(screens);
        self
    }

    /// 生效的屏列表（C# `EffectiveScreens`）：有 `screens` 就用它，否则用旧 `phone` 现场迁移。
    pub fn effective_screens(&self) -> Vec<ScreenConfig> {
        match &self.screens {
            Some(screens) => screens.clone(),
            None => vec![ScreenConfig::legacy_phone(self.phone.clone(), self.phone_screen)],
        }
    }

    /// 主触屏（C# `PrimaryTouchScreen`）：优先竖屏手机，其次任意触屏，最后第一块。
    pub fn primary_touch_screen(&self) -> Option<ScreenConfig> {
        let screens = self.effective_screens();
        screens
            .iter()
            .find(|s| {
                s.orientation == ScreenOrientation::Portrait && s.device.kind == DeviceKind::Phone
            })
            .or_else(|| screens.iter().find(|s| s.device.kind.is_touch()))
            .or_else(|| screens.first())
            .cloned()
    }

    /// 加一块屏（C# `WithScreenAdded`）：满了就不加；电脑强制横屏。
    pub fn with_screen_added(
        &self,
        orientation: ScreenOrientation,
        device: PhoneChoice,
        name: Option<&str>,
    ) -> Self {
        let mut list = self.effective_screens();
        if list.len() >= ScreenConfig::LIMIT {
            return self.clone();
        }
        let clean_orientation = if device.kind == DeviceKind::Computer {
            ScreenOrientation::Landscape
        } else {
            orientation
        };
        let clean_name = match name {
            Some(name) if !name.trim().is_empty() => name.trim().to_string(),
            _ => ScreenConfig::suggested_name(&device, clean_orientation),
        };
        let screen = ScreenConfig {
            id: new_guid(),
            orientation: clean_orientation,
            device,
            enabled: true,
            name: clean_name,
            resolution: Some(ScreenResolution::default_for(clean_orientation)),
        }
        .sanitize();
        list.push(screen);
        Self { screens: Some(list), ..self.clone() }.sanitize()
    }

    /// 删一块屏（C# `WithScreenRemoved`），id 大小写不敏感。
    pub fn with_screen_removed(&self, id: &str) -> Self {
        let list: Vec<ScreenConfig> = self
            .effective_screens()
            .into_iter()
            .filter(|s| !s.id.eq_ignore_ascii_case(id))
            .collect();
        Self { screens: Some(list), ..self.clone() }.sanitize()
    }

    /// 改一块屏（C# `WithScreenUpdated`），id 大小写不敏感。
    pub fn with_screen_updated(
        &self,
        id: &str,
        mut update: impl FnMut(ScreenConfig) -> ScreenConfig,
    ) -> Self {
        let list: Vec<ScreenConfig> = self
            .effective_screens()
            .into_iter()
            .map(|s| if s.id.eq_ignore_ascii_case(id) { update(s) } else { s })
            .collect();
        Self { screens: Some(list), ..self.clone() }.sanitize()
    }

    /// 启动时按机型库核对屏列表里的机型（C# `ResolveCatalog`）。
    pub fn resolve_catalog(&self, catalog: &PhoneCatalog) -> Self {
        let phone = catalog.resolve(self.phone.clone());
        let screens: Vec<ScreenConfig> = self
            .effective_screens()
            .into_iter()
            .map(|s| ScreenConfig { device: catalog.resolve(s.device.clone()), ..s })
            .collect();
        Self { phone, screens: Some(screens), ..self.clone() }.sanitize()
    }

    pub fn from_json_or_default(json: &str) -> Self {
        match serde_json::from_str::<Value>(json) {
            // 解析失败时 C# 直接 `return new AppConfig()`（连 Sanitize 都不跑），照搬。
            Err(_) => Self::default(),
            Ok(value) => Self::from_value(&value),
        }
    }

    /// 大小写不敏感地读取配置对象。
    fn from_value(value: &Value) -> Self {
        let mut cfg = Self::default();
        let Some(obj) = value.as_object() else {
            return cfg;
        };
        let legacy_phone_screen = get_bool(obj, "phonescreen");
        cfg.phone_screen = legacy_phone_screen.unwrap_or(cfg.phone_screen);
        cfg.crossdesk_enabled = get_bool(obj, "crossdeskenabled").unwrap_or(false);
        cfg.gestures = get_bool(obj, "gestures").unwrap_or(cfg.gestures);
        cfg.gesture_scroll = get_bool(obj, "gesturescroll").unwrap_or(cfg.gesture_scroll);
        cfg.gesture_long_press_right_click = get_bool(obj, "gesturelongpressrightclick")
            .unwrap_or(cfg.gesture_long_press_right_click);
        cfg.copy_after_selection =
            get_bool(obj, "copyafterselection").unwrap_or(cfg.copy_after_selection);
        cfg.gestures_injected_only =
            get_bool(obj, "gesturesinjectedonly").unwrap_or(cfg.gestures_injected_only);
        cfg.screenshot = get_bool(obj, "screenshot").unwrap_or(cfg.screenshot);
        cfg.auto_check_updates =
            get_bool(obj, "autocheckupdates").unwrap_or(cfg.auto_check_updates);
        cfg.zoom_steps = get_i64(obj, "zoomsteps").map(|v| v as i32).unwrap_or(cfg.zoom_steps);
        cfg.welcomed = get_bool(obj, "welcomed").unwrap_or(cfg.welcomed);
        cfg.last_version = get_string(obj, "lastversion");
        if let Some(theme) = get_value(obj, "theme").and_then(AppThemeMode::parse) {
            cfg.theme = theme;
        }
        if let Some(phone) = get_value(obj, "phone").and_then(parse_phone_choice) {
            cfg.phone = phone;
        }
        // `screens` 缺失或为 null 才走旧字段迁移；写成了数组就按新格式读（哪怕数组是空的）。
        cfg.screens = match get_value(obj, "screens") {
            Some(Value::Array(items)) => {
                Some(items.iter().filter_map(parse_screen_config).collect())
            }
            _ => None,
        };
        // 只有读取旧文件时使用旧机型公式；新装的 Default 不走这条迁移路径。
        if cfg.screens.is_none() {
            let phone = if cfg.phone.screen.is_valid() { cfg.phone.clone() } else { PhoneChoice::standard() };
            let mut screen = ScreenConfig::legacy_phone(phone, cfg.phone_screen).sanitize();
            screen.resolution = Some(legacy_resolution(&screen));
            cfg.screens = Some(vec![screen]);
        }
        let mut cfg = cfg.sanitize();
        cfg.phone_screen_mode = get_value(obj, "phonescreenmode")
            .and_then(Value::as_str)
            .and_then(PhoneScreenMode::parse)
            .unwrap_or_else(|| {
                if legacy_phone_screen.unwrap_or(cfg.phone_screen) {
                    PhoneScreenMode::WhileConnected
                } else {
                    PhoneScreenMode::Off
                }
            });
        cfg
    }

    /// 写回磁盘用的 JSON（与 C# v1.3 字段一致：PascalCase + 新增 Screens/Theme）。
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.to_value()).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        crate::store::JsonFileStore::new(path).save(&self.to_value())
    }

    fn to_value(&self) -> Value {
        let screens: Vec<Value> = self
            .effective_screens()
            .iter()
            .map(|screen| {
                let mut value = serde_json::json!({
                    "id": screen.id,
                    "orientation": screen.orientation.label(),
                    "device": phone_choice_json(&screen.device),
                    "enabled": screen.enabled,
                    "name": screen.name,
                });
                let resolution = screen.resolution.unwrap_or_else(||
                    ScreenResolution::default_for(screen.orientation));
                value["resolution"] = serde_json::json!({
                    "width": resolution.width, "height": resolution.height,
                });
                value
            })
            .collect();
        serde_json::json!({
            "PhoneScreen": self.phone_screen,
            "PhoneScreenMode": self.phone_screen_mode.config_value(),
            "CrossDeskEnabled": self.crossdesk_enabled,
            "Gestures": self.gestures,
            "GestureScroll": self.gesture_scroll,
            "GestureLongPressRightClick": self.gesture_long_press_right_click,
            "CopyAfterSelection": self.copy_after_selection,
            "GesturesInjectedOnly": self.gestures_injected_only,
            "Screenshot": self.screenshot,
            "AutoCheckUpdates": self.auto_check_updates,
            "ZoomSteps": self.zoom_steps,
            "Theme": self.theme.label(),
            "Phone": phone_choice_json(&self.phone),
            "Screens": screens,
            "Welcomed": self.welcomed,
            "LastVersion": self.last_version,
        })
    }
}

fn phone_choice_json(choice: &PhoneChoice) -> Value {
    serde_json::json!({
        "model": choice.model,
        "screen": {
            "width": choice.screen.width,
            "height": choice.screen.height,
            "points": choice.screen.points,
            "notch": choice.screen.notch,
        },
        "kind": choice.kind.label().to_lowercase(),
    })
}

/// 生成一个 `xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx` 形状的 GUID（C# `Guid.NewGuid().ToString("D")`）。
///
/// 不引 `uuid` 依赖：只要「进程内唯一 + 形状合法」就够用（配置里的屏 id 只在本机被引用）。
pub fn new_guid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn splitmix64(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = nanos
        ^ (std::process::id() as u64).wrapping_mul(0xd1b5_4a32_d192_ed03)
        ^ counter.wrapping_mul(0x9e37_79b9_7f4a_7c15);

    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&splitmix64(seed).to_le_bytes());
    bytes[8..].copy_from_slice(&splitmix64(splitmix64(seed)).to_le_bytes());
    // 版本 4 / variant 1（RFC 4122），和 C# 生成的 GUID 形状一致。
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn get_value<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a Value> {
    obj.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

fn get_bool(obj: &serde_json::Map<String, Value>, key: &str) -> Option<bool> {
    get_value(obj, key).and_then(Value::as_bool)
}

fn get_i64(obj: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    get_value(obj, key).and_then(Value::as_i64)
}

fn get_string(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    get_value(obj, key).and_then(Value::as_str).map(str::to_string)
}

fn parse_phone_choice(value: &Value) -> Option<PhoneChoice> {
    let obj = value.as_object()?;
    let model = get_string(obj, "model");
    let kind = get_value(obj, "kind").and_then(parse_device_kind).unwrap_or(DeviceKind::Phone);
    let screen_value = get_value(obj, "screen")?;
    let screen_obj = screen_value.as_object()?;
    let width = get_i64(screen_obj, "width")? as i32;
    let height = get_i64(screen_obj, "height")? as i32;
    let points = get_i64(screen_obj, "points").map(|p| p as i32);
    let notch = get_i64(screen_obj, "notch").map(|n| n as i32);
    Some(PhoneChoice {
        model,
        screen: PhoneScreenSpec::new(width, height, points).with_notch(notch),
        kind,
    })
}

fn parse_screen_config(value: &Value) -> Option<ScreenConfig> {
    let obj = value.as_object()?;
    let fallback = ScreenConfig::default();
    let id = get_string(obj, "id").unwrap_or(fallback.id);
    let orientation = get_value(obj, "orientation")
        .and_then(parse_orientation)
        .unwrap_or(fallback.orientation);
    let device = get_value(obj, "device").and_then(parse_phone_choice).unwrap_or(fallback.device);
    let enabled = get_bool(obj, "enabled").unwrap_or(fallback.enabled);
    let name = get_string(obj, "name").unwrap_or(fallback.name);
    let resolution = get_value(obj, "resolution").and_then(Value::as_object).and_then(|value| {
        Some(ScreenResolution {
            width: i32::try_from(get_i64(value, "width")?).ok()?,
            height: i32::try_from(get_i64(value, "height")?).ok()?,
        })
    }).filter(|resolution| resolution.is_valid());
    let mut screen = ScreenConfig { id, orientation, device, enabled, name, resolution };
    if get_value(obj, "resolution").is_none() {
        // 按清理后的旧规格求精确像素，迁移后不再跟着机型库变动。
        let normalized = screen.clone().sanitize();
        screen.resolution = Some(legacy_resolution(&normalized));
    }
    Some(screen)
}

fn legacy_resolution(screen: &ScreenConfig) -> ScreenResolution {
    let fit = crate::screen_fit::ScreenFit::from_device(
        &screen.device.screen.to_fit_spec(), screen.device.kind, screen.orientation,
    );
    let (width, height) = fit.pixel_size(fit.base_width());
    ScreenResolution { width, height }
}

/// 设备种类：`"phone"` / `"tablet"` / `"computer"`（大小写不敏感）或 0/1/2。
fn parse_device_kind(value: &Value) -> Option<DeviceKind> {
    match value {
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "phone" => Some(DeviceKind::Phone),
            "tablet" => Some(DeviceKind::Tablet),
            "computer" => Some(DeviceKind::Computer),
            _ => None,
        },
        Value::Number(number) => match number.as_i64()? {
            0 => Some(DeviceKind::Phone),
            1 => Some(DeviceKind::Tablet),
            2 => Some(DeviceKind::Computer),
            _ => None,
        },
        _ => None,
    }
}

/// 屏幕方向：`"portrait"` / `"landscape"`（大小写不敏感）或 0/1。
fn parse_orientation(value: &Value) -> Option<ScreenOrientation> {
    match value {
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "portrait" => Some(ScreenOrientation::Portrait),
            "landscape" => Some(ScreenOrientation::Landscape),
            _ => None,
        },
        Value::Number(number) => match number.as_i64()? {
            0 => Some(ScreenOrientation::Portrait),
            1 => Some(ScreenOrientation::Landscape),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn explicit_invalid_resolution_keeps_its_fallback_after_saving() {
        for resolution in [
            serde_json::Value::Null,
            serde_json::json!({"width": 321, "height": 1280}),
        ] {
            for orientation in [ScreenOrientation::Portrait, ScreenOrientation::Landscape] {
                let json = serde_json::json!({"Screens": [{
                    "id": "phone", "orientation": orientation.label(), "resolution": resolution,
                }]}).to_string();
                let cfg = AppConfig::from_json_or_default(&json);
                let pixels = |cfg: &AppConfig| {
                    let fit = crate::screen_plan::ScreenPlan::fit_of(&cfg.effective_screens()[0]);
                    fit.pixel_size(fit.base_width())
                };
                let expected = ScreenResolution::default_for(orientation);
                assert_eq!(pixels(&cfg), (expected.width, expected.height));
                let saved = AppConfig::from_json_or_default(&cfg.to_json());
                assert_eq!(pixels(&saved), pixels(&cfg), "saving must not trigger legacy migration");
                assert_eq!(AppConfig::from_json_or_default(&saved.to_json()), saved);
            }
        }
    }

    #[test]
    fn new_screens_write_the_resolution_default_and_old_json_migrates_once() {
        let fresh = super::AppConfig::default().sanitize();
        assert_eq!(fresh.effective_screens()[0].resolution, Some(super::ScreenResolution::DEFAULT));
        assert_eq!(super::ScreenConfig::default().resolution, Some(super::ScreenResolution::DEFAULT));
        assert_eq!(super::AppConfig::from_json_or_default(&fresh.to_json()), fresh);
        let old = super::AppConfig::from_json_or_default(r#"{"Screens":[{"id":"old","device":{"screen":{"width":1200,"height":2400},"kind":"Phone"}}]}"#);
        let screen = &old.effective_screens()[0];
        let fit = crate::screen_fit::ScreenFit::from_device(&screen.device.screen.to_fit_spec(), screen.device.kind, screen.orientation);
        let (width, height) = fit.pixel_size(fit.base_width());
        assert_eq!(screen.resolution, Some(super::ScreenResolution { width, height }));
        assert_eq!(super::AppConfig::from_json_or_default(&old.to_json()), old,
            "migration persists exact pixels and must not rerun on the next load");
        let added = fresh.with_screen_added(crate::phone_models::ScreenOrientation::Landscape,
            crate::phone_models::PhoneChoice::standard(), None);
        assert_eq!(added.effective_screens()[1].resolution,
            Some(super::ScreenResolution { width: 1920, height: 1080 }));
    }

    #[test]
    fn manual_resolution_round_trips_without_changing_legacy_defaults() {
        let old = super::AppConfig::from_json_or_default(r#"{"Screens":[{"id":"phone"}]}"#);
        assert_eq!(old.effective_screens()[0].resolution,
            Some(super::ScreenResolution { width: 1080, height: 1900 }));
        let configured = old.with_screen_updated("phone", |mut screen| {
            screen.resolution = Some(super::ScreenResolution { width: 720, height: 1280 });
            screen
        });
        let restored = super::AppConfig::from_json_or_default(&configured.to_json());
        assert_eq!(restored.effective_screens()[0].resolution,
            Some(super::ScreenResolution { width: 720, height: 1280 }));
        for width in [321_i64, -1, 9000, i64::MAX] {
            let json = serde_json::json!({"Screens":[{"id":"phone", "resolution":{"width":width,"height":1280}}]}).to_string();
            assert_eq!(super::AppConfig::from_json_or_default(&json).effective_screens()[0].resolution, None);
        }
    }

    use super::*;

    #[test]
    fn defaults_match_mac_upstream() {
        let cfg = AppConfig::default();
        assert!(cfg.phone_screen);
        assert_eq!(cfg.phone_screen_mode(), PhoneScreenMode::WhileConnected);
        assert!(cfg.gestures);
        assert!(!cfg.screenshot);
        assert_eq!(cfg.zoom_steps, 2);
        assert_eq!(cfg.phone, PhoneChoice::standard());
        assert_eq!(cfg.theme, AppThemeMode::Light);
        assert!(cfg.screens.is_none(), "默认还没有屏列表，等 sanitize 迁移");
    }

    #[test]
    fn legacy_phone_toggle_migrates_independently_of_the_screen_list() {
        for (enabled, expected) in [
            (true, PhoneScreenMode::WhileConnected),
            (false, PhoneScreenMode::Off),
        ] {
            let mut legacy = AppConfig::default().sanitize().to_value();
            let values = legacy.as_object_mut().unwrap();
            values.remove("PhoneScreenMode");
            values.insert("PhoneScreen".into(), Value::Bool(enabled));
            let migrated = AppConfig::from_value(&legacy);
            assert_eq!(migrated.phone_screen_mode(), expected);
            assert!(migrated.effective_screens()[0].enabled);
        }
        let bad_mode = r#"{"PhoneScreen": false, "PhoneScreenMode": "Unknown"}"#;
        assert_eq!(AppConfig::from_json_or_default(bad_mode).phone_screen_mode(), PhoneScreenMode::Off);
    }

    #[test]
    fn phone_screen_mode_survives_edits_and_save() {
        for mode in [PhoneScreenMode::Off, PhoneScreenMode::WhileConnected, PhoneScreenMode::Always] {
            let mut config = AppConfig::default().sanitize();
            config.set_phone_screen_mode(mode);
            let updated = config.with_screen_updated(ScreenConfig::LEGACY_PHONE_ID, |mut screen| {
                screen.enabled = false;
                screen
            });
            let restored = AppConfig::from_json_or_default(&updated.to_json());
            assert_eq!(restored.phone_screen_mode(), mode);
            assert!(!restored.effective_screens()[0].enabled);
        }
    }

    #[test]
    fn parses_real_config_file_shape() {
        let json = r#"{
          "PhoneScreen": true,
          "Gestures": true,
          "GestureScroll": true,
          "GestureLongPressRightClick": true,
          "CopyAfterSelection": true,
          "GesturesInjectedOnly": true,
          "Screenshot": false,
          "AutoCheckUpdates": true,
          "ZoomSteps": 2,
          "Phone": {
            "model": "Apple iPhone 17 Pro Max",
            "screen": { "width": 1320, "height": 2868, "points": 440 }
          },
          "Welcomed": false,
          "LastVersion": null
        }"#;
        let cfg = AppConfig::from_json_or_default(json);
        // 更新保留旧文件的 1080×1900；新装默认使用固定的 1080×1920。
        let mut expected = AppConfig::default().sanitize();
        expected.screens.as_mut().expect("default screen")[0].resolution =
            Some(ScreenResolution { width: 1080, height: 1900 });
        assert_eq!(cfg, expected);
        assert_eq!(cfg.effective_screens().len(), 1);
        assert_eq!(cfg.effective_screens()[0].id, ScreenConfig::LEGACY_PHONE_ID);
    }

    #[test]
    fn key_case_is_ignored() {
        let json = r#"{"phonescreen": false, "zoomsteps": 4, "screenshot": true, "theme": "dark"}"#;
        let cfg = AppConfig::from_json_or_default(json);
        assert!(!cfg.phone_screen);
        assert_eq!(cfg.zoom_steps, 4);
        assert!(cfg.screenshot);
        assert_eq!(cfg.theme, AppThemeMode::Dark);
    }

    #[test]
    fn zoom_steps_are_clamped() {
        assert_eq!(AppConfig::from_json_or_default(r#"{"ZoomSteps": 99}"#).zoom_steps, MAX_ZOOM_STEPS);
        assert_eq!(AppConfig::from_json_or_default(r#"{"ZoomSteps": -5}"#).zoom_steps, MIN_ZOOM_STEPS);
    }

    #[test]
    fn invalid_phone_falls_back_to_standard() {
        let json = r#"{"Phone":{"model":"x","screen":{"width":20,"height":10,"points":0}}}"#;
        let cfg = AppConfig::from_json_or_default(json);
        assert_eq!(cfg.phone, PhoneChoice::standard());
    }

    #[test]
    fn broken_json_gives_defaults() {
        assert_eq!(AppConfig::from_json_or_default("{not json"), AppConfig::default());
        assert_eq!(AppConfig::from_json_or_default("[]"), AppConfig::default());
    }

    #[test]
    fn roundtrip_through_json() {
        let cfg = AppConfig { screenshot: true, zoom_steps: 3, welcomed: true, ..Default::default() }
            .sanitize();
        let back = AppConfig::from_json_or_default(&cfg.to_json());
        assert_eq!(back, cfg);
    }

    #[test]
    fn theme_accepts_names_and_numbers_like_the_csharp_converter() {
        // 抄自 C# `AppConfig_ThemeDefaultsToLightAndSanitizes`。
        assert_eq!(
            AppConfig::from_json_or_default("{}").theme,
            AppThemeMode::Light
        );
        assert_eq!(
            AppConfig::from_json_or_default(r#"{"theme":"Dark"}"#).theme,
            AppThemeMode::Dark
        );
        assert_eq!(
            AppConfig::from_json_or_default(r#"{"theme":"Auto"}"#).theme,
            AppThemeMode::Auto
        );
        assert_eq!(
            AppConfig::from_json_or_default(r#"{"theme":1}"#).theme,
            AppThemeMode::Dark
        );
        assert_eq!(
            AppConfig::from_json_or_default(r#"{"theme":99}"#).theme,
            AppThemeMode::Light,
            "坏值回落 Light"
        );
        assert_eq!(
            AppConfig::from_json_or_default(r#"{"theme":"neon"}"#).theme,
            AppThemeMode::Light
        );
    }

    #[test]
    fn legacy_phone_migrates_into_the_first_screen() {
        // 抄自 C# `AppConfig_MigratesLegacyAndManagesUpToFourScreens` 的第一段。
        let legacy_json = r#"{
          "phoneScreen": false,
          "phone": { "model": "Apple iPhone 16", "screen": { "width": 1179, "height": 2556, "points": 393 } }
        }"#;
        let migrated = AppConfig::from_json_or_default(legacy_json);
        let screens = migrated.effective_screens();
        assert_eq!(screens.len(), 1);
        assert_eq!(screens[0].id, ScreenConfig::LEGACY_PHONE_ID);
        assert!(!screens[0].enabled);
        assert_eq!(screens[0].device.model.as_deref(), Some("Apple iPhone 16"));
        assert_eq!(screens[0].device.kind, DeviceKind::Phone);
        assert_eq!(screens[0].name, PHONE_SCREEN_NAME);
        // 旧字段跟着迁移结果走。
        assert_eq!(migrated.phone.model.as_deref(), Some("Apple iPhone 16"));
        assert!(!migrated.phone_screen);
    }

    #[test]
    fn saving_writes_the_new_screens_format() {
        let cfg = AppConfig::default().sanitize();
        let json = cfg.to_json();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["Theme"], "Light");
        let screens = value["Screens"].as_array().expect("Screens 数组");
        assert_eq!(screens.len(), 1);
        assert_eq!(screens[0]["id"], ScreenConfig::LEGACY_PHONE_ID);
        assert_eq!(screens[0]["orientation"], "Portrait");
        assert_eq!(screens[0]["device"]["kind"], "phone");
        assert_eq!(screens[0]["name"], PHONE_SCREEN_NAME);
        // `Phone` 也照写，C# 版读我们的配置不会丢机型。
        assert_eq!(value["Phone"]["model"], "Apple iPhone 17 Pro Max");
    }

    #[test]
    fn screen_limit_keeps_only_four_screens() {
        let screens: Vec<Value> = (0..5)
            .map(|index| {
                serde_json::json!({
                    "id": format!("00000000-0000-0000-0000-00000000000{index}"),
                    "orientation": "Portrait",
                    "device": {
                        "model": format!("Phone {index}"),
                        "screen": { "width": 1080, "height": 2400, "points": 360 },
                        "kind": "phone"
                    },
                    "enabled": true,
                    "name": format!("屏 {index}")
                })
            })
            .collect();
        let json = serde_json::json!({ "Screens": screens }).to_string();

        let cfg = AppConfig::from_json_or_default(&json);
        assert_eq!(cfg.effective_screens().len(), 4);
        assert_eq!(cfg.effective_screens()[3].name, "屏 3");

        // 加第 5 块时 `WithScreenAdded` 直接拒绝。
        let extra = PhoneChoice {
            model: Some("Phone 5".into()),
            screen: PhoneScreenSpec::new(1080, 2400, Some(360)),
            kind: DeviceKind::Phone,
        };
        let rejected = cfg.with_screen_added(ScreenOrientation::Portrait, extra, Some("屏 5"));
        assert_eq!(rejected.effective_screens().len(), 4);
    }

    #[test]
    fn sanitize_force_feeds_computer_screens_to_landscape() {
        let json = r#"{
          "Screens": [{
            "id": "11111111-1111-4111-8111-111111111111",
            "orientation": "Portrait",
            "device": { "model": "1080P", "screen": { "width": 1080, "height": 1920 }, "kind": "computer" },
            "enabled": true,
            "name": "桌面副屏"
          }]
        }"#;
        let cfg = AppConfig::from_json_or_default(json);
        let screen = &cfg.effective_screens()[0];
        assert_eq!(screen.orientation, ScreenOrientation::Landscape, "电脑只能横屏");
        assert_eq!(screen.device.kind, DeviceKind::Computer);
        assert_eq!(screen.name, "桌面副屏", "有名字就不要覆盖");
        // 没有竖屏手机屏时，旧字段 Phone 保持原样。
        assert_eq!(cfg.phone, PhoneChoice::standard());
        assert!(cfg.phone_screen, "没有主手机屏时按「任意一块启用」算");
    }

    #[test]
    fn screen_sanitize_falls_back_on_broken_spec() {
        // `ScreenConfig.Sanitize` 自己的兜底：坏分辨率回落标准屏（C# 直接调用时走这条）。
        let screen = ScreenConfig {
            id: "   ".into(),
            orientation: ScreenOrientation::Landscape,
            device: PhoneChoice {
                model: Some("Xiaomi 15".into()),
                screen: PhoneScreenSpec::new(10, 5, None),
                kind: DeviceKind::Phone,
            },
            enabled: true,
            name: "   ".into(),
            resolution: None,
        };
        let clean = screen.sanitize();
        assert_eq!(clean.device.screen, PhoneChoice::standard().screen, "坏规格回落标准屏");
        assert_ne!(clean.id.trim(), "", "空 id 要生成新的");
        assert_eq!(clean.device.kind, DeviceKind::Phone, "机型名与种类保留");
        // 手机常用方向是竖屏，这里选了横屏 → 名字追加「横屏」。
        assert_eq!(clean.name, "Xiaomi 15 横屏");
    }

    #[test]
    fn config_sanitize_drops_screens_with_broken_specs() {
        // 与 ScreenConfig 的兜底不同：C# `AppConfig.Sanitize` 对坏规格的屏是**整块丢掉**。
        let json = r#"{
          "Screens": [{
            "id": "22222222-2222-4222-8222-222222222222",
            "device": { "screen": { "width": 10, "height": 5 } },
            "name": "坏屏"
          }]
        }"#;
        let cfg = AppConfig::from_json_or_default(json);
        assert!(cfg.effective_screens().is_empty(), "坏规格的屏要被丢掉，不是修好");
        assert_eq!(cfg.phone, PhoneChoice::standard(), "旧字段回落到标准机型");
    }

    #[test]
    fn sanitize_uses_suggested_name_when_empty() {
        let json = r#"{
          "Screens": [{
            "id": "   ",
            "orientation": "Landscape",
            "device": { "model": "Xiaomi 15", "screen": { "width": 1200, "height": 2400 }, "kind": "phone" },
            "enabled": true,
            "name": "   "
          }]
        }"#;
        let cfg = AppConfig::from_json_or_default(json);
        let screen = &cfg.effective_screens()[0];
        assert_ne!(screen.id.trim(), "", "空 id 要生成新的");
        assert_eq!(screen.name, "Xiaomi 15 横屏", "空名字要用 SuggestedName");
    }

    #[test]
    fn suggested_name_matches_csharp_rules() {
        let choice = PhoneChoice {
            model: Some("iPad Air".into()),
            screen: PhoneScreenSpec::new(1640, 2360, Some(820)),
            kind: DeviceKind::Tablet,
        };
        // 平板常用方向是横屏。
        assert_eq!(ScreenConfig::suggested_name(&choice, ScreenOrientation::Landscape), "iPad Air");
        assert_eq!(
            ScreenConfig::suggested_name(&choice, ScreenOrientation::Portrait),
            "iPad Air 竖屏"
        );
        // 没有机型名时用设备种类的中文名。
        let anonymous = PhoneChoice {
            model: None,
            screen: PhoneScreenSpec::new(1080, 1920, None),
            kind: DeviceKind::Computer,
        };
        assert_eq!(
            ScreenConfig::suggested_name(&anonymous, ScreenOrientation::Landscape),
            "电脑"
        );
    }

    #[test]
    fn duplicate_screen_ids_are_dropped_and_ids_are_trimmed() {
        let json = r#"{
          "Screens": [
            { "id": "ABC", "device": { "screen": { "width": 1080, "height": 2400 } }, "name": "一" },
            { "id": " abc ", "device": { "screen": { "width": 1080, "height": 2400 } }, "name": "二" }
          ]
        }"#;
        let cfg = AppConfig::from_json_or_default(json);
        let screens = cfg.effective_screens();
        assert_eq!(screens.len(), 1);
        assert_eq!(screens[0].id, "ABC");
        assert_eq!(screens[0].name, "一");
    }

    #[test]
    fn with_screen_added_and_removed_follow_the_csharp_test() {
        // 抄自 C# `AppConfig_MigratesLegacyAndManagesUpToFourScreens` 的加法部分。
        let migrated = AppConfig::default().sanitize();
        let tablet = PhoneChoice {
            model: Some("iPad Pro 11\"".into()),
            screen: PhoneScreenSpec::new(1668, 2388, Some(834)),
            kind: DeviceKind::Tablet,
        };
        let desktop = PhoneChoice {
            model: Some("FHD 1080p".into()),
            screen: PhoneScreenSpec::new(1920, 1080, None),
            kind: DeviceKind::Computer,
        };
        let phone2 = PhoneChoice {
            model: Some("Phone 2".into()),
            screen: PhoneScreenSpec::new(1080, 2400, Some(360)),
            kind: DeviceKind::Phone,
        };

        let cur = migrated
            .with_screen_added(ScreenOrientation::Landscape, tablet, Some("我的平板"))
            .with_screen_added(ScreenOrientation::Portrait, desktop, Some("电脑副屏"))
            .with_screen_added(ScreenOrientation::Landscape, phone2, None);

        assert_eq!(cur.effective_screens().len(), 4);
        assert_eq!(
            cur.effective_screens()[2].orientation,
            ScreenOrientation::Landscape,
            "电脑请求竖屏也要被改成横屏"
        );
        assert_eq!(cur.effective_screens()[3].name, "Phone 2 横屏", "没名字用建议名");

        let removed_id = cur.effective_screens()[1].id.clone();
        let after_remove = cur.with_screen_removed(&removed_id);
        assert_eq!(after_remove.effective_screens().len(), 3);

        // id 大小写不敏感。
        let upper = removed_id.to_uppercase();
        assert_eq!(cur.with_screen_removed(&upper).effective_screens().len(), 3);
    }

    #[test]
    fn with_screen_updated_changes_only_the_target() {
        let cfg = AppConfig::default().sanitize();
        let updated = cfg.with_screen_updated(ScreenConfig::LEGACY_PHONE_ID, |s| ScreenConfig {
            enabled: false,
            ..s
        });
        assert!(!updated.effective_screens()[0].enabled);
        assert!(!updated.phone_screen, "关掉主手机屏后旧字段也跟着变");
    }

    #[test]
    fn screens_present_but_empty_stays_empty() {
        let cfg = AppConfig::from_json_or_default(r#"{"screens":[]}"#);
        assert!(cfg.effective_screens().is_empty(), "空数组是用户删光了，不再迁移旧字段");
        assert!(!cfg.phone_screen);
    }

    #[test]
    fn primary_touch_screen_prefers_portrait_phone() {
        let tablet = PhoneChoice {
            model: Some("iPad".into()),
            screen: PhoneScreenSpec::new(1640, 2360, Some(820)),
            kind: DeviceKind::Tablet,
        };
        let cfg = AppConfig::default()
            .sanitize()
            .with_screen_added(ScreenOrientation::Landscape, tablet, Some("平板"));
        let primary = cfg.primary_touch_screen().expect("有屏");
        assert_eq!(primary.device.kind, DeviceKind::Phone);
        assert_eq!(primary.orientation, ScreenOrientation::Portrait);
    }

    #[test]
    fn guid_shape_is_the_dotnet_d_format() {
        let guid = new_guid();
        assert_eq!(guid.len(), 36);
        let parts: Vec<&str> = guid.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(guid.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert!(guid.chars().all(|c| !c.is_ascii_uppercase()), "C# 的 ToString(\"D\") 是小写");
        assert_eq!(&guid[14..15], "4", "版本位是 4");
        assert!(matches!(&guid[19..20], "8" | "9" | "a" | "b"), "variant 位");
        assert_ne!(guid, new_guid());
    }
}
