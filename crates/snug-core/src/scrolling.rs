//! 滚动与惯性，移植自 C# 版 `SnugWin.Core.Touch.Scrolling`。

use crate::geometry::{Point, hypot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollPhase {
    None,
    Began,
    Changed,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MomentumPhase {
    None,
    Begin,
    Continue,
    End,
}

/// 一次滚动事件：整像素量 + 原始增量 + 阶段 + 事件位置。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollStep {
    pub pixels_x: i32,
    pub pixels_y: i32,
    pub delta_x: f64,
    pub delta_y: f64,
    pub phase: ScrollPhase,
    pub momentum: MomentumPhase,
    pub location: Point,
}

/// 把亚像素位移累积成整像素滚轮量，余量下次继续。
#[derive(Debug, Clone, Copy, Default)]
pub struct ScrollWheel {
    remainder_x: f64,
    remainder_y: f64,
}

impl ScrollWheel {
    pub fn reset(&mut self) {
        self.remainder_x = 0.0;
        self.remainder_y = 0.0;
    }

    pub fn step(
        &mut self,
        dx: f64,
        dy: f64,
        phase: ScrollPhase,
        momentum: MomentumPhase,
        location: Point,
    ) -> ScrollStep {
        self.remainder_x += dx;
        self.remainder_y += dy;
        let whole_x = self.remainder_x.trunc();
        let whole_y = self.remainder_y.trunc();
        self.remainder_x -= whole_x;
        self.remainder_y -= whole_y;
        ScrollStep {
            pixels_x: whole_x as i32,
            pixels_y: whole_y as i32,
            delta_x: dx,
            delta_y: dy,
            phase,
            momentum,
            location,
        }
    }
}

/// 松手后的惯性滚动：每帧衰减 0.95，直到慢于阈值停止。
#[derive(Debug, Clone, Copy)]
pub struct Momentum {
    vx: f64,
    vy: f64,
    stop_speed: f64,
    started: bool,
}

impl Momentum {
    pub const FRAME_RATE: f64 = 60.0;
    const DECAY: f64 = 0.95;
    const MINIMUM_SPEED: f64 = 200.0;
    const STOP_SPEED: f64 = 40.0;

    /// 速度不够就不进入惯性阶段。
    pub fn try_create(vx: f64, vy: f64, scale: f64) -> Option<Self> {
        if hypot(vx, vy) <= Self::MINIMUM_SPEED * scale {
            return None;
        }
        Some(Self { vx, vy, stop_speed: Self::STOP_SPEED * scale, started: false })
    }

    /// 取下一帧的位移与阶段；返回 None 表示惯性结束。
    pub fn next_frame(&mut self) -> Option<(f64, f64, MomentumPhase)> {
        self.vx *= Self::DECAY;
        self.vy *= Self::DECAY;
        if hypot(self.vx, self.vy) < self.stop_speed {
            return None;
        }
        let phase = if self.started { MomentumPhase::Continue } else { MomentumPhase::Begin };
        self.started = true;
        Some((self.vx / Self::FRAME_RATE, self.vy / Self::FRAME_RATE, phase))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_accumulates_subpixel_remainder() {
        let mut w = ScrollWheel::default();
        let a = w.step(0.4, 0.6, ScrollPhase::Changed, MomentumPhase::None, Point::ZERO);
        assert_eq!((a.pixels_x, a.pixels_y), (0, 0));
        let b = w.step(0.7, 0.6, ScrollPhase::Changed, MomentumPhase::None, Point::ZERO);
        assert_eq!((b.pixels_x, b.pixels_y), (1, 1));
        let c = w.step(0.0, 0.0, ScrollPhase::Ended, MomentumPhase::None, Point::ZERO);
        assert_eq!((c.pixels_x, c.pixels_y), (0, 0));
    }

    #[test]
    fn momentum_requires_minimum_speed() {
        assert!(Momentum::try_create(10.0, 10.0, 1.0).is_none());
        assert!(Momentum::try_create(1000.0, 0.0, 1.0).is_some());
    }

    #[test]
    fn momentum_decays_to_stop() {
        let mut m = Momentum::try_create(3000.0, 0.0, 1.0).expect("momentum");
        let mut frames = 0;
        let mut first_phase = None;
        while let Some((dx, _dy, phase)) = m.next_frame() {
            if first_phase.is_none() {
                first_phase = Some(phase);
            }
            assert!(dx > 0.0);
            frames += 1;
        }
        assert_eq!(first_phase, Some(MomentumPhase::Begin));
        assert!(frames > 10 && frames < 400, "frames={frames}");
    }
}
