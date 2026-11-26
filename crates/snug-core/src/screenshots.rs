//! 截图覆盖层状态机，移植自 C# 类 `SnugWin.Core.PhoneScreen.Screenshots`
//! （`Screenshots.cs`）：截图 → 写剪贴板 → 预览 / 通知 / 裁剪，以及 5 秒预览、3 秒通知的过期。
//!
//! 依赖的 `Timeline` 与 PhoneDesk 共用（[`crate::desk::services::Timeline`]）；
//! `IScreenshotControl` 的异步方法在这里同样是阻塞方法（见 `desk::services` 的说明）。
//! C# 里的 `Log.*` 诊断输出没有移植，行为（状态、返回值、事件）保持一致。

use std::sync::Arc;

use crate::crop;
use crate::desk::services::Timeline;
use crate::geometry::{Rect, Size};
use crate::windows::{WindowInfo, WindowKey};

/// 预览缩略图停留时长（C# `Screenshots.PreviewDuration`）。
pub const PREVIEW_DURATION: f64 = 5.0;
/// 提示文字停留时长（C# `Screenshots.NoticeDuration`）。
pub const NOTICE_DURATION: f64 = 3.0;

/// 一张截下来的图（对应 C# `CapturedImage`）。
///
/// 相等性按 C# 的自定义 `Equals` 实现：宽高相同且 PNG 字节相同。
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedImage {
    pub png: Vec<u8>,
    pub pixel_width: i32,
    pub pixel_height: i32,
}

impl CapturedImage {
    pub fn new(png: impl Into<Vec<u8>>, pixel_width: i32, pixel_height: i32) -> Self {
        Self { png: png.into(), pixel_width, pixel_height }
    }

    pub fn size(&self) -> Size {
        Size::new(self.pixel_width as f64, self.pixel_height as f64)
    }
}

/// 截图的失败原因（对应 C# `CaptureFailure`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFailure {
    NoPermission,
    WindowGone,
    Unavailable,
}

/// 截图失败（对应 C# `CaptureException`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureError {
    pub failure: CaptureFailure,
}

impl CaptureError {
    pub fn new(failure: CaptureFailure) -> Self {
        Self { failure }
    }
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.failure)
    }
}

impl std::error::Error for CaptureError {}

/// 截图的提示类型（对应 C# `ScreenshotNotice`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenshotNotice {
    Cropped,
    NeedsPermission,
    WindowGone,
    Failed,
    CopyFailed,
    CropFailed,
}

/// 手机屏上盖着的东西（对应 C# `ScreenshotOverlay`）。
#[derive(Debug, Clone, PartialEq)]
pub enum ScreenshotOverlay {
    Preview(CapturedImage),
    Cropping(CapturedImage),
    Notice(ScreenshotNotice),
}

impl ScreenshotOverlay {
    pub fn is_cropping(&self) -> bool {
        matches!(self, Self::Cropping(_))
    }
}

/// 平台截图能力（对应 C# `IScreenshotControl`）。
pub trait ScreenshotControl: Send + Sync {
    /// 是否有录屏权限。
    fn trusted(&self) -> bool;
    fn capture(&self, window: WindowKey) -> Result<CapturedImage, CaptureError>;
    /// 按像素矩形裁剪；裁不出来返回 `None`。
    fn crop(&self, image: &CapturedImage, rect: Rect) -> Option<CapturedImage>;
    /// 写进剪贴板。
    fn copy(&self, image: &CapturedImage) -> bool;
}

/// 截图控制器（对应 C# `Screenshots`）。
pub struct Screenshots {
    system: Arc<dyn ScreenshotControl>,
    timeline: Arc<dyn Timeline>,
    expires_at: Option<f64>,
    serial: i64,
    overlay: Option<ScreenshotOverlay>,
    /// 对应 C# `Changed` 事件。
    changed: Option<Arc<dyn Fn(Option<&ScreenshotOverlay>) + Send + Sync>>,
}

impl Screenshots {
    pub fn new(
        system: Arc<dyn ScreenshotControl>,
        timeline: Option<Arc<dyn Timeline>>,
        changed: Option<Arc<dyn Fn(Option<&ScreenshotOverlay>) + Send + Sync>>,
    ) -> Self {
        Self {
            system,
            timeline: timeline.unwrap_or_else(|| Arc::new(crate::desk::SystemTimeline::new())),
            expires_at: None,
            serial: 0,
            overlay: None,
            changed,
        }
    }

    pub fn trusted(&self) -> bool {
        self.system.trusted()
    }

    pub fn overlay(&self) -> Option<&ScreenshotOverlay> {
        self.overlay.as_ref()
    }

    /// 对应 C# `TakeAsync`：截图并写剪贴板，然后显示预览或提示。
    pub fn take(&mut self, window: &WindowInfo) {
        if self.overlay.as_ref().is_some_and(ScreenshotOverlay::is_cropping) {
            return;
        }
        if !self.system.trusted() {
            self.set(Some(ScreenshotOverlay::Notice(ScreenshotNotice::NeedsPermission)));
            return;
        }

        self.serial += 1;
        let mine = self.serial;

        let captured = self.system.capture(window.key);
        // 截完时如果已经不需要了（又被点了一次 / 已进入裁剪），丢掉这次结果。
        if mine != self.serial
            || self.overlay.as_ref().is_some_and(ScreenshotOverlay::is_cropping)
        {
            return;
        }

        match captured {
            Ok(image) => {
                if !self.system.copy(&image) {
                    self.set(Some(ScreenshotOverlay::Notice(ScreenshotNotice::CopyFailed)));
                    return;
                }
                self.set(Some(ScreenshotOverlay::Preview(image)));
            }
            Err(error) => {
                let notice = match error.failure {
                    CaptureFailure::NoPermission => ScreenshotNotice::NeedsPermission,
                    CaptureFailure::WindowGone => ScreenshotNotice::WindowGone,
                    CaptureFailure::Unavailable => ScreenshotNotice::Failed,
                };
                self.set(Some(ScreenshotOverlay::Notice(notice)));
            }
        }
    }

    /// 对应 C# `OpenCrop`：从预览进入裁剪。
    pub fn open_crop(&mut self) {
        if let Some(ScreenshotOverlay::Preview(image)) = &self.overlay {
            let image = image.clone();
            self.set(Some(ScreenshotOverlay::Cropping(image)));
        }
    }

    /// 对应 C# `CancelCrop`：取消裁剪，回到预览。
    pub fn cancel_crop(&mut self) {
        if let Some(ScreenshotOverlay::Cropping(image)) = &self.overlay {
            let image = image.clone();
            self.set(Some(ScreenshotOverlay::Preview(image)));
        }
    }

    /// 对应 C# `FinishCrop`：按选区（单位矩形）裁剪并写剪贴板。
    pub fn finish_crop(&mut self, selection: Rect) {
        let Some(ScreenshotOverlay::Cropping(image)) = &self.overlay else {
            return;
        };
        let image = image.clone();
        let rect = crop::pixel_rect(selection, image.size());
        let Some(cropped) = self.system.crop(&image, rect) else {
            self.set(Some(ScreenshotOverlay::Notice(ScreenshotNotice::CropFailed)));
            return;
        };
        if !self.system.copy(&cropped) {
            self.set(Some(ScreenshotOverlay::Notice(ScreenshotNotice::CopyFailed)));
            return;
        }
        self.set(Some(ScreenshotOverlay::Notice(ScreenshotNotice::Cropped)));
    }

    /// 对应 C# `Expire`：到点就收起覆盖层（由界面每帧调用）。
    pub fn expire(&mut self) {
        if let Some(expires_at) = self.expires_at
            && self.timeline.now() >= expires_at
        {
            self.set(None);
        }
    }

    /// 对应 C# `Dismiss`：用户手动收起，并让正在进行的截图作废。
    pub fn dismiss(&mut self) {
        self.serial += 1;
        self.set(None);
    }

    /// 对应 C# `Set`：先按类型定过期时间（同一内容再设置也会续期），变了才发 `Changed`。
    fn set(&mut self, overlay: Option<ScreenshotOverlay>) {
        let duration = match overlay {
            Some(ScreenshotOverlay::Preview(_)) => Some(PREVIEW_DURATION),
            Some(ScreenshotOverlay::Notice(_)) => Some(NOTICE_DURATION),
            _ => None,
        };
        self.expires_at = duration.map(|seconds| self.timeline.now() + seconds);
        if overlay == self.overlay {
            return;
        }
        self.overlay = overlay;
        if let Some(changed) = self.changed.as_ref() {
            changed(self.overlay.as_ref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desk::test_support::FakeTimeline;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct FakeScreenshotControl {
        trusted: AtomicBool,
        image: Mutex<CapturedImage>,
        failure: Mutex<Option<CaptureFailure>>,
        copy_result: AtomicBool,
        last_copied: Mutex<Option<CapturedImage>>,
        last_crop_rect: Mutex<Option<Rect>>,
        capture_calls: AtomicUsize,
    }

    impl FakeScreenshotControl {
        fn new() -> Self {
            Self {
                trusted: AtomicBool::new(true),
                image: Mutex::new(CapturedImage::new([1u8, 2, 3, 4], 1080, 1860)),
                failure: Mutex::new(None),
                copy_result: AtomicBool::new(true),
                last_copied: Mutex::new(None),
                last_crop_rect: Mutex::new(None),
                capture_calls: AtomicUsize::new(0),
            }
        }

        fn last_copied(&self) -> Option<CapturedImage> {
            self.last_copied.lock().expect("copied").clone()
        }

        fn last_crop_rect(&self) -> Option<Rect> {
            *self.last_crop_rect.lock().expect("crop rect")
        }
    }

    impl ScreenshotControl for FakeScreenshotControl {
        fn trusted(&self) -> bool {
            self.trusted.load(Ordering::SeqCst)
        }

        fn capture(&self, _window: WindowKey) -> Result<CapturedImage, CaptureError> {
            self.capture_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(failure) = *self.failure.lock().expect("failure") {
                return Err(CaptureError::new(failure));
            }
            Ok(self.image.lock().expect("image").clone())
        }

        fn crop(&self, _image: &CapturedImage, rect: Rect) -> Option<CapturedImage> {
            *self.last_crop_rect.lock().expect("crop rect") = Some(rect);
            if rect.width < 1.0 || rect.height < 1.0 {
                return None;
            }
            Some(CapturedImage::new([9u8, 8, 7], rect.width as i32, rect.height as i32))
        }

        fn copy(&self, image: &CapturedImage) -> bool {
            if !self.copy_result.load(Ordering::SeqCst) {
                return false;
            }
            *self.last_copied.lock().expect("copied") = Some(image.clone());
            true
        }
    }

    fn window() -> WindowInfo {
        WindowInfo {
            key: WindowKey::new(10, 1),
            app_name: "App".into(),
            app_id: Some("app.exe".into()),
            title: "Doc".into(),
            minimized: false,
        }
    }

    fn setup() -> (Arc<FakeScreenshotControl>, Arc<FakeTimeline>, Screenshots) {
        let control = Arc::new(FakeScreenshotControl::new());
        let timeline = Arc::new(FakeTimeline::default());
        let shots = Screenshots::new(
            Arc::clone(&control) as Arc<dyn ScreenshotControl>,
            Some(Arc::clone(&timeline) as Arc<dyn Timeline>),
            None,
        );
        (control, timeline, shots)
    }

    /// 转写自 C# `ScreenshotsTests.Screenshots_TakeCropAndExpireLifecycle`。
    #[test]
    fn take_crop_and_expire_lifecycle() {
        let (control, timeline, mut shots) = setup();
        shots.take(&window());
        assert!(matches!(shots.overlay(), Some(ScreenshotOverlay::Preview(_))));
        assert_eq!(control.last_copied(), Some(control.image.lock().expect("image").clone()));

        shots.open_crop();
        assert!(matches!(shots.overlay(), Some(ScreenshotOverlay::Cropping(_))));

        // 裁剪中不该过期。
        timeline.advance(100.0);
        shots.expire();
        assert!(matches!(shots.overlay(), Some(ScreenshotOverlay::Cropping(_))));

        shots.finish_crop(Rect::new(0.1, 0.2, 0.5, 0.5));
        match shots.overlay() {
            Some(ScreenshotOverlay::Notice(ScreenshotNotice::Cropped)) => {}
            other => panic!("应是裁剪成功提示，实际 {other:?}"),
        }
        assert_eq!(control.last_crop_rect(), Some(Rect::new(108.0, 372.0, 540.0, 930.0)));

        // 通知 3 秒后收起。
        timeline.advance(3.1);
        shots.expire();
        assert!(shots.overlay().is_none());
    }

    #[test]
    fn preview_expires_after_five_seconds() {
        let (_control, timeline, mut shots) = setup();
        shots.take(&window());
        timeline.advance(4.9);
        shots.expire();
        assert!(shots.overlay().is_some(), "5 秒以内还在");
        timeline.advance(0.2);
        shots.expire();
        assert!(shots.overlay().is_none());
    }

    #[test]
    fn untrusted_system_only_shows_permission_notice() {
        let (control, _timeline, mut shots) = setup();
        control.trusted.store(false, Ordering::SeqCst);
        shots.take(&window());
        assert_eq!(
            shots.overlay(),
            Some(&ScreenshotOverlay::Notice(ScreenshotNotice::NeedsPermission))
        );
        assert_eq!(control.capture_calls.load(Ordering::SeqCst), 0, "没权限就别截");
    }

    #[test]
    fn capture_failures_map_to_notices() {
        let (control, _timeline, mut shots) = setup();
        for (failure, notice) in [
            (CaptureFailure::NoPermission, ScreenshotNotice::NeedsPermission),
            (CaptureFailure::WindowGone, ScreenshotNotice::WindowGone),
            (CaptureFailure::Unavailable, ScreenshotNotice::Failed),
        ] {
            *control.failure.lock().expect("failure") = Some(failure);
            shots.take(&window());
            assert_eq!(shots.overlay(), Some(&ScreenshotOverlay::Notice(notice)));
        }
    }

    #[test]
    fn clipboard_failure_shows_copy_failed() {
        let (control, _timeline, mut shots) = setup();
        control.copy_result.store(false, Ordering::SeqCst);
        shots.take(&window());
        assert_eq!(shots.overlay(), Some(&ScreenshotOverlay::Notice(ScreenshotNotice::CopyFailed)));

        // 裁剪时写剪贴板失败也是同一个提示。
        control.copy_result.store(true, Ordering::SeqCst);
        shots.take(&window());
        shots.open_crop();
        control.copy_result.store(false, Ordering::SeqCst);
        shots.finish_crop(Rect::new(0.1, 0.1, 0.5, 0.5));
        assert_eq!(shots.overlay(), Some(&ScreenshotOverlay::Notice(ScreenshotNotice::CopyFailed)));
    }

    #[test]
    fn crop_failure_shows_crop_failed() {
        let (_control, _timeline, mut shots) = setup();
        shots.take(&window());
        shots.open_crop();
        shots.finish_crop(Rect::new(0.0, 0.0, 0.0, 0.0));
        assert_eq!(shots.overlay(), Some(&ScreenshotOverlay::Notice(ScreenshotNotice::CropFailed)));
    }

    #[test]
    fn cancel_crop_returns_to_preview_and_dismiss_clears() {
        let (_control, _timeline, mut shots) = setup();
        shots.take(&window());
        shots.open_crop();
        shots.cancel_crop();
        assert!(matches!(shots.overlay(), Some(ScreenshotOverlay::Preview(_))));

        shots.dismiss();
        assert!(shots.overlay().is_none());
    }

    #[test]
    fn take_while_cropping_is_ignored() {
        let (control, _timeline, mut shots) = setup();
        shots.take(&window());
        shots.open_crop();
        let before = control.capture_calls.load(Ordering::SeqCst);
        shots.take(&window());
        assert_eq!(control.capture_calls.load(Ordering::SeqCst), before, "裁剪中不再截图");
    }

    #[test]
    fn changed_callback_fires_only_when_overlay_changes() {
        let control = Arc::new(FakeScreenshotControl::new());
        let timeline = Arc::new(FakeTimeline::default());
        let calls = Arc::new(Mutex::new(Vec::<Option<ScreenshotOverlay>>::new()));
        let sink = Arc::clone(&calls);
        let mut shots = Screenshots::new(
            Arc::clone(&control) as Arc<dyn ScreenshotControl>,
            Some(Arc::clone(&timeline) as Arc<dyn Timeline>),
            Some(Arc::new(move |overlay: Option<&ScreenshotOverlay>| {
                sink.lock().expect("calls").push(overlay.cloned());
            })),
        );

        shots.take(&window());
        shots.dismiss();
        // 同样的内容再设一次（续期）不算变化。
        shots.dismiss();
        let calls = calls.lock().expect("calls");
        assert_eq!(calls.len(), 2, "Preview 一次、None 一次");
        assert!(matches!(calls[0], Some(ScreenshotOverlay::Preview(_))));
        assert_eq!(calls[1], None);
    }
}
