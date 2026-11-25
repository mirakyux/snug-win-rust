use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use eframe::egui;
use snug_core::Rect;
use snug_win::display::{PhysicalMonitors, WinDisplay, WinPhoneDisplays};

const DESKTOP_REFRESH: Duration = Duration::from_secs(1);
const PHONE_REFRESH: Duration = Duration::from_millis(150);

#[derive(Clone, Copy)]
struct Request {
    desktop: bool,
    phone: Option<u32>,
}

struct Sample {
    request: Request,
    desktop: Option<Arc<PhysicalMonitors>>,
    phone: Option<(Rect, Rect)>,
}

pub(crate) struct DisplayLayout {
    requests: mpsc::Sender<Request>,
    samples: mpsc::Receiver<Sample>,
    pending: bool,
    desktop: Option<Arc<PhysicalMonitors>>,
    phone_id: Option<u32>,
    phone: Option<(Rect, Rect)>,
    next_desktop: Instant,
    next_phone: Instant,
}

impl DisplayLayout {
    pub(crate) fn start(
        displays: Arc<WinPhoneDisplays>,
        ctx: egui::Context,
    ) -> std::io::Result<Self> {
        let (requests, request_rx) = mpsc::channel::<Request>();
        let (sample_tx, samples) = mpsc::channel();
        std::thread::Builder::new()
            .name("snug-display-layout".into())
            .spawn(move || {
                let display = WinDisplay::new();
                while let Ok(request) = request_rx.recv() {
                    let desktop = request
                        .desktop
                        .then(|| display.verified_physical_monitors())
                        .flatten()
                        .map(Arc::new);
                    let phone = request.phone.and_then(|id| displays.phone_layout_of(id));
                    if sample_tx
                        .send(Sample {
                            request,
                            desktop,
                            phone,
                        })
                        .is_err()
                    {
                        break;
                    }
                    ctx.request_repaint_of(egui::ViewportId::ROOT);
                }
            })?;
        let now = Instant::now();
        Ok(Self {
            requests,
            samples,
            pending: false,
            desktop: None,
            phone_id: None,
            phone: None,
            next_desktop: now,
            next_phone: now,
        })
    }

    pub(crate) fn refresh(
        &mut self,
        desktop_open: bool,
        phone_id: Option<u32>,
    ) -> Option<(Rect, Rect)> {
        let now = Instant::now();
        if !desktop_open {
            self.desktop = None;
            self.next_desktop = now;
        }
        if self.phone_id != phone_id {
            self.phone_id = phone_id;
            self.phone = None;
            self.next_phone = now;
        }
        if let Ok(sample) = self.samples.try_recv() {
            self.pending = false;
            if sample.request.desktop && desktop_open {
                self.desktop = sample.desktop;
            }
            if sample.request.phone.is_some() && sample.request.phone == phone_id {
                self.phone = sample.phone;
            }
        }
        if self.pending {
            return self.phone;
        }
        let request = Request {
            desktop: desktop_open && now >= self.next_desktop,
            phone: phone_id.filter(|_| now >= self.next_phone),
        };
        if (request.desktop || request.phone.is_some()) && self.requests.send(request).is_ok() {
            self.pending = true;
            if request.desktop {
                self.next_desktop = now + DESKTOP_REFRESH;
            }
            if request.phone.is_some() {
                self.next_phone = now + PHONE_REFRESH;
            }
        }
        self.phone
    }

    pub(crate) fn desktop_monitors(&mut self) -> Option<Arc<PhysicalMonitors>> {
        if !self.desktop.as_ref()?.geometry_is_current() {
            self.desktop = None;
            self.next_desktop = Instant::now();
            return None;
        }
        self.desktop.clone()
    }
}

#[cfg(test)]
#[test]
fn phone_layout_ignores_an_in_flight_result_for_the_previous_screen() {
    let (requests, request_rx) = mpsc::channel();
    let (sample_tx, samples) = mpsc::channel();
    let now = Instant::now();
    let mut layout = DisplayLayout {
        requests,
        samples,
        pending: false,
        desktop: None,
        phone_id: None,
        phone: None,
        next_desktop: now,
        next_phone: now,
    };
    assert_eq!(layout.refresh(false, Some(1)), None);
    let first = request_rx.try_recv().unwrap();
    assert_eq!(first.phone, Some(1));
    assert_eq!(layout.refresh(false, Some(2)), None);
    assert!(request_rx.try_recv().is_err());

    let bounds = (
        Rect::new(-100.0, 0.0, 100.0, 200.0),
        Rect::new(-100.0, 190.0, 100.0, 10.0),
    );
    sample_tx
        .send(Sample {
            request: first,
            desktop: None,
            phone: Some(bounds),
        })
        .unwrap();
    assert_eq!(layout.refresh(false, Some(2)), None);
    let second = request_rx.try_recv().unwrap();
    assert_eq!(second.phone, Some(2));
    sample_tx
        .send(Sample {
            request: second,
            desktop: None,
            phone: Some(bounds),
        })
        .unwrap();
    assert_eq!(layout.refresh(false, Some(2)), Some(bounds));
    assert_eq!(layout.refresh(false, None), None);
}
