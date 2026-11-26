//! 截图的裁剪几何，移植自 C# 版 `SnugWin.Core.PhoneScreen.CropGeometry`。
//!
//! 选区以 [0..1] 归一化坐标记录（换档时跟着图片缩放），裁切时再换算成原图像素。

use crate::geometry::{Point, Rect, Size};
use crate::screen_fit::round_away_zero;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CropHandle {
    TopLeft,
    Top,
    TopRight,
    Left,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// 把图片等比放进区域，居中显示。
pub fn fitted_rect(image: Size, area: Rect) -> Rect {
    if image.width <= 0.0 || image.height <= 0.0 || area.width <= 0.0 || area.height <= 0.0 {
        return Rect::ZERO;
    }
    let scale = f64::min(area.width / image.width, area.height / image.height);
    let w = image.width * scale;
    let h = image.height * scale;
    Rect::new(area.mid_x() - w / 2.0, area.mid_y() - h / 2.0, w, h)
}

/// 屏幕坐标 → 归一化选区。
pub fn unit_rect(rect: Rect, shown: Rect) -> Rect {
    if shown.width <= 0.0 || shown.height <= 0.0 {
        return Rect::ZERO;
    }
    Rect::new(
        (rect.x - shown.x) / shown.width,
        (rect.y - shown.y) / shown.height,
        rect.width / shown.width,
        rect.height / shown.height,
    )
}

/// 归一化选区 → 屏幕坐标。
pub fn rect_from_unit(unit: Rect, shown: Rect) -> Rect {
    Rect::new(
        shown.x + unit.x * shown.width,
        shown.y + unit.y * shown.height,
        unit.width * shown.width,
        unit.height * shown.height,
    )
}

/// 归一化选区 → 原图像素矩形（四边各自取整，不越出原图）。
pub fn pixel_rect(unit: Rect, image: Size) -> Rect {
    let min_x0 = f64::max(0.0, unit.x);
    let min_y0 = f64::max(0.0, unit.y);
    let max_x0 = f64::min(1.0, unit.max_x());
    let max_y0 = f64::min(1.0, unit.max_y());
    if max_x0 <= min_x0 || max_y0 <= min_y0 || image.width <= 0.0 || image.height <= 0.0 {
        return Rect::ZERO;
    }
    let min_x = round_away_zero(min_x0 * image.width);
    let min_y = round_away_zero(min_y0 * image.height);
    let max_x = round_away_zero(max_x0 * image.width);
    let max_y = round_away_zero(max_y0 * image.height);
    Rect::new(min_x, min_y, f64::max(0.0, max_x - min_x), f64::max(0.0, max_y - min_y))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Min,
    Max,
}

fn nearer(value: f64, lower: f64, upper: f64, radius: f64) -> Option<Side> {
    let to_min = (value - lower).abs();
    let to_max = (value - upper).abs();
    if f64::min(to_min, to_max) > radius {
        return None;
    }
    Some(if to_min <= to_max { Side::Min } else { Side::Max })
}

/// 命中哪个把手（把手的可点区域按 `radius` 外扩）。
pub fn handle_at(point: Point, rect: Rect, radius: f64) -> Option<CropHandle> {
    let expanded = Rect::new(
        rect.x - radius,
        rect.y - radius,
        rect.width + 2.0 * radius,
        rect.height + 2.0 * radius,
    );
    if !expanded.contains(point) {
        return None;
    }
    let hx = nearer(point.x, rect.min_x(), rect.max_x(), radius);
    let hy = nearer(point.y, rect.min_y(), rect.max_y(), radius);
    match (hx, hy) {
        (Some(Side::Min), Some(Side::Min)) => Some(CropHandle::TopLeft),
        (Some(Side::Max), Some(Side::Min)) => Some(CropHandle::TopRight),
        (Some(Side::Min), Some(Side::Max)) => Some(CropHandle::BottomLeft),
        (Some(Side::Max), Some(Side::Max)) => Some(CropHandle::BottomRight),
        (Some(Side::Min), None) => Some(CropHandle::Left),
        (Some(Side::Max), None) => Some(CropHandle::Right),
        (None, Some(Side::Min)) => Some(CropHandle::Top),
        (None, Some(Side::Max)) => Some(CropHandle::Bottom),
        _ => None,
    }
}

/// 拖动把手后的新选区：夹在 `bounds` 内，且不短于 `min_side`。
pub fn resized(rect: Rect, handle: CropHandle, dx: f64, dy: f64, bounds: Rect, min_side: f64) -> Rect {
    let mut min_x = rect.min_x();
    let mut max_x = rect.max_x();
    let mut min_y = rect.min_y();
    let mut max_y = rect.max_y();

    match handle {
        CropHandle::TopLeft | CropHandle::Left | CropHandle::BottomLeft => {
            min_x = f64::min(f64::max(rect.min_x() + dx, bounds.min_x()), max_x - min_side);
        }
        CropHandle::TopRight | CropHandle::Right | CropHandle::BottomRight => {
            max_x = f64::max(f64::min(rect.max_x() + dx, bounds.max_x()), min_x + min_side);
        }
        _ => {}
    }

    match handle {
        CropHandle::TopLeft | CropHandle::Top | CropHandle::TopRight => {
            min_y = f64::min(f64::max(rect.min_y() + dy, bounds.min_y()), max_y - min_side);
        }
        CropHandle::BottomLeft | CropHandle::Bottom | CropHandle::BottomRight => {
            max_y = f64::max(f64::min(rect.max_y() + dy, bounds.max_y()), min_y + min_side);
        }
        _ => {}
    }

    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitted_rect_centers_and_preserves_aspect() {
        let r = fitted_rect(Size::new(100.0, 200.0), Rect::new(0.0, 0.0, 100.0, 100.0));
        assert_eq!(r, Rect::new(25.0, 0.0, 50.0, 100.0));
        assert_eq!(fitted_rect(Size::ZERO, Rect::new(0.0, 0.0, 10.0, 10.0)), Rect::ZERO);
    }

    #[test]
    fn unit_and_screen_rect_roundtrip() {
        let shown = Rect::new(10.0, 20.0, 200.0, 400.0);
        let rect = Rect::new(60.0, 120.0, 100.0, 200.0);
        let unit = unit_rect(rect, shown);
        assert_eq!(unit, Rect::new(0.25, 0.25, 0.5, 0.5));
        assert_eq!(rect_from_unit(unit, shown), rect);
    }

    #[test]
    fn pixel_rect_clamps_and_rounds() {
        let image = Size::new(1000.0, 2000.0);
        let r = pixel_rect(Rect::new(0.25, 0.25, 0.5, 0.5), image);
        assert_eq!(r, Rect::new(250.0, 500.0, 500.0, 1000.0));

        let clamped = pixel_rect(Rect::new(-0.5, -0.5, 2.0, 2.0), image);
        assert_eq!(clamped, Rect::new(0.0, 0.0, 1000.0, 2000.0));

        assert_eq!(pixel_rect(Rect::ZERO, image), Rect::ZERO);
    }

    #[test]
    fn handles_hit_test_corners_edges_and_misses() {
        let rect = Rect::new(100.0, 100.0, 200.0, 100.0);
        let r = 12.0;
        assert_eq!(handle_at(Point::new(100.0, 100.0), rect, r), Some(CropHandle::TopLeft));
        assert_eq!(handle_at(Point::new(300.0, 200.0), rect, r), Some(CropHandle::BottomRight));
        assert_eq!(handle_at(Point::new(200.0, 100.0), rect, r), Some(CropHandle::Top));
        assert_eq!(handle_at(Point::new(300.0, 150.0), rect, r), Some(CropHandle::Right));
        assert_eq!(handle_at(Point::new(200.0, 150.0), rect, r), None);
        assert_eq!(handle_at(Point::new(500.0, 500.0), rect, r), None);
    }

    #[test]
    fn resize_respects_bounds_and_min_side() {
        let rect = Rect::new(100.0, 100.0, 200.0, 100.0);
        let bounds = Rect::new(0.0, 0.0, 500.0, 500.0);

        let grown = resized(rect, CropHandle::TopLeft, -50.0, -50.0, bounds, 20.0);
        assert_eq!(grown, Rect::new(50.0, 50.0, 250.0, 150.0));

        let clamped = resized(rect, CropHandle::TopLeft, -500.0, -500.0, bounds, 20.0);
        assert_eq!(clamped.min_x(), 0.0);
        assert_eq!(clamped.min_y(), 0.0);

        let min_limited = resized(rect, CropHandle::Left, 500.0, 0.0, bounds, 20.0);
        assert_eq!(min_limited.width, 20.0);

        let right = resized(rect, CropHandle::Right, 50.0, 0.0, bounds, 20.0);
        assert_eq!(right.width, 250.0);
    }
}
