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
use snug_core::store::OwnedDisplayStore;
use snug_core::{DisplayModeInfo, OtherDisplay, Paths, Point, Rect, ScreenFit};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_BASE_TYPE,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_DEVICE_INFO_TYPE, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPONENT_VIDEO, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPOSITE_VIDEO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_USB_TUNNEL, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DVI,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_D_JPN, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HD15,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDI,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDTVDONGLE, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SVIDEO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EXTERNAL,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_BASE_TYPE, DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY,
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
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE,
    KEY_READ, REG_BINARY, REG_MULTI_SZ, REG_VALUE_TYPE,
};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_IDW, CM_Get_Parent, CM_Locate_DevNodeW, CM_LOCATE_DEVNODE_NORMAL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EDD_GET_DEVICE_INTERFACE_NAME, GetCursorPos, SetCursorPos,
};
use windows::core::{w, BOOL, PCWSTR, PWSTR};

use crate::vdd::{is_driver_pipe_available, VddClient, VddError, VddMode, VddMonitor};
use crate::wide::{contains_ignore_case, from_wide, to_wide};

/// 驱动里这块虚拟屏的名字（C# `PhoneScreenSpec.Name`）。
pub const MONITOR_NAME: &str = "Snug 手机屏";
/// 建屏时依次尝试的连接器编号（C# `PhoneScreenSpec.CandidateIds`）。
pub const CANDIDATE_IDS: [u32; 4] = [0, 1, 2, 3];
/// 手机/平板的目标缩放（C# `TargetDpiPercent` 的默认档位）。
///
/// v1.3 起电脑屏按 `ScreenFit::target_dpi_percent` 可能是 100%，实际下发以那个为准；
/// 这个常量只作为"普通手机/平板"的参考值（几何对拍与界面默认值还在用）。
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
    /// virtual-display-rs monitor.id, recovered from a verified Cherry EDID when present.
    pub virtual_display_id: Option<u32>,
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

/// 候选连接器编号从「已有虚拟屏数量」的偏移开始轮转（C# `6f548dd` 的
/// `CandidateIdsStartingAfter`）。
///
/// 同机跑 C# 版、或上次崩溃留下孤儿屏时，固定的 `[0,1,2,3]` 会和已有屏撞号：
/// `wait_for_online` 按「设备名不在 existing 集合里」判断新屏，撞号会导致认错屏
/// 或白等超时。从偏移开始试就能避开。
pub fn candidates_starting_after(existing_virtual_count: usize) -> Vec<u32> {
    let ids = CANDIDATE_IDS;
    if existing_virtual_count == 0 || ids.len() <= 1 {
        return ids.to_vec();
    }
    let offset = existing_virtual_count % ids.len();
    if offset == 0 {
        return ids.to_vec();
    }
    (0..ids.len()).map(|i| ids[(i + offset) % ids.len()]).collect()
}

/// 一块屏算不算「虚拟屏」（C# `6f548dd` 的计数口径）。
fn is_virtual_monitor(monitor: &MonitorSnapshot) -> bool {
    let device = monitor.device_string.to_ascii_lowercase();
    let friendly = monitor.friendly_name.to_ascii_lowercase();
    device.contains("virtual")
        || friendly.contains("virtudisplay")
        || friendly.contains("virtual")
}

fn monitor_at_point(monitors: &[MonitorSnapshot], point: Point) -> Option<&MonitorSnapshot> {
    let mut matching = monitors.iter().filter(|monitor| {
        // Windows coordinates put the shared edge on the display to its right or below.
        point.x >= monitor.bounds.min_x()
            && point.x < monitor.bounds.max_x()
            && point.y >= monitor.bounds.min_y()
            && point.y < monitor.bounds.max_y()
    });
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

fn preferred_monitor(
    monitors: &[MonitorSnapshot],
    preferred: Option<Point>,
) -> Option<&MonitorSnapshot> {
    preferred
        .and_then(|point| monitor_at_point(monitors, point))
        .or_else(|| monitors.iter().find(|monitor| monitor.is_primary))
        .or_else(|| monitors.first())
}

fn lost_physical_display<'a>(
    before: &'a [MonitorSnapshot],
    current: &[MonitorSnapshot],
) -> Option<&'a MonitorSnapshot> {
    let is_physical = |monitor: &MonitorSnapshot| {
        monitor.virtual_display_id.is_none() && !is_virtual_monitor(monitor)
    };
    before.iter().filter(|monitor| is_physical(monitor)).find(|previous| {
        !current.iter().filter(|monitor| is_physical(monitor)).any(|monitor| {
            let same_output = if !previous.adapter_id.is_zero() && !monitor.adapter_id.is_zero() {
                previous.adapter_id == monitor.adapter_id && previous.source_id == monitor.source_id
            } else {
                previous.device_name.eq_ignore_ascii_case(&monitor.device_name)
            };
            same_output && (!previous.is_primary || monitor.is_primary)
        })
    })
}

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

/// C# `Math.Round(double)` 的默认舍入：银行家舍入（.5 取偶）。
///
/// 与 `.round()`（远离零）不同：`300.5` 在 C# 里是 300，不是 301。
pub fn round_ties_even(value: f64) -> f64 {
    let floor = value.floor();
    let fraction = value - floor;
    // 三个区间照 C# `Math.Round` 的分支写：>0.5 进位、<0.5 舍去、正好 0.5 取偶。
    // 后两个分支都返回 `floor`（fraction<0.5 或 floor 已经是偶数），clippy 会提
    // `if_same_then_else`——这里合并条件而不是留两个同值分支，语义一样且少一层。
    if fraction > 0.5 {
        floor + 1.0
    } else if fraction < 0.5 || (floor as i64) % 2 == 0 {
        floor
    } else {
        floor + 1.0
    }
}

/// 显示器稳定 ID。
///
/// C# 用 `HashCode.Combine(LUID, sourceId)`（进程内随机化）或设备名哈希；
/// 这里改成确定性的 FNV-1a，让同一个编号跨进程也不变（界面持久化更可靠）。
/// 注意：若将来把 display_id 持久化，这些编号与 C# 版不通用，届时要么与 C#
/// 对齐，要么明确「不跨版本通用」。
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

/// 工作区去掉触控底栏。UI 按整屏物理短边缩放到 400 点，底栏按同一尺度留出 60 点。
pub fn window_area(monitor: &MonitorSnapshot, _logical_width: i32) -> Rect {
    let requested_bar = ScreenFit::home_bar_height_of(snug_core::Size::new(
        monitor.mode.width.max(0) as f64,
        monitor.mode.height.max(0) as f64,
    ));
    let work = monitor.work_area;
    let work_height = work.height.max(0.0);
    // 正常保留至少 100 像素给应用；工作区极矮时优先不越界，底栏使用剩余空间。
    let bar_height = requested_bar.clamp(0.0, (work_height - 100.0).max(0.0));
    Rect::new(work.x, work.y, work.width, work_height - bar_height)
}

/// 触控底栏是工作区里窗口区域剩下的那一条，不额外扩张到工作区外。
pub fn home_bar_area(monitor: &MonitorSnapshot, area: Rect) -> Rect {
    let work = monitor.work_area;
    let bottom = work.y + work.height.max(0.0);
    let top = area.max_y().clamp(work.y, bottom);
    Rect::new(work.x, top, work.width, bottom - top)
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
    // C# `SetCursorPos((int)Math.Round(MidX), (int)Math.Round(MidY))`：银行家舍入。
    Some(Point::new(
        round_ties_even(target.bounds.mid_x()),
        round_ties_even(target.bounds.mid_y()),
    ))
}

/// 从在线列表里认出与本次 Notify 对应的虚拟屏。
///
/// 设备名、显示名称和分辨率可能被实体显示器或别的虚拟屏复用；EDID 序列号才是
/// virtual-display-rs 写入的 Monitor.id。身份缺失或不唯一时 fail closed。
pub fn pick_new_monitor(
    existing: &HashSet<String>,
    monitors: &[MonitorSnapshot],
    driver_id: u32,
) -> Option<MonitorSnapshot> {
    let matches: Vec<&MonitorSnapshot> = monitors
        .iter()
        .filter(|m| !existing.contains(&normalized(&m.device_name)))
        .filter(|m| m.virtual_display_id == Some(driver_id))
        .take(2)
        .collect();
    (matches.len() == 1).then(|| matches[0].clone())
}

/// 建屏时下发给驱动的模式表（每个档位 60Hz，物理像素）。
pub fn build_modes(fit: &ScreenFit) -> Vec<VddMode> {
    // C# v1.3.0 `BuildModes`：按 `RegistrationOrder`（从宽到窄）注册，
    // 让 Windows / 驱动先把高分辨率、横屏档位挂上来（见 C# 注释）。
    fit.registration_order()
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
    /// 临时应用模式。平台不支持时由调用方重新核对归属，再走常规模式写入。
    fn set_temporary_resolution(
        &self,
        _device_name: &str,
        _width: i32,
        _height: i32,
        _refresh_rate: f64,
    ) -> bool {
        false
    }
    /// 改 DPI 缩放（C# `SetDpiScalePercent`）。
    fn set_dpi_scale(&self, adapter_id: AdapterLuid, source_id: u32, percent: i32) -> bool;
    /// 把"已经到了但没启用"的虚拟屏扩展成桌面的一块（C# `TryAttachVirtualDisplay`）。
    fn try_attach_virtual_display(
        &self,
        display_id: u32,
        width: i32,
        height: i32,
        x: i32,
        y: i32,
    ) -> Option<String>;
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
    /// 连续向驱动提交建屏或换比例请求时的最短间隔。
    pub modeset_interval: Duration,
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
            modeset_interval: Duration::from_secs(2),
        }
    }
}

fn sleep_for(duration: Duration) {
    if !duration.is_zero() {
        std::thread::sleep(duration);
    }
}

/// 启动清理成功后的 C# 原文案：`已按归属记录清理残留手机屏：#1, #2`。
///
/// 单独抽成纯函数，单测可以直接盯格式（`#{string.Join(", #", ids)}`）。
fn cleanup_log_line(ids: &[u32]) -> String {
    let joined = ids
        .iter()
        .map(|id| format!("#{id}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("已按归属记录清理残留手机屏：{joined}")
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
    last_modeset: Option<Instant>,
    maps: Arc<Mutex<ScreenMaps>>,
    /// 归属记录（C# v1.3.0 `_ownedStore`）：只记本程序建的屏，启动清理只拆这些。
    /// `None` = 不记账（内存版构造函数、单测用）。
    owned: Option<OwnedDisplayStore>,
}

impl<A: DisplayApi, V: VirtualDisplays> PhoneScreen<A, V> {
    /// 测试里直接建一个完整对象；真机走 [`Self::with_maps`]，与查询共享 map。
    #[cfg(test)]
    fn new(api: A, ipc: V, timings: Timings) -> Self {
        Self::with_maps(api, ipc, timings, Arc::new(Mutex::new(ScreenMaps::default())), None)
    }

    fn with_maps(
        api: A,
        ipc: V,
        timings: Timings,
        maps: Arc<Mutex<ScreenMaps>>,
        owned: Option<OwnedDisplayStore>,
    ) -> Self {
        Self { api, ipc, timings, last_modeset: None, maps, owned }
    }

    fn wait_for_modeset_cooldown(&self) {
        if let Some(previous) = self.last_modeset {
            sleep_for(self.timings.modeset_interval.saturating_sub(previous.elapsed()));
        }
    }

    fn check_physical_displays(&self, before: &[MonitorSnapshot]) -> Result<(), String> {
        let current = self.api.active_monitors();
        if let Some(display) = lost_physical_display(before, &current) {
            return Err(format!("实体屏 {} 已离线或原主屏身份发生变化", display.friendly_name));
        }
        Ok(())
    }

    /// 启动时清掉上次崩溃留下的虚拟屏（C# v1.3.0 `CleanupOrphansAsync`）。
    ///
    /// 只拆归属记录里记着的 id：同机可能还装着 C# 版或别的程序建的虚拟屏，
    /// 以前这里 `remove_all` 会把别人的屏一起拆掉。
    fn cleanup_orphans(&mut self) {
        let Some(store) = &self.owned else {
            return;
        };
        let owned_ids = store.load_ids();
        if owned_ids.is_empty() {
            return;
        }
        if !self.api.driver_pipe_available() {
            // C#：保留记录，等管道恢复后再清。
            crate::warn("启动清理时驱动管道暂未就绪，保留归属记录以便管道恢复后清理");
            return;
        }
        // C# 一次 RemoveAsync(全部 id)；驱动拒收就保留整本账（下一次启动再试）。
        match self.ipc.remove(&owned_ids) {
            Ok(()) => {
                sleep_for(self.timings.cleanup_settle);
                let monitors = self.api.active_monitors();
                let mut removed = Vec::new();
                for id in owned_ids {
                    // IPC 接受删除不等于显示器已下线；无可核对编号的虚拟屏也不能猜成已清理。
                    if retirement_is_online(&monitors, id, None) {
                        crate::warn(&format!("残留手机屏 #{id} 尚未确认下线，保留归属记录"));
                    } else {
                        store.release_owned(id);
                        removed.push(id);
                    }
                }
                if !removed.is_empty() {
                    crate::info(&cleanup_log_line(&removed));
                }
            }
            Err(error) => crate::warn(&format!(
                "启动清理残留虚拟屏时异常：{error}（保留归属记录）"
            )),
        }
    }

    /// 建屏（C# `CreateAsync`）。
    fn create(&mut self, fit: ScreenFit) -> Result<u32, PhoneScreenException> {
        self.create_with_name(fit, None)
    }

    /// 建屏并给驱动一个显示名（C# v1.3 `CreateAsync(fit, displayName, ct)`）。
    ///
    /// `name` 只进 `VddMonitor.name`（驱动的"显示器名字"，设置里看到的就是它）；
    /// 不传/全空白时退回 [`MONITOR_NAME`]。`device_names` 里记的仍是 Windows 的
    /// GDI 设备名（`\\.\DISPLAYx`），这两者不能混——后者是改分辨率/查模式的句柄。
    fn create_with_name(
        &mut self,
        fit: ScreenFit,
        name: Option<&str>,
    ) -> Result<u32, PhoneScreenException> {
        self.wait_for_modeset_cooldown();
        let before_monitors = self.api.active_monitors();
        if !self.api.driver_pipe_available() && !self.api.restart_virtual_display_device() {
            return Err(PhoneScreenException::with_detail(
                PhoneScreenErrorKind::CreateFailed,
                format!(
                    "未检测到 virtual-display-rs 驱动管道（{}），请确认驱动已安装且正常运行",
                    VDD_PIPE_HINT
                ),
            ));
        }

        // C#：`string.IsNullOrWhiteSpace(displayName) ? PhoneScreenSpec.Name : displayName`。
        let vdd_name = name
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(MONITOR_NAME);

        let existing: HashSet<String> = before_monitors
            .iter()
            .map(|m| normalized(&m.device_name))
            .collect();
        // C# `6f548dd`：已经有几块虚拟屏，就从候选 ID 的对应偏移开始试，避免和
        // C# 版（或上次崩溃留下的屏）撞同一个 id。撞号会让 wait_for_online 认错屏。
        let existing_virtual_count = before_monitors
            .iter()
            .filter(|m| is_virtual_monitor(m))
            .count();
        let modes = build_modes(&fit);
        let (base_width, base_height) = fit.pixel_size(fit.base_width());

        for candidate in candidates_starting_after(existing_virtual_count) {
            // C# v1.3.0：已经归本程序管的编号不再重复 Notify，否则会把那块屏
            // 的模式覆盖成这次请求的档位（多屏时两块屏会互踩）。
            if lock_maps(&self.maps).fits.contains_key(&candidate)
                || before_monitors.iter().any(|monitor| monitor.virtual_display_id == Some(candidate))
            {
                continue;
            }
            self.wait_for_modeset_cooldown();
            self.check_physical_displays(&before_monitors).map_err(|detail| {
                PhoneScreenException::with_detail(PhoneScreenErrorKind::CreateFailed, detail)
            })?;
            let monitor = VddMonitor {
                id: candidate,
                name: Some(vdd_name.to_string()),
                enabled: true,
                modes: modes.clone(),
            };
            let submitted = self.ipc.notify(&[monitor]);
            self.last_modeset = Some(Instant::now());
            if let Err(error) = submitted {
                // C# `PhoneDisplayService.cs:73`：`Log.Warn($"手机屏（id={candidateId}）IPC Notify 失败：{ex.Message}")`
                // 这一行是排障关键：没有它，「Notify 全被驱动拒了」和「Notify 成功但驱动
                // 没把屏挂上来」在日志里长得一模一样。
                crate::warn(&format!("手机屏（id={candidate}）IPC Notify 失败：{error}"));
                continue;
            }
            let Some(snapshot) = self.wait_for_online(
                &before_monitors,
                &existing,
                candidate,
                base_width,
                base_height,
            )
            else {
                // C# `6f548dd`：超时后**不要** Remove 这个 id。那个 id 可能对应 C# 版
                // 或别的程序建的屏（同机跑两版时很常见），误删会把别人的手机屏拆掉。
                // 只跳过这个候选，换下一个。
                crate::warn(&format!(
                    "手机屏（id={candidate}）创建后 {}s 内未在 Windows 上线，跳过该候选 ID（不移除未知设备）",
                    self.timings.online.as_secs()
                ));
                self.check_physical_displays(&before_monitors).map_err(|detail| {
                    PhoneScreenException::with_detail(PhoneScreenErrorKind::CreateFailed, detail)
                })?;
                continue;
            };
            {
                let mut maps = lock_maps(&self.maps);
                maps.fits.insert(candidate, fit.clone());
                maps.device_names
                    .insert(candidate, snapshot.device_name.clone());
            }
            // C# v1.3.0：确认上线后才记账；没上线就跳过、绝不写进归属记录。
            if let Some(store) = &self.owned {
                store.record_owned(candidate);
            }
            let mode = self.check_physical_displays(&before_monitors).and_then(|()| {
                if !self.set_width(candidate, fit.base_width()) {
                    return Err(format!(
                        "手机屏 #{candidate} 未能应用并确认目标分辨率 {base_width}×{base_height} @ 60 Hz"
                    ));
                }
                self.check_physical_displays(&before_monitors)
            });
            if let Err(detail) = mode {
                let retired = self.retire(candidate);
                if !retired {
                    crate::warn(&format!(
                        "手机屏 #{candidate} 建屏失败后尚未下线，保留归属以便重试"
                    ));
                }
                let error =
                    PhoneScreenException::with_detail(PhoneScreenErrorKind::CreateFailed, detail);
                return Err(error.with_pending_display(candidate));
            }
            return Ok(candidate);
        }

        Err(PhoneScreenException::new(PhoneScreenErrorKind::NeverOnline))
    }

    /// 等新屏上线，必要时主动把它扩展成桌面（C# `WaitForOnlineAsync`）。
    fn wait_for_online(
        &mut self,
        before_monitors: &[MonitorSnapshot],
        existing: &HashSet<String>,
        driver_id: u32,
        base_width: i32,
        base_height: i32,
    ) -> Option<MonitorSnapshot> {
        let start = Instant::now();
        while start.elapsed() < self.timings.online {
            let all = self.api.active_monitors();
            if let Some(found) = pick_new_monitor(existing, &all, driver_id) {
                return Some(found);
            }
            if lost_physical_display(before_monitors, &all).is_some() {
                sleep_for(self.timings.online_poll);
                continue;
            }

            let right_edge = all
                .iter()
                .map(|m| m.bounds.max_x())
                .fold(f64::NEG_INFINITY, f64::max);
            let right_edge = if right_edge.is_finite() { right_edge as i32 } else { 2560 };
            if self
                .api
                .try_attach_virtual_display(driver_id, base_width, base_height, right_edge, 0)
                .is_some()
            {
                self.last_modeset = Some(Instant::now());
                sleep_for(self.timings.attach_settle);
                let attached_monitors = self.api.active_monitors();
                if let Some(attached) = pick_new_monitor(existing, &attached_monitors, driver_id) {
                    return Some(attached);
                }
            }

            sleep_for(self.timings.online_poll);
        }
        None
    }

    /// 换比例（C# `ReshapeAsync`）。
    fn reshape(&mut self, display_id: u32, fit: ScreenFit) -> bool {
        let Some(old_fit) = lock_maps(&self.maps).fits.get(&display_id).cloned() else {
            return false;
        };
        self.wait_for_modeset_cooldown();
        let before_monitors = self.api.active_monitors();
        let Some(before) = bound_monitor_of(&self.api, &self.maps, display_id) else {
            crate::warn(&format!("手机屏 #{display_id} 身份无法确认，未改变分辨率"));
            return false;
        };
        let modes_sent = match self.send_modes(display_id, &fit) {
            Ok(()) => true,
            Err(error) => {
                crate::warn(&format!("手机屏 #{display_id} 更新模式表失败：{error}"));
                false
            }
        };

        let (width, height) = fit.pixel_size(fit.base_width());
        if modes_sent && self.wait_for_mode(display_id, width, height) {
            let applied = self.check_physical_displays(&before_monitors).and_then(|()| {
                let applied = self.apply_fit(display_id, &fit, fit.base_width());
                self.check_physical_displays(&before_monitors).map(|()| applied)
            });
            match applied {
                Ok(true) => {
                    lock_maps(&self.maps).fits.insert(display_id, fit);
                    return true;
                }
                Ok(false) => {}
                Err(detail) => crate::warn(&detail),
            }
        }

        crate::warn(&format!(
            "手机屏 #{display_id} 未确认新模式 {width}×{height} @ 60 Hz，恢复原模式"
        ));
        // Preserve the last confirmed fit. Legacy replacement may give the same verified
        // connector a different GDI name, so both forward and rollback paths rebind by EDID.
        let restored = self.send_modes(display_id, &old_fit).is_ok()
            && (self
                .refresh_binding(display_id)
                .is_some_and(|monitor| resolution_matches(monitor.mode, before.mode))
                || self.wait_for_mode(display_id, before.mode.width, before.mode.height))
            && self.apply_mode(display_id, before.mode);
        if !restored {
            crate::warn(&format!(
                "手机屏 #{display_id} 恢复原模式未成功，保留归属与上次确认的配置以便重试"
            ));
        }
        false
    }

    fn send_modes(&mut self, display_id: u32, fit: &ScreenFit) -> Result<(), VddError> {
        let monitor = VddMonitor {
            id: display_id,
            name: Some(MONITOR_NAME.to_string()),
            enabled: true,
            modes: build_modes(fit),
        };
        // Let the connected client choose the protocol. It may have changed since the last call.
        let submitted = self.ipc.replace_legacy(&[display_id], &[monitor]);
        self.last_modeset = Some(Instant::now());
        submitted
    }

    fn refresh_binding(&self, display_id: u32) -> Option<MonitorSnapshot> {
        if !lock_maps(&self.maps).device_names.contains_key(&display_id) {
            return None;
        }
        let matches: Vec<_> = self
            .api
            .active_monitors()
            .into_iter()
            .filter(|monitor| monitor.virtual_display_id == Some(display_id))
            .take(2)
            .collect();
        if matches.len() != 1 {
            return None;
        }
        let monitor = matches.into_iter().next()?;
        lock_maps(&self.maps)
            .device_names
            .insert(display_id, monitor.device_name.clone());
        Some(monitor)
    }

    fn wait_for_mode(&self, display_id: u32, width: i32, height: i32) -> bool {
        let start = Instant::now();
        loop {
            if let Some(monitor) = self.refresh_binding(display_id)
                && self
                    .api
                    .has_display_mode(&monitor.device_name, width, height)
            {
                return true;
            }
            if start.elapsed() >= self.timings.reshape {
                return false;
            }
            sleep_for(self.timings.reshape_poll);
        }
    }

    /// 撤屏（C# `RetireAsync`）：等它真的下线，顺手把走丢的光标捞回来。
    fn retire(&mut self, display_id: u32) -> bool {
        let device_name = lock_maps(&self.maps).device_names.get(&display_id).cloned();
        let online = retirement_is_online(
            &self.api.active_monitors(), display_id, device_name.as_deref(),
        );
        if !online {
            self.finish_retirement(display_id);
            self.restore_cursor_if_stranded();
            return true;
        }
        let owned = self.owned.as_ref().is_some_and(|store| store.load_ids().contains(&display_id));
        if device_name.is_none() && !owned {
            // 无内存映射或持久归属时，不能因碰巧匹配编号而删除别的程序的屏。
            return false;
        }
        if let Err(error) = self.ipc.remove(&[display_id]) {
            crate::warn(&format!("移除手机屏 #{display_id} 失败：{error}（保留归属与映射）"));
            return false;
        }
        let start = Instant::now();
        loop {
            if !retirement_is_online(
                &self.api.active_monitors(), display_id, device_name.as_deref(),
            ) {
                self.finish_retirement(display_id);
                self.restore_cursor_if_stranded();
                return true;
            }
            if start.elapsed() >= self.timings.offline {
                return false;
            }
            sleep_for(self.timings.offline_poll);
        }
    }

    fn finish_retirement(&self, display_id: u32) {
        let mut maps = lock_maps(&self.maps);
        maps.fits.remove(&display_id);
        maps.device_names.remove(&display_id);
        drop(maps);
        if let Some(store) = &self.owned {
            store.release_owned(display_id);
        }
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

    /// Restore an external physical display from `other_displays()`. Driver connector IDs
    /// belong to a different namespace and must never select a virtual screen here.
    fn set_mode(&mut self, display_id: u32, mode: DisplayModeInfo) {
        let phone_names = lock_maps(&self.maps).phone_names();
        let targets: Vec<_> = self
            .api
            .active_monitors()
            .into_iter()
            .filter(|monitor| {
                monitor.stable_id == display_id
                    && monitor.virtual_display_id.is_none()
                    && !is_virtual_monitor(monitor)
                    && !phone_names.contains(&normalized(&monitor.device_name))
            })
            .take(2)
            .collect();
        let [target] = targets.as_slice() else {
            return;
        };
        let mut actual = target.clone();
        if !resolution_matches(target.mode, mode) {
            if !self.api.set_resolution(
                &target.device_name,
                mode.width,
                mode.height,
                mode.refresh_rate,
            ) {
                crate::warn(&format!("恢复显示器 {} 的分辨率失败", target.device_name));
                return;
            }
            sleep_for(self.timings.resolution_settle);
            let start = Instant::now();
            loop {
                let Some(observed) = self.api.active_monitors().into_iter().find(|monitor| {
                    monitor.stable_id == display_id
                        && normalized(&monitor.device_name) == normalized(&target.device_name)
                        && monitor.adapter_id == target.adapter_id
                        && monitor.source_id == target.source_id
                        && monitor.virtual_display_id.is_none()
                        && !is_virtual_monitor(monitor)
                }) else {
                    return;
                };
                if resolution_matches(observed.mode, mode) {
                    actual = observed;
                    break;
                }
                if start.elapsed() >= self.timings.reshape {
                    crate::warn(&format!(
                        "恢复显示器 {} 后的实际分辨率未达到目标",
                        target.device_name
                    ));
                    return;
                }
                sleep_for(self.timings.reshape_poll);
            }
        }
        if actual.mode.dpi_scale_percent != mode.dpi_scale_percent {
            self.api
                .set_dpi_scale(actual.adapter_id, actual.source_id, mode.dpi_scale_percent);
        }
    }

    /// 确认目标分辨率和刷新率，再应用 ScreenFit 指定的 DPI。
    fn set_width(&mut self, display_id: u32, width: i32) -> bool {
        let fit = {
            let maps = lock_maps(&self.maps);
            let Some(fit) = maps.fits.get(&display_id).cloned() else {
                return false;
            };
            fit
        };
        self.apply_fit(display_id, &fit, width)
    }

    fn apply_fit(&mut self, display_id: u32, fit: &ScreenFit, width: i32) -> bool {
        let (pixel_width, pixel_height) = fit.pixel_size(width);
        self.apply_mode(
            display_id,
            DisplayModeInfo {
                width: pixel_width,
                height: pixel_height,
                refresh_rate: 60.0,
                dpi_scale_percent: fit.target_dpi_percent(width),
            },
        )
    }

    fn apply_mode(&mut self, display_id: u32, mode: DisplayModeInfo) -> bool {
        let Some(current) = bound_monitor_of(&self.api, &self.maps, display_id) else {
            crate::warn(&format!(
                "手机屏 #{display_id} 的显示器身份已失效，未修改模式"
            ));
            return false;
        };
        let needs_resolution = !resolution_matches(current.mode, mode);
        let mut after = current;
        if needs_resolution {
            let started = std::time::Instant::now();
            let applied = self.api.set_temporary_resolution(
                &after.device_name,
                mode.width,
                mode.height,
                mode.refresh_rate,
            );
            self.last_modeset = Some(Instant::now());
            if !applied {
                let Some(current) = bound_monitor_of(&self.api, &self.maps, display_id) else {
                    return false;
                };
                let applied = self.api.set_resolution(
                    &current.device_name,
                    mode.width,
                    mode.height,
                    mode.refresh_rate,
                );
                self.last_modeset = Some(Instant::now());
                if !applied {
                    return false;
                }
            }
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            if ms >= 200.0 {
                crate::info(&format!(
                    "屏幕：换分辨率到 {}x{} 用了 {ms:.0}ms（{}）",
                    mode.width, mode.height, after.device_name
                ));
            }
            sleep_for(self.timings.resolution_settle);
            let start = Instant::now();
            loop {
                let Some(observed) = bound_monitor_of(&self.api, &self.maps, display_id) else {
                    return false;
                };
                if resolution_matches(observed.mode, mode) {
                    after = observed;
                    break;
                }
                if start.elapsed() >= self.timings.reshape {
                    crate::warn(&format!(
                        "手机屏 #{display_id} 的实际模式仍为 {}×{} @ {} Hz，目标为 {}×{} @ {} Hz",
                        observed.mode.width,
                        observed.mode.height,
                        observed.mode.refresh_rate,
                        mode.width,
                        mode.height,
                        mode.refresh_rate
                    ));
                    return false;
                }
                sleep_for(self.timings.reshape_poll);
            }
        }
        {
            let target_dpi = mode.dpi_scale_percent;
            if after.mode.dpi_scale_percent != target_dpi {
                let started = std::time::Instant::now();
                self.api
                    .set_dpi_scale(after.adapter_id, after.source_id, target_dpi);
                let ms = started.elapsed().as_secs_f64() * 1000.0;
                if ms >= 200.0 {
                    crate::info(&format!("屏幕：换 DPI 到 {target_dpi}% 用了 {ms:.0}ms"));
                }
            }
        }
        true
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

fn bound_monitor_of<A: DisplayApi>(
    api: &A,
    maps: &Mutex<ScreenMaps>,
    display_id: u32,
) -> Option<MonitorSnapshot> {
    let device_name = lock_maps(maps).device_names.get(&display_id).cloned()?;
    let matches: Vec<_> = api
        .active_monitors()
        .into_iter()
        .filter(|monitor| monitor.virtual_display_id == Some(display_id))
        .take(2)
        .collect();
    let [monitor] = matches.as_slice() else {
        return None;
    };
    (normalized(&monitor.device_name) == normalized(&device_name)).then(|| monitor.clone())
}

fn resolution_matches(actual: DisplayModeInfo, target: DisplayModeInfo) -> bool {
    actual.width == target.width
        && actual.height == target.height
        && actual.refresh_rate.is_finite()
        && target.refresh_rate.is_finite()
        && (actual.refresh_rate - target.refresh_rate).abs() < 0.5
}

fn is_online_of<A: DisplayApi>(api: &A, maps: &Mutex<ScreenMaps>, display_id: u32) -> bool {
    let device_name = lock_maps(maps).device_names.get(&display_id).cloned();
    retirement_is_online(&api.active_monitors(), display_id, device_name.as_deref())
}

fn retirement_is_online(monitors: &[MonitorSnapshot], display_id: u32, device_name: Option<&str>) -> bool {
    monitors.iter().any(|monitor| {
        match monitor.virtual_display_id {
            Some(id) => id == display_id,
            None => match device_name {
                Some(name) => normalized(&monitor.device_name) == normalized(name),
                None => is_virtual_monitor(monitor),
            },
        }
    })
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
    let fit = lock_maps(maps).fits.get(&display_id).cloned();
    let logical_width = width_for_mode(monitor.mode.width, fit.as_ref());
    Some(window_area(&monitor, logical_width))
}

fn home_bar_area_of_display<A: DisplayApi>(
    api: &A,
    maps: &Mutex<ScreenMaps>,
    display_id: u32,
) -> Option<Rect> {
    Some(phone_layout_of_display(api, maps, display_id)?.1)
}

fn phone_layout_of_display<A: DisplayApi>(
    api: &A,
    maps: &Mutex<ScreenMaps>,
    display_id: u32,
) -> Option<(Rect, Rect)> {
    let monitor = find_monitor_of(api, maps, display_id)?;
    let fit = lock_maps(maps).fits.get(&display_id).cloned();
    let area = window_area(&monitor, width_for_mode(monitor.mode.width, fit.as_ref()));
    Some((monitor.bounds, home_bar_area(&monitor, area)))
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
    virtual_display_id: Option<u32>,
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

/// 查当前激活的 CCD 路径；拓扑在两次调用之间变化时重试（C# 不重试，见
/// `docs/display-parity.md` 的 C1）。
fn query_active_paths() -> Option<Vec<DISPLAYCONFIG_PATH_INFO>> {
    // QueryDisplayConfig：183 = ERROR_INSUFFICIENT_BUFFER，87 = ERROR_INVALID_PARAMETER。
    const RETRYABLE: [u32; 2] = [183, 87];
    const RETRIES: u32 = 2;

    for attempt in 0..=RETRIES {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        if unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        }
        .0
            != 0
        {
            return None;
        }

        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let status = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        if status.0 == 0 {
            paths.truncate(path_count as usize);
            return Some(paths);
        }
        if !RETRYABLE.contains(&status.0) || attempt == RETRIES {
            return None;
        }
        crate::warn(&format!(
            "QueryDisplayConfig 返回 {}（拓扑可能刚变化），重取缓冲区重试 {}/{}",
            status.0,
            attempt + 1,
            RETRIES
        ));
    }
    None
}

const CHERRY_MONITOR_INTERFACE_PREFIX: &str = r"\\?\DISPLAY#CHY0000#";
const CHERRY_MONITOR_INTERFACE_SUFFIX: &str = "#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
const VIRTUAL_DISPLAY_ADAPTER_HARDWARE_ID: &str = r"ROOT\VIRTUALDISPLAYDRIVER";

/// 将 OS 给出的 Cherry monitor interface path 转成 Enum\DISPLAY 下的实例路径。
fn cherry_monitor_registry_instance(path: &str) -> Option<String> {
    let prefix = path.get(..CHERRY_MONITOR_INTERFACE_PREFIX.len())?;
    if !prefix.eq_ignore_ascii_case(CHERRY_MONITOR_INTERFACE_PREFIX) {
        return None;
    }
    let path = path.get(CHERRY_MONITOR_INTERFACE_PREFIX.len()..)?;
    let suffix_start = path.len().checked_sub(CHERRY_MONITOR_INTERFACE_SUFFIX.len())?;
    if !path.get(suffix_start..)?.eq_ignore_ascii_case(CHERRY_MONITOR_INTERFACE_SUFFIX) {
        return None;
    }
    let instance = &path[..suffix_start];
    if instance.is_empty() || instance.contains(['#', '\\', '/']) {
        return None;
    }
    Some(format!(r"DISPLAY\CHY0000\{instance}"))
}

/// EDID 中的 serial 是 virtual-display-rs 写入的 `Monitor.id`。
fn virtual_display_id_from_edid(edid: &[u8]) -> Option<u32> {
    const EDID_HEADER: [u8; 8] = [0, 255, 255, 255, 255, 255, 255, 0];
    if edid.len() < 128
        || !edid.starts_with(&EDID_HEADER)
        || edid[8..12] != [0x0d, 0x19, 0, 0]
        || edid[..128].iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0
    {
        return None;
    }
    Some(u32::from_le_bytes(edid[12..16].try_into().ok()?))
}

fn registry_value(path: &str, value_name: windows::core::PCWSTR) -> Option<(REG_VALUE_TYPE, Vec<u8>)> {
    unsafe {
        let mut key = HKEY(std::ptr::null_mut());
        let wide_path = to_wide(path);
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(wide_path.as_ptr()), None, KEY_READ, &mut key)
            .0
            != 0
        {
            return None;
        }
        let mut data = vec![0u8; 4096];
        let mut size = data.len() as u32;
        let mut kind = REG_VALUE_TYPE(0);
        let result = RegQueryValueExW(
            key,
            value_name,
            None,
            Some(&mut kind),
            Some(data.as_mut_ptr()),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        if result.0 != 0 || size as usize > data.len() {
            return None;
        }
        data.truncate(size as usize);
        Some((kind, data))
    }
}

fn virtual_display_id_from_device_path(path: &str) -> Option<u32> {
    let instance = cherry_monitor_registry_instance(path)?;
    virtual_display_id_from_instance_id(&instance)
}

fn virtual_display_id_from_instance_id(instance: &str) -> Option<u32> {
    let wide_instance = to_wide(instance);
    let mut node = 0u32;
    if unsafe {
        CM_Locate_DevNodeW(&mut node, PCWSTR(wide_instance.as_ptr()), CM_LOCATE_DEVNODE_NORMAL)
    }
    .0
        != 0
    {
        return None;
    }

    let mut parent = 0u32;
    if unsafe { CM_Get_Parent(&mut parent, node, 0) }.0 != 0 {
        return None;
    }
    let mut parent_id = [0u16; 512];
    if unsafe { CM_Get_Device_IDW(parent, &mut parent_id, 0) }.0 != 0 {
        return None;
    }
    let parent_id = from_wide(&parent_id);
    let hardware_key = format!(r"SYSTEM\CurrentControlSet\Enum\{parent_id}");
    let (kind, hardware_ids) = registry_value(&hardware_key, w!("HardwareID"))?;
    if kind != REG_MULTI_SZ
        || !multi_sz(&hardware_ids)
            .iter()
            .any(|id| id.eq_ignore_ascii_case(VIRTUAL_DISPLAY_ADAPTER_HARDWARE_ID))
    {
        return None;
    }

    let edid_key = format!(r"SYSTEM\CurrentControlSet\Enum\{instance}\Device Parameters");
    let (kind, edid) = registry_value(&edid_key, w!("EDID"))?;
    (kind == REG_BINARY)
        .then(|| virtual_display_id_from_edid(&edid))
        .flatten()
}

/// 真机显示子系统（C# `DisplayConfigService`）。
#[derive(Debug, Default, Clone, Copy)]
pub struct WinDisplay;

#[derive(PartialEq)]
struct MonitorGeometry {
    handle: usize,
    device_name: String,
    bounds: Rect,
    work_area: Rect,
    is_primary: bool,
}

/// The outputs verified by CCD, tied to the GDI geometry active during that check.
pub struct PhysicalMonitors {
    monitors: Vec<MonitorSnapshot>,
    geometry: Vec<MonitorGeometry>,
}

impl PhysicalMonitors {
    pub fn contains_physical_point(&self, point: Point) -> bool {
        monitor_at_point(&self.monitors, point).is_some()
    }

    pub fn preferred_work_area(&self, point: Point) -> Option<Rect> {
        preferred_monitor(&self.monitors, Some(point)).map(|monitor| monitor.work_area)
    }

    /// Fast check for monitor movement, removal, or taskbar changes before placing a window.
    pub fn geometry_is_current(&self) -> bool {
        self.geometry == monitor_geometry()
    }
}

fn is_direct_physical_output(technology: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY) -> bool {
    matches!(
        technology,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HD15
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SVIDEO
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPOSITE_VIDEO
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPONENT_VIDEO
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DVI
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_D_JPN
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDI
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EXTERNAL
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDTVDONGLE
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_USB_TUNNEL
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
    )
}

fn physical_target(path: &DISPLAYCONFIG_PATH_INFO, monitor_path: &str) -> bool {
    if !path.targetInfo.targetAvailable.as_bool()
        || monitor_path.is_empty()
        || cherry_monitor_registry_instance(monitor_path).is_some()
        || !is_direct_physical_output(path.targetInfo.outputTechnology)
    {
        return false;
    }
    // The path can report a wireless sink's own connector instead of Miracast.
    let mut base = DISPLAYCONFIG_TARGET_BASE_TYPE {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_BASE_TYPE,
            size: std::mem::size_of::<DISPLAYCONFIG_TARGET_BASE_TYPE>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        ..Default::default()
    };
    (unsafe { DisplayConfigGetDeviceInfo(&mut base.header) } == 0)
        && is_direct_physical_output(base.baseOutputTechnology)
}

impl WinDisplay {
    pub const fn new() -> Self {
        Self
    }

    /// 优先使用指定位置所在的实体屏，其次是实体主屏或第一块已确认的实体屏。
    pub fn preferred_physical_monitor(&self, preferred: Option<Point>) -> Option<MonitorSnapshot> {
        preferred_monitor(&self.desktop_monitors(true), preferred).cloned()
    }

    pub fn verified_physical_monitors(&self) -> Option<PhysicalMonitors> {
        let geometry = monitor_geometry();
        let monitors = self.desktop_monitors(true);
        (!monitors.is_empty() && geometry == monitor_geometry())
            .then_some(PhysicalMonitors { monitors, geometry })
    }

    /// 返回物理像素位置所在的、已确认属于实体输出的工作区。
    pub fn physical_monitor_at(&self, point: Point) -> Option<MonitorSnapshot> {
        monitor_at_point(&self.desktop_monitors(true), point).cloned()
    }

    fn ccd_sources(&self, physical_only: bool) -> HashMap<String, CcdSource> {
        let mut map = HashMap::new();
        // 对 C# 的有意偏离：C# 只查一次；如果 GetDisplayConfigBufferSizes 与
        // QueryDisplayConfig 之间正好热插拔，整次采样会失败——所有屏的
        // adapter/source 归零、DPI 掉回 100。这里遇 183/87 重取尺寸重试两次，
        // 每次重试记一条 warn，仍失败才返回空表（见 docs/display-parity.md）。
        let Some(paths) = query_active_paths() else {
            return map;
        };
        let mut source_path_counts = HashMap::new();
        for path in &paths {
            *source_path_counts
                .entry((
                    path.sourceInfo.adapterId.LowPart,
                    path.sourceInfo.adapterId.HighPart,
                    path.sourceInfo.id,
                ))
                .or_insert(0usize) += 1;
        }

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
            let source_is_unique = source_path_counts
                .get(&(
                    path.sourceInfo.adapterId.LowPart,
                    path.sourceInfo.adapterId.HighPart,
                    path.sourceInfo.id,
                ))
                .copied()
                == Some(1);
            let (friendly, virtual_display_id) =
                if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } == 0 {
                    let monitor_path = from_wide(&target.monitorDevicePath);
                    // A mirrored source could send the same pixels to a virtual target.
                    if physical_only && (!source_is_unique || !physical_target(&path, &monitor_path)) {
                        continue;
                    }
                    (
                        from_wide(&target.monitorFriendlyDeviceName),
                        virtual_display_id_from_device_path(&monitor_path),
                    )
                } else if physical_only {
                    continue;
                } else {
                    (gdi_name.clone(), None)
                };
            let virtual_display_id = if source_is_unique { virtual_display_id } else { None };

            let adapter_id =
                AdapterLuid::new(path.sourceInfo.adapterId.LowPart, path.sourceInfo.adapterId.HighPart);
            map.entry(normalized(&gdi_name)).or_insert(CcdSource {
                adapter_id,
                source_id: path.sourceInfo.id,
                friendly_name: friendly,
                virtual_display_id,
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

fn change_gdi_resolution(
    device_name: &str,
    width: i32,
    height: i32,
    refresh_rate: f64,
    flags: CDS_TYPE,
) -> bool {
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
        // C# `(uint)Math.Round(refreshRate)`：银行家舍入。
        dev_mode.dmDisplayFrequency = round_ties_even(refresh_rate) as u32;
        fields = fields | DM_DISPLAYFREQUENCY;
    }
    dev_mode.dmFields = fields;

    unsafe {
        let result = ChangeDisplaySettingsExW(
            name,
            Some(&dev_mode as *const DEVMODEW),
            None,
            flags,
            None,
        );
        if result == DISP_CHANGE_SUCCESSFUL {
            true
        } else {
            // C# `Log.Warn($"ChangeDisplaySettingsEx({deviceName}, {w}x{h}) 返回 {res}")`
            crate::warn(&format!(
                "ChangeDisplaySettingsEx({device_name}, {width}x{height}) 返回 {}",
                result.0
            ));
            false
        }
    }
}

impl WinDisplay {
    fn desktop_monitors(&self, physical_only: bool) -> Vec<MonitorSnapshot> {
        let geometry = monitor_geometry();
        let ccd = self.ccd_sources(physical_only);
        let device_strings = self.device_strings();
        let mut monitors = Vec::with_capacity(geometry.len());

        for MonitorGeometry { device_name, bounds, work_area, is_primary, .. } in geometry {
            let source = ccd.get(&normalized(&device_name));
            if physical_only && (source.is_none() || work_area.width <= 0.0 || work_area.height <= 0.0) {
                continue;
            }

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
            let virtual_display_id = source.and_then(|source| source.virtual_display_id);

            let monitor = MonitorSnapshot {
                stable_id: stable_id(&device_name, adapter_id, source_id),
                device_name,
                friendly_name,
                device_string,
                virtual_display_id,
                bounds,
                work_area,
                is_primary,
                mode: DisplayModeInfo { width, height, refresh_rate, dpi_scale_percent },
                adapter_id,
                source_id,
            };
            if physical_only
                && (monitor.virtual_display_id.is_some()
                    || monitor.device_string.is_empty()
                    || contains_ignore_case(&monitor.device_string, "GameViewer")
                    || contains_ignore_case(&monitor.friendly_name, "GameViewer")
                    || is_virtual_monitor(&monitor))
            {
                continue;
            }
            monitors.push(monitor);
        }
        monitors
    }
}

impl DisplayApi for WinDisplay {
    fn active_monitors(&self) -> Vec<MonitorSnapshot> {
        self.desktop_monitors(false)
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
        change_gdi_resolution(device_name, width, height, refresh_rate, CDS_UPDATEREGISTRY)
    }

    fn set_temporary_resolution(
        &self,
        device_name: &str,
        width: i32,
        height: i32,
        refresh_rate: f64,
    ) -> bool {
        change_gdi_resolution(device_name, width, height, refresh_rate, CDS_TYPE(0))
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
        let status = unsafe { DisplayConfigSetDeviceInfo(&set.header) };
        if status != 0 {
            // C# `Log.Warn($"设置 DPI 缩放 {targetPercent}% 失败（status={status}）")`
            crate::warn(&format!("设置 DPI 缩放 {percent}% 失败（status={status}）"));
            return false;
        }
        true
    }

    fn try_attach_virtual_display(
        &self,
        display_id: u32,
        width: i32,
        height: i32,
        x: i32,
        y: i32,
    ) -> Option<String> {
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
            // 顶层 DeviceID 是硬件 ID，不是 ROOT\DISPLAY\000x 这样的 PnP 实例 ID。
            let adapter_hardware_id = from_wide(&device.DeviceID);
            if !adapter_hardware_id.eq_ignore_ascii_case(VIRTUAL_DISPLAY_ADAPTER_HARDWARE_ID)
                || !contains_ignore_case(&device_string, "Virtual Display")
            {
                continue;
            }
            if device.StateFlags.0 & DISPLAY_DEVICE_ACTIVE.0 != 0 {
                continue;
            }

            // 只有本驱动适配器、且 EDID serial 与请求的 connector ID 一致时才扩展。
            let wide_name = to_wide(&device_name);
            let mut has_requested_display = false;
            let mut child_index = 0u32;
            loop {
                let mut attached = DISPLAY_DEVICEW::default();
                attached.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
                if !unsafe {
                    EnumDisplayDevicesW(
                        PCWSTR(wide_name.as_ptr()),
                        child_index,
                        &mut attached,
                        EDD_GET_DEVICE_INTERFACE_NAME,
                    )
                }
                .as_bool()
                {
                    break;
                }
                child_index += 1;
                if virtual_display_id_from_device_path(&from_wide(&attached.DeviceID)) == Some(display_id) {
                    has_requested_display = true;
                    break;
                }
            }
            if !has_requested_display {
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

            // C2：第一次调用成功、apply 失败时会在注册表里留下 CDS_NORESET 的
            // 挂起模式；若随后的 SetDisplayConfig 也失败，挂起变更会留到下次
            // （C# 同样，见 docs/display-parity.md）。
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
        if !crate::is_elevated() {
            crate::warn("驱动管道未就绪，可从设置重试连接或修复驱动");
            return false;
        }
        if self.active_monitors().iter().any(is_virtual_monitor) {
            crate::warn("检测到虚拟显示器在线，跳过适配器重启");
            return false;
        }
        let Some(instance_id) = find_virtual_display_instance_id() else {
            return false;
        };
        let executable = match crate::system_paths::system_tool("pnputil.exe") {
            Ok(path) => path,
            Err(_) => return false,
        };
        let status = std::process::Command::new(executable)
            .args(["/restart-device", &instance_id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        match status {
            Ok(status) if status.success() => {}
            Ok(status) => {
                crate::warn(&format!("重启虚拟显示器设备失败：{status}"));
                return false;
            }
            Err(error) => {
                crate::warn(&format!("无法运行虚拟显示器设备重启：{error}"));
                return false;
            }
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

fn monitor_geometry() -> Vec<MonitorGeometry> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut handles as *mut Vec<HMONITOR> as isize),
        );
    }
    handles.into_iter().filter_map(|handle| {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        unsafe { GetMonitorInfoW(handle, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) }
            .as_bool().then(|| MonitorGeometry {
                handle: handle.0 as usize,
                device_name: from_wide(&info.szDevice),
                bounds: rect_of(info.monitorInfo.rcMonitor),
                work_area: rect_of(info.monitorInfo.rcWork),
                is_primary: info.monitorInfo.dwFlags & 1 != 0,
            })
    }).collect()
}

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
///
/// 对外也用它回答"驱动装没装"：**管道起没起来**和**驱动装没装**是两件事——
/// 登录早期用户态服务还没把管道拉起来，这时候不该把驱动重装一遍（见
/// `snug-app/src/driver.rs` 的 `ensure_installed`）。
pub fn virtual_display_instance_id() -> Option<String> {
    find_virtual_display_instance_id()
}

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
    /// 内存版（不读写归属记录），单测与不需要清理的场景用。
    pub fn new() -> Self {
        Self::build(
            WinDisplay::new(),
            VddClient::default(),
            Timings::default(),
            None,
        )
    }

    /// 真机版：带上 `owned-displays.json` 归属记录（C# v1.3.0 的 `paths` 参数）。
    ///
    /// 启动清理只会拆这里记着的屏，不会动 C# 版或别的程序建的虚拟屏。
    pub fn with_paths(paths: &Paths) -> Self {
        Self::build(
            WinDisplay::new(),
            VddClient::default(),
            Timings::default(),
            Some(OwnedDisplayStore::new(paths.owned_displays_file())),
        )
    }

    pub fn with_timings(timings: Timings) -> Self {
        Self::build(WinDisplay::new(), VddClient::default(), timings, None)
    }

    pub fn with_parts(api: WinDisplay, ipc: VddClient, timings: Timings) -> Self {
        Self::build(api, ipc, timings, None)
    }

    fn build(
        api: WinDisplay,
        ipc: VddClient,
        timings: Timings,
        owned: Option<OwnedDisplayStore>,
    ) -> Self {
        let maps = Arc::new(Mutex::new(ScreenMaps::default()));
        let screen = PhoneScreen::with_maps(api, ipc, timings, maps.clone(), owned);
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

    /// 用同一次显示器快照返回整屏和底栏，供界面布局与触控区域共用。
    pub fn phone_layout_of(&self, display_id: u32) -> Option<(Rect, Rect)> {
        phone_layout_of_display(&self.api, &self.maps, display_id)
    }

    pub fn projection_monitor(&self, display_id: u32) -> Option<MonitorSnapshot> {
        bound_monitor_of(&self.api, &self.maps, display_id)
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

    /// C# v1.3 的带名字建屏：名字写进 VDD 的 `VddMonitor.name`（legacy 协议没有
    /// 这个字段，驱动会退回默认名——见 `vdd.rs` 的 `VddLegacyMonitor`）。
    fn create_named(&self, fit: ScreenFit, name: &str) -> Result<u32, PhoneScreenException> {
        self.with_ops(|screen| screen.create_with_name(fit, Some(name)))
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
    fn candidate_ids_rotate_past_existing_virtual_screens() {
        // C# `6f548dd`：已有虚拟屏越多，越要往后跳，避免撞号。
        assert_eq!(candidates_starting_after(0), vec![0, 1, 2, 3]);
        assert_eq!(candidates_starting_after(1), vec![1, 2, 3, 0]);
        assert_eq!(candidates_starting_after(2), vec![2, 3, 0, 1]);
        assert_eq!(candidates_starting_after(3), vec![3, 0, 1, 2]);
        // 正好轮满一圈就回到原顺序。
        assert_eq!(candidates_starting_after(4), vec![0, 1, 2, 3]);
        assert_eq!(candidates_starting_after(9), vec![1, 2, 3, 0]);
    }

    #[test]
    fn rounding_matches_dotnet_math_round() {
        // C# Math.Round 默认是银行家舍入（.5 取偶），不是远离零。
        assert_eq!(round_ties_even(300.5), 300.0);
        assert_eq!(round_ties_even(301.5), 302.0);
        assert_eq!(round_ties_even(2.4), 2.0);
        assert_eq!(round_ties_even(2.6), 3.0);
        assert_eq!(round_ties_even(-0.5), 0.0);
        assert_eq!(round_ties_even(-1.5), -2.0);
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
        // 1080 短边 / 400 触控设计点 × 60 点底栏 = 162 物理像素，与系统 DPI 无关。
        let monitor = snapshot(
            r"\\.\DISPLAY2",
            "Virtual Display",
            Rect::new(1920.0, 0.0, 1080.0, 1868.0),
            Rect::new(1920.0, 0.0, 1080.0, 1868.0),
            false,
            DisplayModeInfo { width: 1080, height: 1900, refresh_rate: 60.0, dpi_scale_percent: 200 },
        );
        let area = window_area(&monitor, 540);
        assert_eq!(area, Rect::new(1920.0, 0.0, 1080.0, 1706.0));
        assert_eq!(home_bar_area(&monitor, area), Rect::new(1920.0, 1706.0, 1080.0, 162.0));
    }

    #[test]
    fn window_area_keeps_100_pixels_without_extending_bar_past_short_work_area() {
        let monitor = snapshot(
            r"\\.\DISPLAY3",
            "Virtual Display",
            Rect::new(0.0, 0.0, 540.0, 120.0),
            Rect::new(0.0, 0.0, 540.0, 120.0),
            false,
            DisplayModeInfo { width: 540, height: 960, refresh_rate: 60.0, dpi_scale_percent: 100 },
        );
        let area = window_area(&monitor, 1000);
        assert_eq!(area.height, 100.0);
        let bar = home_bar_area(&monitor, area);
        assert_eq!(bar.height, 20.0, "工作区不足时压缩底栏，不越过工作区");
        assert_eq!(bar.y, 100.0);
        assert_eq!(bar.max_y(), monitor.work_area.max_y());
    }

    #[test]
    fn touch_bar_uses_short_side_in_both_orientations_independently_of_native_dpi() {
        for (width, height, dpi, expected) in [
            (1080, 1920, 200, 162.0),
            (1920, 1080, 200, 162.0),
            (525, 950, 200, 79.0),
            (1920, 1080, 100, 162.0),
        ] {
            let bounds = Rect::new(-500.0, 120.0, width as f64, height as f64);
            let monitor = snapshot(
                r"\\.\DISPLAY3", "Virtual Display", bounds, bounds, false,
                DisplayModeInfo { width, height, refresh_rate: 60.0, dpi_scale_percent: dpi },
            );
            let area = window_area(&monitor, width / 2);
            let bar = home_bar_area(&monitor, area);
            assert_eq!(bar.height, expected, "{width}x{height} at {dpi}%");
            assert_eq!(area.max_y(), bar.min_y());
            assert_eq!(bar.max_y(), bounds.max_y());
            assert_eq!(area.height + bar.height, bounds.height);
        }
    }

    #[test]
    fn touch_bar_partition_stays_inside_tiny_and_empty_work_areas() {
        for height in [0.0, 40.0, 100.0, 120.0, 159.0, 160.0, 300.0] {
            let work = Rect::new(-120.0, -80.0, 540.0, height);
            let monitor = snapshot(
                r"\\.\DISPLAY3", "Virtual Display", work, work, false, mode(540, 960),
            );
            let area = window_area(&monitor, 540);
            let bar = home_bar_area(&monitor, area);
            assert!(area.height >= 0.0 && bar.height >= 0.0);
            assert!(area.height >= 100.0_f64.min(height));
            assert_eq!(area.max_y(), bar.min_y());
            assert_eq!(bar.max_y(), work.max_y());
            assert_eq!(area.height + bar.height, height);
        }
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
    fn physical_outputs_require_a_known_direct_connector() {
        use windows::Win32::Devices::Display::{
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_VIRTUAL,
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_MIRACAST,
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_OTHER,
        };
        assert!(is_direct_physical_output(DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL));
        assert!(is_direct_physical_output(DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI));
        assert!(is_direct_physical_output(DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_USB_TUNNEL));
        for technology in [
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_VIRTUAL,
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED,
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_MIRACAST,
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_OTHER,
            DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY(99),
        ] {
            assert!(!is_direct_physical_output(technology));
        }
    }

    #[test]
    fn a_physical_monitor_at_the_preferred_point_takes_precedence() {
        let main = snapshot(
            r"\\.\DISPLAY1",
            "Main",
            Rect::new(0.0, 0.0, 1920.0, 1080.0),
            Rect::new(0.0, 0.0, 1920.0, 1040.0),
            true,
            mode(1920, 1080),
        );
        let secondary = snapshot(
            r"\\.\DISPLAY2",
            "Secondary",
            Rect::new(-1920.0, 0.0, 1920.0, 1080.0),
            Rect::new(-1920.0, 0.0, 1920.0, 1040.0),
            false,
            mode(1920, 1080),
        );
        let monitors = [secondary, main];
        assert_eq!(preferred_monitor(&monitors, Some(Point::new(-100.0, 10.0))), monitors.first());
        assert_eq!(monitor_at_point(&monitors, Point::new(0.0, 10.0)), monitors.get(1));
        // A position on an excluded display deliberately falls back to the physical primary.
        assert!(monitor_at_point(&monitors, Point::new(2000.0, 10.0)).is_none());
        assert_eq!(preferred_monitor(&monitors, Some(Point::new(2000.0, 10.0))), monitors.get(1));
        assert!(preferred_monitor(&[], None).is_none());
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

        // 奇数宽的屏：中点 960.5 按银行家舍入落到 960（C# Math.Round）。
        let odd = vec![snapshot(
            r"\\.\DISPLAY1",
            "DELL",
            Rect::new(0.0, 0.0, 1921.0, 1080.0),
            Rect::new(0.0, 0.0, 1921.0, 1040.0),
            true,
            mode(1921, 1080),
        )];
        assert_eq!(
            cursor_rescue_point(Point::new(5000.0, 0.0), &odd),
            Some(Point::new(960.0, 540.0))
        );
    }

    #[test]
    fn pick_new_monitor_requires_unique_matching_driver_identity() {
        let existing: HashSet<String> = [normalized(r"\\.\DISPLAY1")].into_iter().collect();
        let stranger = snapshot(r"\\.\DISPLAY2", "Some TV", Rect::ZERO, Rect::ZERO, false, mode(1080, 1900));
        let mut phone = snapshot(r"\\.\DISPLAY3", "Virtual Display", Rect::ZERO, Rect::ZERO, false, mode(1920, 1080));
        phone.virtual_display_id = Some(2);

        let picked = pick_new_monitor(&existing, &[stranger.clone(), phone.clone()], 2)
            .expect("应按 EDID connector ID 识别虚拟屏，而不是同分辨率实体屏");
        assert_eq!(picked.device_name, phone.device_name);
        assert!(pick_new_monitor(&existing, &[stranger.clone()], 2).is_none());
        assert!(pick_new_monitor(&existing, &[phone.clone()], 1).is_none());
        let mut duplicate = phone.clone();
        duplicate.device_name = r"\\.\DISPLAY4".into();
        assert!(pick_new_monitor(&existing, &[phone, duplicate], 2).is_none());
        assert!(pick_new_monitor(&existing, &[], 2).is_none());
    }

    #[test]
    fn cherry_monitor_interface_path_resolves_only_expected_instance_shape() {
        assert_eq!(
            cherry_monitor_registry_instance(
                r"\\?\DISPLAY#CHY0000#5&123&0&UID4352#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}"
            ),
            Some(r"DISPLAY\CHY0000\5&123&0&UID4352".to_string())
        );
        assert!(cherry_monitor_registry_instance(
            r"\\?\DISPLAY#DEL0001#5&123#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}"
        )
        .is_none());
        assert!(cherry_monitor_registry_instance(
            r"\\?\DISPLAY#CHY0000#bad/path#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}"
        )
        .is_none());
    }

    #[test]
    fn virtual_display_id_requires_cherry_edid_header_and_checksum() {
        let mut edid = [0u8; 128];
        edid[..12].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 0, 0x0d, 0x19, 0, 0]);
        edid[12..16].copy_from_slice(&2u32.to_le_bytes());
        edid[127] = edid[..127].iter().fold(0u8, |sum, byte| sum.wrapping_sub(*byte));
        assert_eq!(virtual_display_id_from_edid(&edid), Some(2));

        edid[20] ^= 1;
        assert_eq!(virtual_display_id_from_edid(&edid), None);
        edid[20] ^= 1;
        edid[8] ^= 1;
        assert_eq!(virtual_display_id_from_edid(&edid), None);
    }

    #[test]
    fn build_modes_registers_widest_first_with_double_pixels_and_60hz() {
        let fit = ScreenFit::standard();
        let modes = build_modes(&fit);
        // C# v1.3.0：注册顺序 = `RegistrationOrder`（widths 反向，从宽到窄）。
        assert_eq!(modes.len(), fit.registration_order().len());
        assert_eq!((modes[0].width, modes[0].height), (2160, 3800), "最宽的档位要先注册");
        assert_eq!(modes[0].refresh_rates, vec![60]);
        assert_eq!(modes.len(), fit.widths().len());
        let last = modes.last().unwrap();
        assert_eq!((last.width, last.height), (1080, 1900), "最窄的档位最后注册");
        // 每个档位都是逻辑宽 ×2、60Hz。
        for (index, width) in fit.registration_order().iter().enumerate() {
            let (pixel_width, pixel_height) = fit.pixel_size(*width);
            assert_eq!(modes[index].width, pixel_width as u32);
            assert_eq!(modes[index].height, pixel_height as u32);
            assert_eq!(modes[index].refresh_rates, vec![60]);
        }
    }

    // ----- 假实现的整条流程 -----

    enum PrimaryChange {
        Remove,
        SwitchToVirtual,
    }

    /// 一个假的世界：显示器列表、驱动侧"哪些候选编号在线"、调用记录。
    #[derive(Default)]
    struct World {
        monitors: Vec<MonitorSnapshot>,
        snapshot_calls: usize,
        pipe_available: bool,
        restart_ok: bool,
        cursor: Option<Point>,
        moves: Vec<Point>,
        resolutions: Vec<(String, i32, i32, f64)>,
        temporary_attempts: usize,
        temporary_resolution_error: bool,
        dpi_calls: Vec<(u32, u32, i32)>,
        attaches: Vec<(i32, i32, i32, i32)>,
        pending_attach: Option<String>,
        modes: HashSet<(String, i32, i32)>,
        notified: Vec<Vec<u32>>,
        /// 每次 Notify/ReplaceLegacy 带上去的显示名（v2 协议字段 `name`）。
        notified_names: Vec<Vec<String>>,
        removed: Vec<u32>,
        remove_error: bool,
        keep_online_on_remove: HashSet<u32>,
        remove_all_calls: usize,
        legacy: bool,
        online_ids: HashSet<u32>,
        devices: HashMap<u32, String>,
        /// 新屏上线时系统先给它挑的分辨率；`None` = 正好是基准档（1080×1900）。
        online_mode: Option<(i32, i32)>,
        online_refresh_rate: Option<f64>,
        resolution_error: bool,
        ignore_resolution_changes: bool,
        ignore_mode_registration: bool,
        next_device_name: Option<String>,
        fail_next_legacy_add: bool,
        /// 非 `None` 时所有 `notify` 都失败（模拟驱动拒收/管道已断）。
        notify_error: Option<String>,
        primary_change_on_arrival: Option<PrimaryChange>,
    }

    impl World {
        fn driver_online(&mut self, id: u32) {
            if let Some(name) = self.next_device_name.take() {
                self.devices.insert(id, name);
            }
            let Some(device) = self.devices.get(&id).cloned() else {
                return;
            };
            let (width, height) = self.online_mode.unwrap_or((1080, 1900));
            let mut monitor = snapshot(
                &device,
                "Virtual Display",
                Rect::new(1920.0, 0.0, width as f64, height as f64),
                Rect::new(1920.0, 0.0, width as f64, height as f64),
                false,
                DisplayModeInfo {
                    width,
                    height,
                    refresh_rate: self.online_refresh_rate.unwrap_or(60.0),
                    dpi_scale_percent: 100,
                },
            );
            monitor.virtual_display_id = Some(id);
            self.monitors
                .retain(|m| m.device_name != device && m.virtual_display_id != Some(id));
            match self.primary_change_on_arrival.take() {
                Some(PrimaryChange::Remove) => self.monitors.retain(|m| !m.is_primary),
                Some(PrimaryChange::SwitchToVirtual) => {
                    for display in &mut self.monitors {
                        display.is_primary = false;
                    }
                    monitor.is_primary = true;
                }
                None => {}
            }
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
            let mut world = self.0.lock().unwrap();
            world.snapshot_calls += 1;
            world.monitors.clone()
        }

        fn has_display_mode(&self, device_name: &str, width: i32, height: i32) -> bool {
            self.0
                .lock()
                .unwrap()
                .modes
                .contains(&(device_name.to_string(), width, height))
        }

        fn set_resolution(
            &self,
            device_name: &str,
            width: i32,
            height: i32,
            refresh_rate: f64,
        ) -> bool {
            let mut world = self.0.lock().unwrap();
            world
                .resolutions
                .push((device_name.to_string(), width, height, refresh_rate));
            if world.resolution_error {
                return false;
            }
            world.modes.insert((device_name.to_string(), width, height));
            if !world.ignore_resolution_changes {
                for monitor in world.monitors.iter_mut() {
                    if monitor.device_name == device_name {
                        monitor.mode.width = width;
                        monitor.mode.height = height;
                        monitor.mode.refresh_rate = refresh_rate;
                    }
                }
            }
            true
        }

        fn set_temporary_resolution(
            &self,
            device_name: &str,
            width: i32,
            height: i32,
            refresh_rate: f64,
        ) -> bool {
            let fallback = {
                let mut world = self.0.lock().unwrap();
                world.temporary_attempts += 1;
                world.temporary_resolution_error
            };
            !fallback && self.set_resolution(device_name, width, height, refresh_rate)
        }

        fn set_dpi_scale(&self, adapter_id: AdapterLuid, source_id: u32, percent: i32) -> bool {
            let mut world = self.0.lock().unwrap();
            world
                .dpi_calls
                .push((adapter_id.low_part, source_id, percent));
            for monitor in world.monitors.iter_mut() {
                if monitor.adapter_id == adapter_id && monitor.source_id == source_id {
                    monitor.mode.dpi_scale_percent = percent;
                }
            }
            true
        }

        fn try_attach_virtual_display(
            &self,
            display_id: u32,
            width: i32,
            height: i32,
            x: i32,
            y: i32,
        ) -> Option<String> {
            let mut world = self.0.lock().unwrap();
            world.attaches.push((width, height, x, y));
            let device = world.pending_attach.take()?;
            let mut monitor = snapshot(
                &device,
                "Virtual Display",
                Rect::new(x as f64, y as f64, width as f64, height as f64),
                Rect::new(x as f64, y as f64, width as f64, height as f64),
                false,
                DisplayModeInfo {
                    width,
                    height,
                    refresh_rate: 60.0,
                    dpi_scale_percent: 100,
                },
            );
            monitor.virtual_display_id = Some(display_id);
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
            if let Some(message) = world.notify_error.clone() {
                return Err(VddError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    message,
                )));
            }
            world.notified.push(monitors.iter().map(|m| m.id).collect());
            world.notified_names.push(
                monitors
                    .iter()
                    .map(|m| m.name.clone().unwrap_or_default())
                    .collect(),
            );
            for monitor in monitors {
                if world.online_ids.contains(&monitor.id) {
                    world.driver_online(monitor.id);
                }
                if !world.ignore_mode_registration
                    && let Some(device) = world.devices.get(&monitor.id).cloned()
                {
                    for mode in &monitor.modes {
                        world
                            .modes
                            .insert((device.clone(), mode.width as i32, mode.height as i32));
                    }
                }
            }
            Ok(())
        }

        fn replace_legacy(
            &mut self,
            remove_ids: &[u32],
            add: &[VddMonitor],
        ) -> Result<(), VddError> {
            if !self.0.lock().unwrap().legacy {
                return self.notify(add);
            }
            let mut world = self.0.lock().unwrap();
            world.removed.extend_from_slice(remove_ids);
            if world.fail_next_legacy_add {
                world.fail_next_legacy_add = false;
                for &id in remove_ids {
                    world.driver_offline(id);
                }
                return Err(VddError::Io(std::io::ErrorKind::BrokenPipe.into()));
            }
            let ids: Vec<u32> = add.iter().map(|m| m.id).collect();
            world.notified.push(ids.clone());
            world.notified_names.push(
                add.iter()
                    .map(|m| m.name.clone().unwrap_or_default())
                    .collect(),
            );
            for monitor in add {
                if world.online_ids.contains(&monitor.id) {
                    world.driver_online(monitor.id);
                }
                if !world.ignore_mode_registration
                    && let Some(device) = world.devices.get(&monitor.id).cloned()
                {
                    for mode in &monitor.modes {
                        world
                            .modes
                            .insert((device.clone(), mode.width as i32, mode.height as i32));
                    }
                }
            }
            Ok(())
        }

        fn remove(&mut self, ids: &[u32]) -> Result<(), VddError> {
            let mut world = self.0.lock().unwrap();
            world.removed.extend_from_slice(ids);
            if world.remove_error {
                return Err(VddError::Io(std::io::ErrorKind::BrokenPipe.into()));
            }
            for &id in ids {
                if !world.keep_online_on_remove.contains(&id) {
                    world.driver_offline(id);
                }
            }
            Ok(())
        }

        fn remove_all(&mut self) -> Result<(), VddError> {
            let mut world = self.0.lock().unwrap();
            world.remove_all_calls += 1;
            world.monitors.clear();
            Ok(())
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
            modeset_interval: Duration::ZERO,
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

    /// 带归属记录（临时文件）的手机屏对象。
    fn owned_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("snug-display-owned-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("owned-displays.json")
    }

    fn screen_with_owned(
        world: World,
        path: &std::path::Path,
    ) -> (PhoneScreen<FakeApi, FakeVdd>, Arc<Mutex<World>>) {
        let shared = Arc::new(Mutex::new(world));
        let screen = PhoneScreen::with_maps(
            FakeApi(shared.clone()),
            FakeVdd(shared.clone()),
            fast_timings(),
            Arc::new(Mutex::new(ScreenMaps::default())),
            Some(OwnedDisplayStore::new(path)),
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
    fn create_named_sends_the_screen_name_to_the_driver() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let id = screen
            .create_with_name(ScreenFit::standard(), Some("iPad 副屏"))
            .expect("建屏成功");
        assert_eq!(id, 0);
        assert_eq!(
            shared.lock().unwrap().notified_names,
            vec![vec!["iPad 副屏".to_string()]],
            "v2 协议的 Notify 要带上设置里的屏名"
        );
        // `device_names` 存的是 Windows 的 GDI 设备名（改分辨率/查模式要用），
        // 不是驱动的显示名，两者不能混。
        assert_eq!(
            lock_maps(&screen.maps).device_names.get(&0).map(String::as_str),
            Some(r"\\.\DISPLAY2")
        );
    }

    #[test]
    fn blank_or_missing_name_falls_back_to_the_default_monitor_name() {
        // C#：string.IsNullOrWhiteSpace(displayName) ? PhoneScreenSpec.Name : displayName。
        let (mut blank_named, shared) = screen(world_with_phone(&[0]));
        blank_named
            .create_with_name(ScreenFit::standard(), Some("   "))
            .expect("建屏成功");
        assert_eq!(
            shared.lock().unwrap().notified_names,
            vec![vec![MONITOR_NAME.to_string()]]
        );

        let (mut default_named, shared) = screen(world_with_phone(&[0]));
        default_named.create(ScreenFit::standard()).expect("建屏成功");
        assert_eq!(
            shared.lock().unwrap().notified_names,
            vec![vec![MONITOR_NAME.to_string()]]
        );
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
    fn unchanged_width_uses_one_snapshot_without_writing_resolution_or_dpi() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let id = screen.create(ScreenFit::standard()).unwrap();
        {
            let mut world = shared.lock().unwrap();
            world.snapshot_calls = 0;
            world.resolutions.clear();
            world.dpi_calls.clear();
        }
        screen.set_width(id, ScreenFit::standard().base_width());
        let world = shared.lock().unwrap();
        assert_eq!(world.snapshot_calls, 1);
        assert!(world.resolutions.is_empty());
        assert!(world.dpi_calls.is_empty());
    }

    #[test]
    fn area_queries_each_use_one_consistent_monitor_snapshot() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let fit = ScreenFit::standard();
        let id = screen.create(fit.clone()).unwrap();
        let monitor = {
            let mut world = shared.lock().unwrap();
            world.snapshot_calls = 0;
            world.monitors.iter().find(|m| m.virtual_display_id == Some(id)).unwrap().clone()
        };
        let expected_area = window_area(&monitor, fit.base_width());
        assert_eq!(window_area_of_display(&screen.api, &screen.maps, id), Some(expected_area));
        {
            let mut world = shared.lock().unwrap();
            assert_eq!(world.snapshot_calls, 1);
            world.snapshot_calls = 0;
        }
        assert_eq!(
            home_bar_area_of_display(&screen.api, &screen.maps, id),
            Some(home_bar_area(&monitor, expected_area)),
        );
        assert_eq!(shared.lock().unwrap().snapshot_calls, 1);
    }

    #[test]
    fn phone_layout_uses_one_snapshot_and_preserves_the_work_area_offset() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let fit = ScreenFit::standard();
        let id = screen.create(fit.clone()).unwrap();
        let monitor = {
            let mut world = shared.lock().unwrap();
            world.snapshot_calls = 0;
            let monitor = world.monitors.iter_mut().find(|m| m.virtual_display_id == Some(id)).unwrap();
            monitor.work_area = Rect::new(1930.0, 20.0, 1060.0, 1840.0);
            monitor.clone()
        };
        let area = window_area(&monitor, fit.base_width());
        assert_eq!(
            phone_layout_of_display(&screen.api, &screen.maps, id),
            Some((monitor.bounds, home_bar_area(&monitor, area))),
        );
        assert_eq!(shared.lock().unwrap().snapshot_calls, 1);
    }

    #[test]
    fn changed_width_refreshes_the_snapshot_after_resolution_settles() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let id = screen.create(ScreenFit::standard()).unwrap();
        {
            let mut world = shared.lock().unwrap();
            world.snapshot_calls = 0;
            world.resolutions.clear();
            world.dpi_calls.clear();
        }
        screen.set_width(id, 600);
        let world = shared.lock().unwrap();
        assert_eq!(world.snapshot_calls, 2, "模式变化后仍须重新采样 DPI");
        assert_eq!(world.resolutions.len(), 1);
        assert!(world.dpi_calls.is_empty());
    }

    #[test]
    fn big_computer_screen_targets_100_percent_dpi() {
        // 4K 电脑屏（横握 2160×3840 的竖写规格 → 3840×2160 横向）：
        // v1.3 起 2× 会超过 3840×2400 阈值，改用 1 倍像素 + 100% DPI。
        let fit = ScreenFit::from_device(
            &snug_core::PhoneSpec::new(2160.0, 3840.0, None),
            snug_core::DeviceKind::Computer,
            snug_core::ScreenOrientation::Landscape,
        );
        assert_eq!(fit.base_width(), 3840);
        assert_eq!(fit.pixel_size(fit.base_width()), (3840, 2160));
        assert_eq!(fit.target_dpi_percent(fit.base_width()), 100);

        let base_width = fit.base_width();
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        screen.create(fit).expect("建屏成功");
        {
            let world = shared.lock().unwrap();
            assert_eq!(
                world.resolutions,
                vec![(r"\\.\DISPLAY2".to_string(), 3840, 2160, 60.0)],
                "1 倍屏不该再放大"
            );
            // 假驱动上线时报的就是 100%，所以这里不该重复下发 DPI。
            assert!(world.dpi_calls.is_empty(), "已经是 100% 就不用再设");
        }

        // 驱动把 DPI 报成 200% 时，`set_width` 要按 v1.3 拉回 100%。
        {
            let mut world = shared.lock().unwrap();
            for monitor in world.monitors.iter_mut() {
                monitor.mode.dpi_scale_percent = 200;
            }
        }
        screen.set_width(0, base_width);
        let world = shared.lock().unwrap();
        assert_eq!(
            world.dpi_calls.last().map(|call| call.2),
            Some(100),
            "大电脑屏的目标缩放是 100%"
        );
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
        assert_eq!(world.temporary_attempts, 1);
    }

    #[test]
    fn virtual_mode_falls_back_to_persistent_gdi_mode_when_temporary_mode_fails() {
        let mut world = world_with_phone(&[0]);
        world.online_mode = Some((1920, 1080));
        world.temporary_resolution_error = true;
        let (mut screen, shared) = screen(world);
        screen.create(ScreenFit::standard()).unwrap();

        let world = shared.lock().unwrap();
        assert_eq!(world.temporary_attempts, 1);
        assert_eq!(world.resolutions, vec![(r"\\.\DISPLAY2".into(), 1080, 1900, 60.0)]);
    }

    #[test]
    fn a_late_virtual_mode_write_refreshes_the_cooldown() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let id = screen.create(ScreenFit::standard()).unwrap();
        screen.last_modeset = None;

        assert!(screen.set_width(id, 600));
        assert_eq!(shared.lock().unwrap().temporary_attempts, 1);
        assert!(screen.last_modeset.is_some());
    }

    #[test]
    fn create_retires_the_verified_virtual_display_when_the_physical_primary_disappears() {
        let mut world = world_with_phone(&[0, 1]);
        world.primary_change_on_arrival = Some(PrimaryChange::Remove);
        let (mut screen, shared) = screen(world);
        let error = screen.create(ScreenFit::standard()).unwrap_err();

        assert_eq!(error.kind, PhoneScreenErrorKind::CreateFailed);
        assert_eq!(error.pending_display_id, Some(0));
        assert!(error.detail.contains("DELL U2720Q"));
        let world = shared.lock().unwrap();
        assert_eq!(world.notified, vec![vec![0]]);
        assert_eq!(world.removed, vec![0]);
        assert!(world.resolutions.is_empty());
    }

    #[test]
    fn reshape_keeps_the_previous_fit_when_the_physical_primary_is_reassigned() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let old_fit = ScreenFit::standard();
        let id = screen.create(old_fit.clone()).unwrap();
        shared.lock().unwrap().primary_change_on_arrival = Some(PrimaryChange::SwitchToVirtual);
        let new_fit =
            ScreenFit::from_phone(&snug_core::PhoneSpec::new(750.0, 1334.0, Some(375.0)));

        assert!(!screen.reshape(id, new_fit));
        assert_eq!(lock_maps(&screen.maps).fits.get(&id), Some(&old_fit));
        let world = shared.lock().unwrap();
        assert_eq!(world.notified, vec![vec![id], vec![id], vec![id]]);
        assert!(world.resolutions.is_empty());
    }

    #[test]
    fn create_rejects_failed_or_unconfirmed_modes_and_retires_the_owned_screen() {
        for (name, reject, ignore, refresh) in [
            ("rejected", true, false, 60.0),
            ("unconfirmed", false, true, 60.0),
            ("wrong-refresh", false, true, 90.0),
        ] {
            let path = owned_path(name);
            let mut world = world_with_phone(&[0]);
            world.online_mode = Some(if refresh == 60.0 {
                (1920, 1080)
            } else {
                (1080, 1900)
            });
            world.online_refresh_rate = Some(refresh);
            world.resolution_error = reject;
            world.ignore_resolution_changes = ignore;
            let (mut screen, shared) = screen_with_owned(world, &path);
            screen.timings.reshape = Duration::ZERO;

            let error = screen.create(ScreenFit::standard()).expect_err(name);
            assert_eq!(error.kind, PhoneScreenErrorKind::CreateFailed);
            assert_eq!(
                error.pending_display_id,
                Some(0),
                "the core still has to settle the original display layout"
            );
            assert_eq!(shared.lock().unwrap().removed, vec![0]);
            assert!(OwnedDisplayStore::new(&path).load_ids().is_empty());
            assert!(lock_maps(&screen.maps).fits.is_empty());
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }
    }

    #[test]
    fn failed_create_reports_pending_retirement_when_cleanup_is_rejected() {
        let path = owned_path("failed-create-retire");
        let mut world = world_with_phone(&[0]);
        world.online_mode = Some((1920, 1080));
        world.resolution_error = true;
        world.remove_error = true;
        let (mut screen, shared) = screen_with_owned(world, &path);
        let error = screen.create(ScreenFit::standard()).unwrap_err();
        assert_eq!(error.pending_display_id, Some(0));
        assert_eq!(OwnedDisplayStore::new(&path).load_ids(), vec![0]);
        assert!(lock_maps(&screen.maps).device_names.contains_key(&0));
        assert_eq!(shared.lock().unwrap().removed, vec![0]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn reshape_requires_registered_mode_and_actual_readback_before_committing_fit() {
        for skip_registration in [true, false] {
            let (mut screen, shared) = screen(world_with_phone(&[0]));
            let old_fit = ScreenFit::standard();
            let id = screen.create(old_fit.clone()).unwrap();
            {
                let mut world = shared.lock().unwrap();
                world.ignore_mode_registration = skip_registration;
                world.ignore_resolution_changes = !skip_registration;
                world.modes.clear();
            }
            screen.timings.reshape = Duration::ZERO;
            let new_fit =
                ScreenFit::from_phone(&snug_core::PhoneSpec::new(750.0, 1334.0, Some(375.0)));
            assert!(!screen.reshape(id, new_fit));
            assert_eq!(lock_maps(&screen.maps).fits.get(&id), Some(&old_fit));
            let world = shared.lock().unwrap();
            let actual = world
                .monitors
                .iter()
                .find(|m| m.virtual_display_id == Some(id))
                .unwrap();
            assert_eq!(
                (actual.mode.width, actual.mode.height),
                old_fit.pixel_size(old_fit.base_width())
            );
        }
    }

    #[test]
    fn legacy_reshape_rebinds_verified_id_when_the_gdi_name_changes() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let id = screen.create(ScreenFit::standard()).unwrap();
        {
            let mut world = shared.lock().unwrap();
            world.legacy = true;
            world.next_device_name = Some(r"\\.\DISPLAY9".into());
        }
        let fit = ScreenFit::from_phone(&snug_core::PhoneSpec::new(750.0, 1334.0, Some(375.0)));
        screen.timings.reshape = Duration::ZERO;
        assert!(screen.reshape(id, fit.clone()));
        assert_eq!(
            lock_maps(&screen.maps)
                .device_names
                .get(&id)
                .map(String::as_str),
            Some(r"\\.\DISPLAY9")
        );
        let world = shared.lock().unwrap();
        let actual = world
            .monitors
            .iter()
            .find(|m| m.virtual_display_id == Some(id))
            .unwrap();
        assert_eq!(
            (actual.mode.width, actual.mode.height),
            fit.pixel_size(fit.base_width())
        );
    }

    #[test]
    fn legacy_reshape_restores_the_previous_screen_after_partial_ipc_failure() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let old_fit = ScreenFit::standard();
        let id = screen.create(old_fit.clone()).unwrap();
        {
            let mut world = shared.lock().unwrap();
            world.legacy = true;
            world.fail_next_legacy_add = true;
        }
        let fit = ScreenFit::from_phone(&snug_core::PhoneSpec::new(750.0, 1334.0, Some(375.0)));
        assert!(!screen.reshape(id, fit));
        assert_eq!(lock_maps(&screen.maps).fits.get(&id), Some(&old_fit));
        let world = shared.lock().unwrap();
        let actual = world
            .monitors
            .iter()
            .find(|m| m.virtual_display_id == Some(id))
            .unwrap();
        assert_eq!(
            (actual.mode.width, actual.mode.height),
            old_fit.pixel_size(old_fit.base_width())
        );
    }

    #[test]
    fn managed_mode_changes_reject_a_reused_gdi_name_without_matching_edid() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let id = screen.create(ScreenFit::standard()).unwrap();
        {
            let mut world = shared.lock().unwrap();
            let monitor = world
                .monitors
                .iter_mut()
                .find(|m| m.virtual_display_id == Some(id))
                .unwrap();
            monitor.virtual_display_id = None;
            monitor.device_string = "NVIDIA".into();
            world.resolutions.clear();
            world.dpi_calls.clear();
        }
        screen.set_width(id, 600);
        let world = shared.lock().unwrap();
        assert!(world.resolutions.is_empty());
        assert!(world.dpi_calls.is_empty());
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
        // C# `6f548dd`：超时的候选只跳过、**不** Remove —— 那个 id 可能对应
        // C# 版或别的程序创建的屏，误删会把别人的手机屏拆掉。
        assert!(
            !world.removed.contains(&0),
            "超时的候选不能被撤掉（可能误删别人的屏）"
        );
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
    fn notify_failure_logs_candidate_id_and_ipc_error() {
        // C# `PhoneDisplayService.cs:73` 的那行 Warn。没有它，「Notify 全被驱动拒了」
        // 和「Notify 成功但驱动没把屏挂上来」在日志里完全分不出来。
        let mut world = world_with_phone(&[]);
        world.notify_error = Some("管道已断开（ERROR_BROKEN_PIPE）".to_string());
        let (mut screen, _shared) = screen(world);

        let error = screen.create(ScreenFit::standard()).expect_err("Notify 全被拒就不能建屏");
        assert_eq!(error.kind, PhoneScreenErrorKind::NeverOnline);

        let warnings = crate::test_warnings();
        assert!(
            warnings.iter().any(|line| line.contains("手机屏（id=0）IPC Notify 失败")
                && line.contains("管道已断开")),
            "告警里要能读到候选 id 和 IPC 错误：{warnings:?}"
        );
        // 四个候选都失败时每个都要留痕，否则分不清"只挂在 0 号"还是"四个全被拒"。
        for id in 0..4u32 {
            assert!(
                warnings
                    .iter()
                    .any(|line| line.contains(&format!("手机屏（id={id}）IPC Notify 失败"))),
                "缺少候选 {id} 的日志：{warnings:?}"
            );
        }
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
    fn retire_failure_keeps_the_screen_queryable_and_owned_for_retry() {
        let path = owned_path("retire-error");
        let (mut screen, shared) = screen_with_owned(world_with_phone(&[0]), &path);
        let id = screen.create(ScreenFit::standard()).unwrap();
        shared.lock().unwrap().remove_error = true;
        assert!(!screen.retire(id));
        assert!(is_online_of(&screen.api, &screen.maps, id));
        assert!(lock_maps(&screen.maps).fits.contains_key(&id));
        assert_eq!(OwnedDisplayStore::new(&path).load_ids(), vec![id]);

        shared.lock().unwrap().remove_error = false;
        assert!(screen.retire(id));
        assert!(!lock_maps(&screen.maps).fits.contains_key(&id));
        assert!(OwnedDisplayStore::new(&path).load_ids().is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn acknowledged_retire_keeps_ownership_until_the_monitor_really_goes_offline() {
        let path = owned_path("retire-delayed");
        let (mut screen, shared) = screen_with_owned(world_with_phone(&[0]), &path);
        let id = screen.create(ScreenFit::standard()).unwrap();
        shared.lock().unwrap().keep_online_on_remove.insert(id);
        screen.timings.offline = Duration::ZERO;
        assert!(!screen.retire(id));
        assert!(is_online_of(&screen.api, &screen.maps, id));
        assert!(lock_maps(&screen.maps).fits.contains_key(&id));
        assert_eq!(OwnedDisplayStore::new(&path).load_ids(), vec![id]);

        shared.lock().unwrap().driver_offline(id);
        assert!(screen.retire(id));
        assert_eq!(shared.lock().unwrap().removed, vec![id], "下线后只销账，不再发 IPC");
        assert!(OwnedDisplayStore::new(&path).load_ids().is_empty());
        assert!(!lock_maps(&screen.maps).device_names.contains_key(&id));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn owned_orphan_without_memory_mapping_uses_verified_driver_id() {
        let path = owned_path("retire-orphan");
        OwnedDisplayStore::new(&path).record_owned(0);
        let mut world = world_with_phone(&[0]);
        world.driver_online(0);
        world.keep_online_on_remove.insert(0);
        let (mut screen, shared) = screen_with_owned(world, &path);
        screen.timings.offline = Duration::ZERO;
        assert!(is_online_of(&screen.api, &screen.maps, 0));
        assert!(!screen.retire(0), "缺少 map 不能被当成已离线");
        assert_eq!(OwnedDisplayStore::new(&path).load_ids(), vec![0]);
        shared.lock().unwrap().keep_online_on_remove.clear();
        assert!(screen.retire(0));
        assert!(OwnedDisplayStore::new(&path).load_ids().is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unknown_virtual_identity_preserves_ownership_and_never_deletes_unowned_monitors() {
        let path = owned_path("retire-unidentified");
        OwnedDisplayStore::new(&path).record_owned(0);
        let mut world = world_with_phone(&[0]);
        world.driver_online(0);
        world.monitors.last_mut().unwrap().virtual_display_id = None;
        world.keep_online_on_remove.insert(0);
        let (mut screen, shared) = screen_with_owned(world, &path);
        screen.timings.offline = Duration::ZERO;
        assert!(is_online_of(&screen.api, &screen.maps, 0));
        assert!(!screen.retire(0));
        assert_eq!(OwnedDisplayStore::new(&path).load_ids(), vec![0]);
        assert!(!screen.retire(1), "不能删除没有内存映射和归属记录的编号");
        assert_eq!(shared.lock().unwrap().removed, vec![0]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
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
            .find(|m| m.is_primary)
            .map(|m| m.stable_id)
            .expect("实体主屏");

        // 分辨率一样、DPI 也一样 → 什么都不该下发。
        screen.set_mode(
            stable,
            DisplayModeInfo { width: 1920, height: 1080, refresh_rate: 60.0, dpi_scale_percent: 100 },
        );
        {
            let world = shared.lock().unwrap();
            assert!(world.resolutions.is_empty());
            assert!(world.dpi_calls.is_empty());
        }

        // 恢复新的实体屏模式，DPI 保持 100%。
        screen.set_mode(
            stable,
            DisplayModeInfo { width: 1440, height: 2536, refresh_rate: 60.0, dpi_scale_percent: 100 },
        );
        {
            let world = shared.lock().unwrap();
            assert_eq!(
                world.resolutions,
                vec![(r"\\.\DISPLAY1".to_string(), 1440, 2536, 60.0)]
            );
        }
    }

    #[test]
    fn physical_mode_restore_ignores_virtual_and_duplicate_stable_ids() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let id = screen.create(ScreenFit::standard()).unwrap();
        let phone_stable = shared
            .lock()
            .unwrap()
            .monitors
            .iter()
            .find(|m| m.virtual_display_id == Some(id))
            .unwrap()
            .stable_id;
        shared.lock().unwrap().resolutions.clear();
        screen.set_mode(phone_stable, mode(1440, 900));
        assert!(shared.lock().unwrap().resolutions.is_empty());

        let physical_stable = {
            let mut world = shared.lock().unwrap();
            let mut duplicate = world
                .monitors
                .iter()
                .find(|m| m.is_primary)
                .unwrap()
                .clone();
            duplicate.device_name = r"\\.\DISPLAY8".into();
            let stable = duplicate.stable_id;
            world.monitors.push(duplicate);
            stable
        };
        screen.set_mode(physical_stable, mode(1440, 900));
        assert!(shared.lock().unwrap().resolutions.is_empty());
    }

    #[test]
    fn physical_mode_restore_does_not_confuse_a_driver_id_with_its_stable_id() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        let id = screen.create(ScreenFit::standard()).unwrap();
        {
            let mut world = shared.lock().unwrap();
            world
                .monitors
                .iter_mut()
                .find(|m| m.is_primary)
                .unwrap()
                .stable_id = id;
            world
                .monitors
                .iter_mut()
                .find(|m| m.virtual_display_id == Some(id))
                .unwrap()
                .stable_id = id;
            world.resolutions.clear();
        }
        screen.set_mode(id, mode(1440, 900));
        assert_eq!(
            shared.lock().unwrap().resolutions,
            vec![(r"\\.\DISPLAY1".into(), 1440, 900, 60.0)]
        );
    }

    #[test]
    fn cleanup_orphans_without_a_book_removes_nothing() {
        let (mut screen, shared) = screen(world_with_phone(&[0]));
        screen.cleanup_orphans();

        let world = shared.lock().unwrap();
        assert!(world.removed.is_empty(), "没有归属记录就一块都不许撤");
        assert_eq!(world.remove_all_calls, 0, "不能再走 remove_all 把别人的屏拆掉");
    }

    #[test]
    fn cleanup_orphans_only_removes_ids_in_the_book() {
        let path = owned_path("cleanup");
        OwnedDisplayStore::new(&path).record_owned(1);

        let (mut screen, shared) = screen_with_owned(world_with_phone(&[0, 1]), &path);
        {
            let mut world = shared.lock().unwrap();
            world.driver_online(0); // 别人的屏（比如 C# 版建的）
            world.driver_online(1); // 我们上次留下的
        }
        screen.cleanup_orphans();

        {
            let world = shared.lock().unwrap();
            assert_eq!(world.removed, vec![1], "只撤书里记的 1 号");
            assert!(
                world.monitors.iter().any(|m| m.device_name == r"\\.\DISPLAY2"),
                "0 号屏不在书里，必须留着"
            );
            assert!(
                !world.monitors.iter().any(|m| m.device_name == r"\\.\DISPLAY3"),
                "1 号屏在书里，应被撤掉"
            );
        }
        assert!(
            OwnedDisplayStore::new(&path).load_ids().is_empty(),
            "清理成功后账要清空"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn cleanup_logs_the_ids_it_removed() {
        // C# 成功分支的 `Log.Info("已按归属记录清理残留手机屏：#…")`：成功路径也要留痕。
        let path = owned_path("cleanup-log");
        OwnedDisplayStore::new(&path).record_owned(1);

        let (mut screen, _shared) = screen_with_owned(world_with_phone(&[1]), &path);
        screen.cleanup_orphans();

        assert!(
            crate::test_infos()
                .iter()
                .any(|line| line == "已按归属记录清理残留手机屏：#1"),
            "清理成功要按 C# 文案记一条 info"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn cleanup_log_line_matches_csharp_format() {
        // `$"已按归属记录清理残留手机屏：#{string.Join(", #", ownedIds)}"`
        assert_eq!(cleanup_log_line(&[1]), "已按归属记录清理残留手机屏：#1");
        assert_eq!(
            cleanup_log_line(&[2, 3]),
            "已按归属记录清理残留手机屏：#2, #3"
        );
    }

    #[test]
    fn cleanup_without_leftovers_logs_nothing() {
        // 账本空/管道没就绪都走不到成功分支，不该冒出一条"已清理"。
        // 用 #7 这种别处不会出现的 id，避免并行测试互相干扰。
        let path = owned_path("cleanup-quiet");
        OwnedDisplayStore::new(&path).record_owned(7);

        let mut world = world_with_phone(&[0]);
        world.pipe_available = false;
        let (mut screen, _shared) = screen_with_owned(world, &path);
        screen.cleanup_orphans();

        assert!(
            !crate::test_infos()
                .iter()
                .any(|line| line.contains("#7")),
            "没清成就不该记这条 info"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn cleanup_orphans_keeps_the_book_when_the_pipe_is_missing() {
        let path = owned_path("cleanup-nopipe");
        OwnedDisplayStore::new(&path).record_owned(1);

        let mut world = world_with_phone(&[1]);
        world.pipe_available = false;
        let (mut screen, shared) = screen_with_owned(world, &path);
        screen.cleanup_orphans();

        assert!(shared.lock().unwrap().removed.is_empty(), "管道没就绪别乱发删除");
        assert_eq!(
            OwnedDisplayStore::new(&path).load_ids(),
            vec![1],
            "管道恢复后还要靠这本账清理，不能清空"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn cleanup_keeps_only_the_ownership_of_monitors_still_online() {
        let path = owned_path("cleanup-partial");
        let store = OwnedDisplayStore::new(&path);
        store.record_owned(0);
        store.record_owned(1);
        let mut world = world_with_phone(&[0, 1]);
        world.driver_online(0);
        world.driver_online(1);
        world.keep_online_on_remove.insert(1);
        let (mut screen, shared) = screen_with_owned(world, &path);
        screen.cleanup_orphans();
        assert_eq!(store.load_ids(), vec![1]);
        assert!(is_online_of(&screen.api, &screen.maps, 1));
        assert!(!is_online_of(&screen.api, &screen.maps, 0));
        shared.lock().unwrap().keep_online_on_remove.clear();
        screen.cleanup_orphans();
        assert!(store.load_ids().is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn create_records_ownership_and_retire_releases_it() {
        let path = owned_path("rotation");
        let (mut screen, _) = screen_with_owned(world_with_phone(&[0]), &path);

        let id = screen.create(ScreenFit::standard()).expect("建屏成功");
        assert_eq!(id, 0);
        assert_eq!(OwnedDisplayStore::new(&path).load_ids(), vec![0], "上线后才记账");

        assert!(screen.retire(id));
        assert!(
            OwnedDisplayStore::new(&path).load_ids().is_empty(),
            "撤屏成功后要销账"
        );
        assert!(!path.exists(), "最后一条销掉后文件也该删");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn create_does_not_record_a_screen_that_never_came_online() {
        let path = owned_path("never-online");
        let (mut screen, _) = screen_with_owned(world_with_phone(&[]), &path);
        screen.timings.online = Duration::from_millis(5);

        let error = screen.create(ScreenFit::standard()).expect_err("没有候选上线");
        assert_eq!(error.kind, PhoneScreenErrorKind::NeverOnline);
        assert!(
            OwnedDisplayStore::new(&path).load_ids().is_empty(),
            "没确认上线绝不能记账"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn create_skips_an_id_this_instance_already_manages() {
        let (mut screen, shared) = screen(world_with_phone(&[1]));
        {
            // 假装 0 号已经是本程序在管的屏（当前不在线）。
            let mut maps = lock_maps(&screen.maps);
            maps.fits.insert(0, ScreenFit::standard());
            maps.device_names.insert(0, r"\\.\DISPLAY2".to_string());
        }

        let id = screen.create(ScreenFit::standard()).expect("应换到 1 号");
        assert_eq!(id, 1);
        assert_eq!(
            shared.lock().unwrap().notified,
            vec![vec![1]],
            "已归本程序管的 0 号不该再 Notify（会把它的模式覆盖掉）"
        );
    }

    #[test]
    fn two_phone_screens_get_separate_state_and_devices() {
        let path = owned_path("multi");
        let (mut screen, shared) = screen_with_owned(world_with_phone(&[0, 1]), &path);

        let first = screen.create(ScreenFit::standard()).expect("第一块");
        let second = screen.create(ScreenFit::standard()).expect("第二块");
        assert_eq!((first, second), (0, 1), "候选 id 要轮转，不能撞同一个");

        {
            let maps = lock_maps(&screen.maps);
            assert_eq!(maps.fits.len(), 2);
            assert_eq!(maps.device_names.get(&0).map(String::as_str), Some(r"\\.\DISPLAY2"));
            assert_eq!(maps.device_names.get(&1).map(String::as_str), Some(r"\\.\DISPLAY3"));
        }
        assert!(is_online_of(&screen.api, &screen.maps, first));
        assert!(is_online_of(&screen.api, &screen.maps, second));
        assert_eq!(width_of_display(&screen.api, &screen.maps, 0), Some(540));
        assert_eq!(width_of_display(&screen.api, &screen.maps, 1), Some(540));
        assert_eq!(OwnedDisplayStore::new(&path).load_ids(), vec![0, 1]);

        // 撤掉第一块不能影响第二块的状态与设备名。
        assert!(screen.retire(first));
        assert_eq!(OwnedDisplayStore::new(&path).load_ids(), vec![1]);
        assert!(is_online_of(&screen.api, &screen.maps, second));
        assert!(!is_online_of(&screen.api, &screen.maps, first));
        assert_eq!(
            lock_maps(&screen.maps).device_names.get(&1).map(String::as_str),
            Some(r"\\.\DISPLAY3")
        );
        assert_eq!(shared.lock().unwrap().notified, vec![vec![0], vec![1]]);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
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
            virtual_display_id: None,
            bounds,
            work_area,
            is_primary,
            mode,
            adapter_id: AdapterLuid::default(),
            source_id: stable_id(device_name, AdapterLuid::default(), 0),
        }
    }
}
