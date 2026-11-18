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
use snug_core::Point;

/// 气泡在屏幕上停留多久（C# `DispatcherTimer` 5.5 秒）。
const LINGER: Duration = Duration::from_millis(5_500);
/// 气泡尺寸（点）。C# 里最小 160×36 物理像素。
const BUBBLE_SIZE: egui::Vec2 = egui::vec2(104.0, 38.0);

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
        self.deadline = Some(Instant::now() + LINGER);
    }

    /// 收起（选区被取消、或者已经处理过一次点击）。
    pub fn hide(&mut self) {
        self.visible = false;
        self.deadline = None;
        self.handled = false;
        *self.shared.at.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 每帧调：处理超时、画气泡、返回"用户在哪个点按了复制"。
    pub fn poll(&mut self, ctx: &egui::Context) -> Option<Point> {
        if !self.visible {
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

        // 物理像素 → 点。气泡很小，用主视口的缩放比就够。
        let ppp = ctx.pixels_per_point().max(0.5) as f64;
        let width = BUBBLE_SIZE.x as f64 * ppp;
        let height = BUBBLE_SIZE.y as f64 * ppp;
        let x = at.x - width / 2.0;
        // 放在选区上方 18 物理像素处（C# `selectionPoint.Y - pxH - 18 * dpiRatio`）。
        let y = at.y - height - 18.0 * ppp;

        let shared = self.shared.clone();
        let builder = egui::ViewportBuilder::default()
            .with_title("Snug 复制")
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top()
            .with_inner_size(BUBBLE_SIZE)
            .with_position([(x / ppp) as f32, (y / ppp) as f32]);

        let mut outcome = None;
        ctx.show_viewport_deferred(
            egui::ViewportId::from_hash_of("snug-copy-bubble"),
            builder,
            move |ctx, _class| {
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::default()
                            .fill(egui::Color32::from_rgb(28, 30, 36))
                            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(90)))
                            .inner_margin(egui::Margin::symmetric(6, 4))
                            .corner_radius(8.0),
                    )
                    .show(ctx, |ui| {
                        ui.horizontal_centered(|ui| {
                            if ui.small_button("复制").clicked() {
                                shared.clicked.store(true, Ordering::Relaxed);
                            }
                            if ui.small_button("×").clicked() {
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
}
