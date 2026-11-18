//! 屏规划，移植自 C# v1.3.0 `SnugWin.Core.PhoneScreen.ScreenPlan`。
//!
//! 这是「配置」到「要建哪些屏」之间的薄层：把 [`AppConfig::effective_screens`]
//! 里启用的屏（最多 [`ScreenConfig::LIMIT`] 块）翻译成建屏请求
//! [`ScreenWish`]（设备、方向、档位表、行为）。
//!
//! 关键语义（照 C#）：`remote == false && preview == false` 时**一块屏都不要**，
//! 因为用户既没连着远程、也没在预览，建屏纯属打扰。

use crate::config::{AppConfig, ScreenConfig};
use crate::phone_models::{DeviceKind, PhoneChoice, ScreenOrientation};
use crate::screen_fit::ScreenFit;

/// 这块屏是触屏（接管触摸手势）还是电脑副屏（只摆窗口）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenBehavior {
    TouchScreen,
    Computer,
}

/// 一块要建的屏（C# `ScreenWish`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenWish {
    pub id: String,
    pub name: String,
    pub device: PhoneChoice,
    pub orientation: ScreenOrientation,
    pub fit: ScreenFit,
    pub behavior: ScreenBehavior,
}

impl ScreenWish {
    /// 是否接管触摸（C# `IsTouch`）。
    pub fn is_touch(&self) -> bool {
        self.behavior == ScreenBehavior::TouchScreen
    }
}

/// 屏规划入口（C# 静态类 `ScreenPlan`）。
pub struct ScreenPlan;

impl ScreenPlan {
    /// 设备种类决定的行为：触屏设备接管手势，电脑屏不接管。
    pub fn behavior_of(kind: DeviceKind) -> ScreenBehavior {
        if kind.is_touch() { ScreenBehavior::TouchScreen } else { ScreenBehavior::Computer }
    }

    /// 这块屏的档位表（C# `FitOf`）。
    pub fn fit_of(screen: &ScreenConfig) -> ScreenFit {
        ScreenFit::from_device(&screen.device.screen.to_fit_spec(), screen.device.kind, screen.orientation)
    }

    /// 当前该建哪些屏（C# `Wishes`）。
    ///
    /// `remote` = UU 远程是否连着；`preview` = 用户是否在桌面预览。
    /// 两者都为假时返回空表；否则取启用的屏、最多 4 块、按配置顺序。
    pub fn wishes(config: &AppConfig, remote: bool, preview: bool) -> Vec<ScreenWish> {
        if !remote && !preview {
            return Vec::new();
        }
        config
            .effective_screens()
            .into_iter()
            .filter(|screen| screen.enabled)
            .take(ScreenConfig::LIMIT)
            .map(|screen| ScreenWish {
                fit: Self::fit_of(&screen),
                behavior: Self::behavior_of(screen.device.kind),
                id: screen.id,
                name: screen.name,
                device: screen.device,
                orientation: screen.orientation,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phone_models::PhoneScreenSpec;

    fn tablet() -> PhoneChoice {
        PhoneChoice {
            model: Some("iPad Air".into()),
            screen: PhoneScreenSpec::new(1640, 2360, Some(820)),
            kind: DeviceKind::Tablet,
        }
    }

    fn computer() -> PhoneChoice {
        PhoneChoice {
            model: Some("1080P".into()),
            screen: PhoneScreenSpec::new(1920, 1080, None),
            kind: DeviceKind::Computer,
        }
    }

    #[test]
    fn wishes_are_empty_unless_remote_or_preview() {
        let cfg = AppConfig::default().sanitize();
        // remote×preview 四种组合里，只有全假时为空。
        assert!(ScreenPlan::wishes(&cfg, false, false).is_empty());
        assert_eq!(ScreenPlan::wishes(&cfg, true, false).len(), 1);
        assert_eq!(ScreenPlan::wishes(&cfg, false, true).len(), 1);
        assert_eq!(ScreenPlan::wishes(&cfg, true, true).len(), 1);
    }

    #[test]
    fn wishes_honour_enabled_limit_and_behavior() {
        // 抄自 C# `ScreenPlan_Wishes_HonorsEnabledLimitAndBehavior`。
        let cfg = AppConfig::default()
            .sanitize()
            .with_screen_added(ScreenOrientation::Landscape, tablet(), Some("平板"))
            .with_screen_added(ScreenOrientation::Landscape, computer(), Some("桌面副屏"));

        // 关掉第一块（旧版手机屏）。
        let cfg = cfg.with_screen_updated(ScreenConfig::LEGACY_PHONE_ID, |s| ScreenConfig {
            enabled: false,
            ..s
        });

        assert!(ScreenPlan::wishes(&cfg, false, false).is_empty());

        let wishes = ScreenPlan::wishes(&cfg, true, false);
        assert_eq!(wishes.len(), 2, "关掉的那块不出现在请求里");
        assert_eq!(wishes[0].behavior, ScreenBehavior::TouchScreen, "平板接管触摸");
        assert_eq!(wishes[1].behavior, ScreenBehavior::Computer, "电脑屏不接管触摸");
        assert!(wishes[0].is_touch());
        assert!(!wishes[1].is_touch());
        assert_eq!(wishes[0].name, "平板");
        assert_eq!(wishes[1].name, "桌面副屏");
    }

    #[test]
    fn wishes_take_at_most_four_screens_in_config_order() {
        let mut cfg = AppConfig::default().sanitize();
        for index in 0..3 {
            let device = PhoneChoice {
                model: Some(format!("Phone {index}")),
                screen: PhoneScreenSpec::new(1080, 2400, Some(360)),
                kind: DeviceKind::Phone,
            };
            cfg = cfg.with_screen_added(ScreenOrientation::Portrait, device, Some("追加"));
        }
        let wishes = ScreenPlan::wishes(&cfg, true, false);
        assert_eq!(wishes.len(), 4);
        assert_eq!(wishes[0].id, ScreenConfig::LEGACY_PHONE_ID);
        assert!(wishes.iter().all(|wish| wish.name.starts_with("追加") || wish.id == ScreenConfig::LEGACY_PHONE_ID));
    }

    #[test]
    fn fit_of_uses_the_device_kind_and_orientation() {
        let screen = ScreenConfig {
            id: "s".into(),
            orientation: ScreenOrientation::Landscape,
            device: computer(),
            enabled: true,
            name: "副屏".into(),
        };
        let fit = ScreenPlan::fit_of(&screen);
        assert_eq!(fit.kind(), DeviceKind::Computer);
        assert_eq!(fit.orientation(), ScreenOrientation::Landscape);
        // 电脑横屏基准宽取高（1080p 的 1080），只有一档。
        assert_eq!(fit.base_width(), 1080);
        assert_eq!(fit.widths(), &[1080]);

        let portrait_tablet = ScreenConfig {
            orientation: ScreenOrientation::Portrait,
            device: tablet(),
            ..screen
        };
        let fit = ScreenPlan::fit_of(&portrait_tablet);
        assert_eq!(fit.kind(), DeviceKind::Tablet);
        assert_eq!(fit.orientation(), ScreenOrientation::Portrait);
    }

    #[test]
    fn behavior_of_matches_touch_devices() {
        assert_eq!(ScreenPlan::behavior_of(DeviceKind::Phone), ScreenBehavior::TouchScreen);
        assert_eq!(ScreenPlan::behavior_of(DeviceKind::Tablet), ScreenBehavior::TouchScreen);
        assert_eq!(ScreenPlan::behavior_of(DeviceKind::Computer), ScreenBehavior::Computer);
    }
}
