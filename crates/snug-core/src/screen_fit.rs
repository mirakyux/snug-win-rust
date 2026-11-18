//! 手机屏的档位计算，移植自 C# v1.3.0 `SnugWin.Core.PhoneScreen.ScreenFit`（整块重写）。
//!
//! 规则（与 C# 版逐条对应）：
//! - 竖屏手机沿用老公式：高/宽 减去 UU 工具栏上下各 91 点，再按逻辑宽度归一，夹在 1.0~2.0；
//! - 横屏手机、平板、电脑各有自己的比例与基准宽算法（见 [`ScreenFit::from_device`]）；
//! - 宽度档位按 10 点一档；竖屏手机从 540 起步到 1080，
//!   其余设备从「够 800 点高」的基准宽起步，最多到基准宽的两倍（受最大像素夹取）；
//! - 物理像素：触屏与 HiDPI 电脑按 2 倍（Windows 200% DPI 虚拟屏）下发，超大电脑屏退回 1 倍；
//!   宽高都保证偶数，便于编码器处理。
//!
//! `ScreenFit` 不可变，值相等按 C# record 的四个字段比较（比例、基准宽、方向、设备种类），
//! **不比较档位表**——档位表由前三者唯一决定。

use serde::{Deserialize, Serialize};

use crate::geometry::Size;
use crate::phone_models::{DeviceKind, ScreenOrientation};

pub const DESIGN_WIDTH: f64 = 540.0;
/// 横屏设备的最短长边（保证铺满后仍有 800 点可用宽）。
pub const MIN_LONG_SIDE: i32 = 800;
/// 竖屏设备的最短短边。
pub const MIN_SHORT_SIDE: i32 = 525;
pub const MIN_WIDTH: i32 = 540;
pub const MIN_HEIGHT: f64 = 800.0;
pub const MAX_WIDTH: i32 = 1080;
pub const STEP: i32 = 10;
pub const MIN_RATIO: f64 = 1.0;
pub const MAX_RATIO: f64 = 2.0;
pub const MIN_LANDSCAPE_RATIO: f64 = 0.3;
pub const MAX_LANDSCAPE_RATIO: f64 = 1.0;

/// 竖屏手机允许的最大像素（逻辑像素 × 2，见 C# `MaxPixels`）。
pub const MAX_PIXELS: (f64, f64) = (MAX_WIDTH as f64 * 2.0, (MAX_WIDTH as f64 * MAX_RATIO) * 2.0);
/// 横屏设备允许的最大像素（C# `LandscapeMaxPixels`）。
pub const LANDSCAPE_MAX_PIXELS: (f64, f64) = (6144.0, 3840.0);

/// UU 远程工具栏在手机屏上下的占位（点）。
const UU_INSET: f64 = 91.0;
/// 横屏时左右两侧工具栏占位之和（82 + 72）。
const UU_LANDSCAPE_INSETS: f64 = 82.0 + 72.0;
/// 安卓等没有逻辑宽度数据时的估算宽度。
const ASSUMED_POINTS: f64 = 400.0;
/// 平板没有逻辑宽度数据时的估算宽度。
const ASSUMED_TABLET_POINTS: f64 = 800.0;
/// 平板基准宽比逻辑宽度小一点，铺满后留出工具栏。
const TABLET_SCALE: f64 = 1.24;
const BASE_HOME_BAR_HEIGHT: f64 = 28.0;

/// 一款设备的屏幕规格（`assets/phone-models.json` 里的一条）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhoneSpec {
    pub width: f64,
    pub height: f64,
    /// Apple 设备才有逻辑宽度。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<f64>,
    /// 挖孔/刘海宽度（电脑屏横屏时从可用宽度里扣掉）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notch: Option<f64>,
}

impl PhoneSpec {
    pub const fn new(width: f64, height: f64, points: Option<f64>) -> Self {
        Self { width, height, points, notch: None }
    }

    pub const fn with_notch(mut self, notch: Option<f64>) -> Self {
        self.notch = notch;
        self
    }

    /// iPhone 17 Pro Max，540×950 的基准（与 C# `PhoneChoice.Standard` 一致）。
    pub const fn standard() -> Self {
        Self::new(1320.0, 2868.0, Some(440.0))
    }
}

#[derive(Debug, Clone)]
pub struct ScreenFit {
    ratio: f64,
    base_width: i32,
    widths: Vec<i32>,
    orientation: ScreenOrientation,
    kind: DeviceKind,
}

impl ScreenFit {
    /// 竖屏手机的档位表（公开构造，对应 C# `ScreenFit(double ratio)`）。
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
        Self {
            ratio,
            base_width,
            widths,
            orientation: ScreenOrientation::Portrait,
            kind: DeviceKind::Phone,
        }
    }

    /// 竖屏手机：老公式 `高/宽 - 2*91/逻辑宽`。
    pub fn from_phone(phone: &PhoneSpec) -> Self {
        let points = phone.points.unwrap_or(ASSUMED_POINTS);
        Self::new(phone.height / phone.width - 2.0 * UU_INSET / points)
    }

    /// 任意设备 × 方向（C# `FromDevice(PhoneScreenSpecData, DeviceKind, ScreenOrientation)`）。
    ///
    /// 四路分支逐行照搬 C#，别在这里"顺手优化"：
    /// - 手机竖屏：老公式，保证现有 7 条 screen_fit 测试数值不变；
    /// - 手机横屏：`points / (长边*points/短边 - 82 - 72)`，比例通常小于 1；
    /// - 平板：纵向比例 `长/短`，横向 `短/长`，基准宽 = 逻辑宽 / 1.24；
    /// - 电脑：横屏 `(宽-挖孔)/高`、竖屏 `高/宽`，基准宽直接取请求的那一边。
    pub fn from_device(
        device: &PhoneSpec,
        kind: DeviceKind,
        orientation: ScreenOrientation,
    ) -> Self {
        let short_side = device.width;
        let long_side = device.height;

        match (kind, orientation) {
            (DeviceKind::Phone, ScreenOrientation::Portrait) => Self::from_phone(device),

            (DeviceKind::Phone, ScreenOrientation::Landscape) =>
                Self::create_phone_landscape(short_side, long_side, device.points),

            (DeviceKind::Tablet, _) =>
                Self::create_tablet(short_side, long_side, device.points, orientation),

            (DeviceKind::Computer, ScreenOrientation::Landscape) => Self::with_orientation(
                (short_side - device.notch.unwrap_or(0.0)) / long_side,
                ScreenOrientation::Landscape,
                DeviceKind::Computer,
                Some(device.height as i32),
            ),

            (DeviceKind::Computer, ScreenOrientation::Portrait) => Self::with_orientation(
                long_side / short_side,
                ScreenOrientation::Portrait,
                DeviceKind::Computer,
                Some(device.width as i32),
            ),
        }
    }

    fn create_phone_landscape(short_side: f64, long_side: f64, points_spec: Option<f64>) -> Self {
        let points = points_spec.unwrap_or(ASSUMED_POINTS);
        let ratio = points / (long_side * points / short_side - UU_LANDSCAPE_INSETS);
        Self::with_orientation(
            ratio,
            ScreenOrientation::Landscape,
            DeviceKind::Phone,
            None,
        )
    }

    fn create_tablet(
        short_side: f64,
        long_side: f64,
        points_spec: Option<f64>,
        orientation: ScreenOrientation,
    ) -> Self {
        let points = points_spec.unwrap_or(ASSUMED_TABLET_POINTS);
        let across = if orientation == ScreenOrientation::Portrait {
            points
        } else {
            long_side * points / short_side
        };
        let ratio = if orientation == ScreenOrientation::Portrait {
            long_side / short_side
        } else {
            short_side / long_side
        };
        let wanted_width = round_away_zero(across / TABLET_SCALE) as i32;
        Self::with_orientation(ratio, orientation, DeviceKind::Tablet, Some(wanted_width))
    }

    /// 非手机竖屏的构造（C# 私有构造）。`wanted_width` 是调用方希望从哪一档起步。
    fn with_orientation(
        ratio: f64,
        orientation: ScreenOrientation,
        kind: DeviceKind,
        wanted_width: Option<i32>,
    ) -> Self {
        let ratio = if orientation == ScreenOrientation::Portrait {
            ratio.clamp(MIN_RATIO, MAX_RATIO)
        } else {
            ratio.clamp(MIN_LANDSCAPE_RATIO, MAX_LANDSCAPE_RATIO)
        };

        let max_pixels = if orientation == ScreenOrientation::Portrait {
            MAX_PIXELS
        } else {
            LANDSCAPE_MAX_PIXELS
        };
        // C# `(int)Math.Min(...)`：正数截断，取「宽的一半」与「高的一半按比例反算」的较小者。
        let limit = (max_pixels.0 / 2.0).min((max_pixels.1 / 2.0 / ratio).floor()) as i32;

        if kind == DeviceKind::Computer {
            let w = MIN_LONG_SIDE.max(wanted_width.unwrap_or(1920));
            return Self { ratio, base_width: w, widths: vec![w], orientation, kind };
        }

        let min_w = if orientation == ScreenOrientation::Portrait { MIN_SHORT_SIDE } else { MIN_LONG_SIDE };
        let min_h = if orientation == ScreenOrientation::Portrait { MIN_LONG_SIDE } else { MIN_SHORT_SIDE };
        // 先算出「高至少 min_h」所需的最窄宽度，再让调用方想要的值夹在 [threshold, limit] 里。
        let threshold = min_w.max(((min_h as f64 - 0.5) / ratio).ceil() as i32);
        let clamped_base = (wanted_width.unwrap_or(threshold)).max(threshold).min(threshold.max(limit));
        let base_width = clamped_base;

        let mut widths = Vec::new();
        let max_range = (base_width * 2).min(base_width.max(limit));
        let mut w = base_width;
        while w <= max_range {
            widths.push(w);
            w += STEP;
        }
        if widths.is_empty() {
            widths.push(base_width);
        }
        Self { ratio, base_width, widths, orientation, kind }
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

    pub fn orientation(&self) -> ScreenOrientation {
        self.orientation
    }

    pub fn kind(&self) -> DeviceKind {
        self.kind
    }

    /// 注册顺序（C# `RegistrationOrder`）：从宽到窄。
    pub fn registration_order(&self) -> Vec<i32> {
        let mut widths = self.widths.clone();
        widths.reverse();
        widths
    }

    /// 某个逻辑宽度对应的逻辑尺寸（宽 × 高）。
    pub fn size(&self, width: i32) -> Size {
        Size::new(width as f64, round_away_zero(width as f64 * self.ratio))
    }

    /// 下发给虚拟屏的物理像素尺寸，保证为偶数。
    ///
    /// 触屏固定 2 倍（200% DPI）；电脑屏超过 3840×2400 时退回 1 倍，
    /// 否则 4K 屏会被放大成 8K，虚拟屏驱动扛不住。
    pub fn pixel_size(&self, logical_width: i32) -> (i32, i32) {
        let logical = self.size(logical_width);
        let multiplier = if self.kind == DeviceKind::Computer
            && (logical.width * 2.0 > 3840.0 || logical.height * 2.0 > 2400.0)
        {
            1
        } else {
            2
        };
        let mut pw = logical.width as i32 * multiplier;
        let mut ph = logical.height as i32 * multiplier;
        if pw % 2 != 0 {
            pw += 1;
        }
        if ph % 2 != 0 {
            ph += 1;
        }
        (pw, ph)
    }

    /// 目标 DPI 百分比（C# `TargetDpiPercent`），与 [`Self::pixel_size`] 同一判据。
    pub fn target_dpi_percent(&self, logical_width: i32) -> i32 {
        let logical = self.size(logical_width);
        if self.kind == DeviceKind::Computer
            && (logical.width * 2.0 > 3840.0 || logical.height * 2.0 > 2400.0)
        {
            100
        } else {
            200
        }
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
        self.widths.last().copied().unwrap_or(MAX_WIDTH)
    }

    /// 屏幕宽度相对设计宽度的倍率。
    pub fn scale(screen_width: f64) -> f64 {
        screen_width / DESIGN_WIDTH
    }

    /// 按「短边」算倍率（C# `Scale(SizeD)`）。
    pub fn scale_of(screen: Size) -> f64 {
        screen.width.min(screen.height) / DESIGN_WIDTH
    }

    /// 底部指示条高度，随屏幕宽度等比放大。
    pub fn home_bar_height(screen_width: f64) -> f64 {
        round_away_zero(BASE_HOME_BAR_HEIGHT * Self::scale(screen_width))
    }

    /// 按屏幕尺寸算底部指示条高度（C# `HomeBarHeight(SizeD)`）。
    pub fn home_bar_height_of(screen: Size) -> f64 {
        round_away_zero(BASE_HOME_BAR_HEIGHT * Self::scale_of(screen))
    }

    /// 桌面预览的缩放布局（C# `ComputePreviewLayout`）：返回设计宽、设计高、缩放系数。
    ///
    /// 最多占工作区的 72% 宽、84% 高（再让出 48 点给标题栏），
    /// 缩放系数夹在 0.35~0.84 之间——小屏也看得清、大屏不夸张。
    pub fn compute_preview_layout(work_area: Size, fit: &ScreenFit) -> (f64, f64, f64) {
        let logical = fit.size(fit.base_width());
        let max_w = f64::max(300.0, work_area.width * 0.72);
        let max_h = f64::max(420.0, work_area.height * 0.84 - 48.0);
        let scale = 0.84_f64.min(max_w / logical.width).min(max_h / logical.height);
        let scale = scale.max(0.35);
        (logical.width, logical.height, scale)
    }
}

impl PartialEq for ScreenFit {
    /// 对应 C# record 的 `Equals`：只比这四个字段，档位表是推导出来的。
    fn eq(&self, other: &Self) -> bool {
        self.ratio == other.ratio
            && self.base_width == other.base_width
            && self.orientation == other.orientation
            && self.kind == other.kind
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

    fn spec(width: i32, height: i32, points: Option<i32>) -> PhoneSpec {
        PhoneSpec::new(width as f64, height as f64, points.map(|p| p as f64))
    }

    #[test]
    fn standard_iphone_17_pro_max_is_540x950_with_all_steps() {
        let fit = ScreenFit::standard();
        assert_eq!(fit.size(540), Size::new(540.0, 950.0));
        assert_eq!(fit.size(1080), Size::new(1080.0, 1900.0));
        assert_eq!(fit.pixel_size(540), (1080, 1900));
        assert_eq!(fit.widths()[0], 540);
        assert_eq!(*fit.widths().last().unwrap(), 1080);
        assert_eq!(fit.orientation(), ScreenOrientation::Portrait);
        assert_eq!(fit.kind(), DeviceKind::Phone);
        for pair in fit.widths().windows(2) {
            assert_eq!(pair[1] - pair[0], 10);
        }
    }

    #[test]
    fn short_screen_widens_base_width_to_meet_800_height() {
        let iphone8 = ScreenFit::from_phone(&spec(750, 1334, Some(375)));
        assert_eq!(iphone8.base_width(), 620);
        assert!(iphone8.size(620).height >= 800.0);
        assert!(iphone8.size(610).height < 800.0);
    }

    #[test]
    fn ratio_is_clamped_between_one_and_two() {
        let fold = ScreenFit::from_phone(&spec(1856, 2160, None));
        assert_eq!(fold.ratio(), 1.0);
        assert_eq!(fold.size(fold.base_width()), Size::new(800.0, 800.0));

        let tall = ScreenFit::from_phone(&spec(9, 30, None));
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

    #[test]
    fn from_device_portrait_phone_is_identical_to_from_phone() {
        for (w, h, p) in [(1320, 2868, Some(440)), (1170, 2532, Some(390)), (720, 1280, None)] {
            let device = spec(w, h, p);
            let portrait = ScreenFit::from_device(
                &device,
                DeviceKind::Phone,
                ScreenOrientation::Portrait,
            );
            let legacy = ScreenFit::from_phone(&device);
            assert_eq!(portrait, legacy, "{w}x{h}");
            assert_eq!(portrait.widths(), legacy.widths());
        }
    }

    #[test]
    fn landscape_phone_ratio_matches_the_csharp_formula() {
        // C#：ratio = points / (longSide * points / shortSide - (82 + 72))
        let device = spec(1206, 2622, Some(402));
        let fit = ScreenFit::from_device(&device, DeviceKind::Phone, ScreenOrientation::Landscape);
        let expected = 402.0 / (2622.0 * 402.0 / 1206.0 - 154.0);
        assert!((fit.ratio() - expected).abs() < 1e-9, "ratio={} expected={expected}", fit.ratio());
        assert!(fit.ratio() < 1.0, "横屏比例应小于 1");
        assert_eq!(fit.orientation(), ScreenOrientation::Landscape);
        assert_eq!(fit.kind(), DeviceKind::Phone);

        // 基准宽来自「高至少 525 点」反算，再和 800（横屏最短长边）取大。
        let threshold = 800_i32.max((524.5_f64 / expected).ceil() as i32);
        assert_eq!(fit.base_width(), threshold);
        assert_eq!(fit.widths()[0], threshold);

        // 档位从基准宽每 10 点一档，最多到 min(基准×2, 最大像素反算)。
        let limit = (LANDSCAPE_MAX_PIXELS.0 / 2.0)
            .min((LANDSCAPE_MAX_PIXELS.1 / 2.0 / expected).floor()) as i32;
        let max_range = (threshold * 2).min(threshold.max(limit));
        assert_eq!(*fit.widths().last().unwrap(), threshold + (max_range - threshold) / 10 * 10);
    }

    #[test]
    fn tablet_uses_own_scale_and_points_fallback() {
        // C#：横向 ratio = 短/长，基准宽 = round(长*逻辑宽/短 / 1.24)
        let device = spec(1668, 2388, Some(834));
        let fit = ScreenFit::from_device(&device, DeviceKind::Tablet, ScreenOrientation::Landscape);
        let across = 2388.0 * 834.0 / 1668.0;
        let expected_width = round_away_zero(across / 1.24) as i32;
        assert_eq!(fit.kind(), DeviceKind::Tablet);
        assert!((fit.ratio() - 1668.0 / 2388.0).abs() < 1e-12);
        assert_eq!(fit.base_width(), expected_width.max(MIN_LONG_SIDE));
        assert_eq!(fit.target_dpi_percent(fit.base_width()), 200);

        // 没有逻辑宽度时按 800 点估算，同样不能落回手机公式。
        let no_points = ScreenFit::from_device(
            &spec(1668, 2388, None),
            DeviceKind::Tablet,
            ScreenOrientation::Portrait,
        );
        let expected = round_away_zero(800.0 / 1.24) as i32;
        assert_eq!(no_points.ratio(), 2388.0 / 1668.0);
        assert_eq!(no_points.base_width(), expected.max(MIN_SHORT_SIDE));
    }

    #[test]
    fn computer_landscape_and_portrait_use_requested_side() {
        // 16:9 屏的 (宽-挖孔)/高 大于 1，横屏比例按 C# 会被夹到 1.0，
        // 基准宽直接取高——C# 的测试也只断言 DPI 与单档，不断言「看起来像 16:9」。
        let landscape = ScreenFit::from_device(
            &spec(1920, 1080, None),
            DeviceKind::Computer,
            ScreenOrientation::Landscape,
        );
        assert_eq!(landscape.ratio(), MAX_LANDSCAPE_RATIO);
        assert_eq!(landscape.base_width(), 1080, "横屏取高作为基准宽");
        assert_eq!(landscape.widths(), &[1080], "电脑屏只有一档");

        // 竖屏电脑用 高/宽，反向 16:9 规格恰好落在 [1,2] 内，能直接验公式。
        let portrait = ScreenFit::from_device(
            &spec(1080, 1920, None),
            DeviceKind::Computer,
            ScreenOrientation::Portrait,
        );
        assert_eq!(portrait.ratio(), 1920.0 / 1080.0);
        assert_eq!(portrait.base_width(), 1080, "竖屏取宽作为基准宽");
        assert_eq!(portrait.size(1080), Size::new(1080.0, 1920.0));

        // 挖孔要从可用宽度里扣掉；用 4:3 屏让比例留在 [0.3, 1.0] 内才看得出扣减。
        let notched = ScreenFit::from_device(
            &PhoneSpec::new(1600.0, 1200.0, None).with_notch(Some(600.0)),
            DeviceKind::Computer,
            ScreenOrientation::Landscape,
        );
        assert_eq!(notched.ratio(), 1000.0 / 1200.0);
        assert_eq!(notched.base_width(), 1200);
        assert_eq!(notched.size(1200), Size::new(1200.0, 1000.0));
    }

    #[test]
    fn pixel_size_and_dpi_switch_to_1x_for_big_computer_screens() {
        let hd = ScreenFit::from_device(
            &spec(1920, 1080, None),
            DeviceKind::Computer,
            ScreenOrientation::Landscape,
        );
        assert_eq!(hd.size(1080), Size::new(1080.0, 1080.0));
        assert_eq!(hd.pixel_size(1080), (2160, 2160), "2×1080 不超 3840，仍是 2 倍");
        assert_eq!(hd.target_dpi_percent(1080), 200);

        let uhd = ScreenFit::from_device(
            &spec(3840, 2160, None),
            DeviceKind::Computer,
            ScreenOrientation::Landscape,
        );
        assert_eq!(uhd.base_width(), 2160);
        assert_eq!(uhd.size(2160), Size::new(2160.0, 2160.0));
        assert_eq!(uhd.pixel_size(2160), (2160, 2160), "4K 屏退回 1 倍");
        assert_eq!(uhd.target_dpi_percent(2160), 100);

        // 触屏永远是 2 倍，不受这个判据影响。
        assert_eq!(ScreenFit::standard().target_dpi_percent(540), 200);
    }

    #[test]
    fn registration_order_is_widths_reversed() {
        let fit = ScreenFit::from_device(
            &spec(1206, 2622, Some(402)),
            DeviceKind::Phone,
            ScreenOrientation::Landscape,
        );
        let mut expected = fit.widths().to_vec();
        expected.reverse();
        assert_eq!(fit.registration_order(), expected);
        assert!(fit.registration_order()[0] >= *fit.registration_order().last().unwrap());
    }

    #[test]
    fn equality_ignores_the_derived_width_table_like_the_csharp_record() {
        let a = ScreenFit::standard();
        let b = ScreenFit::standard();
        assert_eq!(a, b);
        let landscape = ScreenFit::from_device(
            &spec(1320, 2868, Some(440)),
            DeviceKind::Phone,
            ScreenOrientation::Landscape,
        );
        assert_ne!(a, landscape, "方向不同就不相等");
    }

    #[test]
    fn compute_preview_layout_fits_within_the_work_area() {
        // 抄自 C# `ScreenFit_ComputePreviewLayout_FitsWithinWorkAreaAcrossResolutions`。
        for (work_w, work_h, phone_w, phone_h) in [
            (1920.0, 1032.0, 1320, 2868),
            (2048.0, 1240.0, 1320, 2868),
            (1280.0, 680.0, 1080, 2400),
        ] {
            let fit = ScreenFit::from_phone(&spec(phone_w, phone_h, None));
            let (design_w, design_h, scale) =
                ScreenFit::compute_preview_layout(Size::new(work_w, work_h), &fit);
            assert!((0.35..=0.84).contains(&scale), "scale={scale}");
            assert!(design_w * scale <= work_w * 0.72 + 0.01);
            assert!(design_h * scale <= work_h * 0.84 + 0.01);
        }
    }

    #[test]
    fn scale_of_uses_the_shorter_side() {
        assert_eq!(ScreenFit::scale_of(Size::new(1080.0, 1920.0)), 2.0);
        assert_eq!(ScreenFit::home_bar_height_of(Size::new(1080.0, 1920.0)), 56.0);
    }
}
