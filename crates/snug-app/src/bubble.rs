//! 「选后复制」的气泡。
//!
//! 对应 C# 版 `CopySelectionBubbleWindow`：选区一结束，在选区上方弹一个小气泡，
//! 5.5 秒后自己消失；点"复制"才真的按 `Ctrl+C`（终端是 `Ctrl+Shift+C`）。
//!
//! 为什么是独立窗口而不是画在我们的主窗口里：选区发生的时候，我们的主窗口已经
//! 收成了手机屏底部那条指示条（`HomeBarWindow`），气泡必须出现在**选区旁边**，
//! 那可能在屏幕任何位置。所以这里用 egui 的 deferred viewport 开第二个置顶小窗，
//! 和 C# 那边"另一个 WPF 窗口"是一样的结构。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use snug_core::{Point, Rect};
use snug_win::display::DisplayApi;

/// 气泡在屏幕上停留多久（C# `DispatcherTimer` 5.5 秒）。
const LINGER: Duration = Duration::from_millis(5_500);
/// 气泡尺寸（点）：120 点复制区 + 64 点取消区、8 点间隔及左右各 16 点边距。
const BUBBLE_SIZE: egui::Vec2 = egui::vec2(224.0, 64.0);
const COPY_SIZE: egui::Vec2 = egui::vec2(120.0, 48.0);
const CANCEL_SIZE: egui::Vec2 = egui::vec2(64.0, 48.0);

/// 先放在选区上方；靠近上边缘时改放下方，再夹进当前显示器工作区。
fn bubble_origin(at: Point, ppp: f64, work_area: Option<Rect>) -> Point {
    let width = f64::from(BUBBLE_SIZE.x) * ppp;
    let height = f64::from(BUBBLE_SIZE.y) * ppp;
    let gap = 18.0 * ppp;
    let mut point = Point::new(at.x - width / 2.0, at.y - height - gap);
    if let Some(work) = work_area {
        if point.y < work.min_y() {
            point.y = at.y + gap;
        }
        point.x = point.x.clamp(work.min_x(), work.min_x().max(work.max_x() - width));
        point.y = point.y.clamp(work.min_y(), work.min_y().max(work.max_y() - height));
    }
    point
}

/// 气泡与主窗口共享的状态。deferred viewport 的回调要求 `'static`，
/// 所以点没点、要不要收起来都放这里。
struct Shared {
    /// 选区结束的位置（物理像素）。
    at: std::sync::Mutex<Option<Point>>,
    /// 用户点了"复制"。
    clicked: AtomicBool,
    /// 用户点了"取消"或者被外部收起。
    dismissed: AtomicBool,
}

pub struct CopyBubble {
    shared: Arc<Shared>,
    /// 当前是否应当显示。
    visible: bool,
    /// 到点自动收起（C# 5.5 秒）。
    deadline: Option<Instant>,
    /// 这次气泡是不是已经处理过点击了。
    handled: bool,
    /// 同一次弹出只查一次显示器边界，不在每帧枚举 CCD/PnP。
    origin: Option<Point>,
}

impl Default for CopyBubble {
    fn default() -> Self {
        Self::new()
    }
}

impl CopyBubble {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                at: std::sync::Mutex::new(None),
                clicked: AtomicBool::new(false),
                dismissed: AtomicBool::new(false),
            }),
            visible: false,
            deadline: None,
            handled: false,
            origin: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 选区结束：记下位置，开始存在 5.5 秒。
    pub fn show_at(&mut self, at: Point) {
        *self.shared.at.lock().unwrap_or_else(|e| e.into_inner()) = Some(at);
        self.shared.clicked.store(false, Ordering::Relaxed);
        self.shared.dismissed.store(false, Ordering::Relaxed);
        self.visible = true;
        self.handled = false;
        self.origin = None;
        self.deadline = Some(Instant::now() + LINGER);
    }

    /// 收起（选区被取消、或者已经处理过一次点击）。
    pub fn hide(&mut self) {
        self.visible = false;
        self.deadline = None;
        self.handled = false;
        self.origin = None;
        *self.shared.at.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 每帧调：处理超时、画气泡、返回"用户在哪个点按了复制"。
    pub fn poll(&mut self, ctx: &egui::Context) -> Option<Point> {
        if !self.is_visible() {
            return None;
        }
        if let Some(deadline) = self.deadline
            && Instant::now() >= deadline
        {
            crate::info!("复制气泡：超时收起");
            self.hide();
            return None;
        }
        let at = (*self.shared.at.lock().unwrap_or_else(|e| e.into_inner()))?;

        // 物理像素 → 点；与主视口一致，气泡本身仍由独立 viewport 承担。
        let ppp = ctx.pixels_per_point().max(0.5) as f64;
        let origin = *self.origin.get_or_insert_with(|| {
            let monitors = snug_win::WinDisplay::new().active_monitors();
            let work_area = monitors
                .iter()
                .find(|monitor| monitor.bounds.contains(at))
                .or_else(|| monitors.iter().find(|monitor| monitor.is_primary))
                .map(|monitor| monitor.work_area);
            bubble_origin(at, ppp, work_area)
        });

        let shared = self.shared.clone();
        let builder = egui::ViewportBuilder::default()
            .with_title("Snug 复制")
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top()
            .with_inner_size(BUBBLE_SIZE)
            .with_position([(origin.x / ppp) as f32, (origin.y / ppp) as f32]);

        let mut outcome = None;
        ctx.show_viewport_deferred(
            egui::ViewportId::from_hash_of("snug-copy-bubble"),
            builder,
            move |ctx, _class| {
                let p = crate::theme::palette(ctx.style().visuals.dark_mode);
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::default()
                            .fill(p.card)
                            .stroke(egui::Stroke::new(1.0_f32, p.text_weak))
                            .inner_margin(egui::Margin::symmetric(16, 8))
                            .corner_radius(12.0),
                    )
                    .show(ctx, |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if ui.add_sized(COPY_SIZE, egui::Button::new("复制")).clicked() {
                                shared.clicked.store(true, Ordering::Relaxed);
                            }
                            if ui.add_sized(CANCEL_SIZE, egui::Button::new("取消")).clicked() {
                                shared.dismissed.store(true, Ordering::Relaxed);
                            }
                        });
                    });
                ctx.request_repaint_after(Duration::from_millis(100));
            },
        );

        if self.shared.clicked.swap(false, Ordering::Relaxed) {
            self.hide();
            outcome = Some(at);
        } else if self.shared.dismissed.swap(false, Ordering::Relaxed) {
            crate::info!("复制气泡：用户取消");
            self.hide();
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bubble_starts_hidden() {
        let bubble = CopyBubble::new();
        assert!(!bubble.is_visible());
    }

    #[test]
    fn show_at_then_hide_leaves_no_pending_state() {
        let mut bubble = CopyBubble::new();
        bubble.show_at(Point::new(100.0, 200.0));
        assert!(bubble.is_visible());
        bubble.hide();
        assert!(!bubble.is_visible());
        assert!(bubble.deadline.is_none());
    }

    #[test]
    fn deadline_is_five_and_a_half_seconds() {
        // C# 是 5.5 秒；这里只钉住这个数，别在改 UI 时被顺手调掉。
        assert_eq!(LINGER.as_millis(), 5500);
    }

    #[test]
    fn bubble_flips_below_selection_and_stays_inside_a_negative_origin_monitor() {
        let work = Rect::new(-1920.0, 0.0, 1920.0, 1040.0);
        let point = bubble_origin(Point::new(-1910.0, 8.0), 1.0, Some(work));
        assert_eq!(point, Point::new(-1920.0, 26.0));
        let right_bottom = bubble_origin(Point::new(-2.0, 1030.0), 1.0, Some(work));
        assert!(right_bottom.x + f64::from(BUBBLE_SIZE.x) <= work.max_x());
        assert!(right_bottom.y + f64::from(BUBBLE_SIZE.y) <= work.max_y());
    }

    #[test]
    fn bubble_fits_scaled_monitor_without_overlapping_action_targets() {
        let work = Rect::new(1920.0, 0.0, 1080.0, 1900.0);
        let point = bubble_origin(Point::new(2990.0, 5.0), 2.0, Some(work));
        assert!(point.x >= work.min_x());
        assert!(point.x + f64::from(BUBBLE_SIZE.x) * 2.0 <= work.max_x());
        assert_eq!(point.y, 41.0);
        assert!(COPY_SIZE.y >= 48.0 && CANCEL_SIZE.y >= 48.0 && CANCEL_SIZE.x >= 48.0);
        assert_eq!(COPY_SIZE.x + 8.0 + CANCEL_SIZE.x + 32.0, BUBBLE_SIZE.x);
    }
}
