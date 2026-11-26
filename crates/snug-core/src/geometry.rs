//! 基础几何类型，对应 C# 版的 `PointD` / `SizeD` / `RectD` 与 `MathEx`。

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// 两点欧氏距离（C# `PointD.DistanceTo`）。
    pub fn distance_to(self, other: Point) -> f64 {
        hypot(self.x - other.x, self.y - other.y)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Size {
    pub const ZERO: Size = Size { width: 0.0, height: 0.0 };

    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const ZERO: Rect = Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };

    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { x, y, width, height }
    }

    pub fn from_origin_size(origin: Point, size: Size) -> Self {
        Self::new(origin.x, origin.y, size.width, size.height)
    }

    pub fn min_x(self) -> f64 {
        self.x
    }

    pub fn min_y(self) -> f64 {
        self.y
    }

    pub fn max_x(self) -> f64 {
        self.x + self.width
    }

    pub fn max_y(self) -> f64 {
        self.y + self.height
    }

    pub fn mid_x(self) -> f64 {
        self.x + self.width / 2.0
    }

    pub fn mid_y(self) -> f64 {
        self.y + self.height / 2.0
    }

    pub fn size(self) -> Size {
        Size::new(self.width, self.height)
    }

    pub fn center(self) -> Point {
        Point::new(self.mid_x(), self.mid_y())
    }

    /// 含边界的点包含判定（C# `RectD.Contains`）。
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.min_x() && p.x <= self.max_x() && p.y >= self.min_y() && p.y <= self.max_y()
    }

    /// 容差比较：位置与尺寸都在 `tolerance` 内视为同一矩形。
    pub fn matches(self, other: Rect, tolerance: f64) -> bool {
        (self.min_x() - other.min_x()).abs() < tolerance
            && (self.min_y() - other.min_y()).abs() < tolerance
            && (self.width - other.width).abs() < tolerance
            && (self.height - other.height).abs() < tolerance
    }

    /// 日志用的紧凑写法，如 `(12,34 100x200)`。
    pub fn short(self) -> String {
        format!(
            "({},{} {}x{})",
            self.min_x() as i32,
            self.min_y() as i32,
            self.width as i32,
            self.height as i32
        )
    }
}

impl fmt::Display for Rect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.short())
    }
}

pub fn hypot(x: f64, y: f64) -> f64 {
    (x * x + y * y).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_edges_and_center() {
        let r = Rect::new(10.0, 20.0, 100.0, 200.0);
        assert_eq!(r.min_x(), 10.0);
        assert_eq!(r.max_x(), 110.0);
        assert_eq!(r.mid_x(), 60.0);
        assert_eq!(r.max_y(), 220.0);
        assert_eq!(r.mid_y(), 120.0);
        assert_eq!(r.center(), Point::new(60.0, 120.0));
        assert_eq!(r.size(), Size::new(100.0, 200.0));
    }

    #[test]
    fn contains_includes_boundaries() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains(Point::new(0.0, 0.0)));
        assert!(r.contains(Point::new(10.0, 10.0)));
        assert!(!r.contains(Point::new(10.1, 5.0)));
    }

    #[test]
    fn matches_uses_tolerance() {
        let a = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert!(a.matches(Rect::new(1.0, 1.0, 101.0, 101.0), 2.0));
        assert!(!a.matches(Rect::new(3.0, 0.0, 100.0, 100.0), 2.0));
    }

    #[test]
    fn short_format_rounds_to_int() {
        assert_eq!(Rect::new(12.4, 34.6, 99.9, 200.2).short(), "(12,34 99x200)");
    }

    #[test]
    fn distance_is_euclidean() {
        assert_eq!(Point::new(0.0, 0.0).distance_to(Point::new(3.0, 4.0)), 5.0);
    }
}
