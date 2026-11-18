//! 相机键：截图 → 预览 → 裁剪 → 剪贴板。
//!
//! 对应 C# 版 `ScreenshotOverlayWindow` + `CropWindow` + `SnugWin.Core.Screenshots` 的界面部分。
//! 状态机（什么时候过期、什么时候作废上一次截图）全在 `snug_core::screenshots` 里，
//! 这里只负责画：预览图、裁剪框、提示语和三个按钮。

use std::sync::Arc;

use eframe::egui;
use snug_core::screenshots::{ScreenshotNotice, ScreenshotOverlay, Screenshots};
use snug_core::windows::WindowInfo;
use snug_core::Rect;
use snug_win::WinScreenshot;

/// 裁剪时拖出的选区在屏幕上的位置；`None` 表示还没拖。
#[derive(Default)]
struct Drag {
    start: Option<egui::Pos2>,
    current: Option<egui::Pos2>,
}

pub struct Shots {
    inner: Screenshots,
    /// 当前预览图的纹理，跟着 `CapturedImage` 的 PNG 换（用 PNG 长度+首字节做粗判足够）。
    texture: Option<(usize, egui::TextureHandle)>,
    drag: Drag,
}

impl Shots {
    pub fn new(on_change: Arc<dyn Fn() + Send + Sync>) -> Self {
        let system: Arc<dyn snug_core::ScreenshotControl> = Arc::new(WinScreenshot::new());
        Self {
            inner: Screenshots::new(system, None, Some(Arc::new(move |_overlay| on_change()))),
            texture: None,
            drag: Drag::default(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.inner.overlay().is_some()
    }

    /// 相机键：抓这个窗口（C# `TakeAsync`）。
    pub fn take(&mut self, window: &WindowInfo) {
        crate::info!("截图：抓取 {}", window.label());
        self.drag = Drag::default();
        self.inner.take(window);
    }

    /// 每帧调一次，让"5 秒后自动收起"这件事有机会跑。
    pub fn poll(&mut self) {
        self.inner.expire();
    }

    /// 画覆盖层。它会盖在手机屏界面上（对应 C# 的 `ScreenshotOverlayWindow`）。
    pub fn show(&mut self, ctx: &egui::Context) {
        let Some(overlay) = self.inner.overlay().cloned() else { return };

        egui::Area::new(egui::Id::new("snug-screenshot-overlay"))
            .order(egui::Order::Foreground)
            .interactable(true)
            .show(ctx, |ui| {
                let screen = ctx.screen_rect();
                egui::Frame::default()
                    .fill(egui::Color32::from_black_alpha(210))
                    .show(ui, |ui| {
                        ui.set_min_size(screen.size());
                        ui.vertical_centered(|ui| match overlay {
                            ScreenshotOverlay::Preview(image) => {
                                ui.label(
                                    egui::RichText::new("已复制到剪贴板").color(egui::Color32::LIGHT_GREEN),
                                );
                                self.show_image(ctx, ui, &image, screen);
                                ui.horizontal(|ui| {
                                    if ui.button("裁剪").clicked() {
                                        self.inner.open_crop();
                                    }
                                    if ui.button("关闭").clicked() {
                                        self.inner.dismiss();
                                    }
                                });
                            }
                            ScreenshotOverlay::Cropping(image) => {
                                ui.label("拖动框选要裁剪的区域");
                                let image_rect = self.show_image(ctx, ui, &image, screen);
                                let response = ui.allocate_rect(
                                    image_rect,
                                    egui::Sense::click_and_drag(),
                                );
                                if response.drag_started() {
                                    self.drag.start = response.interact_pointer_pos();
                                    self.drag.current = self.drag.start;
                                }
                                if response.dragged() {
                                    self.drag.current = response.interact_pointer_pos();
                                }
                                if let (Some(start), Some(current)) = (self.drag.start, self.drag.current)
                                {
                                    let selection = egui::Rect::from_two_pos(start, current);
                                    ui.painter().rect_stroke(
                                        selection,
                                        0.0,
                                        egui::Stroke::new(1.5_f32, egui::Color32::LIGHT_BLUE),
                                        egui::StrokeKind::Middle,
                                    );
                                }
                                let finished = response.drag_stopped();
                                ui.horizontal(|ui| {
                                    if ui.button("取消").clicked() {
                                        self.inner.cancel_crop();
                                        self.drag = Drag::default();
                                    }
                                    if finished {
                                        if let (Some(start), Some(current), Some(unit)) = (
                                            self.drag.start,
                                            self.drag.current,
                                            unit_selection(self.drag.start, self.drag.current, image_rect),
                                        ) {
                                            let _ = (start, current);
                                            crate::info!("截图：裁剪到 {unit:?}");
                                            self.inner.finish_crop(unit);
                                            self.drag = Drag::default();
                                        }
                                    }
                                });
                            }
                            ScreenshotOverlay::Notice(notice) => {
                                ui.label(egui::RichText::new(notice_text(notice)).size(16.0));
                                if ui.button("关闭").clicked() {
                                    self.inner.dismiss();
                                }
                            }
                        });
                    });
            });
    }

    /// 把预览图画出来，返回它占的屏幕矩形（裁剪要用这个矩形换算单位坐标）。
    fn show_image(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        image: &snug_core::CapturedImage,
        screen: egui::Rect,
    ) -> egui::Rect {
        let key = image.png.len();
        let texture = match &self.texture {
            Some((len, texture)) if *len == key => texture.clone(),
            _ => {
                let loaded = snug_win::screenshot::decode_png(&image.png).and_then(|pixels| {
                    let size = [pixels.width.max(1) as usize, pixels.height.max(1) as usize];
                    let color = pixels
                        .rgba
                        .chunks_exact(4)
                        .map(|px| egui::Color32::from_rgba_unmultiplied(px[0], px[1], px[2], px[3]))
                        .collect();
                    Some(ctx.load_texture(
                        "snug-shot-preview",
                        egui::ColorImage {
                            size,
                            pixels: color,
                            source_size: egui::Vec2::new(size[0] as f32, size[1] as f32),
                        },
                        egui::TextureOptions::LINEAR,
                    ))
                });
                match loaded {
                    Some(texture) => {
                        self.texture = Some((key, texture.clone()));
                        texture
                    }
                    None => {
                        crate::warn!("截图：PNG 解码失败，无法预览");
                        return egui::Rect::NOTHING;
                    }
                }
            }
        };

        // 等比缩放到屏幕里（留出按钮的位置）。
        let available = egui::vec2(screen.width() - 40.0, screen.height() - 120.0);
        let size = egui::vec2(image.pixel_width as f32, image.pixel_height as f32);
        let scale = (available.x / size.x).min(available.y / size.y).min(1.0);
        let shown = size * scale;
        let (rect, _) = ui.allocate_exact_size(shown, egui::Sense::hover());
        ui.painter().image(
            texture.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        rect
    }
}

/// 屏幕上的选区 → 图片内的单位矩形（0..1），按显示矩形换算。
fn unit_selection(
    start: Option<egui::Pos2>,
    current: Option<egui::Pos2>,
    image_rect: egui::Rect,
) -> Option<Rect> {
    let (start, current) = (start?, current?);
    if image_rect.width() <= 0.0 || image_rect.height() <= 0.0 {
        return None;
    }
    let selection = egui::Rect::from_two_pos(start, current);
    let left = ((selection.left() - image_rect.left()) / image_rect.width()).clamp(0.0, 1.0);
    let right = ((selection.right() - image_rect.left()) / image_rect.width()).clamp(0.0, 1.0);
    let top = ((selection.top() - image_rect.top()) / image_rect.height()).clamp(0.0, 1.0);
    let bottom = ((selection.bottom() - image_rect.top()) / image_rect.height()).clamp(0.0, 1.0);
    if right - left < 0.01 || bottom - top < 0.01 {
        return None;
    }
    Some(Rect::new(left as f64, top as f64, (right - left) as f64, (bottom - top) as f64))
}

/// 提示文案（逐字对齐 C# `ScreenshotOverlayWindow`）。
fn notice_text(notice: ScreenshotNotice) -> &'static str {
    match notice {
        ScreenshotNotice::Cropped => "已裁剪并复制到剪贴板",
        ScreenshotNotice::NeedsPermission => "没有截屏权限：请检查系统的屏幕捕获权限",
        ScreenshotNotice::WindowGone => "窗口已经不在了",
        ScreenshotNotice::Failed => "截图失败",
        ScreenshotNotice::CopyFailed => "截图成功，但写入剪贴板失败",
        ScreenshotNotice::CropFailed => "裁剪失败",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_selection_normalizes_to_the_image_rect() {
        let image = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(200.0, 400.0));
        // 拖出正中间的四分之一区域。
        let got = unit_selection(
            Some(egui::pos2(150.0, 150.0)),
            Some(egui::pos2(250.0, 250.0)),
            image,
        )
        .expect("应当给出选区");
        assert!((got.x - 0.25).abs() < 1e-6);
        assert!((got.y - 0.25).abs() < 1e-6);
        assert!((got.width - 0.5).abs() < 1e-6);
        assert!((got.height - 0.25).abs() < 1e-6);
    }

    #[test]
    fn tiny_selection_is_rejected() {
        let image = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 100.0));
        assert!(unit_selection(
            Some(egui::pos2(10.0, 10.0)),
            Some(egui::pos2(10.5, 60.0)),
            image
        )
        .is_none());
    }

    #[test]
    fn selection_is_clamped_inside_the_image() {
        let image = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 100.0));
        let got = unit_selection(
            Some(egui::pos2(-50.0, -50.0)),
            Some(egui::pos2(50.0, 50.0)),
            image,
        )
        .expect("夹回图内应当有效");
        assert!((got.x - 0.0).abs() < 1e-6);
        assert!((got.width - 0.5).abs() < 1e-6);
    }

    #[test]
    fn notices_have_distinct_texts() {
        let texts: Vec<&str> = [
            ScreenshotNotice::Cropped,
            ScreenshotNotice::NeedsPermission,
            ScreenshotNotice::WindowGone,
            ScreenshotNotice::Failed,
            ScreenshotNotice::CopyFailed,
            ScreenshotNotice::CropFailed,
        ]
        .into_iter()
        .map(notice_text)
        .collect();
        let unique: std::collections::HashSet<&str> = texts.iter().copied().collect();
        assert_eq!(unique.len(), texts.len(), "提示文案不能有重复");
    }
}
