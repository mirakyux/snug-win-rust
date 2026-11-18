//! 自绘窗口的摆放，移植自 C# App 层 `SnugWin.App.Views.WindowPlacementHelper`。
//!
//! C# 这个类是给 WPF 窗口用的：先把窗口的扩展样式调好（工具窗口 / 不抢焦点 / 点击穿透），
//! 再按物理像素摆到指定位置。Rust 版的界面不是 WPF，但**这套规则照样要**——
//! 手机屏上的覆盖层、Home Bar、截图遮罩都是"无边框、置顶、可能不抢焦点"的窗口。
//!
//! 这里只做两件事：
//! - **纯计算**：扩展样式怎么变、物理矩形怎么取整、`SetWindowPos` 的参数怎么拼、
//!   这一帧到底要不要真的调 `SetWindowPos`（C# 为了不每帧重复调用做了比较）。
//! - **应用**：[`apply`] 把上面的结果写进真实的窗口。
//!
//! 手动验证（自动测试只测纯计算）：
//! 1. 打开 Home Bar 覆盖层，鼠标点上去不应把焦点从下面的窗口抢走；
//! 2. 改成点击穿透后，点在覆盖层上应该落到下面的窗口；
//! 3. 同一位置每帧调用 `apply`，任务管理器里本进程的 CPU 占用应保持很低（没重复 SetWindowPos）。

use snug_core::Rect;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, GetWindowRect, IsWindow, IsWindowVisible, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
};

/// 点击穿透（`WS_EX_TRANSPARENT`）。
pub const WS_EX_TRANSPARENT: i32 = 0x0000_0020;
/// 不抢焦点（`WS_EX_NOACTIVATE`）。
pub const WS_EX_NOACTIVATE: i32 = 0x0800_0000;
/// 工具窗口：不进任务栏、不参与 Alt+Tab（`WS_EX_TOOLWINDOW`）。
pub const WS_EX_TOOLWINDOW: i32 = 0x0000_0080;

/// `SetWindowPos` 的 `HWND_TOPMOST`。
pub const HWND_TOPMOST: isize = -1;
/// `SetWindowPos` 的 `HWND_NOTOPMOST`。
pub const HWND_NOTOPMOST: isize = -2;

/// 一次摆放请求（对应 C# `PlacePhysical` 的参数）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacementRequest {
    /// 目标矩形（物理像素，直接对应屏幕坐标）。
    pub bounds: Rect,
    /// 置顶。
    pub topmost: bool,
    /// 不抢焦点（覆盖层和 Home Bar 用）。
    pub no_activate: bool,
    /// 点击穿透（鼠标事件落到下面的窗口）。
    pub passthrough: bool,
    /// 允许抢到前台时真的抢一次。
    pub focus_if_activated: bool,
}

impl PlacementRequest {
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            topmost: true,
            no_activate: true,
            passthrough: false,
            focus_if_activated: false,
        }
    }
}

/// 按请求算新的扩展样式（C# `PlacePhysical` 里那段位运算）。
///
/// 工具窗口位永远加上；`WS_EX_NOACTIVATE` 与 `WS_EX_TRANSPARENT` 按请求开或关。
pub fn desired_ex_style(current: i32, no_activate: bool, passthrough: bool) -> i32 {
    let mut desired = current | WS_EX_TOOLWINDOW;
    desired = if no_activate {
        desired | WS_EX_NOACTIVATE
    } else {
        desired & !WS_EX_NOACTIVATE
    };
    if passthrough {
        desired | WS_EX_TRANSPARENT
    } else {
        desired & !WS_EX_TRANSPARENT
    }
}

/// 目标矩形 → `SetWindowPos` 的整数参数；宽高至少 1（C# `Math.Max(1, ...)`）。
pub fn placement_rect(bounds: Rect) -> (i32, i32, i32, i32) {
    (
        bounds.x.round() as i32,
        bounds.y.round() as i32,
        i32::max(1, bounds.width.round() as i32),
        i32::max(1, bounds.height.round() as i32),
    )
}

/// `SetWindowPos` 的 flags（C# 只按"要不要抢焦点"决定加不加 `SWP_NOACTIVATE`）。
pub fn set_window_pos_flags(no_activate: bool) -> u32 {
    SWP_SHOWWINDOW.0 | if no_activate { SWP_NOACTIVATE.0 } else { 0 }
}

/// 置顶时插在 `HWND_TOPMOST`，否则 `HWND_NOTOPMOST`。
pub fn insert_after(topmost: bool) -> isize {
    if topmost { HWND_TOPMOST } else { HWND_NOTOPMOST }
}

/// 这一帧要不要真的调 `SetWindowPos`（C# 为了避免每帧重复调用做的比较）。
///
/// 已经可见、样式没变、也没要求抢焦点、位置又正好一样 → 不用调。
pub fn needs_reposition(
    was_visible: bool,
    style_changed: bool,
    focus_if_activated: bool,
    current: Option<(i32, i32, i32, i32)>,
    target: (i32, i32, i32, i32),
) -> bool {
    if !was_visible || style_changed || focus_if_activated {
        return true;
    }
    current != Some(target)
}

/// 把摆放请求应用到真实窗口。返回 `false` 表示句柄已经失效。
pub fn apply(hwnd: u64, request: &PlacementRequest) -> bool {
    if hwnd == 0 {
        return false;
    }
    let window = HWND(hwnd as usize as *mut core::ffi::c_void);
    unsafe {
        if !IsWindow(Some(window)).as_bool() {
            return false;
        }

        let current_ex = GetWindowLongPtrW(window, GWL_EXSTYLE) as i32;
        let desired = desired_ex_style(current_ex, request.no_activate, request.passthrough);
        let style_changed = desired != current_ex;
        if style_changed {
            SetWindowLongPtrW(window, GWL_EXSTYLE, desired as isize);
        }

        let target = placement_rect(request.bounds);
        let was_visible = IsWindowVisible(window).as_bool();
        let current_rect = current_window_rect(window);

        if needs_reposition(
            was_visible,
            style_changed,
            request.focus_if_activated,
            current_rect,
            target,
        ) {
            let insert = HWND(insert_after(request.topmost) as usize as *mut core::ffi::c_void);
            let flags = windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS(
                set_window_pos_flags(request.no_activate),
            );
            let _ = SetWindowPos(
                window,
                Some(insert),
                target.0,
                target.1,
                target.2,
                target.3,
                flags,
            );
        }

        if !request.no_activate && request.focus_if_activated {
            let _ = SetForegroundWindow(window);
        }
    }
    true
}

fn current_window_rect(window: HWND) -> Option<(i32, i32, i32, i32)> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(window, &mut rect) }.ok()?;
    Some((rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ex_style_always_marks_tool_window() {
        let desired = desired_ex_style(0, false, false);
        assert_eq!(desired, WS_EX_TOOLWINDOW);
        assert_eq!(desired & WS_EX_NOACTIVATE, 0);
        assert_eq!(desired & WS_EX_TRANSPARENT, 0);
    }

    #[test]
    fn ex_style_toggles_noactivate_and_transparent() {
        assert_eq!(
            desired_ex_style(0, true, false),
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
        );
        assert_eq!(
            desired_ex_style(0, false, true),
            WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT
        );
        // 关的时候要把旧位清掉，而不是留着。
        let with_both = desired_ex_style(0, true, true);
        assert_eq!(desired_ex_style(with_both, false, false), WS_EX_TOOLWINDOW);
        // 别的位不能动（比如 WS_EX_APPWINDOW）。
        let other = desired_ex_style(0x0004_0000, true, true);
        assert_ne!(other & 0x0004_0000, 0);
    }

    #[test]
    fn placement_rect_rounds_and_clamps() {
        assert_eq!(
            placement_rect(Rect::new(10.4, 20.6, 100.5, 200.4)),
            (10, 21, 101, 200)
        );
        // 0 尺寸必须变成 1，否则 SetWindowPos 会认为是"隐藏"。
        assert_eq!(placement_rect(Rect::new(0.0, 0.0, 0.0, 0.0)), (0, 0, 1, 1));
        assert_eq!(
            placement_rect(Rect::new(-100.0, -50.0, 540.0, 960.0)),
            (-100, -50, 540, 960)
        );
    }

    #[test]
    fn flags_and_z_order_follow_the_request() {
        assert_eq!(
            set_window_pos_flags(true),
            SWP_SHOWWINDOW.0 | SWP_NOACTIVATE.0
        );
        assert_eq!(set_window_pos_flags(false), SWP_SHOWWINDOW.0);
        assert_eq!(insert_after(true), HWND_TOPMOST);
        assert_eq!(insert_after(false), HWND_NOTOPMOST);
    }

    #[test]
    fn reposition_is_skipped_only_when_nothing_changed() {
        let target = (100, 200, 540, 960);
        // 已可见、样式没变、位置也对 → 跳过。
        assert!(!needs_reposition(true, false, false, Some(target), target));
        // 位置不对 → 要动。
        assert!(needs_reposition(true, false, false, Some((0, 0, 1, 1)), target));
        // 看不见 → 要动。
        assert!(needs_reposition(false, false, false, Some(target), target));
        // 样式变了 → 要动。
        assert!(needs_reposition(true, true, false, Some(target), target));
        // 要求抢焦点 → 要动。
        assert!(needs_reposition(true, false, true, Some(target), target));
        // 拿不到当前位置 → 要动。
        assert!(needs_reposition(true, false, false, None, target));
    }

    #[test]
    fn default_request_is_a_topmost_overlay_that_does_not_steal_focus() {
        let request = PlacementRequest::new(Rect::new(0.0, 0.0, 540.0, 960.0));
        assert!(request.topmost);
        assert!(request.no_activate);
        assert!(!request.passthrough);
        assert!(!request.focus_if_activated);
        assert_eq!(placement_rect(request.bounds), (0, 0, 540, 960));
    }

    #[test]
    fn apply_rejects_a_dead_handle() {
        assert!(!apply(0, &PlacementRequest::new(Rect::ZERO)));
    }
}
