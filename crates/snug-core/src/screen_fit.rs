//! 手机屏的档位计算，移植自 C# 版 `SnugWin.Core.PhoneScreen.ScreenFit`。
//!
//! 规则（与 C# 版逐条对应）：
//! - 高宽比取「手机屏高/宽」减去 UU 工具栏上下各 91 点，再按逻辑宽度归一，夹在 1.0~2.0；
//! - 宽度档位从 540 点起每 10 点一档，最窄的「高度 ≥ 800 点」档位作为基准宽度；
//! - 支持的档位从基准宽度一直到 1080 点。
//! - 物理像素按 2 倍下发（Windows 200% DPI 虚拟屏），并保证偶数，便于编码器处理。

use serde::{Deserialize, Serialize};

use crate::geometry::Size;

pub const DESIGN_WIDTH: f64 = 540.0;
pub const MIN_WIDTH: i32 = 540;
pub const MIN_HEIGHT: f64 = 800.0;
pub const MAX_WIDTH: i32 = 1080;
pub const STEP: i32 = 10;
pub const MIN_RATIO: f64 = 1.0;
pub const MAX_RATIO: f64 = 2.0;

/// UU 远程工具栏在手机屏上下的占位（点）。
const UU_INSET: f64 = 91.0;
/// 安卓等没有逻辑分辨率数据时的估算宽度。
const ASSUMED_POINTS: f64 = 400.0;
const BASE_HOME_BAR_HEIGHT: f64 = 28.0;

/// 机型库中的一条手机屏规格（`assets/phone-models.json`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhoneSpec {
    pub width: f64,
    pub height: f64,
    /// Apple 设备才有逻辑宽度。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<f64>,
}

impl PhoneSpec {
    pub const fn new(width: f64, height: f64, points: Option<f64>) -> Self {
        Self { width, height, points }
    }

    /// iPhone 17 Pro Max，540×950 的基准（与 C# `PhoneChoice.Standard` 一致）。
    pub const fn standard() -> Self {
        Self::new(1320.0, 2868.0, Some(440.0))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScreenFit {
    ratio: f64,
    base_width: i32,
    widths: Vec<i32>,
}

impl ScreenFit {
    /// 按高宽比构建档位表。
    pub fn new(ratio: f64) -> Self {
        let ratio = ratio.clamp(MIN_RATIO, MAX_RATIO);
        let mut widths = Vec::new();
        let mut base_width: Option<i32> = None;
        let mut w = MIN_WIDTH;
        while w <= MAX_WIDTH {
            if base_width.is_none() && round_away_zero(w as f64 * ratio) >= MIN_HEIGHT {
                base_width = Some(w);
            }
            if base_width.is_some() {
                widths.push(w);
            }
            w += STEP;
        }
        let base_width = base_width.unwrap_or(MAX_WIDTH);
        if widths.is_empty() {
            widths.push(MAX_WIDTH);
        }
        Self { ratio, base_width, widths }
    }

    pub fn from_phone(phone: &PhoneSpec) -> Self {
        let points = phone.points.unwrap_or(ASSUMED_POINTS);
        Self::new(phone.height / phone.width - 2.0 * UU_INSET / points)
    }

    pub fn standard() -> Self {
        Self::from_phone(&PhoneSpec::standard())
    }

    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    pub fn base_width(&self) -> i32 {
        self.base_width
    }

    pub fn widths(&self) -> &[i32] {
        &self.widths
    }

    /// 某个逻辑宽度对应的逻辑尺寸（宽 × 高）。
    pub fn size(&self, width: i32) -> Size {
        Size::new(width as f64, round_away_zero(width as f64 * self.ratio))
    }

    /// Windows 200% DPI 下发给虚拟屏的物理像素尺寸，保证为偶数。
    pub fn pixel_size(&self, logical_width: i32) -> (i32, i32) {
        let logical = self.size(logical_width);
        let mut pw = logical.width as i32 * 2;
        let mut ph = logical.height as i32 * 2;
        if pw % 2 != 0 {
            pw += 1;
        }
        if ph % 2 != 0 {
            ph += 1;
        }
        (pw, ph)
    }

    /// 窗口比当前可放区域大时，选出能容纳它的最窄档位。
    pub fn width_to_fit(&self, window: Size, area: Size, current_width: i32) -> i32 {
        const TOLERANCE: f64 = 2.0;
        if window.width <= area.width + TOLERANCE && window.height <= area.height + TOLERANCE {
            return current_width;
        }
        let needed = f64::max(
            current_width as f64 * window.width / area.width,
            current_width as f64 * window.height / area.height,
        );
        let target = needed.ceil() as i32;
        for &w in &self.widths {
            if w >= target {
                return w;
            }
        }
        MAX_WIDTH
    }

    /// 屏幕宽度相对设计宽度的倍率。
    pub fn scale(screen_width: f64) -> f64 {
        screen_width / DESIGN_WIDTH
    }

    /// 底部指示条高度，随屏幕宽度等比放大。
    pub fn home_bar_height(screen_width: f64) -> f64 {
        round_away_zero(BASE_HOME_BAR_HEIGHT * Self::scale(screen_width))
    }
}

/// C# `Math.Round(..., MidpointRounding.AwayFromZero)`：.5 远离零。
pub fn round_away_zero(v: f64) -> f64 {
    v.round()
}

impl std::fmt::Display for ScreenFit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.size(self.base_width);
        write!(f, "{}x{}", self.base_width, s.height as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_area() -> Size {
        Size::new(540.0, 897.0)
    }

    #[test]
    fn standard_iphone_17_pro_max_is_540x950_with_all_steps() {
        let fit = ScreenFit::standard();
        assert_eq!(fit.size(540), Size::new(540.0, 950.0));
        assert_eq!(fit.size(1080), Size::new(1080.0, 1900.0));
        assert_eq!(fit.pixel_size(540), (1080, 1900));
        assert_eq!(fit.widths()[0], 540);
        assert_eq!(*fit.widths().last().unwrap(), 1080);
        for pair in fit.widths().windows(2) {
            assert_eq!(pair[1] - pair[0], 10);
        }
    }

    #[test]
    fn short_screen_widens_base_width_to_meet_800_height() {
        let iphone8 = ScreenFit::from_phone(&PhoneSpec::new(750.0, 1334.0, Some(375.0)));
        assert_eq!(iphone8.base_width(), 620);
        assert!(iphone8.size(620).height >= 800.0);
        assert!(iphone8.size(610).height < 800.0);
    }

    #[test]
    fn ratio_is_clamped_between_one_and_two() {
        let fold = ScreenFit::from_phone(&PhoneSpec::new(1856.0, 2160.0, None));
        assert_eq!(fold.ratio(), 1.0);
        assert_eq!(fold.size(fold.base_width()), Size::new(800.0, 800.0));

        let tall = ScreenFit::from_phone(&PhoneSpec::new(9.0, 30.0, None));
        assert_eq!(tall.ratio(), 2.0);
        assert_eq!(tall.size(540), Size::new(540.0, 1080.0));
    }

    #[test]
    fn width_to_fit_selects_narrowest_step_that_fits() {
        let fit = ScreenFit::standard();
        let area = base_area();
        assert_eq!(fit.width_to_fit(area, area, 540), 540);
        assert_eq!(fit.width_to_fit(Size::new(542.0, 898.0), area, 540), 540);
        assert_eq!(fit.width_to_fit(Size::new(740.0, 897.0), area, 540), 740);
        assert_eq!(fit.width_to_fit(Size::new(574.0, 897.0), area, 540), 580);
        assert_eq!(fit.width_to_fit(Size::new(580.0, 897.0), area, 540), 580);
        assert_eq!(fit.width_to_fit(Size::new(540.0, 1000.0), area, 540), 610);
        assert_eq!(fit.width_to_fit(Size::new(1200.0, 897.0), area, 540), 1080);
    }

    #[test]
    fn scale_and_home_bar_height_match_spec() {
        assert_eq!(ScreenFit::scale(540.0), 1.0);
        assert_eq!(ScreenFit::scale(1080.0), 2.0);
        assert_eq!(ScreenFit::home_bar_height(540.0), 28.0);
        assert_eq!(ScreenFit::home_bar_height(1080.0), 56.0);
    }

    #[test]
    fn round_away_from_zero_matches_dotnet() {
        assert_eq!(round_away_zero(0.5), 1.0);
        assert_eq!(round_away_zero(1.5), 2.0);
        assert_eq!(round_away_zero(2.5), 3.0);
        assert_eq!(round_away_zero(-0.5), -1.0);
        assert_eq!(round_away_zero(949.5), 950.0);
    }

    #[test]
    fn iphone17_pro_max_ratio_is_1_759() {
        let fit = ScreenFit::standard();
        assert!((fit.ratio() - 1.759_090_909).abs() < 1e-6, "ratio={}", fit.ratio());
    }
}
