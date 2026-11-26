use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use eframe::egui;
use snug_core::{Point, Rect};
use snug_win::display::{MonitorSnapshot, WinPhoneDisplays};
use snug_win::screenshot::{PixelImage, capture_projection};

const REFRESH: Duration = Duration::from_millis(80);
const MAX_FRAME_AGE: Duration = Duration::from_millis(450);
const MAX_CLICK_DELAY: Duration = Duration::from_millis(150);

struct Frame {
    monitor: Arc<MonitorSnapshot>,
    captured_at: Instant,
    image: Arc<egui::ColorImage>,
}

#[derive(Clone, Copy, PartialEq)]
struct CaptureRequest {
    display_id: u32,
    generation: u64,
}

struct Click {
    monitor: Arc<MonitorSnapshot>,
    point: Point,
    captured_at: Instant,
    queued_at: Instant,
}

impl Click {
    fn is_fresh(&self, now: Instant) -> bool {
        now.duration_since(self.captured_at) <= MAX_FRAME_AGE
            && now.duration_since(self.queued_at) <= MAX_CLICK_DELAY
    }
}

enum Work {
    Capture,
    Click(Click),
}

struct Command {
    request: CaptureRequest,
    work: Work,
    ctx: egui::Context,
    viewport: egui::ViewportId,
}

enum Reply {
    Frame(CaptureRequest, Option<Frame>),
    Click(CaptureRequest, bool),
}

#[derive(Default)]
struct CaptureCache {
    request: Option<CaptureRequest>,
    monitor: Option<(Arc<MonitorSnapshot>, Instant)>,
    image: Option<(PixelImage, Arc<egui::ColorImage>)>,
}

impl CaptureCache {
    fn capture(
        &mut self,
        request: CaptureRequest,
        lookup: impl Fn() -> Option<MonitorSnapshot>,
        capture: impl FnOnce(Rect) -> Option<PixelImage>,
    ) -> Option<Frame> {
        if self.request != Some(request) {
            *self = Self {
                request: Some(request),
                ..Self::default()
            };
        }
        let monitor = self
            .monitor
            .take()
            .filter(|(_, verified_at)| verified_at.elapsed() <= MAX_FRAME_AGE)
            .map(|(monitor, _)| monitor)
            .or_else(|| lookup().map(Arc::new))?;
        let captured_at = Instant::now();
        let pixels = capture(monitor.bounds);
        // 采完核对归属、模式和位置。确认过的快照也可作为下一帧的采集坐标。
        let current = lookup()?;
        let verified_at = Instant::now();
        if &current != monitor.as_ref() {
            self.monitor = Some((Arc::new(current), verified_at));
            return None;
        }
        self.monitor = Some((Arc::clone(&monitor), verified_at));
        if captured_at.elapsed() > MAX_FRAME_AGE {
            return None;
        }
        let image = self.color_image(pixels?);
        Some(Frame {
            monitor,
            captured_at,
            image,
        })
    }

    fn color_image(&mut self, pixels: PixelImage) -> Arc<egui::ColorImage> {
        if let Some((previous, image)) = &self.image
            && previous == &pixels
        {
            return Arc::clone(image);
        }
        let image = Arc::new(egui::ColorImage::from_rgba_premultiplied(
            [pixels.width as usize, pixels.height as usize],
            &pixels.rgba,
        ));
        self.image = Some((pixels, Arc::clone(&image)));
        image
    }
}

struct Worker {
    command: mpsc::SyncSender<Command>,
    reply: mpsc::Receiver<Reply>,
    generation: Arc<AtomicU64>,
    pending_frame: bool,
    pending_click: bool,
}

impl Worker {
    fn start(displays: Arc<WinPhoneDisplays>) -> std::io::Result<Self> {
        let (command, commands) = mpsc::sync_channel::<Command>(2);
        let (replies, reply) = mpsc::sync_channel(2);
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&generation);
        std::thread::Builder::new()
            .name("snug-projection".into())
            .spawn(move || {
                let mut cache = CaptureCache::default();
                while let Ok(Command {
                    request,
                    work,
                    ctx,
                    viewport,
                }) = commands.recv()
                {
                    let current =
                        || worker_generation.load(Ordering::Acquire) == request.generation;
                    let reply = match work {
                        Work::Capture => {
                            let frame = if current() {
                                cache.capture(
                                    request,
                                    || displays.projection_monitor(request.display_id),
                                    capture_projection,
                                )
                            } else {
                                None
                            };
                            Reply::Frame(request, frame)
                        }
                        Work::Click(click) => {
                            let ready = || current() && click.is_fresh(Instant::now());
                            let delivered = ready()
                                && displays.projection_monitor(request.display_id).as_ref()
                                    == Some(click.monitor.as_ref())
                                && ready()
                                && snug_win::touch::click_projected_display(
                                    click.monitor.bounds,
                                    click.point,
                                );
                            Reply::Click(request, delivered)
                        }
                    };
                    if replies.send(reply).is_err() {
                        break;
                    }
                    if current() {
                        ctx.request_repaint_of(viewport);
                    }
                }
            })?;
        Ok(Self {
            command,
            reply,
            generation,
            pending_frame: false,
            pending_click: false,
        })
    }

    fn send(&self, ctx: &egui::Context, display_id: u32, work: Work) -> bool {
        self.command
            .try_send(Command {
                request: CaptureRequest {
                    display_id,
                    generation: self.generation.load(Ordering::Relaxed),
                },
                work,
                ctx: ctx.clone(),
                viewport: ctx.viewport_id(),
            })
            .is_ok()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

pub(crate) struct Projection {
    worker: Option<Worker>,
    texture: Option<egui::TextureHandle>,
    frame: Option<Frame>,
    display_id: Option<u32>,
    next_capture: Instant,
    click_failed: bool,
}

impl Projection {
    pub(crate) fn new(displays: Arc<WinPhoneDisplays>) -> Self {
        let worker = Worker::start(displays)
            .map_err(|error| {
                crate::warn!("虚拟屏投影：无法启动采集线程：{error}");
            })
            .ok();
        Self {
            worker,
            texture: None,
            frame: None,
            display_id: None,
            next_capture: Instant::now(),
            click_failed: false,
        }
    }

    pub(crate) fn suspend(&mut self, ctx: &egui::Context) {
        self.refresh(ctx, None);
    }

    fn refresh(&mut self, ctx: &egui::Context, display_id: Option<u32>) {
        if display_id != self.display_id {
            self.display_id = display_id;
            if let Some(worker) = &self.worker {
                worker.generation.fetch_add(1, Ordering::AcqRel);
            }
            self.frame = None;
            self.texture = None;
            self.click_failed = false;
            self.next_capture = Instant::now();
        }
        let Some(worker) = &mut self.worker else {
            return;
        };
        let request = display_id.map(|display_id| CaptureRequest {
            display_id,
            generation: worker.generation.load(Ordering::Relaxed),
        });
        loop {
            match worker.reply.try_recv() {
                Ok(Reply::Frame(completed, frame)) => {
                    worker.pending_frame = false;
                    if request == Some(completed) {
                        if let Some(frame) = &frame
                            && self
                                .frame
                                .as_ref()
                                .is_none_or(|previous| !Arc::ptr_eq(&previous.image, &frame.image))
                        {
                            let image = Arc::clone(&frame.image);
                            let options = egui::TextureOptions::LINEAR;
                            if let Some(texture) = &mut self.texture {
                                texture.set(image, options);
                            } else {
                                self.texture =
                                    Some(ctx.load_texture("snug-virtual-screen", image, options));
                            }
                        }
                        self.frame = frame;
                    }
                }
                Ok(Reply::Click(completed, delivered)) => {
                    worker.pending_click = false;
                    if request == Some(completed) {
                        self.click_failed = !delivered;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.worker = None;
                    self.frame = None;
                    return;
                }
            }
        }
    }

    fn request_frame(&mut self, ctx: &egui::Context) {
        let (Some(display_id), Some(worker)) = (self.display_id, &mut self.worker) else {
            return;
        };
        let now = Instant::now();
        if !worker.pending_frame
            && now >= self.next_capture
            && worker.send(ctx, display_id, Work::Capture)
        {
            self.next_capture = now + REFRESH;
            worker.pending_frame = true;
        }
        let wait = if worker.pending_frame {
            REFRESH
        } else {
            self.next_capture
                .saturating_duration_since(now)
                .max(Duration::from_millis(1))
        };
        ctx.request_repaint_after(wait);
    }

    /// Returns true when the user navigates back to desktop settings.
    pub(crate) fn show(&mut self, ctx: &egui::Context, display_id: Option<u32>) -> bool {
        self.refresh(ctx, display_id);
        let mut back = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                back = ui.button("返回屏幕设置").clicked();
                ui.heading("虚拟屏投影");
            });
            ui.separator();
            if back {
                return;
            }
            let frame = self
                .frame
                .as_ref()
                .filter(|frame| frame.captured_at.elapsed() <= MAX_FRAME_AGE);
            let (Some(id), Some(texture), Some(frame)) = (display_id, &self.texture, frame) else {
                ui.weak(if self.worker.is_none() {
                    "无法启动画面采集。返回后可重试。"
                } else if display_id.is_none() {
                    "虚拟屏尚未在线。可在屏幕设置中选择「始终显示」，或连入远控。"
                } else {
                    "正在等待虚拟屏画面…"
                });
                return;
            };
            if self.click_failed {
                ui.weak("点击未送达。虚拟屏或输入状态可能已变化。");
            } else {
                ui.weak("单击画面即可在虚拟屏对应位置点击。");
            }
            let room = ui.available_rect_before_wrap();
            let monitor = &frame.monitor;
            let ratio = monitor.bounds.width as f32 / monitor.bounds.height as f32;
            let height = room.height().min(room.width() / ratio);
            if height <= 0.0 {
                return;
            }
            let rect =
                egui::Rect::from_center_size(room.center(), egui::vec2(height * ratio, height));
            let image = egui::Image::new((texture.id(), rect.size())).sense(egui::Sense::click());
            let response = ui
                .put(rect, image)
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked()
                && let Some(pos) = response.interact_pointer_pos()
                && let Some(point) = project_point(rect, monitor.bounds, pos)
                && let Some(worker) = &mut self.worker
                && !worker.pending_click
            {
                let click = Click {
                    monitor: Arc::clone(monitor),
                    point,
                    captured_at: frame.captured_at,
                    queued_at: Instant::now(),
                };
                worker.pending_click = worker.send(ctx, id, Work::Click(click));
                self.click_failed = !worker.pending_click;
            }
        });
        if !back {
            self.request_frame(ctx);
        }
        back
    }
}

fn project_point(view: egui::Rect, display: Rect, at: egui::Pos2) -> Option<Point> {
    if !view.contains(at) || at.x >= view.max.x || at.y >= view.max.y {
        return None;
    }
    let x = f64::from(at.x - view.min.x) / f64::from(view.width());
    let y = f64::from(at.y - view.min.y) / f64::from(view.height());
    Some(Point::new(
        display.x + (display.width * x).floor(),
        display.y + (display.height * y).floor(),
    ))
}

#[cfg(test)]
#[test]
fn projection_compares_frames_before_uploading_another_texture() {
    let mut cache = CaptureCache::default();
    let pixels = PixelImage::new(1, 1, vec![20, 40, 60, 255]).unwrap();
    let first = cache.color_image(pixels.clone());
    let same = cache.color_image(pixels);
    let changed = cache.color_image(PixelImage::new(1, 1, vec![21, 40, 60, 255]).unwrap());
    assert!(Arc::ptr_eq(&first, &same));
    assert!(!Arc::ptr_eq(&same, &changed));
    assert_eq!(changed.pixels, [egui::Color32::from_rgb(21, 40, 60)]);
}

#[cfg(test)]
fn projection_test_monitor(bounds: Rect) -> MonitorSnapshot {
    MonitorSnapshot {
        stable_id: 1,
        device_name: String::new(),
        friendly_name: String::new(),
        device_string: String::new(),
        virtual_display_id: Some(1),
        bounds,
        work_area: bounds,
        is_primary: false,
        mode: snug_core::windows::DisplayModeInfo {
            width: 800,
            height: 1200,
            refresh_rate: 60.,
            dpi_scale_percent: 100,
        },
        adapter_id: snug_win::display::AdapterLuid::new(1, 0),
        source_id: 1,
    }
}

#[cfg(test)]
#[test]
fn projection_revalidates_cached_coordinates_after_capture() {
    let mut cache = CaptureCache::default();
    let request = CaptureRequest {
        display_id: 1,
        generation: 1,
    };
    let original = Rect::new(-800., 0., 800., 1200.);
    let moved = Rect::new(1200., 0., 800., 1200.);
    let live_bounds = std::cell::Cell::new(original);
    let probes = std::cell::Cell::new(0);
    let lookup = || {
        probes.set(probes.get() + 1);
        Some(projection_test_monitor(live_bounds.get()))
    };
    let pixels = |_| PixelImage::new(1, 1, vec![20, 40, 60, 255]);

    assert!(cache.capture(request, lookup, pixels).is_some());
    assert_eq!(probes.get(), 2, "first capture verifies before and after");
    assert!(cache.capture(request, lookup, pixels).is_some());
    assert_eq!(
        probes.get(),
        3,
        "steady state reuses the previous verification"
    );
    assert!(
        cache
            .capture(request, lookup, |captured| {
                assert_eq!(captured, original);
                live_bounds.set(moved);
                pixels(captured)
            })
            .is_none(),
        "discard pixels when the capture coordinates have changed"
    );
    assert!(
        cache
            .capture(request, lookup, |captured| {
                assert_eq!(captured, moved);
                pixels(captured)
            })
            .is_some()
    );
}

#[cfg(test)]
#[test]
fn projection_click_expires_when_queued_or_frame_is_stale() {
    let now = Instant::now();
    let bounds = Rect::new(-800., 0., 800., 1200.);
    let mut click = Click {
        monitor: Arc::new(projection_test_monitor(bounds)),
        point: bounds.center(),
        captured_at: now - MAX_FRAME_AGE,
        queued_at: now - MAX_CLICK_DELAY,
    };
    let after_limit = now + Duration::from_millis(1);
    assert!(click.is_fresh(now));
    click.captured_at = now;
    assert!(!click.is_fresh(after_limit));
    click.captured_at = now - MAX_FRAME_AGE;
    click.queued_at = now;
    assert!(!click.is_fresh(after_limit));
}

#[cfg(test)]
#[test]
fn projection_click_maps_the_image_excluding_letterbox_and_outer_edges() {
    let image = egui::Rect::from_min_size(egui::pos2(50.0, 30.0), egui::vec2(300.0, 500.0));
    let display = Rect::new(-1200.0, -600.0, 1200.0, 2000.0);
    assert_eq!(
        project_point(image, display, egui::pos2(200.0, 280.0)),
        Some(Point::new(-600.0, 400.0))
    );
    assert_eq!(
        project_point(image, display, image.min),
        Some(Point::new(-1200.0, -600.0))
    );
    assert!(project_point(image, display, egui::pos2(49.0, 200.0)).is_none());
    assert!(project_point(image, display, image.max).is_none());
}
