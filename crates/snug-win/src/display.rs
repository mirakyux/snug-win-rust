//! 显示配置与虚拟手机屏，移植自 C# 版
//! `SnugWin.Windows.Display.DisplayConfigService` / `PhoneDisplayService`。
//!
//! 分三层：
//! - **纯逻辑**（本文件里不碰系统的自由函数）：DPI 缩放档位换算、显示器稳定 ID、
//!   窗口可放区域 / Home Bar 切分、主屏挑选、光标救援、候选屏识别。都有单测。
//! - **平台接口**：[`DisplayApi`] / [`VirtualDisplays`] 两个 trait，把 Win32 与
//!   虚拟屏管道挡住。测试用假实现，真机用 [`WinDisplay`] / [`VddClient`]。
//! - **[`WinPhoneDisplays`]**：实现 `snug_core::desk::PhoneDisplays`，给手机屏状态机用。
//!
//! 与 C# 的差别只在"异步"的表达：C# 的 `async/await` 在这里是阻塞调用 + 固定等待，
//! 由 `snug-core` 的串行队列（`SerialLane`）负责不卡界面线程，语义与 C# 一致。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use snug_core::desk::{PhoneDisplays, PhoneScreenErrorKind, PhoneScreenException};
use snug_core::screen_fit::DESIGN_WIDTH;
use snug_core::{DisplayModeInfo, OtherDisplay, Point, Rect, ScreenFit};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_DEVICE_INFO_TYPE, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
    QueryDisplayConfig, SetDisplayConfig, QDC_ONLY_ACTIVE_PATHS, SDC_APPLY, SDC_TOPOLOGY_EXTEND,
};
use windows::Win32::Foundation::{LPARAM, LUID, POINT, POINTL, RECT};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplayMonitors, EnumDisplaySettingsExW,
    GetMonitorInfoW, HDC, HMONITOR, CDS_NORESET, CDS_TYPE, CDS_UPDATEREGISTRY, DEVMODEW,
    DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICEW, DISP_CHANGE_SUCCESSFUL, ENUM_CURRENT_SETTINGS,
    ENUM_DISPLAY_SETTINGS_FLAGS, ENUM_DISPLAY_SETTINGS_MODE, MONITORINFO, MONITORINFOEXW,
    DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    REG_VALUE_TYPE,
};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};
use windows::core::{w, BOOL, PCWSTR, PWSTR};

use crate::vdd::{is_driver_pipe_available, VddClient, VddError, VddMode, VddMonitor};
use crate::wide::{contains_ignore_case, from_wide, to_wide};

/// 驱动里这块虚拟屏的名字（C# `PhoneScreenSpec.Name`）。
pub const MONITOR_NAME: &str = "Snug 手机屏";
/// 建屏时依次尝试的连接器编号（C# `PhoneScreenSpec.CandidateIds`）。
pub const CANDIDATE_IDS: [u32; 4] = [0, 1, 2, 3];
/// 手机屏的目标缩放（C# `PhoneDisplayService.TargetDpiPercent`）。
pub const TARGET_DPI_PERCENT: i32 = 200;

/// Windows「设置 → 缩放」的档位表（C# `DisplayConfigService.DpiScaleTable`）。
pub const DPI_SCALE_TABLE: [i32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

/// 显卡适配器 + 输出编号（C# `AdapterLuid`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AdapterLuid {
    pub low_part: u32,
    pub high_part: i32,
}

impl AdapterLuid {
    pub const fn new(low_part: u32, high_part: i32) -> Self {
        Self { low_part, high_part }
    }

    pub fn is_zero(self) -> bool {
        self.low_part == 0 && self.high_part == 0
    }
}

/// 一块在线显示器的快照（C# `MonitorSnapshot`）。
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorSnapshot {
    /// 会话内稳定的编号（显卡 LUID + 输出编号的哈希，退化为设备名哈希）。
    pub stable_id: u32,
    /// GDI 设备名，如 `\\.\DISPLAY2`。
    pub device_name: String,
    /// 显示器友好名，取不到时等于设备名。
    pub friendly_name: String,
    /// 适配器描述（用来区分是不是虚拟屏）。
    pub device_string: String,
    pub bounds: Rect,
    pub work_area: Rect,
    pub is_primary: bool,
    pub mode: DisplayModeInfo,
    pub adapter_id: AdapterLuid,
    pub source_id: u32,
}

// ---------------------------------------------------------------------------
// 纯逻辑
// ---------------------------------------------------------------------------

/// C# `GetDpiScalePercent` 的反算：把 CCD 报回的相对档位换成百分比。
///
/// `min_rel` 是「推荐档位相对最小档」的偏移（负数），`cur_rel` 是当前档位相对推荐档位。
pub fn dpi_percent_from_rel(min_rel: i32, cur_rel: i32) -> i32 {
    let last = DPI_SCALE_TABLE.len() as i32 - 1;
    let recommended = (-min_rel).clamp(0, last);
    let index = (recommended + cur_rel).clamp(0, last);
    DPI_SCALE_TABLE[index as usize]
}

/// C# `SetDpiScalePercent` 里的档位换算：目标百分比 → 要下发给驱动的 `scaleRel`。
///
/// 百分比不在档位表里返回 `None`（对应 C# 返回 false）。
pub fn dpi_rel_for_percent(target_percent: i32, min_rel: i32, max_rel: i32) -> Option<i32> {
    let last = DPI_SCALE_TABLE.len() as i32 - 1;
    let recommended = (-min_rel).clamp(0, last);
    let target_index = DPI_SCALE_TABLE.iter().position(|&p| p == target_percent)? as i32;
    Some((target_index - recommended).clamp(min_rel, max_rel))
}

/// 显示器稳定 ID。
///
/// C# 用 `HashCode.Combine(LUID, sourceId)`（进程内随机化）或设备名哈希；
/// 这里改成确定性的 FNV-1a，让同一个编号跨进程也不变（界面持久化更可靠）。
pub fn stable_id(device_name: &str, adapter_id: AdapterLuid, source_id: u32) -> u32 {
    if !adapter_id.is_zero() {
        let mut hash = FNV_OFFSET_BASIS;
        hash = fnv_write(hash, adapter_id.low_part);
        hash = fnv_write(hash, adapter_id.high_part as u32);
        fnv_write(hash, source_id)
    } else {
        // C# 用 OrdinalIgnoreCase 的名字哈希；这里按小写名字算。
        let mut hash = FNV_OFFSET_BASIS;
        for byte in device_name.to_lowercase().bytes() {
            hash = fnv_write(hash, byte as u32);
        }
        hash
    }
}

const FNV_OFFSET_BASIS: u32 = 0x811C_9DC5;
const FNV_PRIME: u32 = 0x0100_0193;

fn fnv_write(mut hash: u32, value: u32) -> u32 {
    for byte in value.to_le_bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// 屏幕上能放窗口的区域：工作区去掉底部 Home Bar（C# `PhoneDisplayService.WindowAreaOf`）。
pub fn window_area(monitor: &MonitorSnapshot, logical_width: i32) -> Rect {
    let scale = f64::max(1.0, monitor.mode.width as f64 / logical_width.max(1) as f64);
    let home_bar_px = (ScreenFit::home_bar_height(logical_width.max(1) as f64) * scale).round();
    let work = monitor.work_area;
    Rect::new(work.x, work.y, work.width, f64::max(100.0, work.height - home_bar_px))
}

/// Home Bar 区域：工作区里窗口区域剩下的那一条（C# `HomeBarAreaOf`）。
pub fn home_bar_area(monitor: &MonitorSnapshot, area: Rect) -> Rect {
    let work = monitor.work_area;
    let bar_height = f64::max(28.0, work.max_y() - area.max_y());
    Rect::new(work.x, area.max_y(), work.width, bar_height)
}

/// 主屏工作区：优先非手机屏的主显示器，否则第一块非手机屏。
pub fn main_area(monitors: &[MonitorSnapshot], phone_names: &HashSet<String>) -> Option<Rect> {
    let is_phone = |m: &MonitorSnapshot| phone_names.contains(&normalized(&m.device_name));
    let main = monitors
        .iter()
        .find(|m| m.is_primary && !is_phone(m))
        .or_else(|| monitors.iter().find(|m| !is_phone(m)))?;
    Some(main.work_area)
}

/// 其它显示器（给用户在手机屏之外挑一块用）：剔除手机屏与虚拟屏。
pub fn other_displays(monitors: &[MonitorSnapshot], phone_names: &HashSet<String>) -> Vec<OtherDisplay> {
    monitors
        .iter()
        .filter(|m| !phone_names.contains(&normalized(&m.device_name)))
        .filter(|m| !contains_ignore_case(&m.device_string, "Virtual"))
        .filter(|m| !contains_ignore_case(&m.friendly_name, "VirtuDisplay"))
        .map(|m| OtherDisplay {
            display_id: m.stable_id,
            device_name: m.device_name.clone(),
            friendly_name: m.friendly_name.clone(),
            mode: m.mode,
            asleep: false,
        })
        .collect()
}

/// 由当前分辨率和档位表反推逻辑宽度（C# `WidthOf`）。
///
/// 命中某个物理像素档位就用它，否则按 200% DPI 折半（.5 远离零）。
pub fn width_for_mode(mode_width: i32, fit: Option<&ScreenFit>) -> i32 {
    if let Some(fit) = fit {
        for &width in fit.widths() {
            if fit.pixel_size(width).0 == mode_width {
                return width;
            }
        }
    }
    (mode_width as f64 / 2.0).round() as i32
}

/// 撤屏后光标可能留在已经消失的屏幕上：算出需要挪到的位置（`None` = 不用动）。
pub fn cursor_rescue_point(cursor: Point, monitors: &[MonitorSnapshot]) -> Option<Point> {
    if monitors.is_empty() {
        return None;
    }
    if monitors.iter().any(|m| m.bounds.contains(cursor)) {
        return None;
    }
    let target = monitors
        .iter()
        .find(|m| m.is_primary)
        .unwrap_or(&monitors[0]);
    Some(target.bounds.center())
}

/// 从在线列表里认出"刚建出来的那块屏"（C# `WaitForOnlineAsync` 的挑选顺序）。
///
/// 1. 新出现、且像虚拟屏（适配器名/友好名带 Virtual）或已具备基准分辨率；
/// 2. 否则只要是新出现的就先认。
pub fn pick_new_monitor(
    existing: &HashSet<String>,
    monitors: &[MonitorSnapshot],
    base_width: i32,
    base_height: i32,
    has_mode: impl Fn(&str, i32, i32) -> bool,
) -> Option<MonitorSnapshot> {
    let fresh: Vec<&MonitorSnapshot> = monitors
        .iter()
        .filter(|m| !existing.contains(&normalized(&m.device_name)))
        .collect();
    let virtual_like = fresh.iter().find(|m| {
        contains_ignore_case(&m.device_string, "Virtual")
            || contains_ignore_case(&m.friendly_name, "Virtual")
            || has_mode(&m.device_name, base_width, base_height)
    });
    virtual_like.or_else(|| fresh.first()).map(|m| (*m).clone())
}

/// 建屏时下发给驱动的模式表（每个档位 60Hz，物理像素）。
pub fn build_modes(fit: &ScreenFit) -> Vec<VddMode> {
    fit.widths()
        .iter()
        .map(|&width| {
            let (pixel_width, pixel_height) = fit.pixel_size(width);
            VddMode {
                width: pixel_width.max(0) as u32,
                height: pixel_height.max(0) as u32,
                refresh_rates: vec![60],
            }
        })
        .collect()
}

fn normalized(name: &str) -> String {
    name.to_lowercase()
}

// ---------------------------------------------------------------------------
// 平台接口
// ---------------------------------------------------------------------------

/// 显示子系统（CCD / GDI）的能力。真机实现是 [`WinDisplay`]。
pub trait DisplayApi: Send {
    /// 当前在线的所有显示器。
    fn active_monitors(&self) -> Vec<MonitorSnapshot>;
    /// 该设备是否支持这个分辨率（C# `HasDisplayMode`）。
    fn has_display_mode(&self, device_name: &str, width: i32, height: i32) -> bool;
    /// 改分辨率（C# `SetResolution`）。
    fn set_resolution(&self, device_name: &str, width: i32, height: i32, refresh_rate: f64) -> bool;
    /// 改 DPI 缩放（C# `SetDpiScalePercent`）。
    fn set_dpi_scale(&self, adapter_id: AdapterLuid, source_id: u32, percent: i32) -> bool;
    /// 把"已经到了但没启用"的虚拟屏扩展成桌面的一块（C# `TryAttachVirtualDisplay`）。
    fn try_attach_virtual_display(&self, width: i32, height: i32, x: i32, y: i32) -> Option<String>;
    fn cursor_position(&self) -> Option<Point>;
    fn move_cursor(&self, point: Point);
    /// 虚拟屏驱动的管道在不在。
    fn driver_pipe_available(&self) -> bool;
    /// 重启虚拟屏设备（C# `TryRestartVirtualDisplayDeviceAsync`，会跑 `pnputil`）。
    fn restart_virtual_display_device(&self) -> bool;
}

/// 虚拟屏驱动管道。真机实现是 [`VddClient`]。
pub trait VirtualDisplays: Send {
    fn notify(&mut self, monitors: &[VddMonitor]) -> Result<(), VddError>;
    fn replace_legacy(&mut self, remove_ids: &[u32], add: &[VddMonitor]) -> Result<(), VddError>;
    fn remove(&mut self, ids: &[u32]) -> Result<(), VddError>;
    fn remove_all(&mut self) -> Result<(), VddError>;
    fn is_legacy(&self) -> bool;
}

impl VirtualDisplays for VddClient {
    fn notify(&mut self, monitors: &[VddMonitor]) -> Result<(), VddError> {
        VddClient::notify(self, monitors)
    }

    fn replace_legacy(&mut self, remove_ids: &[u32], add: &[VddMonitor]) -> Result<(), VddError> {
        VddClient::replace_legacy(self, remove_ids, add)
    }

    fn remove(&mut self, ids: &[u32]) -> Result<(), VddError> {
        VddClient::remove(self, ids)
    }

    fn remove_all(&mut self) -> Result<(), VddError> {
        VddClient::remove_all(self)
    }

    fn is_legacy(&self) -> bool {
        VddClient::is_legacy(self)
    }
}

/// 轮询节奏，对应 C# 里的几个常量与 `Task.Delay` 值。测试里改成 0 就瞬间返回。
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// 建屏后等系统上线的上限（C# `OnlineTimeout`，8s）。
    pub online: Duration,
    /// 撤屏后等系统下线的上限（C# `OfflineTimeout`，10s）。
    pub offline: Duration,
    /// 换比例后等新模式出现（C# 里是 4s）。
    pub reshape: Duration,
    pub online_poll: Duration,
    /// 尝试扩展桌面之后的固定等待（C# 250ms）。
    pub attach_settle: Duration,
    pub offline_poll: Duration,
    pub reshape_poll: Duration,
    /// 改分辨率之后的固定等待（C# 150ms）。
    pub resolution_settle: Duration,
    /// 启动清理之后等驱动反映（C# 300ms）。
    pub cleanup_settle: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            online: Duration::from_secs(8),
            offline: Duration::from_secs(10),
            reshape: Duration::from_secs(4),
            online_poll: Duration::from_millis(150),
            attach_settle: Duration::from_millis(250),
            offline_poll: Duration::from_millis(150),
            reshape_poll: Duration::from_millis(100),
            resolution_settle: Duration::from_millis(150),
            cleanup_settle: Duration::from_millis(300),
        }
    }
}

fn sleep_for(duration: Duration) {
    if !duration.is_zero() {
        std::thread::sleep(duration);
    }
}

// ---------------------------------------------------------------------------
// 手机屏（可注入假实现的逻辑本体）
// ---------------------------------------------------------------------------

/// 手机屏的映射状态：逻辑宽度档位与 GDI 设备名。
///
/// 这把锁**只**保护这两个 map，锁内不做任何系统调用；建/改/撤那些会睡几秒的
/// 慢操作由调用方另用一把「操作锁」串行，查询不会被它们挡住。
#[derive(Debug, Default)]
struct ScreenMaps {
    fits: HashMap<u32, ScreenFit>,
    device_names: HashMap<u32, String>,
}

impl ScreenMaps {
    fn phone_names(&self) -> HashSet<String> {
        self.device_names.values().map(|name| normalized(name)).collect()
    }
}

/// 锁中毒（上一次调用 panic）也照常继续：这两个 map 没有会因此破坏的不变量。
fn lock_maps(maps: &Mutex<ScreenMaps>) -> std::sync::MutexGuard<'_, ScreenMaps> {
    match maps.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 手机屏的建/改/撤逻辑，对应 C# `PhoneDisplayService` 的正体。
struct PhoneScreen<A: DisplayApi, V: VirtualDisplays> {
    api: A,
    ipc: V,
    timings: Timings,
    maps: Arc<Mutex<ScreenMaps>>,
}

impl<A: DisplayApi, V: VirtualDisplays> PhoneScreen<A, V> {
    /// 测试里直接建一个完整对象；真机走 [`Self::with_maps`]，与查询共享 map。
    #[cfg(test)]
    fn new(api: A, ipc: V, timings: Timings) -> Self {
        Self::with_maps(api, ipc, timings, Arc::new(Mutex::new(ScreenMaps::default())))
    }

    fn with_maps(api: A, ipc: V, timings: Timings, maps: Arc<Mutex<ScreenMaps>>) -> Self {
        Self { api, ipc, timings, maps }
    }

    /// 启动时清掉上次崩溃留下的虚拟屏（C# `CleanupOrphansAsync`）。
    fn cleanup_orphans(&mut self) {
        if !self.api.driver_pipe_available() {
            return;
        }
        let _ = self.ipc.remove_all();
        sleep_for(self.timings.cleanup_settle);
    }

    /// 建屏（C# `CreateAsync`）。
    fn create(&mut self, fit: ScreenFit) -> Result<u32, PhoneScreenException> {
        if !self.api.driver_pipe_available() && !self.api.restart_virtual_display_device() {
            return Err(PhoneScreenException::with_detail(
                PhoneScreenErrorKind::CreateFailed,
                format!(
                    "未检测到 virtual-display-rs 驱动管道（{}），请确认驱动已安装且正常运行",
                    VDD_PIPE_HINT
                ),
            ));
        }

        let existing: HashSet<String> = self
            .api
            .active_monitors()
            .iter()
            .map(|m| normalized(&m.device_name))
            .collect();
        let modes = build_modes(&fit);
        let (base_width, base_height) = fit.pixel_size(fit.base_width());

        for candidate in CANDIDATE_IDS {
            let monitor = VddMonitor {
                id: candidate,
                name: Some(MONITOR_NAME.to_string()),
                enabled: true,
                modes: modes.clone(),
            };
            if self.ipc.notify(&[monitor]).is_err() {
                // 驱动拒了这块（多半是编号被占），换下一个候选。
                continue;
            }
            let Some(snapshot) = self.wait_for_online(&existing, base_width, base_height) else {
                let _ = self.ipc.remove(&[candidate]);
                continue;
            };
            {
                let mut maps = lock_maps(&self.maps);
                maps.fits.insert(candidate, fit.clone());
                maps.device_names.insert(candidate, snapshot.device_name.clone());
            }
            self.set_width(candidate, fit.base_width());
            return Ok(candidate);
        }

        Err(PhoneScreenException::new(PhoneScreenErrorKind::NeverOnline))
    }

    /// 等新屏上线，必要时主动把它扩展成桌面（C# `WaitForOnlineAsync`）。
    fn wait_for_online(
        &self,
        existing: &HashSet<String>,
        base_width: i32,
        base_height: i32,
    ) -> Option<MonitorSnapshot> {
        let start = Instant::now();
        while start.elapsed() < self.timings.online {
            let all = self.api.active_monitors();
            if let Some(found) =
                pick_new_monitor(existing, &all, base_width, base_height, |device, w, h| {
                    self.api.has_display_mode(device, w, h)
                })
            {
                return Some(found);
            }

            let right_edge = all
                .iter()
                .map(|m| m.bounds.max_x())
                .fold(f64::NEG_INFINITY, f64::max);
            let right_edge = if right_edge.is_finite() { right_edge as i32 } else { 2560 };
            if self
                .api
                .try_attach_virtual_display(base_width, base_height, right_edge, 0)
                .is_some()
            {
                sleep_for(self.timings.attach_settle);
                if let Some(attached) = self
                    .api
                    .active_monitors()
                    .into_iter()
                    .find(|m| !existing.contains(&normalized(&m.device_name)))
                {
                    return Some(attached);
                }
            }

            sleep_for(self.timings.online_poll);
        }
        None
    }

    /// 换比例（C# `ReshapeAsync`）。
    fn reshape(&mut self, display_id: u32, fit: ScreenFit) -> bool {
        let Some(device_name) = lock_maps(&self.maps).device_names.get(&display_id).cloned() else {
            return false;
        };
        let monitor = VddMonitor {
            id: display_id,
            name: Some(MONITOR_NAME.to_string()),
            enabled: true,
            modes: build_modes(&fit),
        };
        let notified = if self.ipc.is_legacy() {
            self.ipc.replace_legacy(&[display_id], &[monitor])
        } else {
            self.ipc.notify(&[monitor])
        };
        if notified.is_err() {
            return false;
        }

        lock_maps(&self.maps).fits.insert(display_id, fit.clone());
        let (base_width, base_height) = fit.pixel_size(fit.base_width());
        let start = Instant::now();
        while start.elapsed() < self.timings.reshape {
            if self.api.has_display_mode(&device_name, base_width, base_height) {
                break;
            }
            sleep_for(self.timings.reshape_poll);
        }
        self.set_width(display_id, fit.base_width());
        true
    }

    /// 撤屏（C# `RetireAsync`）：等它真的下线，顺手把走丢的光标捞回来。
    fn retire(&mut self, display_id: u32) -> bool {
        let device_name = {
            let mut maps = lock_maps(&self.maps);
            maps.fits.remove(&display_id);
            maps.device_names.remove(&display_id)
        };
        let _ = self.ipc.remove(&[display_id]);

        let Some(device_name) = device_name else {
            return true;
        };
        let start = Instant::now();
        let mut gone = false;
        while start.elapsed() < self.timings.offline {
            let still_there = self
                .api
                .active_monitors()
                .iter()
                .any(|m| normalized(&m.device_name) == normalized(&device_name));
            if !still_there {
                gone = true;
                break;
            }
            sleep_for(self.timings.offline_poll);
        }
        if gone {
            self.restore_cursor_if_stranded();
        }
        gone
    }

    /// 光标停在被撤掉的屏上时挪到主屏中央（C# `RestoreCursorIfStranded`）。
    fn restore_cursor_if_stranded(&self) {
        let Some(cursor) = self.api.cursor_position() else {
            return;
        };
        let monitors = self.api.active_monitors();
        if let Some(target) = cursor_rescue_point(cursor, &monitors) {
            self.api.move_cursor(target);
        }
    }

    /// 把某块屏切到给定模式（C# `SetModeAsync`，`permanently` 在 C# 里也没用）。
    fn set_mode(&mut self, display_id: u32, mode: DisplayModeInfo) {
        let Some(target) = self
            .api
            .active_monitors()
            .into_iter()
            .find(|m| m.stable_id == display_id)
        else {
            return;
        };
        if target.mode.width != mode.width
            || target.mode.height != mode.height
            || (target.mode.refresh_rate - mode.refresh_rate).abs() >= 1.0
        {
            self.api.set_resolution(
                &target.device_name,
                mode.width,
                mode.height,
                mode.refresh_rate,
            );
        }
        if target.mode.dpi_scale_percent != mode.dpi_scale_percent {
            self.api.set_dpi_scale(
                target.adapter_id,
                target.source_id,
                mode.dpi_scale_percent,
            );
        }
    }

    /// 切到某个逻辑宽度：先改分辨率，再把 DPI 拉到 200%（C# `SetWidthAsync`）。
    fn set_width(&mut self, display_id: u32, width: i32) {
        let (fit, device_name) = {
            let maps = lock_maps(&self.maps);
            let (Some(fit), Some(name)) = (
                maps.fits.get(&display_id).cloned(),
                maps.device_names.get(&display_id).cloned(),
            ) else {
                return;
            };
            (fit, name)
        };

        let (pixel_width, pixel_height) = fit.pixel_size(width);
        let current = find_monitor_of(&self.api, &self.maps, display_id);
        let needs_resolution = match &current {
            Some(m) => m.mode.width != pixel_width || m.mode.height != pixel_height,
            None => true,
        };
        if needs_resolution {
            if !self
                .api
                .set_resolution(&device_name, pixel_width, pixel_height, 60.0)
            {
                return;
            }
            sleep_for(self.timings.resolution_settle);
        }

        if let Some(after) = find_monitor_of(&self.api, &self.maps, display_id)
            && after.mode.dpi_scale_percent != TARGET_DPI_PERCENT
        {
            self.api.set_dpi_scale(
                after.adapter_id,
                after.source_id,
                TARGET_DPI_PERCENT,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 查询（不碰慢操作锁：界面每帧调用的路径）
// ---------------------------------------------------------------------------

fn find_monitor_of<A: DisplayApi>(
    api: &A,
    maps: &Mutex<ScreenMaps>,
    display_id: u32,
) -> Option<MonitorSnapshot> {
    let device_name = lock_maps(maps).device_names.get(&display_id).cloned()?;
    let wanted = normalized(&device_name);
    api.active_monitors()
        .into_iter()
        .find(|m| normalized(&m.device_name) == wanted)
}

fn is_online_of<A: DisplayApi>(api: &A, maps: &Mutex<ScreenMaps>, display_id: u32) -> bool {
    let Some(device_name) = lock_maps(maps).device_names.get(&display_id).cloned() else {
        return false;
    };
    let wanted = normalized(&device_name);
    api.active_monitors()
        .iter()
        .any(|m| normalized(&m.device_name) == wanted)
}

fn width_of_display<A: DisplayApi>(
    api: &A,
    maps: &Mutex<ScreenMaps>,
    display_id: u32,
) -> Option<i32> {
    let monitor = find_monitor_of(api, maps, display_id)?;
    let fit = lock_maps(maps).fits.get(&display_id).cloned();
    Some(width_for_mode(monitor.mode.width, fit.as_ref()))
}

fn window_area_of_display<A: DisplayApi>(
    api: &A,
    maps: &Mutex<ScreenMaps>,
    display_id: u32,
) -> Option<Rect> {
    let monitor = find_monitor_of(api, maps, display_id)?;
    let logical_width = width_of_display(api, maps, display_id)
        .unwrap_or(DESIGN_WIDTH as i32);
    Some(window_area(&monitor, logical_width))
}

fn home_bar_area_of_display<A: DisplayApi>(
    api: &A,
    maps: &Mutex<ScreenMaps>,
    display_id: u32,
) -> Option<Rect> {
    let monitor = find_monitor_of(api, maps, display_id)?;
    let area = window_area(&monitor, width_of_display(api, maps, display_id).unwrap_or(DESIGN_WIDTH as i32));
    Some(home_bar_area(&monitor, area))
}

fn main_area_of<A: DisplayApi>(api: &A, maps: &Mutex<ScreenMaps>) -> Option<Rect> {
    let names = lock_maps(maps).phone_names();
    main_area(&api.active_monitors(), &names)
}

fn other_displays_of<A: DisplayApi>(api: &A, maps: &Mutex<ScreenMaps>) -> Vec<OtherDisplay> {
    let names = lock_maps(maps).phone_names();
    other_displays(&api.active_monitors(), &names)
}

/// 供 `home_bar_area_of` 复用「窗口区域」的算式，但不重复查两次显示器列表。
const VDD_PIPE_HINT: &str = r"\\.\pipe\virtualdisplaydriver";

// ---------------------------------------------------------------------------
// 真机实现
// ---------------------------------------------------------------------------

/// CCD 里查到的"这个 GDI 设备名对应哪个适配器/输出"（C# `QueryCcdSources` 的结果）。
#[derive(Debug, Clone)]
struct CcdSource {
    adapter_id: AdapterLuid,
    source_id: u32,
    friendly_name: String,
}

///  undocumented 的 DPI 缩放请求包（`GetSourceDpiScale`，type = -3）。
#[repr(C)]
struct DpiScaleGet {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    min_scale_rel: i32,
    cur_scale_rel: i32,
    max_scale_rel: i32,
}

/// undocumented 的 DPI 缩放设置包（`SetSourceDpiScale`，type = -4）。
#[repr(C)]
struct DpiScaleSet {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    scale_rel: i32,
}

const DEVICE_INFO_GET_DPI_SCALE: i32 = -3;
const DEVICE_INFO_SET_DPI_SCALE: i32 = -4;

/// 真机显示子系统（C# `DisplayConfigService`）。
#[derive(Debug, Default, Clone, Copy)]
pub struct WinDisplay;

impl WinDisplay {
    pub const fn new() -> Self {
        Self
    }

    fn ccd_sources(&self) -> HashMap<String, CcdSource> {
        let mut map = HashMap::new();
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        if unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        }
        .0
            != 0
        {
            return map;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        if unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        }
        .0
            != 0
        {
            return map;
        }
        paths.truncate(path_count as usize);

        for path in &paths {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            source.header.adapterId = path.sourceInfo.adapterId;
            source.header.id = path.sourceInfo.id;
            if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 {
                continue;
            }
            let gdi_name = from_wide(&source.viewGdiDeviceName);
            if gdi_name.is_empty() {
                continue;
            }

            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            target.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            target.header.adapterId = path.targetInfo.adapterId;
            target.header.id = path.targetInfo.id;
            let friendly = if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } == 0 {
                from_wide(&target.monitorFriendlyDeviceName)
            } else {
                gdi_name.clone()
            };

            let adapter_id =
                AdapterLuid::new(path.sourceInfo.adapterId.LowPart, path.sourceInfo.adapterId.HighPart);
            map.entry(normalized(&gdi_name)).or_insert(CcdSource {
                adapter_id,
                source_id: path.sourceInfo.id,
                friendly_name: friendly,
            });
        }
        map
    }

    fn device_strings(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        let mut index = 0u32;
        loop {
            let mut device = DISPLAY_DEVICEW::default();
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0) }.as_bool() {
                break;
            }
            index += 1;
            let name = from_wide(&device.DeviceName);
            if !name.is_empty() {
                map.insert(normalized(&name), from_wide(&device.DeviceString));
            }
        }
        map
    }

    fn dpi_scale_percent(&self, adapter_id: LUID, source_id: u32) -> i32 {
        let mut packet = DpiScaleGet {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(DEVICE_INFO_GET_DPI_SCALE),
                size: std::mem::size_of::<DpiScaleGet>() as u32,
                adapterId: adapter_id,
                id: source_id,
            },
            min_scale_rel: 0,
            cur_scale_rel: 0,
            max_scale_rel: 0,
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut packet.header) } != 0 {
            return 100;
        }
        dpi_percent_from_rel(packet.min_scale_rel, packet.cur_scale_rel)
    }
}

impl DisplayApi for WinDisplay {
    fn active_monitors(&self) -> Vec<MonitorSnapshot> {
        let mut handles: Vec<HMONITOR> = Vec::new();
        unsafe {
            let _ = EnumDisplayMonitors(
                None,
                None,
                Some(collect_monitor),
                LPARAM(&mut handles as *mut Vec<HMONITOR> as isize),
            );
        }

        let ccd = self.ccd_sources();
        let device_strings = self.device_strings();
        let mut monitors = Vec::with_capacity(handles.len());

        for handle in handles {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if !unsafe { GetMonitorInfoW(handle, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) }
                .as_bool()
            {
                continue;
            }

            let device_name = from_wide(&info.szDevice);
            let bounds = rect_of(info.monitorInfo.rcMonitor);
            let work_area = rect_of(info.monitorInfo.rcWork);
            let is_primary = info.monitorInfo.dwFlags & 1 != 0;

            let mut width = bounds.width as i32;
            let mut height = bounds.height as i32;
            let mut refresh_rate = 60.0;
            let wide_name = to_wide(&device_name);
            let mut dev_mode = DEVMODEW::default();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            if unsafe {
                EnumDisplaySettingsExW(
                    PCWSTR(wide_name.as_ptr()),
                    ENUM_CURRENT_SETTINGS,
                    &mut dev_mode,
                    ENUM_DISPLAY_SETTINGS_FLAGS(0),
                )
            }
            .as_bool()
            {
                width = dev_mode.dmPelsWidth as i32;
                height = dev_mode.dmPelsHeight as i32;
                if dev_mode.dmDisplayFrequency > 0 {
                    refresh_rate = dev_mode.dmDisplayFrequency as f64;
                }
            }

            let source = ccd.get(&normalized(&device_name));
            let (adapter_id, source_id, friendly_name) = match source {
                Some(source) => (source.adapter_id, source.source_id, {
                    if source.friendly_name.trim().is_empty() {
                        device_name.clone()
                    } else {
                        source.friendly_name.clone()
                    }
                }),
                None => (AdapterLuid::default(), 0, device_name.clone()),
            };
            let dpi_scale_percent = match source {
                Some(source) => self.dpi_scale_percent(
                    LUID { LowPart: source.adapter_id.low_part, HighPart: source.adapter_id.high_part },
                    source.source_id,
                ),
                None => 100,
            };
            let device_string = device_strings
                .get(&normalized(&device_name))
                .cloned()
                .unwrap_or_default();

            monitors.push(MonitorSnapshot {
                stable_id: stable_id(&device_name, adapter_id, source_id),
                device_name,
                friendly_name,
                device_string,
                bounds,
                work_area,
                is_primary,
                mode: DisplayModeInfo { width, height, refresh_rate, dpi_scale_percent },
                adapter_id,
                source_id,
            });
        }
        monitors
    }

    fn has_display_mode(&self, device_name: &str, width: i32, height: i32) -> bool {
        let wide_name = to_wide(device_name);
        let name = PCWSTR(wide_name.as_ptr());
        let mut index = 0u32;
        loop {
            let mut dev_mode = DEVMODEW::default();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            if !unsafe {
                EnumDisplaySettingsExW(
                    name,
                    ENUM_DISPLAY_SETTINGS_MODE(index),
                    &mut dev_mode,
                    ENUM_DISPLAY_SETTINGS_FLAGS(0),
                )
            }
            .as_bool()
            {
                return false;
            }
            index += 1;
            if dev_mode.dmPelsWidth == width as u32 && dev_mode.dmPelsHeight == height as u32 {
                return true;
            }
        }
    }

    fn set_resolution(&self, device_name: &str, width: i32, height: i32, refresh_rate: f64) -> bool {
        let wide_name = to_wide(device_name);
        let name = PCWSTR(wide_name.as_ptr());
        let mut dev_mode = DEVMODEW::default();
        dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if !unsafe {
            EnumDisplaySettingsExW(
                name,
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
                ENUM_DISPLAY_SETTINGS_FLAGS(0),
            )
        }
        .as_bool()
        {
            return false;
        }

        dev_mode.dmPelsWidth = width.max(0) as u32;
        dev_mode.dmPelsHeight = height.max(0) as u32;
        let mut fields = DM_PELSWIDTH | DM_PELSHEIGHT;
        if refresh_rate > 0.0 {
            dev_mode.dmDisplayFrequency = refresh_rate.round().max(1.0) as u32;
            fields = fields | DM_DISPLAYFREQUENCY;
        }
        dev_mode.dmFields = fields;

        unsafe {
            ChangeDisplaySettingsExW(
                name,
                Some(&dev_mode as *const DEVMODEW),
                None,
                CDS_UPDATEREGISTRY,
                None,
            ) == DISP_CHANGE_SUCCESSFUL
        }
    }

    fn set_dpi_scale(&self, adapter_id: AdapterLuid, source_id: u32, percent: i32) -> bool {
        let luid = LUID { LowPart: adapter_id.low_part, HighPart: adapter_id.high_part };
        let mut get = DpiScaleGet {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(DEVICE_INFO_GET_DPI_SCALE),
                size: std::mem::size_of::<DpiScaleGet>() as u32,
                adapterId: luid,
                id: source_id,
            },
            min_scale_rel: 0,
            cur_scale_rel: 0,
            max_scale_rel: 0,
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut get.header) } != 0 {
            return false;
        }
        let Some(scale_rel) = dpi_rel_for_percent(percent, get.min_scale_rel, get.max_scale_rel)
        else {
            return false;
        };
        if scale_rel == get.cur_scale_rel {
            return true;
        }

        let set = DpiScaleSet {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(DEVICE_INFO_SET_DPI_SCALE),
                size: std::mem::size_of::<DpiScaleSet>() as u32,
                adapterId: luid,
                id: source_id,
            },
            scale_rel,
        };
        unsafe { DisplayConfigSetDeviceInfo(&set.header) == 0 }
    }

    fn try_attach_virtual_display(&self, width: i32, height: i32, x: i32, y: i32) -> Option<String> {
        let mut index = 0u32;
        loop {
            let mut device = DISPLAY_DEVICEW::default();
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0) }.as_bool() {
                return None;
            }
            index += 1;

            let device_name = from_wide(&device.DeviceName);
            let device_string = from_wide(&device.DeviceString);
            if !contains_ignore_case(&device_string, "Virtual Display") {
                continue;
            }
            if device.StateFlags.0 & DISPLAY_DEVICE_ACTIVE.0 != 0 {
                continue;
            }

            // 确认这块适配器下确实挂着一个显示器。
            let wide_name = to_wide(&device_name);
            let mut attached = DISPLAY_DEVICEW::default();
            attached.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if !unsafe {
                EnumDisplayDevicesW(PCWSTR(wide_name.as_ptr()), 0, &mut attached, 0)
            }
            .as_bool()
            {
                continue;
            }

            let mut dev_mode = DEVMODEW::default();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            dev_mode.dmFields = DM_POSITION
                | DM_PELSWIDTH
                | DM_PELSHEIGHT
                | DM_BITSPERPEL
                | DM_DISPLAYFREQUENCY;
            // 联合体字段写入是安全的（读才需要 unsafe）：dmPosition 与打印字段共用同一段内存。
            dev_mode.Anonymous1.Anonymous2.dmPosition = POINTL { x, y };
            dev_mode.dmBitsPerPel = 32;
            dev_mode.dmPelsWidth = width.max(0) as u32;
            dev_mode.dmPelsHeight = height.max(0) as u32;
            dev_mode.dmDisplayFrequency = 60;

            let r1 = unsafe {
                ChangeDisplaySettingsExW(
                    PCWSTR(wide_name.as_ptr()),
                    Some(&dev_mode as *const DEVMODEW),
                    None,
                    CDS_UPDATEREGISTRY | CDS_NORESET,
                    None,
                )
            };
            let r2 = unsafe {
                ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None)
            };
            if r1 == DISP_CHANGE_SUCCESSFUL && r2 == DISP_CHANGE_SUCCESSFUL {
                return Some(device_name);
            }

            let applied = unsafe {
                SetDisplayConfig(None, None, SDC_APPLY | SDC_TOPOLOGY_EXTEND)
            };
            if applied == 0 {
                return Some(device_name);
            }
        }
    }

    fn cursor_position(&self) -> Option<Point> {
        let mut point = POINT::default();
        unsafe { GetCursorPos(&mut point) }.ok()?;
        Some(Point::new(point.x as f64, point.y as f64))
    }

    fn move_cursor(&self, point: Point) {
        let _ = unsafe { SetCursorPos(point.x.round() as i32, point.y.round() as i32) };
    }

    fn driver_pipe_available(&self) -> bool {
        is_driver_pipe_available(crate::vdd::DEFAULT_PIPE_NAME)
    }

    fn restart_virtual_display_device(&self) -> bool {
        let Some(instance_id) = find_virtual_display_instance_id() else {
            return false;
        };
        let status = std::process::Command::new("pnputil.exe")
            .args(["/restart-device", &instance_id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        if status.is_err() {
            return false;
        }

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if self.driver_pipe_available() {
                return true;
            }
            sleep_for(Duration::from_millis(200));
        }
        false
    }
}

use std::os::windows::process::CommandExt;

/// `CREATE_NO_WINDOW`：跑 `pnputil` 时不闪黑框。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    if data.0 != 0 {
        let handles = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        handles.push(monitor);
    }
    BOOL(1)
}

fn rect_of(rect: RECT) -> Rect {
    Rect::new(
        rect.left as f64,
        rect.top as f64,
        (rect.right - rect.left) as f64,
        (rect.bottom - rect.top) as f64,
    )
}

/// 在注册表 `HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\DISPLAY` 下找虚拟屏设备实例
/// （C# `FindVirtualDisplayInstanceId`）。
fn find_virtual_display_instance_id() -> Option<String> {
    unsafe {
        let mut root = HKEY(std::ptr::null_mut());
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!(r"SYSTEM\CurrentControlSet\Enum\ROOT\DISPLAY"),
            None,
            KEY_READ,
            &mut root,
        )
        .0
            != 0
        {
            return None;
        }

        let mut found = None;
        let mut index = 0u32;
        loop {
            let mut name = [0u16; 256];
            let mut name_len = name.len() as u32;
            let status = RegEnumKeyExW(
                root,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                None,
                None,
                None,
                None,
            );
            if status.0 != 0 {
                break;
            }
            index += 1;
            let subkey = String::from_utf16_lossy(&name[..name_len as usize]);
            if subkey.is_empty() {
                continue;
            }

            let mut device = HKEY(std::ptr::null_mut());
            let subkey_wide = to_wide(&subkey);
            if RegOpenKeyExW(root, PCWSTR(subkey_wide.as_ptr()), None, KEY_READ, &mut device).0 != 0 {
                continue;
            }

            let mut buffer = [0u8; 4096];
            let mut size = buffer.len() as u32;
            let mut value_type = REG_VALUE_TYPE(0);
            let queried = RegQueryValueExW(
                device,
                w!("HardwareID"),
                None,
                Some(&mut value_type),
                Some(buffer.as_mut_ptr()),
                Some(&mut size),
            );
            if queried.0 == 0 {
                let hardware_ids = multi_sz(&buffer[..size as usize]);
                if hardware_ids
                    .iter()
                    .any(|id| contains_ignore_case(id, "VirtualDisplayDriver"))
                {
                    found = Some(format!(r"ROOT\DISPLAY\{subkey}"));
                }
            }
            let _ = RegCloseKey(device);
            if found.is_some() {
                break;
            }
        }

        let _ = RegCloseKey(root);
        found
    }
}

/// 把 `REG_MULTI_SZ` 的原始字节拆成若干字符串。
fn multi_sz(bytes: &[u8]) -> Vec<String> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let mut out = Vec::new();
    let mut current = Vec::new();
    for unit in units {
        if unit == 0 {
            if current.is_empty() {
                break; // 双零结尾
            }
            out.push(String::from_utf16_lossy(&current));
            current.clear();
        } else {
            current.push(unit);
        }
    }
    if !current.is_empty() {
        out.push(String::from_utf16_lossy(&current));
    }
    out
}

// ---------------------------------------------------------------------------
// 给 `snug-core` 的接口实现
// ---------------------------------------------------------------------------

/// 真机手机屏服务：`snug_core::desk::PhoneDisplays` 的 Windows 实现。
pub struct WinPhoneDisplays {
    /// 查询用。`WinDisplay` 无状态，每次现查系统，不与慢操作抢锁。
    api: WinDisplay,
    /// 只保护 `fits` / `device_names`，锁内没有系统调用。
    maps: Arc<Mutex<ScreenMaps>>,
    /// 慢操作串行：create / reshape / retire / set_width / cleanup 才拿这把锁。
    ///
    /// C# 这些方法是 async，等待时把线程让出去；Rust 用「慢操作锁 + 短查询锁」
    /// 达到同样效果：查询（界面每帧调 `home_bar_area`/`phone_bounds`）不会
    /// 被几秒的轮询挡住。
    ops: Mutex<PhoneScreen<WinDisplay, VddClient>>,
}

impl Default for WinPhoneDisplays {
    fn default() -> Self {
        Self::new()
    }
}

impl WinPhoneDisplays {
    pub fn new() -> Self {
        Self::with_timings(Timings::default())
    }

    pub fn with_timings(timings: Timings) -> Self {
        Self::with_parts(WinDisplay::new(), VddClient::default(), timings)
    }

    pub fn with_parts(api: WinDisplay, ipc: VddClient, timings: Timings) -> Self {
        let maps = Arc::new(Mutex::new(ScreenMaps::default()));
        let screen = PhoneScreen::with_maps(api, ipc, timings, maps.clone());
        Self { api, maps, ops: Mutex::new(screen) }
    }

    /// 启动时清掉残留的虚拟屏（C# `CleanupOrphansAsync`）。
    pub fn cleanup_orphans(&self) {
        self.with_ops(|screen| screen.cleanup_orphans());
    }

    /// Home Bar 区域（C# `PhoneDisplayService.HomeBarAreaOf`）。
    ///
    /// C# 的 `IPhoneDisplays` 里没有这个方法，界面那边用的是具体类型，所以这里也放在具体类型上。
    pub fn home_bar_area_of(&self, display_id: u32) -> Option<Rect> {
        home_bar_area_of_display(&self.api, &self.maps, display_id)
    }

    /// 慢操作串行入口；锁中毒（上一次调用 panic）也照常继续。
    fn with_ops<T>(&self, body: impl FnOnce(&mut PhoneScreen<WinDisplay, VddClient>) -> T) -> T {
        let mut guard = match self.ops.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        body(&mut guard)
    }
}

impl PhoneDisplays for WinPhoneDisplays {
    fn create(&self, fit: ScreenFit) -> Result<u32, PhoneScreenException> {
        self.with_ops(|screen| screen.create(fit))
    }

    fn reshape(&self, display_id: u32, fit: ScreenFit) -> bool {
        self.with_ops(|screen| screen.reshape(display_id, fit))
    }

    fn fit_of(&self, display_id: u32) -> Option<ScreenFit> {
        lock_maps(&self.maps).fits.get(&display_id).cloned()
    }

    fn retire(&self, display_id: u32) -> bool {
        self.with_ops(|screen| screen.retire(display_id))
    }

    // TODO(取消)：C# 每个等待循环都带 CancellationToken，这里没有 token，
    // 建屏/换比例/撤屏的轮询最长 8s/4s/10s 不可中断（见 docs/display-parity.md）。
    fn is_online(&self, display_id: u32) -> bool {
        is_online_of(&self.api, &self.maps, display_id)
    }

    fn width_of(&self, display_id: u32) -> Option<i32> {
        width_of_display(&self.api, &self.maps, display_id)
    }

    fn set_width(&self, display_id: u32, width: i32) {
        self.with_ops(|screen| screen.set_width(display_id, width));
    }

    fn bounds_of(&self, display_id: u32) -> Option<Rect> {
        Some(find_monitor_of(&self.api, &self.maps, display_id)?.bounds)
    }

    fn window_area_of(&self, display_id: u32) -> Option<Rect> {
        window_area_of_display(&self.api, &self.maps, display_id)
    }

    fn main_area(&self) -> Option<Rect> {
        main_area_of(&self.api, &self.maps)
    }

    fn other_displays(&self) -> Vec<OtherDisplay> {
        other_displays_of(&self.api, &self.maps)
    }

    fn set_mode(&self, display_id: u32, mode: DisplayModeInfo, permanently: bool) {
        let _ = permanently; // C# 版本同样忽略这个参数。
        self.with_ops(|screen| screen.set_mode(display_id, mode));
    }
}

/// 便于日志与诊断：这块服务用的管道名。
pub fn driver_pipe_name() -> &'static str {
    crate::vdd::DEFAULT_PIPE_NAME
}

// ---------------------------------------------------------------------------
// 单测（只测纯逻辑与假实现，绝不碰真实显示配置）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    // ----- 纯函数 -----

    #[test]
    fn dpi_table_maps_relative_indices() {
        // 推荐档位 = 第 3 档（175%）：min_rel = -3。
        assert_eq!(dpi_percent_from_rel(-3, 0), 175);
        assert_eq!(dpi_percent_from_rel(-3, 1), 200);
        assert_eq!(dpi_percent_from_rel(-3, 3), 250);
        assert_eq!(dpi_percent_from_rel(-3, -3), 100);
        // 越界要夹住，不能 panic。
        assert_eq!(dpi_percent_from_rel(-3, 99), 500);
        assert_eq!(dpi_percent_from_rel(-3, -99), 100);
        assert_eq!(dpi_percent_from_rel(0, 0), 100);
    }

    #[test]
    fn dpi_rel_for_percent_only_accepts_table_values() {
        // 推荐 175%（index 3），要 200% → 相对 +1。
        assert_eq!(dpi_rel_for_percent(200, -3, 3), Some(1));
        assert_eq!(dpi_rel_for_percent(100, -3, 3), Some(-3));
        assert_eq!(dpi_rel_for_percent(175, -3, 3), Some(0));
        // 不在档位表里。
        assert_eq!(dpi_rel_for_percent(133, -3, 3), None);
        // 超出 max_rel 时夹住。
        assert_eq!(dpi_rel_for_percent(500, -3, 1), Some(1));
    }

    #[test]
    fn stable_id_is_deterministic_and_separates_outputs() {
        let adapter = AdapterLuid::new(0x1234, 0x5678);
        let a = stable_id(r"\\.\DISPLAY1", adapter, 0);
        let b = stable_id(r"\\.\DISPLAY1", adapter, 0);
        assert_eq!(a, b, "同一块屏要给出同一个编号");
        assert_ne!(a, stable_id(r"\\.\DISPLAY1", adapter, 1));
        assert_ne!(a, stable_id(r"\\.\DISPLAY1", AdapterLuid::new(1, 0x5678), 0));

        // 没有 CCD 信息时退回设备名哈希，且大小写不敏感（C# OrdinalIgnoreCase）。
        let zero = AdapterLuid::default();
        assert_eq!(
            stable_id(r"\\.\display2", zero, 0),
            stable_id(r"\\.\DISPLAY2", zero, 0)
        );
        assert_ne!(
            stable_id(r"\\.\DISPLAY2", zero, 0),
            stable_id(r"\\.\DISPLAY3", zero, 0)
        );
    }

    #[test]
    fn window_area_removes_home_bar_at_200_percent() {
        // 1080×1900 物理像素 = 540 点 * 2 倍；Home Bar 28 点 → 56 像素。
        let monitor = snapshot(
            r"\\.\DISPLAY2",
            "Virtual Display",
            Rect::new(1920.0, 0.0, 1080.0, 1868.0),
            Rect::new(1920.0, 0.0, 1080.0, 1868.0),
            false,
            DisplayModeInfo { width: 1080, height: 1900, refresh_rate: 60.0, dpi_scale_percent: 200 },
        );
        let area = window_area(&monitor, 540);
        assert_eq!(area, Rect::new(1920.0, 0.0, 1080.0, 1812.0));
        assert_eq!(home_bar_area(&monitor, area), Rect::new(1920.0, 1812.0, 1080.0, 56.0));
    }

    #[test]
    fn window_area_keeps_at_least_100_points_and_28_point_bar() {
        let monitor = snapshot(
            r"\\.\DISPLAY3",
            "Virtual Display",
            Rect::new(0.0, 0.0, 540.0, 120.0),
            Rect::new(0.0, 0.0, 540.0, 120.0),
            false,
            DisplayModeInfo { width: 540, height: 960, refresh_rate: 60.0, dpi_scale_percent: 100 },
        );
        let area = window_area(&monitor, 1000); // 缩放按 1.0 兜底
        assert_eq!(area.height, 100.0);
        let bar = home_bar_area(&monitor, area);
        assert_eq!(bar.height, 28.0, "Home Bar 最少 28 点");
        assert_eq!(bar.y, 100.0);
    }

    #[test]
    fn main_area_skips_phone_displays() {
        let primary = snapshot(
            r"\\.\DISPLAY2",
            "Virtual Display",
            Rect::new(0.0, 0.0, 1080.0, 1900.0),
            Rect::new(0.0, 0.0, 1080.0, 1900.0),
            true,
            mode(1080, 1900),
        );
        let laptop = snapshot(
            r"\\.\DISPLAY1",
            "BOE 0x0BCA",
            Rect::new(1080.0, 0.0, 2560.0, 1600.0),
            Rect::new(1080.0, 0.0, 2560.0, 1560.0),
            false,
            mode(2560, 1600),
        );
        let phones: HashSet<String> = [normalized(r"\\.\DISPLAY2")].into_iter().collect();
        assert_eq!(
            main_area(&[primary.clone(), laptop.clone()], &phones),
            Some(laptop.work_area.clone())
        );
        // 没有手机屏时用真正的主屏。
        assert_eq!(
            main_area(&[primary.clone(), laptop.clone()], &HashSet::new()),
            Some(primary.work_area)
        );
        // 一块屏都没有。
        assert_eq!(main_area(&[], &HashSet::new()), None);
    }

    #[test]
    fn other_displays_hides_phone_and_virtual_screens() {
        let phone = snapshot(r"\\.\DISPLAY2", "Virtual Display", Rect::ZERO, Rect::ZERO, false, mode(1080, 1900));
        let virtual_ = snapshot(r"\\.\DISPLAY3", "VirtuDisplay", Rect::ZERO, Rect::ZERO, false, mode(1080, 1900));
        let real = snapshot(r"\\.\DISPLAY1", "DELL U2720Q", Rect::ZERO, Rect::ZERO, true, mode(3840, 2160));
        let phones: HashSet<String> = [normalized(r"\\.\DISPLAY2")].into_iter().collect();

        let list = other_displays(&[phone, virtual_, real.clone()], &phones);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].device_name, real.device_name);
        assert_eq!(list[0].friendly_name, "DELL U2720Q");
        assert!(!list[0].asleep);
        assert_eq!(list[0].mode.width, 3840);
    }

    #[test]
    fn width_for_mode_matches_steps_then_falls_back_to_half() {
        let fit = ScreenFit::standard();
        assert_eq!(width_for_mode(1080, Some(&fit)), 540);
        assert_eq!(width_for_mode(2160, Some(&fit)), 1080);
        // 不在档位上就按 200% DPI 折半（.5 远离零）。
        assert_eq!(width_for_mode(1001, Some(&fit)), 501);
        assert_eq!(width_for_mode(999, None), 500);
    }

    #[test]
    fn cursor_rescue_only_when_stranded() {
        let left = snapshot(
            r"\\.\DISPLAY1",
            "DELL",
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            Rect::new(0.0, 0.0, 1920.0, 1040.0),
            true,
            mode(1920, 1080),
        );
        let right = snapshot(
            r"\\.\DISPLAY2",
            "Virtual Display",
            Rect::new(1920.0, 0.0, 540.0, 960.0),
            Rect::new(1920.0, 0.0, 540.0, 960.0),
            false,
            mode(540, 960),
        );
        let monitors = vec![left, right];

        assert_eq!(cursor_rescue_point(Point::new(10.0, 10.0), &monitors), None);
        assert_eq!(cursor_rescue_point(Point::new(2000.0, 100.0), &monitors), None);
        // 手机屏撤掉后光标留在原地 → 挪到主屏中央。
        let remaining = vec![snapshot(
            r"\\.\DISPLAY1",
            "DELL",
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            Rect::new(0.0, 0.0, 1920.0, 1040.0),
            true,
            mode(1920, 1080),
        )];
        assert_eq!(
            cursor_rescue_point(Point::new(2000.0, 100.0), &remaining),
            Some(Point::new(960.0, 540.0))
        );
        assert_eq!(cursor_rescue_point(Point::new(0.0, 0.0), &[]), None);
    }

    #[test]
    fn pick_new_monitor_prefers_virtual_then_any_new() {
        let existing: HashSet<String> = [normalized(r"\\.\DISPLAY1")].into_iter().collect();
        let stranger = snapshot(r"\\.\DISPLAY2", "Some TV", Rect::ZERO, Rect::ZERO, false, mode(1920, 1080));
        let phone = snapshot(r"\\.\DISPLAY3", "Virtual Display", Rect::ZERO, Rect::ZERO, false, mode(1080, 1900));

        let picked = pick_new_monitor(&existing, &[stranger.clone(), phone.clone()], 1080, 1900, |_, _, _| false)
            .expect("应该认出虚拟屏");
        assert_eq!(picked.device_name, phone.device_name);

        // 都不像虚拟屏时，认出「具备基准分辨率」的那块。
        let picked = pick_new_monitor(&existing, &[stranger.clone(), phone.clone()], 1080, 1900, |device, w, h| {
            device == phone.device_name && w == 1080 && h == 1900
        })
        .expect("按分辨率认出");
        assert_eq!(picked.device_name, phone.device_name);

        // 什么线索都没有也要退回"新出现的第一个"。
        let picked = pick_new_monitor(&existing, &[stranger.clone()], 1080, 1900, |_, _, _| false)
            .expect("兜底");
        assert_eq!(picked.device_name, stranger.device_name);

        // 没有新屏。
        assert!(pick_new_monitor(&existing, &[], 1080, 1900, |_, _, _| false).is_none());
    }

    #[test]
    fn build_modes_uses_double_pixels_and_60hz() {
        let fit = ScreenFit::standard();
        let modes = build_modes(&fit);
        assert_eq!(modes.len(), fit.widths().len());
        assert_eq!(modes[0].width, 1080);
        assert_eq!(modes[0].height, 1900);
        assert_eq!(modes[0].refresh_rates, vec![60]);
        let last = modes.last().unwrap();
        assert_eq!((last.width, last.height), (2160, 3800));
    }

    // ----- 假实现的整条流程 -----

    /// 一个假的世界：显示器列表、驱动侧"哪些候选编号在线"、调用记录。
    #[derive(Default)]
    struct World {
        monitors: Vec<MonitorSnapshot>,
        pipe_available: bool,
        restart_ok: bool,
        cursor: Option<Point>,
        moves: Vec<Point>,
        resolutions: Vec<(String, i32, i32, f64)>,
        dpi_calls: Vec<(u32, u32, i32)>,
        attaches: Vec<(i32, i32, i32, i32)>,
        pending_attach: Option<String>,
        modes: HashSet<(String, i32, i32)>,
        notified: Vec<Vec<u32>>,
        removed: Vec<u32>,
        remove_all_calls: usize,
        legacy: bool,
        online_ids: HashSet<u32>,
        devices: HashMap<u32, String>,
        /// 新屏上线时系统先给它挑的分辨率；`None` = 正好是基准档（1080×1900）。
        online_mode: Option<(i32, i32)>,
    }

    impl World {
        fn driver_online(&mut self, id: u32) {
            let Some(device) = self.devices.get(&id).cloned() else {
                return;
            };
            let (width, height) = self.online_mode.unwrap_or((1080, 1900));
            let monitor = snapshot(
                &device,
                "Virtual Display",
                Rect::new(1920.0, 0.0, width as f64, height as f64),
                Rect::new(1920.0, 0.0, width as f64, height as f64),
                false,
                DisplayModeInfo { width, height, refresh_rate: 60.0, dpi_scale_percent: 100 },
            );
            self.monitors.retain(|m| m.device_name != device);
            self.monitors.push(monitor);
        }

        fn driver_offline(&mut self, id: u32) {
            if let Some(device) = self.devices.get(&id) {
                self.monitors.retain(|m| &m.device_name != device);
            }
        }
    }

    struct FakeApi(Arc<Mutex<World>>);

    impl DisplayApi for FakeApi {
        fn active_monitors(&self) -> Vec<MonitorSnapshot> {
            self.0.lock().unwrap().monitors.clone()
        }

        fn has_display_mode(&self, device_name: &str, width: i32, height: i32) -> bool {
            self.0
                .lock()
                .unwrap()
                .modes
                .contains(&(device_name.to_string(), width, height))
        }

        fn set_resolution(&self, device_name: &str, width: i32, height: i32, refresh_rate: f64) -> bool {
            let mut world = self.0.lock().unwrap();
            world.resolutions.push((device_name.to_string(), width, height, refresh_rate));
            world.modes.insert((device_name.to_string(), width, height));
            for monitor in world.monitors.iter_mut() {
                if monitor.device_name == device_name {
                    monitor.mode.width = width;
                    monitor.mode.height = height;
                    monitor.mode.refresh_rate = refresh_rate;
                }
            }
            true
        }

        fn set_dpi_scale(&self, adapter_id: AdapterLuid, source_id: u32, percent: i32) -> bool {
            let mut world = self.0.lock().unwrap();
            world.dpi_calls.push((adapter_id.low_part, source_id, percent));
            for monitor in world.monitors.iter_mut() {
                if monitor.adapter_id == adapter_id && monitor.source_id == source_id {
                    monitor.mode.dpi_scale_percent = percent;
                }
            }
            true
        }

        fn try_attach_virtual_display(&self, width: i32, height: i32, x: i32, y: i32) -> Option<String> {
            let mut world = self.0.lock().unwrap();
            world.attaches.push((width, height, x, y));
            let device = world.pending_attach.take()?;
            let monitor = snapshot(
                &device,
                "Virtual Display",
                Rect::new(x as f64, y as f64, width as f64, height as f64),
                Rect::new(x as f64, y as f64, width as f64, height as f64),
                false,
                DisplayModeInfo { width, height, refresh_rate: 60.0, dpi_scale_percent: 100 },
            );
            world.monitors.push(monitor);
            Some(device)
        }

        fn cursor_position(&self) -> Option<Point> {
            self.0.lock().unwrap().cursor
        }

        fn move_cursor(&self, point: Point) {
            self.0.lock().unwrap().moves.push(point);
        }

        fn driver_pipe_available(&self) -> bool {
            self.0.lock().unwrap().pipe_available
        }

        fn restart_virtual_display_device(&self) -> bool {
            self.0.lock().unwrap().restart_ok
        }
    }

    struct FakeVdd(Arc<Mutex<World>>);

    impl VirtualDisplays for FakeVdd {
        fn notify(&mut self, monitors: &[VddMonitor]) -> Result<(), VddError> {
            let mut world = self.0.lock().unwrap();
            world.notified.push(monitors.iter().map(|m| m.id).collect());
            let ids: Vec<u32> = monitors.iter().map(|m| m.id).collect();
            for id in ids {
                if world.online_ids.contains(&id) {
                    world.driver_online(id);
                }
            }
            Ok(())
        }

        fn replace_legacy(&mut self, remove_ids: &[u32], add: &[VddMonitor]) -> Result<(), VddError> {
            let mut world = self.0.lock().unwrap();
            world.removed.extend_from_slice(remove_ids);
            let ids: Vec<u32> = add.iter().map(|m| m.id).collect();
            world.notified.push(ids.clone());
            for id in ids {
                if world.online_ids.contains(&id) {
                    world.driver_online(id);
                }
            }
            Ok(())
        }

        fn remove(&mut self, ids: &[u32]) -> Result<(), VddError> {
            let mut world = self.0.lock().unwrap();
            world.removed.extend_from_slice(ids);
            for &id in ids {
                world.driver_offline(id);
            }
            Ok(())
        }

        fn remove_all(&mut self) -> Result<(), VddError> {
            let mut world = self.0.lock().unwrap();
            world.remove_all_calls += 1;
            world.monitors.clear();
            Ok(())
        }

        fn is_legacy(&self) -> bool {
            self.0.lock().unwrap().legacy
        }
    }

    fn fast_timings() -> Timings {
        Timings {
            online: Duration::from_secs(2),
            offline: Duration::from_secs(2),
            reshape: Duration::from_secs(2),
            online_poll: Duration::ZERO,
            attach_settle: Duration::ZERO,
            offline_poll: Duration::ZERO,
            reshape_poll: Duration::ZERO,
            resolution_settle: Duration::ZERO,
            cleanup_settle: Duration::ZERO,
        }
    }

    fn world_with_phone(online_ids: &[u32]) -> World {
        let mut world = World {
            pipe_available: true,
            devices: HashMap::from([
                (0u32, r"\\.\DISPLAY2".to_string()),
                (1u32, r"\\.\DISPLAY3".to_string()),
                (2u32, r"\\.\DISPLAY4".to_string()),
                (3u32, r"\\.\DISPLAY5".to_string()),
            ]),
            online_ids: online_ids.iter().copied().collect(),
            ..World::default()
        };
        world.monitors.push(snapshot(
            r"\\.\DISPLAY1",
            "DELL U2720Q",
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            Rect::new(0.0, 0.0, 1920.0, 1040.0),
            true,
            mode(1920, 1080),
        ));
        world
    }

    fn screen(world: World) -> (PhoneScreen<FakeApi, FakeVdd>, Arc<Mutex<World>>) {
        let shared = Arc::new(Mutex::new(world));
        let screen = PhoneScreen::new(
            FakeApi(shared.clone()),
            FakeVdd(shared.clone()),
            fast_timings(),
        );
        (screen, shared)
    }

    #[test]
    fn create_reports_missing_driver_pipe() {
        let mut world = world_with_phone(&[0]);
        world.pipe_available = false;
        world.restart_ok = false;
        let (mut screen, _) = screen(world);

        let error = screen.create(ScreenFit::standard()).expect_err("驱动不在就该失败");
        assert_eq!(error.kind, PhoneScreenErrorKind::CreateFailed);
        assert!(error.detail.contains("virtualdisplaydriver"), "{}", error.detail);
    }

    #[test]
    fn create_recovers_after_restarting_the_device() {
        let mut world = world_with_phone(&[0]);
        world.pipe_available = false;
        world.restart_ok = true;
        let (mut screen, shared) = screen(world);
        // 重启后管道会回来；这里直接让 if 分支通过（restart 成功即继续）。
        shared.lock().unwrap().pipe_available = true;

        let id = screen.create(ScreenFit::standard()).expect("重启后建屏");
        assert_eq!(id, 0);
    }

    #[test]
    fn create_sets_200_percent_dpi_after_going_online() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let id = screen.create(ScreenFit::standard()).expect("建屏成功");
        assert_eq!(id, 0);

        {
            let world = shared.lock().unwrap();
            assert_eq!(world.notified, vec![vec![0]]);
            // 驱动上线时系统已经给了 1080×1900，不用再改分辨率。
            assert!(world.resolutions.is_empty(), "分辨率已经对了就别再下发");
            assert_eq!(world.dpi_calls.len(), 1);
            assert_eq!(world.dpi_calls[0].2, TARGET_DPI_PERCENT);
        }
        assert!(is_online_of(&screen.api, &screen.maps, id));
        assert_eq!(width_of_display(&screen.api, &screen.maps, id), Some(540));
        assert_eq!(lock_maps(&screen.maps).fits.get(&id).cloned(), Some(ScreenFit::standard()));
    }

    #[test]
    fn create_fixes_resolution_when_desktop_picks_another_mode() {
        let mut world = world_with_phone(&[0]);
        // 系统先把这块屏摆成了 1920×1080，得按档位改回来。
        world.online_mode = Some((1920, 1080));
        let (mut screen, shared) = screen(world);
        screen.create(ScreenFit::standard()).expect("建屏成功");

        let world = shared.lock().unwrap();
        assert_eq!(
            world.resolutions,
            vec![(r"\\.\DISPLAY2".to_string(), 1080, 1900, 60.0)]
        );
        assert_eq!(world.dpi_calls.len(), 1);
    }

    #[test]
    fn create_skips_candidate_that_never_comes_online() {
        // 只有 1 号候选能上线，0 号会等超时。
        let (mut screen, shared) = screen(world_with_phone(&[1]));
        // 超时调短，免得单测真的等 2 秒。
        screen.timings.online = Duration::from_millis(5);

        let id = screen.create(ScreenFit::standard()).expect("换候选后成功");
        assert_eq!(id, 1);

        let world = shared.lock().unwrap();
        assert_eq!(world.notified, vec![vec![0], vec![1]]);
        assert!(world.removed.contains(&0), "失败的候选要被撤掉");
    }

    #[test]
    fn create_attaches_arrived_but_disabled_virtual_screen() {
        let mut world = world_with_phone(&[]);
        world.pending_attach = Some(r"\\.\DISPLAY2".to_string());
        let (mut screen, shared) = screen(world);
        screen.timings.online = Duration::from_millis(200);

        let id = screen.create(ScreenFit::standard()).expect("主动扩展桌面后上线");
        assert_eq!(id, 0);
        let world = shared.lock().unwrap();
        assert_eq!(world.attaches, vec![(1080, 1900, 1920, 0)]);
    }

    #[test]
    fn reshape_sends_new_modes_and_keeps_legacy_protocol() {
        let (mut screen, shared) = screen(world_with_phone(&[1]));
        let id = screen.create(ScreenFit::standard()).expect("建屏成功");
        {
            let mut world = shared.lock().unwrap();
            world.notified.clear();
            world.legacy = true;
            world.modes.clear();
        }

        let iphone8 = ScreenFit::from_phone(&snug_core::PhoneSpec::new(750.0, 1334.0, Some(375.0)));
        screen.timings.reshape = Duration::from_millis(50);
        assert!(screen.reshape(id, iphone8.clone()));

        {
            let world = shared.lock().unwrap();
            assert_eq!(world.notified, vec![vec![id]], "legacy 走 ReplaceLegacy 也要把新模式发出去");
        }
        assert_eq!(lock_maps(&screen.maps).fits.get(&id).cloned(), Some(iphone8));
    }

    #[test]
    fn retire_waits_for_offline_and_rescues_stranded_cursor() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let id = screen.create(ScreenFit::standard()).expect("建屏成功");
        {
            let mut world = shared.lock().unwrap();
            // 光标停在手机屏上（1920..2460）。
            world.cursor = Some(Point::new(2000.0, 100.0));
        }

        assert!(screen.retire(id), "撤屏后应报告已下线");
        {
            let world = shared.lock().unwrap();
            assert!(world.removed.contains(&id));
            assert_eq!(world.moves, vec![Point::new(960.0, 540.0)], "光标挪回主屏中央");
        }
        assert!(!is_online_of(&screen.api, &screen.maps, id));
        assert_eq!(lock_maps(&screen.maps).fits.get(&id).cloned(), None);
    }

    #[test]
    fn retire_without_device_name_is_a_no_op() {
        let (mut screen, _) = screen(world_with_phone(&[0]));
        assert!(screen.retire(9), "没建过的编号直接算撤干净");
    }

    #[test]
    fn set_mode_only_touches_what_changed() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        screen.create(ScreenFit::standard()).expect("建屏成功");
        {
            let mut world = shared.lock().unwrap();
            world.resolutions.clear();
            world.dpi_calls.clear();
        }
        let stable = screen
            .api
            .active_monitors()
            .into_iter()
            .find(|m| m.mode.width == 1080)
            .map(|m| m.stable_id)
            .expect("手机屏");

        // 分辨率一样、DPI 也一样 → 什么都不该下发。
        screen.set_mode(
            stable,
            DisplayModeInfo { width: 1080, height: 1900, refresh_rate: 60.0, dpi_scale_percent: TARGET_DPI_PERCENT },
        );
        {
            let world = shared.lock().unwrap();
            assert!(world.resolutions.is_empty());
            assert!(world.dpi_calls.is_empty());
        }

        // 换成 720 逻辑宽 → 1440×2536 物理像素，同时 DPI 已经是 200% 不用再设。
        screen.set_mode(
            stable,
            DisplayModeInfo { width: 1440, height: 2536, refresh_rate: 60.0, dpi_scale_percent: TARGET_DPI_PERCENT },
        );
        {
            let world = shared.lock().unwrap();
            assert_eq!(
                world.resolutions,
                vec![(r"\\.\DISPLAY2".to_string(), 1440, 2536, 60.0)]
            );
        }
    }

    #[test]
    fn cleanup_orphans_only_talks_to_the_driver_when_pipe_exists() {
        let mut offline_world = world_with_phone(&[]);
        offline_world.pipe_available = false;
        let (mut screen, shared) = screen(offline_world);
        screen.cleanup_orphans();
        assert_eq!(shared.lock().unwrap().remove_all_calls, 0);

        let (mut screen, shared) = self::screen(world_with_phone(&[0]));
        screen.cleanup_orphans();
        assert_eq!(shared.lock().unwrap().remove_all_calls, 1);
    }

    // ----- 测试夹具 -----

    fn mode(width: i32, height: i32) -> DisplayModeInfo {
        DisplayModeInfo { width, height, refresh_rate: 60.0, dpi_scale_percent: 100 }
    }

    fn snapshot(
        device_name: &str,
        friendly_name: &str,
        bounds: Rect,
        work_area: Rect,
        is_primary: bool,
        mode: DisplayModeInfo,
    ) -> MonitorSnapshot {
        MonitorSnapshot {
            stable_id: stable_id(device_name, AdapterLuid::default(), 0),
            device_name: device_name.to_string(),
            friendly_name: friendly_name.to_string(),
            device_string: friendly_name.to_string(),
            bounds,
            work_area,
            is_primary,
            mode,
            adapter_id: AdapterLuid::default(),
            source_id: 0,
        }
    }
}
