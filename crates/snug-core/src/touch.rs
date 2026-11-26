//! 触控翻译状态机，移植自 C# 版 `SnugWin.Core.Touch.TouchTranslator`。
//!
//! 输入是 UU 注入的鼠标事件（按下/拖动/抬起 + 时间戳），输出是"放行原始事件"或
//! "吞掉并重放一组指令"。所有判定只依赖纯数值，不碰任何 Win32 API。

use crate::geometry::{Point, Rect};
use crate::screen_fit::ScreenFit;
use crate::scrolling::{Momentum, MomentumPhase, ScrollPhase, ScrollStep, ScrollWheel};

/// 手机屏上窗口自带的关闭按钮位置，用于把"点关闭"翻译成"先归位再关闭"。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CloseButton {
    pub frame: Rect,
    pub window_handle: u64,
    pub window_frame: Rect,
}

impl CloseButton {
    /// 该点按下时，这个关闭按钮是否位于最前（换言之点击会真的命中它）。
    pub fn is_frontmost(&self, point: Point, on_screen: &[(u64, Rect)]) -> bool {
        if !self.frame.contains(point) {
            return false;
        }
        for (handle, frame) in on_screen {
            if frame.contains(point) {
                return *handle == self.window_handle && frame.matches(self.window_frame, 2.0);
            }
        }
        false
    }
}

/// 手势生效区域：手机屏整体边界、当前缩放倍率，以及可选的关闭按钮。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchRegion {
    pub frame: Rect,
    pub scale: f64,
    pub close_button: Option<CloseButton>,
}

impl TouchRegion {
    pub fn for_phone_screen(bounds: Rect, close_button: Option<CloseButton>) -> Self {
        Self { frame: bounds, scale: ScreenFit::scale(bounds.width), close_button }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchInputKind {
    Press,
    Drag,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchInput {
    pub kind: TouchInputKind,
    pub location: Point,
    pub time: f64,
}

impl TouchInput {
    pub const fn new(kind: TouchInputKind, x: f64, y: f64, time: f64) -> Self {
        Self { kind, location: Point::new(x, y), time }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchOptions {
    pub allow_scroll: bool,
    pub allow_right_click: bool,
    pub notify_selection_ended: bool,
}

impl Default for TouchOptions {
    fn default() -> Self {
        Self { allow_scroll: true, allow_right_click: true, notify_selection_ended: false }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchCommand {
    ReplayPress,
    ReplayCurrent,
    RightClick { at: Point },
    Scroll(ScrollStep),
    CloseWindow { at: Point },
    SelectionCompleted { at: Point },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TouchResponse {
    pub pass_original: bool,
    pub commands: Vec<TouchCommand>,
}

impl TouchResponse {
    pub fn pass() -> Self {
        Self { pass_original: true, commands: Vec::new() }
    }

    pub fn swallow_empty() -> Self {
        Self { pass_original: false, commands: Vec::new() }
    }

    pub fn swallow(commands: Vec<TouchCommand>) -> Self {
        Self { pass_original: false, commands }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Pending,
    Scrolling,
    Selecting,
}

#[derive(Debug, Default)]
pub struct TouchTranslator {
    phase: Phase,
    pending_since: f64,
    origin: Point,
    last: Point,
    samples: Vec<(f64, Point)>,
    wheel: ScrollWheel,
    momentum: Option<Momentum>,
    scale: f64,
    press_stopped_coast: bool,
    press_on_close: bool,
}

impl Default for Phase {
    fn default() -> Self {
        Phase::Idle
    }
}

impl TouchTranslator {
    /// 判定"滑动"的位移阈值（点）；按屏宽等比放大。
    pub const SLOP: f64 = 10.0;
    /// 判定"长按"的时长（秒）。
    pub const HOLD_DURATION: f64 = 0.5;
    const FLING_WINDOW: f64 = 0.1;
    const MIN_FLING_SPAN: f64 = 0.04;
    const MAX_FLING_SPEED: f64 = 4000.0;
    const MAX_SAMPLES: usize = 30;

    pub fn is_coasting(&self) -> bool {
        self.momentum.is_some()
    }

    pub fn origin(&self) -> Point {
        self.origin
    }

    pub fn handle(&mut self, input: TouchInput, region: Option<&TouchRegion>) -> TouchResponse {
        self.handle_with(input, region, TouchOptions::default())
    }

    pub fn handle_with(
        &mut self,
        input: TouchInput,
        region: Option<&TouchRegion>,
        options: TouchOptions,
    ) -> TouchResponse {
        match input.kind {
            TouchInputKind::Press => self.press(input, region),
            TouchInputKind::Drag => self.drag(input, options),
            TouchInputKind::Release => self.release(input, options),
        }
    }

    /// 惯性滚动的一帧；None 表示本次没有惯性或刚结束（结束时会补一个 End 步）。
    pub fn momentum_frame(&mut self) -> Option<ScrollStep> {
        let mut momentum = self.momentum?;
        match momentum.next_frame() {
            None => {
                self.momentum = None;
                Some(self.wheel.step(
                    0.0,
                    0.0,
                    ScrollPhase::None,
                    MomentumPhase::End,
                    self.origin,
                ))
            }
            Some((dx, dy, phase)) => {
                self.momentum = Some(momentum);
                Some(self.wheel.step(dx, dy, ScrollPhase::None, phase, self.origin))
            }
        }
    }

    fn press(&mut self, input: TouchInput, region: Option<&TouchRegion>) -> TouchResponse {
        let stop = self.stop_momentum();
        let inside = region.map(|r| r.frame.contains(input.location)).unwrap_or(false);
        if !inside {
            self.phase = Phase::Idle;
            return match stop {
                None => TouchResponse::pass(),
                Some(step) => TouchResponse {
                    pass_original: true,
                    commands: vec![TouchCommand::Scroll(step)],
                },
            };
        }
        let region = region.expect("inside region");
        self.scale = if region.scale <= 0.0 { 1.0 } else { region.scale };
        self.press_stopped_coast = stop.is_some();
        self.press_on_close = region
            .close_button
            .map(|cb| cb.frame.contains(input.location))
            .unwrap_or(false);
        self.phase = Phase::Pending;
        self.pending_since = input.time;
        self.origin = input.location;
        self.last = input.location;
        self.samples.clear();
        self.samples.push((input.time, input.location));
        self.wheel.reset();
        match stop {
            None => TouchResponse::swallow_empty(),
            Some(step) => TouchResponse::swallow(vec![TouchCommand::Scroll(step)]),
        }
    }

    fn drag(&mut self, input: TouchInput, options: TouchOptions) -> TouchResponse {
        match self.phase {
            Phase::Idle | Phase::Selecting => TouchResponse::pass(),
            Phase::Pending => {
                if self.origin.distance_to(input.location) < Self::SLOP * self.scale {
                    return TouchResponse::swallow_empty();
                }
                if input.time - self.pending_since >= Self::HOLD_DURATION || !options.allow_scroll {
                    self.phase = Phase::Selecting;
                    return TouchResponse::swallow(vec![
                        TouchCommand::ReplayPress,
                        TouchCommand::ReplayCurrent,
                    ]);
                }
                self.phase = Phase::Scrolling;
                let began =
                    self.wheel.step(0.0, 0.0, ScrollPhase::Began, MomentumPhase::None, self.origin);
                TouchResponse::swallow(vec![
                    TouchCommand::Scroll(began),
                    TouchCommand::Scroll(self.scroll_step_to(input)),
                ])
            }
            Phase::Scrolling => {
                TouchResponse::swallow(vec![TouchCommand::Scroll(self.scroll_step_to(input))])
            }
        }
    }

    fn release(&mut self, input: TouchInput, options: TouchOptions) -> TouchResponse {
        match self.phase {
            Phase::Idle => {
                self.phase = Phase::Idle;
                TouchResponse::pass()
            }
            Phase::Selecting => {
                self.phase = Phase::Idle;
                if options.notify_selection_ended
                    && self.origin.distance_to(input.location) >= Self::SLOP * self.scale
                {
                    TouchResponse {
                        pass_original: true,
                        commands: vec![TouchCommand::SelectionCompleted { at: input.location }],
                    }
                } else {
                    TouchResponse::pass()
                }
            }
            Phase::Pending => {
                self.phase = Phase::Idle;
                if input.time - self.pending_since >= Self::HOLD_DURATION && options.allow_right_click
                {
                    return TouchResponse::swallow(vec![TouchCommand::RightClick { at: self.origin }]);
                }
                if self.press_stopped_coast {
                    return TouchResponse::swallow_empty();
                }
                if self.press_on_close {
                    return TouchResponse::swallow(vec![TouchCommand::CloseWindow {
                        at: self.origin,
                    }]);
                }
                TouchResponse::swallow(vec![
                    TouchCommand::ReplayPress,
                    TouchCommand::ReplayCurrent,
                ])
            }
            Phase::Scrolling => {
                self.phase = Phase::Idle;
                let ended = self.wheel.step(
                    0.0,
                    0.0,
                    ScrollPhase::Ended,
                    MomentumPhase::None,
                    self.origin,
                );
                let (vx, vy) = self.fling_velocity(input.time);
                self.momentum = Momentum::try_create(vx, vy, self.scale);
                TouchResponse::swallow(vec![TouchCommand::Scroll(ended)])
            }
        }
    }

    fn scroll_step_to(&mut self, input: TouchInput) -> ScrollStep {
        let dx = input.location.x - self.last.x;
        let dy = input.location.y - self.last.y;
        self.last = input.location;
        self.samples.push((input.time, input.location));
        if self.samples.len() > Self::MAX_SAMPLES {
            let drop = self.samples.len() - Self::MAX_SAMPLES;
            self.samples.drain(0..drop);
        }
        self.wheel.step(dx, dy, ScrollPhase::Changed, MomentumPhase::None, self.origin)
    }

    fn stop_momentum(&mut self) -> Option<ScrollStep> {
        self.momentum?;
        self.momentum = None;
        Some(self.wheel.step(0.0, 0.0, ScrollPhase::None, MomentumPhase::End, self.origin))
    }

    fn fling_velocity(&self, time: f64) -> (f64, f64) {
        if self.samples.len() < 2 {
            return (0.0, 0.0);
        }
        let mut first_recent = None;
        for (i, (t, _)) in self.samples.iter().enumerate() {
            if time - t <= Self::FLING_WINDOW {
                first_recent = Some(i);
                break;
            }
        }
        let Some(first_recent) = first_recent else {
            return (0.0, 0.0);
        };
        if first_recent >= self.samples.len() - 1 {
            return (0.0, 0.0);
        }
        let mut a = self.samples[first_recent];
        let b = self.samples[self.samples.len() - 1];
        if b.0 <= a.0 {
            return (0.0, 0.0);
        }
        if b.0 - a.0 < Self::MIN_FLING_SPAN {
            a = self.samples[0];
            for (t, p) in self.samples.iter().rev() {
                if b.0 - t >= Self::MIN_FLING_SPAN {
                    a = (*t, *p);
                    break;
                }
            }
        }
        let dt = b.0 - a.0;
        if dt <= 0.0 {
            return (0.0, 0.0);
        }
        let mut vx = (b.1.x - a.1.x) / dt;
        let mut vy = (b.1.y - a.1.y) / dt;
        let speed = crate::geometry::hypot(vx, vy);
        let cap = Self::MAX_FLING_SPEED * self.scale;
        if speed > cap {
            vx = vx / speed * cap;
            vy = vy / speed * cap;
        }
        (vx, vy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone() -> TouchRegion {
        TouchRegion { frame: Rect::new(0.0, 0.0, 540.0, 950.0), scale: 1.0, close_button: None }
    }

    fn elsewhere() -> TouchRegion {
        TouchRegion {
            frame: Rect::new(2000.0, 0.0, 540.0, 950.0),
            scale: 1.0,
            close_button: None,
        }
    }

    fn input(kind: TouchInputKind, x: f64, y: f64, t: f64) -> TouchInput {
        TouchInput::new(kind, x, y, t)
    }

    #[test]
    fn outside_region_passes_through() {
        let mut t = TouchTranslator::default();
        let region = elsewhere();
        assert_eq!(
            t.handle(input(TouchInputKind::Press, 100.0, 300.0, 0.0), Some(&region)),
            TouchResponse::pass()
        );
        assert_eq!(
            t.handle(input(TouchInputKind::Drag, 160.0, 300.0, 0.1), Some(&region)),
            TouchResponse::pass()
        );
        assert_eq!(
            t.handle(input(TouchInputKind::Release, 160.0, 300.0, 0.2), Some(&region)),
            TouchResponse::pass()
        );
    }

    #[test]
    fn tap_holds_press_then_replays_on_release() {
        let mut t = TouchTranslator::default();
        let region = phone();
        assert!(!t.handle(input(TouchInputKind::Press, 100.0, 300.0, 0.0), Some(&region)).pass_original);
        assert!(
            t.handle(input(TouchInputKind::Drag, 104.0, 305.0, 0.05), Some(&region))
                .commands
                .is_empty()
        );
        let rel = t.handle(input(TouchInputKind::Release, 104.0, 305.0, 0.1), Some(&region));
        assert!(!rel.pass_original);
        assert_eq!(rel.commands.len(), 2);
        assert_eq!(rel.commands[0], TouchCommand::ReplayPress);
        assert_eq!(rel.commands[1], TouchCommand::ReplayCurrent);
    }

    #[test]
    fn close_button_tap_emits_close_window_and_verifies_z_order() {
        let win_rect = Rect::new(0.0, 22.0, 540.0, 900.0);
        let cb = CloseButton {
            frame: Rect::new(8.0, 30.0, 14.0, 14.0),
            window_handle: 7,
            window_frame: win_rect,
        };
        let region = TouchRegion { close_button: Some(cb), ..phone() };
        let mut t = TouchTranslator::default();

        assert!(
            t.handle(input(TouchInputKind::Press, 15.0, 37.0, 0.0), Some(&region))
                .commands
                .is_empty()
        );
        let rel = t.handle(input(TouchInputKind::Release, 16.0, 38.0, 0.1), Some(&region));
        assert_eq!(rel.commands.len(), 1);
        assert_eq!(
            rel.commands[0],
            TouchCommand::CloseWindow { at: Point::new(15.0, 37.0) }
        );

        let point = Point::new(15.0, 37.0);
        assert!(cb.is_frontmost(
            point,
            &[(7, win_rect), (3, Rect::new(-500.0, 0.0, 1500.0, 1500.0))]
        ));
        assert!(!cb.is_frontmost(
            point,
            &[(9, Rect::new(0.0, 22.0, 540.0, 892.0)), (7, win_rect)]
        ));
    }

    #[test]
    fn swipe_produces_scroll_events_and_fling_momentum() {
        let mut t = TouchTranslator::default();
        let region = phone();
        t.handle(input(TouchInputKind::Press, 100.0, 300.0, 0.0), Some(&region));
        let first = t.handle(input(TouchInputKind::Drag, 100.0, 340.0, 0.05), Some(&region));
        let steps: Vec<ScrollStep> = first
            .commands
            .iter()
            .filter_map(|c| match c {
                TouchCommand::Scroll(s) => Some(*s),
                _ => None,
            })
            .collect();
        assert_eq!(steps[0].phase, ScrollPhase::Began);
        assert_eq!(steps[1].phase, ScrollPhase::Changed);
        assert_eq!(steps[1].pixels_y, 40);

        t.handle(input(TouchInputKind::Drag, 100.0, 420.0, 0.09), Some(&region));
        let end = t.handle(input(TouchInputKind::Release, 100.0, 460.0, 0.12), Some(&region));
        assert!(end.commands.iter().any(|c| matches!(
            c,
            TouchCommand::Scroll(s) if s.phase == ScrollPhase::Ended
        )));
        assert!(t.is_coasting());

        let m1 = t.momentum_frame().expect("momentum frame");
        assert_eq!(m1.momentum, MomentumPhase::Begin);

        // 惯性中轻点：只停惯性，不回放点击
        let stop = t.handle(input(TouchInputKind::Press, 100.0, 300.0, 0.2), Some(&region));
        assert!(stop.commands.iter().any(|c| matches!(
            c,
            TouchCommand::Scroll(s) if s.momentum == MomentumPhase::End
        )));
        assert!(!t.is_coasting());
        assert!(
            t.handle(input(TouchInputKind::Release, 100.0, 300.0, 0.25), Some(&region))
                .commands
                .is_empty()
        );
    }

    #[test]
    fn hold_then_drag_selects_text_and_hold_then_release_right_clicks() {
        let mut t = TouchTranslator::default();
        let region = phone();
        t.handle(input(TouchInputKind::Press, 100.0, 300.0, 0.0), Some(&region));
        let drag = t.handle(input(TouchInputKind::Drag, 130.0, 300.0, 0.6), Some(&region));
        assert_eq!(drag.commands.len(), 2);
        assert_eq!(drag.commands[0], TouchCommand::ReplayPress);
        assert_eq!(drag.commands[1], TouchCommand::ReplayCurrent);
        assert_eq!(
            t.handle(input(TouchInputKind::Drag, 160.0, 300.0, 0.7), Some(&region)),
            TouchResponse::pass()
        );
        assert_eq!(
            t.handle(input(TouchInputKind::Release, 160.0, 300.0, 0.8), Some(&region)),
            TouchResponse::pass()
        );

        t.handle(input(TouchInputKind::Press, 100.0, 300.0, 1.0), Some(&region));
        let rc = t.handle(input(TouchInputKind::Release, 102.0, 301.0, 1.6), Some(&region));
        assert_eq!(rc.commands.len(), 1);
        assert_eq!(
            rc.commands[0],
            TouchCommand::RightClick { at: Point::new(100.0, 300.0) }
        );
    }

    #[test]
    fn drag_ignored_until_slop_exceeded() {
        let mut t = TouchTranslator::default();
        let region = phone();
        t.handle(input(TouchInputKind::Press, 100.0, 300.0, 0.0), Some(&region));
        let small = t.handle(input(TouchInputKind::Drag, 105.0, 303.0, 0.05), Some(&region));
        assert!(small.commands.is_empty());
        assert!(!small.pass_original);
    }

    #[test]
    fn selection_ended_notification_uses_release_point() {
        let mut t = TouchTranslator::default();
        let region = phone();
        let opts = TouchOptions { notify_selection_ended: true, ..Default::default() };
        t.handle_with(input(TouchInputKind::Press, 100.0, 300.0, 0.0), Some(&region), opts);
        t.handle_with(input(TouchInputKind::Drag, 130.0, 300.0, 0.6), Some(&region), opts);
        let rel = t.handle_with(input(TouchInputKind::Release, 200.0, 320.0, 0.8), Some(&region), opts);
        assert!(rel.pass_original);
        assert_eq!(
            rel.commands[0],
            TouchCommand::SelectionCompleted { at: Point::new(200.0, 320.0) }
        );
    }

    #[test]
    fn scroll_disabled_turns_swipe_into_selection() {
        let mut t = TouchTranslator::default();
        let region = phone();
        let opts = TouchOptions { allow_scroll: false, ..Default::default() };
        t.handle_with(input(TouchInputKind::Press, 100.0, 300.0, 0.0), Some(&region), opts);
        let drag = t.handle_with(input(TouchInputKind::Drag, 160.0, 300.0, 0.1), Some(&region), opts);
        assert_eq!(drag.commands[0], TouchCommand::ReplayPress);
        assert_eq!(drag.commands[1], TouchCommand::ReplayCurrent);
    }
}
