//! 配置，移植自 C# 版 `SnugWin.Core.Config.AppConfig`。
//!
//! 与 C# 版一致：字段名大小写不敏感（老配置里是 PascalCase），
//! 读不出或数值越界时回落到默认值，绝不因为一份坏配置起不来。

use serde_json::Value;

use crate::phone_models::{PhoneChoice, PhoneScreenSpec};

pub const MIN_ZOOM_STEPS: i32 = 0;
pub const MAX_ZOOM_STEPS: i32 = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct AppConfig {
    pub phone_screen: bool,
    pub gestures: bool,
    pub gesture_scroll: bool,
    pub gesture_long_press_right_click: bool,
    pub copy_after_selection: bool,
    pub gestures_injected_only: bool,
    pub screenshot: bool,
    pub auto_check_updates: bool,
    pub zoom_steps: i32,
    pub phone: PhoneChoice,
    pub welcomed: bool,
    pub last_version: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            phone_screen: true,
            gestures: true,
            gesture_scroll: true,
            gesture_long_press_right_click: true,
            copy_after_selection: true,
            gestures_injected_only: true,
            screenshot: false,
            auto_check_updates: true,
            zoom_steps: 2,
            phone: PhoneChoice::standard(),
            welcomed: false,
            last_version: None,
        }
    }
}

impl AppConfig {
    /// 把越界数值夹回合法范围；手机规格无效时回到标准机型。
    pub fn sanitize(mut self) -> Self {
        self.zoom_steps = self.zoom_steps.clamp(MIN_ZOOM_STEPS, MAX_ZOOM_STEPS);
        if !self.phone.screen.is_valid() {
            self.phone = PhoneChoice::standard();
        }
        self
    }

    pub fn from_json_or_default(json: &str) -> Self {
        match serde_json::from_str::<Value>(json) {
            Ok(v) => Self::from_value(&v),
            Err(_) => Self::default(),
        }
    }

    /// 大小写不敏感地读取配置对象。
    fn from_value(value: &Value) -> Self {
        let mut cfg = Self::default();
        let Some(obj) = value.as_object() else {
            return cfg;
        };
        cfg.phone_screen = get_bool(obj, "phonescreen").unwrap_or(cfg.phone_screen);
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
        if let Some(phone) = get_value(obj, "phone").and_then(parse_phone_choice) {
            cfg.phone = phone;
        }
        cfg.sanitize()
    }

    /// 写回磁盘用的 JSON（与 C# 版字段名一致，PascalCase）。
    pub fn to_json(&self) -> String {
        let phone = serde_json::json!({
            "model": self.phone.model,
            "screen": {
                "width": self.phone.screen.width,
                "height": self.phone.screen.height,
                "points": self.phone.screen.points,
            }
        });
        let value = serde_json::json!({
            "PhoneScreen": self.phone_screen,
            "Gestures": self.gestures,
            "GestureScroll": self.gesture_scroll,
            "GestureLongPressRightClick": self.gesture_long_press_right_click,
            "CopyAfterSelection": self.copy_after_selection,
            "GesturesInjectedOnly": self.gestures_injected_only,
            "Screenshot": self.screenshot,
            "AutoCheckUpdates": self.auto_check_updates,
            "ZoomSteps": self.zoom_steps,
            "Phone": phone,
            "Welcomed": self.welcomed,
            "LastVersion": self.last_version,
        });
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
    }
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
    let screen_value = get_value(obj, "screen")?;
    let screen_obj = screen_value.as_object()?;
    let width = get_i64(screen_obj, "width")? as i32;
    let height = get_i64(screen_obj, "height")? as i32;
    let points = get_i64(screen_obj, "points").map(|p| p as i32);
    Some(PhoneChoice { model, screen: PhoneScreenSpec::new(width, height, points) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_mac_upstream() {
        let cfg = AppConfig::default();
        assert!(cfg.phone_screen);
        assert!(cfg.gestures);
        assert!(!cfg.screenshot);
        assert_eq!(cfg.zoom_steps, 2);
        assert_eq!(cfg.phone, PhoneChoice::standard());
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
        assert_eq!(cfg, AppConfig::default());
    }

    #[test]
    fn key_case_is_ignored() {
        let json = r#"{"phonescreen": false, "zoomsteps": 4, "screenshot": true}"#;
        let cfg = AppConfig::from_json_or_default(json);
        assert!(!cfg.phone_screen);
        assert_eq!(cfg.zoom_steps, 4);
        assert!(cfg.screenshot);
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
        let cfg = AppConfig { screenshot: true, zoom_steps: 3, welcomed: true, ..Default::default() };
        let back = AppConfig::from_json_or_default(&cfg.to_json());
        assert_eq!(back, cfg);
    }
}
