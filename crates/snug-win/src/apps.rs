//! App 库与启动的 Windows 实现。
//!
//! 对应 C# `SnugWin.Windows.Windows.AppService`（`AppService.cs`）以及 App 层的
//! `AppIconCache`（`SnugWin.App/Controllers/AppIconCache.cs`）里的图标读取部分。
//!
//! 这个文件分两层：
//! - **纯逻辑**（本文件上半部分）：进程名/路径处理、已安装 App 的去重合并规则、
//!   「哪些窗口属于这个 App」的判定、运行中 App 的分组、快捷方式（`.lnk`）目标解析、
//!   图标尺寸换算。全部有单测，不碰系统。
//! - **Win32 接线**（下半部分）：进程枚举、注册表 App Paths、开始菜单扫描、
//!   `shell:AppsFolder`、图标与启动/退出。所有有副作用的系统调用都经
//!   [`AppSystem`] trait，单测只用假实现。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use snug_core::apps::ordered as ordered_running;
use snug_core::apps::sorted as sorted_apps;
use snug_core::desk::{AppControl, WindowControl};
use snug_core::windows::{AppInfo, RunningApp, WindowInfo};
use snug_core::WindowKey;
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, HWND, LPARAM, SIZE, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, IBindCtx, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY,
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ,
};
use windows::Win32::System::Threading::{
    GetProcessId, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_CONTROL, VK_N, VK_SHIFT,
};
use windows::Win32::UI::Shell::{
    ExtractIconExW, SHCreateItemFromParsingName, SHGetKnownFolderItem, SHGetKnownFolderPath,
    ShellExecuteExW, BHID_EnumItems, FOLDERID_AppsFolder, FOLDERID_CommonPrograms,
    FOLDERID_Programs, IEnumShellItems, IShellItem, IShellItemImageFactory, SEE_MASK_NOASYNC,
    SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, SIIGBF, SIIGBF_BIGGERSIZEOK,
    SIIGBF_ICONONLY, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_NORMALDISPLAY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, EnumWindows, GetIconInfo, GetWindowTextLengthW, GetWindowThreadProcessId,
    IsWindowVisible, PostMessageW, ICONINFO, SW_SHOWNORMAL, WM_CLOSE,
};

use crate::wide::{from_wide, to_wide};

// ---------------------------------------------------------------------------
// 常量（对应 C# `AppIconCache`）
// ---------------------------------------------------------------------------

/// 从 shell 取图标的边长（C# `ShellIconSize`）。
pub const SHELL_ICON_SIZE: i32 = 128;
/// 从 exe 资源取图标的边长（C# `BitmapSizeOptions.FromWidthAndHeight(64, 64)`）。
pub const EXE_ICON_SIZE: i32 = 64;

/// `SIIGBF_BIGGERSIZEOK`：允许返回比请求更大的图标。
pub const SIIGBF_BIGGER_SIZE_OK: u32 = 0x01;
/// `SIIGBF_ICONONLY`：只要图标，不要缩略图。
pub const SIIGBF_ICON_ONLY: u32 = 0x04;

/// 一个解码好的图标（RGBA，非预乘），供界面直接上传纹理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconImage {
    pub width: i32,
    pub height: i32,
    /// 每行 `width * 4` 字节，顺序 R、G、B、A。
    pub rgba: Vec<u8>,
}

impl IconImage {
    pub fn new(width: i32, height: i32, rgba: Vec<u8>) -> Self {
        Self { width, height, rgba }
    }

    /// 字节数是否与宽高自洽。
    pub fn is_consistent(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.rgba.len() == (self.width as usize) * (self.height as usize) * 4
    }
}

/// 把 32 位 BGRA（偏蓝、自上而下）重排成 RGBA，**保留预乘 alpha**。
///
/// C# 用 `GetDIBits` 拿到 BGRA 后按 `PixelFormats.Pbgra32` 交给 WPF；
/// egui 的纹理同样要求预乘 alpha，所以这里只换通道顺序、不做除法。
pub fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for pixel in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    rgba
}

/// 取图标时要的边长（`SIIGBF_BIGGERSIZEOK` 允许系统给更大的）。
pub fn icon_box_size() -> (i32, i32) {
    (SHELL_ICON_SIZE, SHELL_ICON_SIZE)
}

// ---------------------------------------------------------------------------
// 路径与进程名
// ---------------------------------------------------------------------------

/// `Path.GetFileName`（`/` 与 `\` 都当分隔符）。
pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or("")
}

/// `Path.GetFileNameWithoutExtension`。
pub fn exe_stem(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(index) if index > 0 => &name[..index],
        _ => name,
    }
}

/// `Path.GetDirectoryName`（保留盘符根）。
pub fn parent_dir(path: &str) -> Option<String> {
    let index = path.rfind(['\\', '/'])?;
    if index == 0 {
        return Some(path[..1].to_string());
    }
    let head = &path[..index];
    if head.ends_with(':') {
        Some(format!("{head}\\"))
    } else {
        Some(head.to_string())
    }
}

/// UWP / Store 应用：AppId 里带 `!`（例如 `Microsoft.WindowsTerminal_8wekyb3d8bbwe!App`）。
pub fn is_uwp(app_id: &str) -> bool {
    app_id.contains('!')
}

/// 开始菜单里要忽略的快捷方式名（C# `IsIgnoredShortcutName`）。
pub fn is_ignored_shortcut_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    name.contains("卸载")
        || lower.contains("uninstall")
        || name.contains("帮助")
        || lower.contains("help")
        || lower.contains("readme")
        || lower.contains("website")
}

/// 安装器 / 更新器 / 辅助程序（C# `IsInstallerOrHelper`）。
pub fn is_installer_or_helper(exe: &str, name: &str) -> bool {
    let lower = exe.to_lowercase();
    lower.contains("unins")
        || lower.contains("setup")
        || lower.contains("update")
        || name.eq_ignore_ascii_case("SnugWin")
}

// ---------------------------------------------------------------------------
// 「这个窗口属于这个 App 吗」（C# `AppWindowsAsync` 里的谓词）
// ---------------------------------------------------------------------------

/// 判定一个窗口是不是 `app` 的窗口。
///
/// 规则与 C# 一致，按顺序：
/// 1. 调用方指定的 `launched_pid` 命中窗口进程；
/// 2. 窗口的 `AppId` 等于目标 exe 名；
/// 3. 窗口所属进程的可执行文件在 App 的安装目录下（`image_path_of` 提供）；
/// 4. UWP App：窗口标题或 App 名与 App 显示名匹配。
///
/// 判定窗口匹配时**不**调用任何系统 API：进程镜像路径由调用方注入（便于测试）。
pub fn window_belongs_to(
    app: &AppInfo,
    launched_pid: Option<i32>,
    window: &WindowInfo,
    image_path_of: impl Fn(i32) -> Option<PathBuf>,
) -> bool {
    if let Some(pid) = launched_pid
        && pid > 0
        && window.key.pid == pid
    {
        return true;
    }

    let uwp = is_uwp(&app.app_id);
    let target_exe = if uwp {
        String::new()
    } else {
        file_name(app.executable_path.as_deref().unwrap_or(&app.app_id)).to_string()
    };
    let target_dir = app
        .executable_path
        .as_deref()
        .filter(|path| !path.is_empty())
        .and_then(parent_dir);

    if !target_exe.is_empty()
        && window
            .app_id
            .as_deref()
            .is_some_and(|app_id| app_id.eq_ignore_ascii_case(&target_exe))
    {
        return true;
    }

    if let Some(target_dir) = target_dir.as_deref()
        && let Some(image) = image_path_of(window.key.pid)
        && let Some(image) = image.to_str()
        && starts_with_ignore_case(image, target_dir)
    {
        return true;
    }

    if uwp {
        return window.title.to_lowercase().contains(&app.name.to_lowercase())
            || window.app_name.to_lowercase().contains(&app.name.to_lowercase());
    }

    false
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix)
}

// ---------------------------------------------------------------------------
// 已安装 App 的合并（Start Menu → App Paths → shell:AppsFolder）
// ---------------------------------------------------------------------------

/// 已安装 App 收集器：去重规则逐条对应 C# `ScanInstalledApps`。
#[derive(Debug, Default)]
pub struct InstalledCollector {
    apps: Vec<AppInfo>,
    /// 已占用的 AppId（大小写不敏感）。
    ids: HashSet<String>,
    /// 已占用的显示名（C# 用 `CurrentCultureIgnoreCase`，这里用小写近似）。
    names: HashSet<String>,
}

impl InstalledCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    pub fn contains_id(&self, app_id: &str) -> bool {
        self.ids.contains(&app_id.to_ascii_lowercase())
    }

    pub fn contains_name(&self, name: &str) -> bool {
        self.names.contains(&name.to_lowercase())
    }

    /// 开始菜单快捷方式（C# 第一轮）：只按 exe 名去重。
    pub fn add_shortcut(&mut self, exe: &str, name: &str, target: &str, lnk: &str) -> bool {
        if self.contains_id(exe) {
            return false;
        }
        self.push(AppInfo {
            app_id: exe.to_string(),
            name: name.to_string(),
            executable_path: Some(target.to_string()),
            launch_target: Some(lnk.to_string()),
            arguments: None,
            working_directory: None,
        });
        true
    }

    /// 注册表 App Paths（C# 第二轮）：同样只按 exe 名去重。
    pub fn add_app_path(&mut self, exe: &str, name: &str, target: &str) -> bool {
        if self.contains_id(exe) {
            return false;
        }
        self.push(AppInfo {
            app_id: exe.to_string(),
            name: name.to_string(),
            executable_path: Some(target.to_string()),
            launch_target: Some(target.to_string()),
            arguments: None,
            working_directory: None,
        });
        true
    }

    /// `shell:AppsFolder`（C# 第三轮）：先按显示名去重，再按 AppId 去重；
    /// `.exe` 目标当普通 App，AUMID（带 `!`）当 UWP App。
    ///
    /// `exists` 是 `File.Exists(target)` 的结果——C# 里 exe 目标必须真的存在，
    /// 判定留在收集器外面做会漏掉这条分支，所以这里显式传进来。
    pub fn add_apps_folder(&mut self, name: &str, target: &str, exists: bool) -> bool {
        if is_ignored_shortcut_name(name) || self.contains_name(name) {
            return false;
        }
        if target.to_lowercase().ends_with(".exe") {
            let exe = file_name(target).to_string();
            if !exists
                || self.contains_id(&exe)
                || is_installer_or_helper(&exe, name)
                || crate::window::is_excluded_app(Some(&exe))
            {
                return false;
            }
            self.push(AppInfo {
                app_id: exe,
                name: name.to_string(),
                executable_path: Some(target.to_string()),
                launch_target: Some(target.to_string()),
                arguments: None,
                working_directory: None,
            });
            true
        } else if is_uwp(target) {
            if self.contains_id(target) {
                return false;
            }
            self.push(AppInfo {
                app_id: target.to_string(),
                name: name.to_string(),
                executable_path: None,
                launch_target: Some("explorer.exe".to_string()),
                arguments: Some(format!("shell:AppsFolder\\{target}")),
                working_directory: None,
            });
            true
        } else {
            false
        }
    }

    fn push(&mut self, app: AppInfo) {
        self.ids.insert(app.app_id.to_ascii_lowercase());
        self.names.insert(app.name.to_lowercase());
        self.apps.push(app);
    }

    /// 收尾：按拼音排序（C# `AppList.Sorted`）。
    pub fn finish(self) -> Vec<AppInfo> {
        sorted_apps(&self.apps)
    }
}

// ---------------------------------------------------------------------------
// 运行中 App 的分组（C# `GetRunningApps`）
// ---------------------------------------------------------------------------

/// 把窗口按 App 分组，再交给 `AppList.Ordered` 按 Z 序排（C# `GetRunningApps`）。
///
/// 分组键是窗口的 `AppId`，没有 AppId 时用进程号；组内第一个窗口决定 App 的
/// 显示名、AppId 与可执行文件路径。
pub fn group_running_apps(
    windows: &[WindowInfo],
    image_path_of: impl Fn(i32) -> Option<PathBuf>,
) -> Vec<RunningApp> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<WindowInfo>> = HashMap::new();
    for window in windows {
        let key = match window.app_id.as_deref() {
            Some(app_id) => app_id.to_ascii_lowercase(),
            None => window.key.pid.to_string(),
        };
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(window.clone());
    }

    let mut list = Vec::new();
    for key in order {
        let Some(group) = groups.remove(&key) else {
            continue;
        };
        let first = &group[0];
        let image_path = image_path_of(first.key.pid).and_then(|path| path.to_str().map(str::to_string));
        list.push(RunningApp {
            app: AppInfo {
                app_id: first.app_id.clone().unwrap_or_else(|| first.app_name.clone()),
                name: first.app_name.clone(),
                executable_path: image_path,
                launch_target: None,
                arguments: None,
                working_directory: None,
            },
            pid: first.key.pid,
            windows: group,
        });
    }
    list
}

/// 挑进程号时优先有主窗口的那个（C# `PidOfAsync`）。
pub fn pick_pid(candidates: &[i32], has_main_window: impl Fn(i32) -> bool) -> Option<i32> {
    candidates
        .iter()
        .copied()
        .find(|pid| has_main_window(*pid))
        .or_else(|| candidates.first().copied())
}

// ---------------------------------------------------------------------------
// .lnk 目标解析（纯逻辑）
// ---------------------------------------------------------------------------

/// Shell Link 头长度。
const LNK_HEADER_SIZE: usize = 0x4C;
/// 桌面 Shell Link 的 CLSID。
const LNK_CLSID: [u8; 16] = [
    0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];
const LNK_HAS_LINK_TARGET_ID_LIST: u32 = 0x01;
const LNK_HAS_LINK_INFO: u32 = 0x02;

/// 从 `.lnk` 字节里解析目标可执行文件。
///
/// 只处理最常见的"本地目标"形状：`LinkInfo` 里的 `LocalBasePath`（优先 Unicode，
/// 退回 ANSI）。C# 版走的是 `WScript.Shell` COM，Rust 版自己解析二进制，
/// 免得为一次快捷方式解析去背 COM 的依赖。
///
/// 解析不出来（例如目标只存在 IDList 里、或路径是环境变量）时返回 `None`，
/// 调用方跳过该快捷方式即可。
pub fn parse_lnk_target(bytes: &[u8]) -> Option<PathBuf> {
    if bytes.len() < LNK_HEADER_SIZE {
        return None;
    }
    let header_size = read_u32(bytes, 0)? as usize;
    if header_size != LNK_HEADER_SIZE {
        return None;
    }
    if bytes[4..20] != LNK_CLSID {
        return None;
    }
    let flags = read_u32(bytes, 0x14)?;

    let mut offset = LNK_HEADER_SIZE;
    if flags & LNK_HAS_LINK_TARGET_ID_LIST != 0 {
        let id_list_size = read_u16(bytes, offset)? as usize;
        offset = offset.checked_add(2 + id_list_size)?;
    }
    if flags & LNK_HAS_LINK_INFO == 0 {
        return None;
    }
    if offset + 0x1C > bytes.len() {
        return None;
    }

    let link_info_size = read_u32(bytes, offset)? as usize;
    let link_info_header = read_u32(bytes, offset + 4)? as usize;
    let link_info_flags = read_u32(bytes, offset + 8)?;
    let local_base_ansi = read_u32(bytes, offset + 16)? as usize;
    // header >= 0x24 时尾部多了 Unicode 偏移。
    let local_base_unicode = if link_info_header >= 0x24 {
        read_u32(bytes, offset + 0x1C).map(|value| value as usize)
    } else {
        None
    };
    if link_info_size == 0 || offset + link_info_size > bytes.len() {
        return None;
    }
    // 只有 `VolumeIDAndLocalBasePath` 才有本地基准路径。
    if link_info_flags & 0x01 == 0 {
        return None;
    }

    if let Some(unicode_offset) = local_base_unicode
        && unicode_offset != 0
    {
        let start = offset + unicode_offset;
        if let Some(text) = read_utf16z(bytes, start) {
            return Some(PathBuf::from(text));
        }
    }
    if local_base_ansi != 0 {
        let start = offset + local_base_ansi;
        if let Some(text) = read_ansi_z(bytes, start) {
            return Some(PathBuf::from(text));
        }
    }
    None
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_utf16z(bytes: &[u8], start: usize) -> Option<String> {
    let tail = bytes.get(start..)?;
    let mut units = Vec::new();
    for pair in tail.chunks_exact(2) {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    if units.is_empty() {
        return None;
    }
    Some(String::from_utf16_lossy(&units))
}

fn read_ansi_z(bytes: &[u8], start: usize) -> Option<String> {
    let tail = bytes.get(start..)?;
    let end = tail.iter().position(|byte| *byte == 0)?;
    if end == 0 {
        return None;
    }
    // 非 ASCII 的 ANSI 路径按本机代码页编码；这里按 UTF-8 尽最大努力，
    // 解不出来的快捷方式会被跳过（Unicode 偏移存在时不会走到这里）。
    Some(String::from_utf8_lossy(&tail[..end]).to_string())
}

/// App 的可执行文件路径（`ExecutablePath` 优先，其次由 `LaunchTarget` 推断）。
pub fn executable_of(app: &AppInfo) -> Option<&Path> {
    app.executable_path.as_deref().map(Path::new)
}

/// 启动参数：给 `shell:AppsFolder\...` 这种 UWP 目标补 explorer 前缀。
pub fn launch_command(app: &AppInfo) -> (String, Option<String>) {
    let program = app
        .launch_target
        .clone()
        .or_else(|| app.executable_path.clone())
        .unwrap_or_else(|| app.app_id.clone());
    (program, app.arguments.clone())
}

// ---------------------------------------------------------------------------
// 图标：请求参数（纯逻辑）
// ---------------------------------------------------------------------------

/// `IShellItemImageFactory::GetImage` 的请求尺寸与标志（C# `LoadShellIcon`）。
pub fn shell_icon_request() -> (i32, i32, u32) {
    (SHELL_ICON_SIZE, SHELL_ICON_SIZE, SIIGBF_BIGGER_SIZE_OK | SIIGBF_ICON_ONLY)
}

/// 解析图标用的路径：`shell:` 开头的用 `LaunchTarget`，否则用 exe 路径；
/// 都没有时退回进程镜像路径（C# `GetIcon`）。
pub fn icon_parsing_path(app: &AppInfo, pid_image: Option<&str>) -> Option<String> {
    let exe = app.executable_path.clone().or_else(|| pid_image.map(str::to_string));
    if let Some(target) = app.launch_target.as_deref()
        && target.to_lowercase().starts_with("shell:")
    {
        return Some(target.to_string());
    }
    exe
}

// ---------------------------------------------------------------------------
// 平台接口（对应 C# `AppService` 里每个方法用到的那一小块系统能力）
// ---------------------------------------------------------------------------

/// 按进程名查到的进程（`Process.GetProcessesByName` 的一行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessEntry {
    pub pid: i32,
    /// `MainWindowHandle != IntPtr.Zero` 的等价物。
    pub has_main_window: bool,
}

/// 注册表根（C# 里 `ScanAppPaths` 被 HKLM、HKCU 各调一次）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hive {
    LocalMachine,
    CurrentUser,
}

/// App 子系统的全部有副作用能力。
///
/// 单测只实现这个 trait 的假版本；真机实现是 [`WinSystem`]。
/// 方法粒度与 C# `AppService` 的调用点一一对应，方便逐条对照。
pub trait AppSystem: Send + Sync {
    /// `Process.GetProcessesByName`（按不带扩展名的进程名匹配）。
    fn processes_named(&self, stem: &str) -> Vec<ProcessEntry>;
    /// `Process.GetProcessById(pid)` 之后 `!HasExited`。
    fn process_alive(&self, pid: i32) -> bool;
    /// `Win32.GetProcessImagePath`。
    fn image_path(&self, pid: i32) -> Option<String>;
    /// `WindowService.EnumerateStandardWindows`。
    fn standard_windows(&self, include_minimized: bool) -> Vec<WindowInfo>;
    /// `WindowService.GetOnScreenWindowZOrder(...).Select(x => x.Handle)`。
    fn z_order(&self) -> Vec<u64>;
    /// `Process.Start(UseShellExecute = true)`：返回启动后的 pid；拿不到（UWP 交接）给 `Some(0)`；
    /// 抛异常给 `None`。
    fn start(&self, program: &str, arguments: Option<&str>, working_directory: Option<&str>) -> Option<i32>;
    /// `PostMessageW(hwnd, WM_CLOSE, 0, 0)`。
    fn post_close(&self, handle: u64) -> bool;
    /// `WindowControl::focus`（会先还原最小化窗口）。
    fn focus(&self, window: WindowKey);
    /// `UiaHelper.TryInvokeNewWindowMenu`。
    fn invoke_new_window(&self, handle: u64) -> bool;
    /// `WindowService.SendCtrlShortcut(VK_N, withShift)`。
    fn send_ctrl_n(&self, with_shift: bool);
    /// `Process.Kill(entireProcessTree: true)`。
    fn kill_tree(&self, pid: i32) -> bool;
    /// 开始菜单的两个根（CommonPrograms、Programs）。
    fn start_menu_roots(&self) -> Vec<String>;
    /// 递归枚举某个根下的 `*.lnk`。
    fn lnk_files(&self, root: &str) -> Vec<String>;
    /// `File.ReadAllBytes`。
    fn read_file(&self, path: &str) -> Option<Vec<u8>>;
    /// `File.Exists`。
    fn file_exists(&self, path: &str) -> bool;
    /// `Registry...App Paths` 下的 `(子键名, 默认值)`。
    fn app_paths(&self, hive: Hive) -> Vec<(String, String)>;
    /// `FileVersionInfo.GetVersionInfo(path).FileDescription`。
    fn file_description(&self, path: &str) -> Option<String>;
    /// `shell:AppsFolder` 里的 `(显示名, 解析路径 / AUMID)`。
    fn apps_folder_entries(&self) -> Vec<(String, String)>;
    /// `IShellItemImageFactory::GetImage`（128px）。
    fn shell_icon(&self, parsing_path: &str) -> Option<IconImage>;
    /// `Icon.ExtractAssociatedIcon`（exe 资源图标，64px 兜底）。
    fn exe_icon(&self, exe_path: &str) -> Option<IconImage>;
}

// ---------------------------------------------------------------------------
// App 服务（对应 C# `AppService`）
// ---------------------------------------------------------------------------

/// App 库、图标与进程控制。泛型参数只是为了让单测能塞假系统。
pub struct Apps<S: AppSystem> {
    system: S,
}

/// 真机实现（`AppService` + `AppIconCache` 的图标部分）。
pub type WinApps = Apps<WinSystem>;

impl<S: AppSystem> Apps<S> {
    /// 用指定的系统实现建一个 App 服务（测试用）。
    pub fn with_system(system: S) -> Self {
        Self { system }
    }

    /// 只读访问底层系统实现。
    pub fn system(&self) -> &S {
        &self.system
    }

    /// 扫描已安装 App（C# `ScanInstalledApps`）：开始菜单 → App Paths → AppsFolder，
    /// 最后按拼音排序。三轮的顺序影响去重结果，不能换。
    pub fn scan_installed_apps(&self) -> Vec<AppInfo> {
        let mut collector = InstalledCollector::new();

        // 1. 开始菜单快捷方式。
        for root in self.system.start_menu_roots() {
            for lnk in self.system.lnk_files(&root) {
                let name = exe_stem(&lnk).to_string();
                if is_ignored_shortcut_name(&name) {
                    continue;
                }
                let Some(bytes) = self.system.read_file(&lnk) else {
                    continue;
                };
                let Some(target) = parse_lnk_target(&bytes) else {
                    continue;
                };
                let Some(target) = target.to_str() else {
                    continue;
                };
                if !target.to_lowercase().ends_with(".exe") {
                    continue;
                }
                if !self.system.file_exists(target) {
                    continue;
                }
                let exe = file_name(target).to_string();
                if crate::window::is_excluded_app(Some(&exe)) || is_installer_or_helper(&exe, &name) {
                    continue;
                }
                collector.add_shortcut(&exe, &name, target, &lnk);
            }
        }

        // 2. 注册表 App Paths（先 HKLM 再 HKCU，与 C# 相同）。
        for hive in [Hive::LocalMachine, Hive::CurrentUser] {
            for (sub, raw_path) in self.system.app_paths(hive) {
                if !sub.to_lowercase().ends_with(".exe") {
                    continue;
                }
                if crate::window::is_excluded_app(Some(&sub)) || is_installer_or_helper(&sub, &sub) {
                    continue;
                }
                // C# 的 `GetValue(null)?.ToString()?.Trim('"')`。
                let raw_path = raw_path.trim().trim_matches('"').trim().to_string();
                if raw_path.is_empty() || !self.system.file_exists(&raw_path) {
                    continue;
                }
                if collector.contains_id(&sub) {
                    continue;
                }
                let mut name = exe_stem(&raw_path).to_string();
                if let Some(description) = self.system.file_description(&raw_path) {
                    let description = description.trim();
                    if !description.is_empty() {
                        name = description.to_string();
                    }
                }
                collector.add_app_path(&sub, &name, &raw_path);
            }
        }

        // 3. shell:AppsFolder（Store / UWP 应用）。C# 先按名字去重，再按 AppId。
        for (name, target) in self.system.apps_folder_entries() {
            let exists = self.system.file_exists(&target);
            collector.add_apps_folder(&name, &target, exists);
        }

        collector.finish()
    }

    /// 正在运行的 App（C# `GetRunningApps`）：按 AppId 分组后按屏幕 Z 序排。
    pub fn running_apps(&self) -> Vec<RunningApp> {
        let windows = self.system.standard_windows(true);
        let z_order = self.system.z_order();
        let list = group_running_apps(&windows, |pid| {
            self.system.image_path(pid).map(PathBuf::from)
        });
        ordered_running(&list, &z_order)
    }

    /// App 图标（C# `AppIconCache.GetIcon`）：先 shell 图标，再 exe 资源图标，都失败返回 `None`
    /// （C# 里还会画一个占位图标，那是界面层的事，平台层不造图）。
    pub fn icon(&self, app: &AppInfo, pid: Option<i32>) -> Option<IconImage> {
        let pid_image = pid.and_then(|pid| self.system.image_path(pid));
        let exe_path = app
            .executable_path
            .clone()
            .filter(|path| !path.is_empty())
            .or_else(|| pid_image.clone());

        let shell_target = icon_parsing_path(app, pid_image.as_deref());
        if let Some(image) = shell_target.and_then(|target| self.system.shell_icon(&target)) {
            return Some(image);
        }
        exe_path.and_then(|path| self.system.exe_icon(&path))
    }

    /// 某个进程的"可见应用窗口"（C# `_windows.ShownWindowsAsync(pid)`）。
    fn shown_windows(&self, pid: i32) -> Vec<WindowInfo> {
        self.system
            .standard_windows(false)
            .into_iter()
            .filter(|window| window.key.pid == pid)
            .collect()
    }
}

impl Apps<WinSystem> {
    /// 真机 App 服务。
    pub fn new() -> Self {
        Self::with_system(WinSystem::new())
    }
}

impl Default for Apps<WinSystem> {
    fn default() -> Self {
        Self::new()
    }
}

/// 新窗口快捷键要不要带 Shift（C# `NewWindowAsync` 里的白名单）。
pub fn new_window_needs_shift(app_id: Option<&str>) -> bool {
    matches!(
        app_id.map(str::to_lowercase).as_deref(),
        Some("windowsterminal.exe")
            | Some("wt.exe")
            | Some("code.exe")
            | Some("cursor.exe")
            | Some("windsurf.exe")
            | Some("zed.exe")
    )
}

impl<S: AppSystem> AppControl for Apps<S> {
    /// C# `PidOfAsync`：同名进程里优先有主窗口的那个。
    fn pid_of(&self, app: &AppInfo) -> Option<i32> {
        let target = app.executable_path.as_deref().unwrap_or(&app.app_id);
        let entries = self.system.processes_named(exe_stem(target));
        let candidates: Vec<i32> = entries.iter().map(|entry| entry.pid).collect();
        pick_pid(&candidates, |pid| {
            entries.iter().any(|entry| entry.pid == pid && entry.has_main_window)
        })
    }

    /// C# `PidsAsync`。
    fn pids(&self, app_id: &str) -> Vec<i32> {
        self.system
            .processes_named(exe_stem(app_id))
            .into_iter()
            .map(|entry| entry.pid)
            .collect()
    }

    /// C# `OpenAsync`。
    fn open(&self, app: &AppInfo) -> Option<i32> {
        let (program, arguments) = launch_command(app);
        match self.system.start(
            &program,
            arguments.as_deref(),
            app.working_directory.as_deref(),
        ) {
            // 启动失败：C# 记一条 Warn 后返回 null。
            None => {
                crate::warn(&format!("启动 {} 失败", app.name));
                None
            }
            Some(pid) if pid > 0 => Some(pid),
            // UWP 是交给 explorer 打开的，C# 直接返回 0。
            Some(_) if is_uwp(&app.app_id) => Some(0),
            // 普通 App 被既有实例接管时，按 exe 名找回进程；找不到也给 0。
            Some(_) => {
                let exe = app.executable_path.as_deref().unwrap_or(&app.app_id);
                Some(
                    self.system
                        .processes_named(exe_stem(exe))
                        .first()
                        .map(|entry| entry.pid)
                        .unwrap_or(0),
                )
            }
        }
    }

    /// C# `NewWindowAsync`：聚焦已有窗口 → UIA 菜单 → Ctrl(+Shift)+N。
    fn new_window(&self, pid: i32) -> bool {
        let Some(target) = self.shown_windows(pid).into_iter().next() else {
            return false;
        };
        self.system.focus(target.key);
        std::thread::sleep(Duration::from_millis(80));

        if self.system.invoke_new_window(target.key.handle) {
            return true;
        }
        self.system
            .send_ctrl_n(new_window_needs_shift(target.app_id.as_deref()));
        true
    }

    /// C# `QuitAsync`：先给主窗口发 `WM_CLOSE`，没有窗口也算成功。
    ///
    /// C# 是 `MainWindowHandle != 0` 时 `CloseMainWindow()`，否则枚举可见窗口逐个
    /// `PostMessage(WM_CLOSE)`。真机上 `MainWindowHandle == 0` 又存在可见窗口的情况
    /// 极少，这里统一为"给第一个可见窗口发关闭"，其余窗口由应用自己收尾。
    fn quit(&self, pid: i32) -> bool {
        if !self.system.process_alive(pid) {
            return true;
        }
        if let Some(first) = self.shown_windows(pid).into_iter().next() {
            self.system.post_close(first.key.handle);
        }
        true
    }

    /// C# `ForceQuitAsync`。
    fn force_quit(&self, pid: i32) -> bool {
        self.system.kill_tree(pid)
    }

    /// C# `IsRunningAsync`。
    fn is_running(&self, pid: i32) -> bool {
        self.system.process_alive(pid)
    }

    /// C# `AppWindowsAsync`。
    fn app_windows(&self, app: &AppInfo, launched_pid: Option<i32>) -> Vec<WindowInfo> {
        self.system
            .standard_windows(false)
            .into_iter()
            .filter(|window| {
                window_belongs_to(app, launched_pid, window, |pid| {
                    self.system.image_path(pid).map(PathBuf::from)
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 真机实现
// ---------------------------------------------------------------------------

/// 真机 App 子系统。
pub struct WinSystem {
    windows: crate::window::WinWindows,
}

impl WinSystem {
    pub fn new() -> Self {
        Self { windows: crate::window::WinWindows::new() }
    }
}

impl Default for WinSystem {
    fn default() -> Self {
        Self::new()
    }
}

/// `OpenProcess` + `QueryFullProcessImageNameW`（C# `Win32.GetProcessImagePath`）。
fn process_image_path(pid: i32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32).ok()?;
        let mut buffer = [0u16; 1024];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        Some(from_wide(&buffer[..length as usize]))
    }
}

/// 进程快照：`(pid, 父进程 pid, 映像文件名)`。
fn snapshot_processes() -> Vec<(i32, i32, String)> {
    let mut result = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return result;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                result.push((
                    entry.th32ProcessID as i32,
                    entry.th32ParentProcessID as i32,
                    from_wide(&entry.szExeFile),
                ));
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    result
}

thread_local! {
    /// 枚举"有没有主窗口"用的临时缓冲。
    static PID_WINDOW_FOUND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

unsafe extern "system" fn find_window_of_pid(window: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let pid = lparam.0 as u32;
    let mut owner = 0u32;
    unsafe { GetWindowThreadProcessId(window, Some(&mut owner)) };
    if owner == pid
        && unsafe { IsWindowVisible(window).as_bool() }
        && unsafe { GetWindowTextLengthW(window) } > 0
    {
        PID_WINDOW_FOUND.with(|found| found.set(true));
        return windows::core::BOOL(0);
    }
    windows::core::BOOL(1)
}

fn has_main_window(pid: i32) -> bool {
    PID_WINDOW_FOUND.with(|found| found.set(false));
    unsafe {
        let _ = EnumWindows(Some(find_window_of_pid), LPARAM(pid as isize));
    }
    PID_WINDOW_FOUND.with(std::cell::Cell::get)
}

fn pid_exists(pid: i32) -> bool {
    snapshot_processes().iter().any(|(other, _, _)| *other == pid)
}

fn process_tree(root: i32) -> Vec<i32> {
    let all: Vec<(i32, i32)> = snapshot_processes()
        .into_iter()
        .map(|(pid, parent, _)| (pid, parent))
        .collect();
    let mut order = Vec::new();
    fn visit(pid: i32, all: &[(i32, i32)], order: &mut Vec<i32>, depth: usize) {
        if depth > 32 {
            return;
        }
        for &(child, parent) in all {
            if parent == pid && child != pid {
                visit(child, all, order, depth + 1);
            }
        }
        order.push(pid);
    }
    visit(root, &all, &mut order, 0);
    order
}

/// `SHGetKnownFolderPath` 取一个已知文件夹的宽字符串。
fn known_folder_path(id: &windows::core::GUID) -> Option<String> {
    unsafe {
        let path = SHGetKnownFolderPath(
            id,
            windows::Win32::UI::Shell::KF_FLAG_DEFAULT,
            None,
        )
        .ok()?;
        Some(take_pwstr(path))
    }
}

/// 取走 shell 分配的宽字符串（`PWSTR`）并释放。
fn take_pwstr(path: PWSTR) -> String {
    if path.is_null() {
        return String::new();
    }
    let text = unsafe {
        let mut length = 0usize;
        while *path.0.add(length) != 0 {
            length += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(path.0, length));
        CoTaskMemFree(Some(path.0 as *const core::ffi::c_void));
        text
    };
    text
}

/// 递归收集 `*.lnk`（C# `Directory.EnumerateFiles(root, "*.lnk", AllDirectories)`）。
fn walk_lnk(dir: &Path, depth: usize, out: &mut Vec<String>) {
    // C# 会跟着目录联接走，理论上能绕圈；这里限深，避免异常目录拓扑把扫描挂住。
    if depth > 16 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_lnk(&path, depth + 1, out);
        } else if path
            .extension()
            .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("lnk"))
            && let Some(text) = path.to_str()
        {
            out.push(text.to_string());
        }
    }
}

/// 读注册表某个子键的默认值。
unsafe fn registry_default_value(parent: HKEY, sub: &str) -> Option<String> {
    use windows::Win32::System::Registry::{REG_EXPAND_SZ, REG_VALUE_TYPE};
    let wide = to_wide(sub);
    let mut key = HKEY::default();
    if unsafe { RegOpenKeyExW(parent, PCWSTR(wide.as_ptr()), None, KEY_READ, &mut key) }.0 != 0 {
        return None;
    }
    let mut kind = REG_VALUE_TYPE::default();
    let mut size = 0u32;
    let probe = unsafe {
        RegQueryValueExW(key, PCWSTR::null(), None, Some(&mut kind), None, Some(&mut size))
    };
    if probe.0 != 0 || size == 0 || (kind != REG_SZ && kind != REG_EXPAND_SZ) {
        let _ = unsafe { RegCloseKey(key) };
        return None;
    }
    let mut buffer = vec![0u8; size as usize];
    let read = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR::null(),
            None,
            Some(&mut kind),
            Some(buffer.as_mut_ptr()),
            Some(&mut size),
        )
    };
    let _ = unsafe { RegCloseKey(key) };
    if read.0 != 0 {
        return None;
    }
    let units: Vec<u16> = buffer
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let text = String::from_utf16_lossy(&units);
    Some(text.trim_end_matches('\0').to_string())
}

/// `HBITMAP` → 图标位图（32 位 BGRA，自上而下）。
fn hbitmap_to_icon(bitmap: HBITMAP) -> Option<IconImage> {
    if bitmap.is_invalid() {
        return None;
    }
    let mut raw = BITMAP::default();
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(bitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut raw as *mut _ as *mut core::ffi::c_void),
        )
    };
    if got == 0 || raw.bmWidth <= 0 || raw.bmHeight <= 0 {
        return None;
    }
    let width = raw.bmWidth;
    let height = raw.bmHeight;

    let mut info = BITMAPINFO::default();
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        // 负高度 = 自上而下，省一次翻转。
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: 0,
        ..Default::default()
    };
    let stride = width as usize * 4;
    let mut pixels = vec![0u8; stride * height as usize];
    let dc = unsafe { GetDC(None) };
    let lines = unsafe {
        GetDIBits(
            dc,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr() as *mut core::ffi::c_void),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    unsafe { ReleaseDC(None, dc) };
    if lines == 0 {
        return None;
    }
    Some(IconImage::new(width, height, bgra_to_rgba(&pixels)))
}

/// 枚举 `shell:AppsFolder`（C# `ScanAppsFolder`）。
///
/// C# 走 `Shell.Application` 自动化；这里等价地用 `IShellItem` + `IEnumShellItems`
/// 枚举同一个命名空间：显示名取 `SIGDN_NORMALDISPLAY`，目标取
/// `SIGDN_DESKTOPABSOLUTEPARSING`（UWP 是 AUMID，普通 App 是 exe 路径）。
unsafe fn enumerate_apps_folder() -> Vec<(String, String)> {
    use windows::Win32::UI::Shell::KF_FLAG_DEFAULT;
    let mut entries = Vec::new();
    // 与 C# 一样跑在 STA 线程：shell 命名空间在单线程套间里最稳。
    let Some(_com) = ComApartment::enter() else {
        return entries;
    };
    if let Ok(folder) = unsafe {
        SHGetKnownFolderItem::<IShellItem>(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)
    } && let Ok(items) = unsafe {
        folder.BindToHandler::<Option<&IBindCtx>, IEnumShellItems>(None, &BHID_EnumItems)
    } {
        loop {
            let mut batch = [None];
            let mut fetched = 0u32;
            let next = unsafe { items.Next(&mut batch, Some(&mut fetched)) };
            if next.is_err() || fetched == 0 {
                break;
            }
            let Some(item) = batch[0].take() else {
                break;
            };
            let name = unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY) }
                .ok()
                .map(take_pwstr);
            let target = unsafe { item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING) }
                .ok()
                .map(take_pwstr);
            if let (Some(name), Some(target)) = (name, target) {
                let name = name.trim().to_string();
                let target = target.trim().to_string();
                if !name.is_empty() && !target.is_empty() {
                    entries.push((name, target));
                }
            }
        }
    }
    entries
}

/// COM 套间守卫（对应 C# WPF 里"UI 线程自动是 STA"这件事）。
///
/// `SHCreateItemFromParsingName` / `ShellExecuteExW` 这类 shell 调用要求调用线程
/// 进过 COM。egui/winit 的界面线程不一定做过，所以每次进 COM 路径时自己申请：
/// 拿到 S_OK/S_FALSE 的在这里配对 `CoUninitialize`；线程本来就在 MTA 里
/// （`RPC_E_CHANGED_MODE`）就沿用它、不配对。别的失败返回 `None`，调用方降级。
struct ComApartment(bool);

impl ComApartment {
    fn enter() -> Option<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        // 只有 S_OK（本次真正初始化）才配对 CoUninitialize；S_FALSE 表示线程早就
        // 初始化过，多减一次会把别人的 COM 拆了。已经在 MTA 里的线程沿用它。
        if hr == windows::Win32::Foundation::S_OK {
            Some(Self(true))
        } else if hr == windows::Win32::Foundation::S_FALSE
            || hr == windows::Win32::Foundation::RPC_E_CHANGED_MODE
        {
            Some(Self(false))
        } else {
            None
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

impl AppSystem for WinSystem {
    fn processes_named(&self, stem: &str) -> Vec<ProcessEntry> {
        let wanted = stem.to_lowercase();
        snapshot_processes()
            .into_iter()
            .filter(|(_, _, name)| exe_stem(name).to_lowercase() == wanted)
            .map(|(pid, _, _)| ProcessEntry { pid, has_main_window: has_main_window(pid) })
            .collect()
    }

    fn process_alive(&self, pid: i32) -> bool {
        pid_exists(pid)
    }

    fn image_path(&self, pid: i32) -> Option<String> {
        process_image_path(pid)
    }

    fn standard_windows(&self, include_minimized: bool) -> Vec<WindowInfo> {
        use crate::window::WindowApi;
        let desktop = crate::window::WinDesktop;
        let probes = WindowApi::probes(&desktop);
        crate::window::enumerate_standard(
            &probes,
            std::process::id() as i32,
            None,
            include_minimized,
            |pid| WindowApi::process_meta(&desktop, pid),
        )
    }

    fn z_order(&self) -> Vec<u64> {
        use crate::window::WindowApi;
        let desktop = crate::window::WinDesktop;
        crate::window::on_screen_z_order(&WindowApi::probes(&desktop))
            .into_iter()
            .map(|(handle, _)| handle)
            .collect()
    }

    fn start(
        &self,
        program: &str,
        arguments: Option<&str>,
        working_directory: Option<&str>,
    ) -> Option<i32> {
        let _com = ComApartment::enter()?;
        let file = to_wide(program);
        let parameters = arguments.map(to_wide);
        let directory = working_directory.map(to_wide);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: w!("open"),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: parameters
                .as_ref()
                .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr())),
            lpDirectory: directory
                .as_ref()
                .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr())),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        unsafe { ShellExecuteExW(&mut info) }.ok()?;

        let process = info.hProcess;
        if process.is_invalid() {
            return Some(0);
        }
        let pid = unsafe { GetProcessId(process) } as i32;
        // 进程立刻退出（UWP 交给 explorer）就拿不到 pid。
        let alive = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
        let _ = unsafe { CloseHandle(process) };
        Some(if alive { pid } else { 0 })
    }

    fn post_close(&self, handle: u64) -> bool {
        unsafe {
            PostMessageW(
                Some(HWND(handle as usize as *mut core::ffi::c_void)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            )
            .is_ok()
        }
    }

    fn focus(&self, window: WindowKey) {
        WindowControl::focus(&self.windows, window);
    }

    fn invoke_new_window(&self, handle: u64) -> bool {
        crate::uia::Uia::new()
            .map(|uia| uia.invoke_new_window(handle))
            .unwrap_or(false)
    }

    fn send_ctrl_n(&self, with_shift: bool) {
        fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        wScan: 0,
                        dwFlags: KEYBD_EVENT_FLAGS(if up { KEYEVENTF_KEYUP.0 } else { 0 }),
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }
        }
        let mut inputs = vec![key(VK_CONTROL, false)];
        if with_shift {
            inputs.push(key(VK_SHIFT, false));
        }
        inputs.push(key(VK_N, false));
        inputs.push(key(VK_N, true));
        if with_shift {
            inputs.push(key(VK_SHIFT, true));
        }
        inputs.push(key(VK_CONTROL, true));
        unsafe {
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }

    fn kill_tree(&self, pid: i32) -> bool {
        let tree = process_tree(pid);
        if tree.is_empty() {
            return false;
        }
        let mut root_ok = false;
        // 先子后父，避免父进程先死导致子进程成为孤儿。
        for target in tree {
            unsafe {
                let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, target as u32) else {
                    continue;
                };
                let ok = TerminateProcess(handle, 1).is_ok();
                let _ = CloseHandle(handle);
                if target == pid {
                    root_ok = ok;
                }
            }
        }
        root_ok
    }

    fn start_menu_roots(&self) -> Vec<String> {
        [&FOLDERID_CommonPrograms, &FOLDERID_Programs]
            .into_iter()
            .filter_map(known_folder_path)
            .filter(|path| !path.is_empty() && Path::new(path).is_dir())
            .collect()
    }

    fn lnk_files(&self, root: &str) -> Vec<String> {
        let mut files = Vec::new();
        walk_lnk(Path::new(root), 0, &mut files);
        files
    }

    fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }

    fn file_exists(&self, path: &str) -> bool {
        Path::new(path).is_file()
    }

    fn app_paths(&self, hive: Hive) -> Vec<(String, String)> {
        let root = match hive {
            Hive::LocalMachine => HKEY_LOCAL_MACHINE,
            Hive::CurrentUser => HKEY_CURRENT_USER,
        };
        let mut key = HKEY::default();
        let opened = unsafe {
            RegOpenKeyExW(
                root,
                w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths"),
                None,
                KEY_READ,
                &mut key,
            )
        };
        if opened.0 != 0 {
            return Vec::new();
        }
        let mut values = Vec::new();
        let mut index = 0u32;
        loop {
            let mut name = [0u16; 255];
            let mut length = name.len() as u32;
            let status = unsafe {
                RegEnumKeyExW(
                    key,
                    index,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut length,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if status.0 != 0 {
                break;
            }
            let sub = String::from_utf16_lossy(&name[..length as usize]);
            if let Some(value) = unsafe { registry_default_value(key, &sub) } {
                values.push((sub, value));
            }
            index += 1;
        }
        let _ = unsafe { RegCloseKey(key) };
        values
    }

    fn file_description(&self, path: &str) -> Option<String> {
        use windows::Win32::Storage::FileSystem::{
            GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
        };
        let wide_path = to_wide(path);
        unsafe {
            let size = GetFileVersionInfoSizeW(PCWSTR(wide_path.as_ptr()), None);
            if size == 0 {
                return None;
            }
            let mut block = vec![0u8; size as usize];
            GetFileVersionInfoW(
                PCWSTR(wide_path.as_ptr()),
                None,
                size,
                block.as_mut_ptr() as *mut core::ffi::c_void,
            )
            .ok()?;

            let mut translation: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut translation_len = 0u32;
            if !VerQueryValueW(
                block.as_ptr() as *const core::ffi::c_void,
                w!("\\VarFileInfo\\Translation"),
                &mut translation,
                &mut translation_len,
            )
            .as_bool()
                || translation_len < 4
                || translation.is_null()
            {
                return None;
            }
            let language = *(translation as *const u16);
            let codepage = *((translation as *const u16).add(1));
            let query = format!("\\StringFileInfo\\{language:04x}{codepage:04x}\\FileDescription");
            let wide_query = to_wide(&query);
            let mut value: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut value_len = 0u32;
            if !VerQueryValueW(
                block.as_ptr() as *const core::ffi::c_void,
                PCWSTR(wide_query.as_ptr()),
                &mut value,
                &mut value_len,
            )
            .as_bool()
                || value.is_null()
                || value_len == 0
            {
                return None;
            }
            let units = std::slice::from_raw_parts(value as *const u16, value_len as usize);
            let text = from_wide(units).trim().to_string();
            if text.is_empty() { None } else { Some(text) }
        }
    }

    fn apps_folder_entries(&self) -> Vec<(String, String)> {
        // C# 把枚举放在 STA 线程上等 4 秒；这里的枚举本身是 IShellItem 调用，
        // 但仍保留"最多等 4 秒"的行为，避免某个 shell 扩展卡住整个扫描。
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("snug-apps-folder".to_string())
            .spawn(move || {
                let entries = unsafe { enumerate_apps_folder() };
                let _ = sender.send(entries);
            });
        if spawned.is_err() {
            return Vec::new();
        }
        receiver.recv_timeout(Duration::from_secs(4)).unwrap_or_default()
    }

    fn shell_icon(&self, parsing_path: &str) -> Option<IconImage> {
        if parsing_path.trim().is_empty() {
            return None;
        }
        // 界面线程每帧都可能来取图标，这里按调用配对进出 COM 套间。
        let _com = ComApartment::enter()?;
        let wide = to_wide(parsing_path);
        let factory: IShellItemImageFactory = unsafe {
            SHCreateItemFromParsingName::<PCWSTR, Option<&IBindCtx>, IShellItemImageFactory>(
                PCWSTR(wide.as_ptr()),
                None,
            )
        }
        .ok()?;
        let (width, height, _) = shell_icon_request();
        let bitmap = unsafe {
            factory.GetImage(
                SIZE { cx: width, cy: height },
                SIIGBF(SIIGBF_ICONONLY.0 | SIIGBF_BIGGERSIZEOK.0),
            )
        }
        .ok()?;
        let image = hbitmap_to_icon(bitmap);
        let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
        image
    }

    fn exe_icon(&self, exe_path: &str) -> Option<IconImage> {
        if exe_path.is_empty() || !Path::new(exe_path).is_file() {
            return None;
        }
        let _com = ComApartment::enter()?;
        let wide = to_wide(exe_path);
        let mut icon = windows::Win32::UI::WindowsAndMessaging::HICON::default();
        let count = unsafe {
            ExtractIconExW(
                PCWSTR(wide.as_ptr()),
                0,
                Some(&mut icon),
                None,
                1,
            )
        };
        if count == 0 || icon.is_invalid() {
            return None;
        }
        let mut info = ICONINFO::default();
        let image = if unsafe { GetIconInfo(icon, &mut info) }.is_ok() {
            let image = hbitmap_to_icon(info.hbmColor);
            if !info.hbmColor.is_invalid() {
                let _ = unsafe { DeleteObject(HGDIOBJ(info.hbmColor.0)) };
            }
            if !info.hbmMask.is_invalid() {
                let _ = unsafe { DeleteObject(HGDIOBJ(info.hbmMask.0)) };
            }
            image
        } else {
            None
        };
        let _ = unsafe { DestroyIcon(icon) };
        image
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app(app_id: &str, name: &str) -> AppInfo {
        AppInfo {
            app_id: app_id.into(),
            name: name.into(),
            executable_path: None,
            launch_target: None,
            arguments: None,
            working_directory: None,
        }
    }

    fn window(pid: i32, handle: u64, app_name: &str, app_id: Option<&str>, title: &str) -> WindowInfo {
        WindowInfo {
            key: snug_core::WindowKey::new(pid, handle),
            app_name: app_name.into(),
            app_id: app_id.map(str::to_string),
            title: title.into(),
            minimized: false,
        }
    }

    #[test]
    fn path_helpers_match_dotnet() {
        assert_eq!(file_name(r"C:\Tools\app.exe"), "app.exe");
        assert_eq!(file_name("app.exe"), "app.exe");
        assert_eq!(exe_stem(r"C:\Tools\Code.Insiders.exe"), "Code.Insiders");
        assert_eq!(exe_stem("noext"), "noext");
        assert_eq!(parent_dir(r"C:\Tools\app.exe").as_deref(), Some(r"C:\Tools"));
        assert_eq!(parent_dir(r"C:\app.exe").as_deref(), Some(r"C:\"));
        assert_eq!(parent_dir("app.exe"), None);
    }

    #[test]
    fn ignored_names_and_installers_match_csharp() {
        assert!(is_ignored_shortcut_name("卸载 微信"));
        assert!(is_ignored_shortcut_name("Uninstall Foo"));
        assert!(is_ignored_shortcut_name("帮助文档"));
        assert!(is_ignored_shortcut_name("readme"));
        assert!(!is_ignored_shortcut_name("微信"));

        assert!(is_installer_or_helper("unins000.exe", "Foo"));
        assert!(is_installer_or_helper("setup.exe", "Foo"));
        assert!(is_installer_or_helper("Updater.exe", "Foo"));
        assert!(is_installer_or_helper("SnugWin.exe", "SnugWin"));
        assert!(!is_installer_or_helper("Code.exe", "VS Code"));
    }

    #[test]
    fn window_belongs_to_uses_pid_then_app_id_then_directory_then_uwp_name() {
        let code = AppInfo { executable_path: Some(r"C:\Apps\VSCode\Code.exe".into()), ..app("Code.exe", "VS Code") };

        // 1) 启动时拿到的 pid 直接命中。
        let w = window(100, 1, "Code", Some("Code.exe"), "a.rs");
        assert!(window_belongs_to(&code, Some(100), &w, |_| None));

        // 2) AppId 相同。
        assert!(window_belongs_to(&code, None, &w, |_| None));

        // 3) 进程镜像在 App 目录下（例如 Code 的辅助进程）。
        let helper = window(101, 2, "Code Helper", Some("CodeHelper.exe"), "x");
        assert!(window_belongs_to(&code, None, &helper, |pid| {
            (pid == 101).then(|| PathBuf::from(r"C:\Apps\VSCode\helper.exe"))
        }));

        // 4) 都不匹配。
        let other = window(102, 3, "Notepad", Some("notepad.exe"), "t");
        assert!(!window_belongs_to(&code, None, &other, |_| None));
    }

    #[test]
    fn window_belongs_to_matches_uwp_by_display_name() {
        let wt = AppInfo {
            executable_path: None,
            launch_target: Some("explorer.exe".into()),
            arguments: Some("shell:AppsFolder\\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App".into()),
            ..app("Microsoft.WindowsTerminal_8wekyb3d8bbwe!App", "Windows Terminal")
        };
        let w = window(7, 9, "Windows Terminal", Some("WindowsTerminal.exe"), "终端");
        assert!(window_belongs_to(&wt, None, &w, |_| None));
        let other = window(7, 9, "Notepad", Some("notepad.exe"), "记事本");
        assert!(!window_belongs_to(&wt, None, &other, |_| None));
    }

    #[test]
    fn installed_collector_dedupes_like_csharp() {
        let mut collector = InstalledCollector::new();
        assert!(collector.add_shortcut("Code.exe", "VS Code", r"C:\Code\Code.exe", r"C:\lnk\Code.lnk"));
        // 同一个 exe 不再收。
        assert!(!collector.add_shortcut("code.exe", "VS Code 2", r"C:\Code2.exe", "x"));
        assert!(!collector.add_app_path("code.exe", "dup", r"C:\Code3.exe"));

        // App Paths 的新 exe 可以进。
        assert!(collector.add_app_path("Notepad.exe", "记事本", r"C:\Windows\notepad.exe"));

        // AppsFolder：名字已存在（忽略大小写）跳过。
        assert!(!collector.add_apps_folder("vs code", r"C:\Other\Code.exe", true));
        // 被忽略的名字。
        assert!(!collector.add_apps_folder("Uninstall Foo", r"C:\Foo\unins000.exe", true));
        // 目标 exe 不存在（C# `File.Exists` 那道闸）。
        assert!(!collector.add_apps_folder("Ghost", r"C:\Ghost\ghost.exe", false));
        // UWP 目标。
        assert!(collector.add_apps_folder(
            "Windows Terminal",
            "Microsoft.WindowsTerminal_8wekyb3d8bbwe!App",
            true
        ));
        // AUMID 已存在。
        assert!(!collector.add_apps_folder(
            "Windows Terminal 2",
            "Microsoft.WindowsTerminal_8wekyb3d8bbwe!App",
            true
        ));

        let apps = collector.finish();
        assert_eq!(apps.len(), 3);
        assert!(apps.iter().any(|app| app.app_id == "Code.exe"));
        assert!(apps.iter().any(|app| app.app_id == "Microsoft.WindowsTerminal_8wekyb3d8bbwe!App"));
        // UWP 用 explorer 启动。
        let uwp = apps
            .iter()
            .find(|app| app.app_id.contains('!'))
            .expect("UWP app");
        assert_eq!(uwp.launch_target.as_deref(), Some("explorer.exe"));
        assert!(uwp.arguments.as_deref().unwrap_or("").starts_with("shell:AppsFolder\\"));
    }

    #[test]
    fn grouping_uses_app_id_then_pid_and_keeps_first_window_as_face() {
        let windows = vec![
            window(10, 1, "Code", Some("Code.exe"), "a.rs"),
            window(10, 2, "Code", Some("Code.exe"), "b.rs"),
            window(11, 3, "<unknown>", None, "x"),
        ];
        let running = group_running_apps(&windows, |pid| {
            (pid == 10).then(|| PathBuf::from(r"C:\Code\Code.exe"))
        });
        assert_eq!(running.len(), 2);
        assert_eq!(running[0].app.app_id, "Code.exe");
        assert_eq!(running[0].windows.len(), 2);
        assert_eq!(running[1].app.app_id, "<unknown>", "没有 AppId 时用进程号分组，显示名取 AppName");
    }

    #[test]
    fn pick_pid_prefers_a_process_with_a_main_window() {
        assert_eq!(pick_pid(&[1, 2, 3], |pid| pid == 2), Some(2));
        assert_eq!(pick_pid(&[1, 2], |_| false), Some(1));
        assert_eq!(pick_pid(&[], |_| true), None);
    }

    #[test]
    fn icon_helpers_convert_bgra_and_pick_parsing_path() {
        // BGRA → RGBA，预乘 alpha 原样保留（egui 也吃预乘）。
        assert_eq!(bgra_to_rgba(&[0, 0, 255, 255]), vec![255, 0, 0, 255]);
        assert_eq!(bgra_to_rgba(&[0, 0, 128, 128]), vec![128, 0, 0, 128]);
        assert_eq!(bgra_to_rgba(&[0, 0, 0, 0]), vec![0, 0, 0, 0]);

        assert_eq!(icon_box_size(), (128, 128));
        assert_eq!(shell_icon_request(), (128, 128, SIIGBF_BIGGER_SIZE_OK | SIIGBF_ICON_ONLY));

        let uwp = AppInfo {
            executable_path: None,
            launch_target: Some("explorer.exe".into()),
            arguments: None,
            ..app("Aumid!App", "UWP")
        };
        assert_eq!(icon_parsing_path(&uwp, None), None, "explorer.exe 不是 shell: 目标");

        let shell = AppInfo {
            launch_target: Some("shell:AppsFolder\\Aumid!App".into()),
            ..app("Aumid!App", "UWP")
        };
        assert_eq!(
            icon_parsing_path(&shell, None).as_deref(),
            Some("shell:AppsFolder\\Aumid!App")
        );

        let code = app("Code.exe", "VS Code");
        assert_eq!(icon_parsing_path(&code, Some(r"C:\Code\Code.exe")).as_deref(), Some(r"C:\Code\Code.exe"));
    }

    #[test]
    fn icon_image_consistency_check() {
        let ok = IconImage::new(2, 2, vec![0; 16]);
        assert!(ok.is_consistent());
        let bad = IconImage::new(2, 2, vec![0; 15]);
        assert!(!bad.is_consistent());
    }

    // ---- .lnk 解析 ----

    /// 拼一个最小可用的 Shell Link：LinkInfo + 本地 ANSI/Unicode 路径。
    fn build_lnk(local_path: Option<&str>, unicode: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        // Header
        bytes.extend_from_slice(&(LNK_HEADER_SIZE as u32).to_le_bytes());
        bytes.extend_from_slice(&LNK_CLSID);
        bytes.extend_from_slice(&LNK_HAS_LINK_INFO.to_le_bytes()); // LinkFlags
        bytes.extend_from_slice(&[0u8; 0x34]); // FileAttributes..Reserved

        // LinkInfo
        let header_size: u32 = if unicode { 0x24 } else { 0x1C };
        let path_bytes: Vec<u8> = if unicode {
            let mut data = Vec::new();
            for unit in local_path.unwrap_or("").encode_utf16() {
                data.extend_from_slice(&unit.to_le_bytes());
            }
            data.extend_from_slice(&0u16.to_le_bytes());
            data
        } else {
            let mut data = local_path.unwrap_or("").as_bytes().to_vec();
            data.push(0);
            data
        };
        let ansi_offset: u32 = if unicode { 0 } else { header_size };
        let unicode_offset: u32 = if unicode { header_size } else { 0 };
        let link_info_size = header_size + path_bytes.len() as u32;

        bytes.extend_from_slice(&link_info_size.to_le_bytes());
        bytes.extend_from_slice(&header_size.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes()); // VolumeIDAndLocalBasePath
        bytes.extend_from_slice(&0u32.to_le_bytes()); // VolumeIDOffset
        bytes.extend_from_slice(&ansi_offset.to_le_bytes()); // LocalBasePathOffset
        bytes.extend_from_slice(&0u32.to_le_bytes()); // CommonNetworkRelativeLinkOffset
        bytes.extend_from_slice(&0u32.to_le_bytes()); // CommonPathSuffixOffset
        if unicode {
            // header 0x24：LocalBasePathOffsetUnicode + CommonPathSuffixOffsetUnicode
            bytes.extend_from_slice(&unicode_offset.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
        }
        bytes.extend_from_slice(&path_bytes);
        bytes
    }

    #[test]
    fn lnk_parser_reads_ansi_and_unicode_local_paths() {
        let ansi = build_lnk(Some(r"C:\Program Files\Foo\foo.exe"), false);
        assert_eq!(
            parse_lnk_target(&ansi),
            Some(PathBuf::from(r"C:\Program Files\Foo\foo.exe"))
        );

        let unicode = build_lnk(Some(r"C:\程序\工具\工具.exe"), true);
        assert_eq!(
            parse_lnk_target(&unicode),
            Some(PathBuf::from(r"C:\程序\工具\工具.exe"))
        );
    }

    #[test]
    fn lnk_parser_rejects_bad_input() {
        assert_eq!(parse_lnk_target(&[]), None);
        // CLSID 不对。
        let mut bad = build_lnk(Some(r"C:\a.exe"), false);
        bad[4] ^= 0xFF;
        assert_eq!(parse_lnk_target(&bad), None);
        // 没有 LinkInfo 标志。
        let mut no_info = build_lnk(Some(r"C:\a.exe"), false);
        no_info[0x14..0x18].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse_lnk_target(&no_info), None);
        // 截断。
        let mut short = build_lnk(Some(r"C:\a.exe"), false);
        short.truncate(0x50);
        assert_eq!(parse_lnk_target(&short), None);
    }

    #[test]
    fn launch_command_prefers_launch_target_then_executable() {
        let uwp = AppInfo {
            executable_path: None,
            launch_target: Some("explorer.exe".into()),
            arguments: Some("shell:AppsFolder\\A!App".into()),
            ..app("A!App", "A")
        };
        assert_eq!(
            launch_command(&uwp),
            ("explorer.exe".to_string(), Some("shell:AppsFolder\\A!App".to_string()))
        );
        let plain = AppInfo { executable_path: Some(r"C:\a\a.exe".into()), ..app("a.exe", "A") };
        assert_eq!(launch_command(&plain), (r"C:\a\a.exe".to_string(), None));
    }

    // ---- Win32 接线（用假系统，绝不碰真机） ----

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum StartBehavior {
        /// 启动成功，拿到这个 pid。
        Pid(i32),
        /// 启动后进程立刻退出（UWP 交给 explorer），拿不到 pid。
        Handoff,
        /// 抛异常。
        Fail,
    }

    impl Default for StartBehavior {
        fn default() -> Self {
            Self::Fail
        }
    }

    /// 假系统：显式喂给它的每一条数据都对应 C# 里一次系统调用。
    #[derive(Default)]
    struct FakeSystem {
        /// `(进程名小写, 进程)`；进程名不带 .exe，与 `GetProcessesByName` 一致。
        processes: Vec<(String, ProcessEntry)>,
        alive: Vec<i32>,
        images: HashMap<i32, String>,
        windows: Vec<WindowInfo>,
        z_order: Vec<u64>,
        start_behavior: StartBehavior,
        started: std::sync::Mutex<Vec<(String, Option<String>, Option<String>)>>,
        closed: std::sync::Mutex<Vec<u64>>,
        focused: std::sync::Mutex<Vec<WindowKey>>,
        ctrl_n: std::sync::Mutex<Vec<bool>>,
        killed: std::sync::Mutex<Vec<i32>>,
        roots: Vec<String>,
        lnks: HashMap<String, Vec<String>>,
        files: HashMap<String, Vec<u8>>,
        exists: Vec<String>,
        app_paths_lm: Vec<(String, String)>,
        app_paths_cu: Vec<(String, String)>,
        descriptions: HashMap<String, String>,
        apps_folder: Vec<(String, String)>,
        shell_icons: HashMap<String, IconImage>,
        exe_icons: HashMap<String, IconImage>,
        icon_requests: std::sync::Mutex<Vec<String>>,
    }

    impl FakeSystem {
        fn window(&mut self, pid: i32, handle: u64, app_name: &str, app_id: Option<&str>, title: &str) {
            self.windows.push(window(pid, handle, app_name, app_id, title));
        }

        fn started(&self) -> std::sync::MutexGuard<'_, Vec<(String, Option<String>, Option<String>)>> {
            self.started.lock().unwrap()
        }
    }

    impl AppSystem for FakeSystem {
        fn processes_named(&self, stem: &str) -> Vec<ProcessEntry> {
            self.processes
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case(stem))
                .map(|(_, entry)| *entry)
                .collect()
        }

        fn process_alive(&self, pid: i32) -> bool {
            self.alive.contains(&pid)
        }

        fn image_path(&self, pid: i32) -> Option<String> {
            self.images.get(&pid).cloned()
        }

        fn standard_windows(&self, include_minimized: bool) -> Vec<WindowInfo> {
            self.windows
                .iter()
                .filter(|window| include_minimized || !window.minimized)
                .cloned()
                .collect()
        }

        fn z_order(&self) -> Vec<u64> {
            self.z_order.clone()
        }

        fn start(
            &self,
            program: &str,
            arguments: Option<&str>,
            working_directory: Option<&str>,
        ) -> Option<i32> {
            self.started.lock().unwrap().push((
                program.to_string(),
                arguments.map(str::to_string),
                working_directory.map(str::to_string),
            ));
            match self.start_behavior {
                StartBehavior::Pid(pid) => Some(pid),
                StartBehavior::Handoff => Some(0),
                StartBehavior::Fail => None,
            }
        }

        fn post_close(&self, handle: u64) -> bool {
            self.closed.lock().unwrap().push(handle);
            true
        }

        fn focus(&self, window: WindowKey) {
            self.focused.lock().unwrap().push(window);
        }

        fn invoke_new_window(&self, _handle: u64) -> bool {
            false
        }

        fn send_ctrl_n(&self, with_shift: bool) {
            self.ctrl_n.lock().unwrap().push(with_shift);
        }

        fn kill_tree(&self, pid: i32) -> bool {
            self.killed.lock().unwrap().push(pid);
            true
        }

        fn start_menu_roots(&self) -> Vec<String> {
            self.roots.clone()
        }

        fn lnk_files(&self, root: &str) -> Vec<String> {
            self.lnks.get(root).cloned().unwrap_or_default()
        }

        fn read_file(&self, path: &str) -> Option<Vec<u8>> {
            self.files.get(path).cloned()
        }

        fn file_exists(&self, path: &str) -> bool {
            self.files.contains_key(path) || self.exists.iter().any(|known| known == path)
        }

        fn app_paths(&self, hive: Hive) -> Vec<(String, String)> {
            match hive {
                Hive::LocalMachine => self.app_paths_lm.clone(),
                Hive::CurrentUser => self.app_paths_cu.clone(),
            }
        }

        fn file_description(&self, path: &str) -> Option<String> {
            self.descriptions.get(path).cloned()
        }

        fn apps_folder_entries(&self) -> Vec<(String, String)> {
            self.apps_folder.clone()
        }

        fn shell_icon(&self, parsing_path: &str) -> Option<IconImage> {
            self.icon_requests
                .lock()
                .unwrap()
                .push(format!("shell:{parsing_path}"));
            self.shell_icons.get(parsing_path).cloned()
        }

        fn exe_icon(&self, exe_path: &str) -> Option<IconImage> {
            self.icon_requests
                .lock()
                .unwrap()
                .push(format!("exe:{exe_path}"));
            self.exe_icons.get(exe_path).cloned()
        }
    }

    #[test]
    fn scan_installed_apps_runs_three_rounds_like_csharp() {
        let mut system = FakeSystem::default();
        system.roots = vec![r"C:\StartMenu".into()];
        // 第一轮：一条正常快捷方式、一条安装器、一条被忽略的名字、一条目标不存在。
        let code_lnk = r"C:\StartMenu\VS Code.lnk";
        system.files.insert(code_lnk.into(), build_lnk(Some(r"C:\Apps\VSCode\Code.exe"), true));
        system.exists.push(r"C:\Apps\VSCode\Code.exe".into());
        let setup_lnk = r"C:\StartMenu\Foo Setup.lnk";
        system.files.insert(setup_lnk.into(), build_lnk(Some(r"C:\Foo\setup.exe"), true));
        system.exists.push(r"C:\Foo\setup.exe".into());
        let ignored_lnk = r"C:\StartMenu\卸载 Foo.lnk";
        system.files.insert(ignored_lnk.into(), build_lnk(Some(r"C:\Foo\foo.exe"), true));
        system.exists.push(r"C:\Foo\foo.exe".into());
        let gone_lnk = r"C:\StartMenu\Gone.lnk";
        system.files.insert(gone_lnk.into(), build_lnk(Some(r"C:\Gone\gone.exe"), true));
        system.lnks.insert(
            r"C:\StartMenu".into(),
            vec![
                code_lnk.into(),
                setup_lnk.into(),
                ignored_lnk.into(),
                gone_lnk.into(),
            ],
        );

        // 第二轮：HKLM 的记事本带文件描述；HKCU 里 Code.exe 是重复项。
        system.app_paths_lm = vec![("Notepad.exe".into(), r"C:\Windows\notepad.exe".into())];
        system.app_paths_cu = vec![
            ("Code.exe".into(), r"C:\Other\Code.exe".into()),
            ("Zed.exe".into(), r"C:\Zed\zed.exe".into()),
        ];
        system.exists.push(r"C:\Windows\notepad.exe".into());
        system.exists.push(r"C:\Zed\zed.exe".into());
        system
            .descriptions
            .insert(r"C:\Windows\notepad.exe".into(), "记事本".into());

        // 第三轮：一个 UWP、一个重名、一个不存在的 exe、一个安装器。
        system.apps_folder = vec![
            (
                "Windows Terminal".into(),
                "Microsoft.WindowsTerminal_8wekyb3d8bbwe!App".into(),
            ),
            ("vs code".into(), r"C:\Other\Code.exe".into()),
            ("Ghost".into(), r"C:\Ghost\ghost.exe".into()),
            ("Uninstall Foo".into(), r"C:\Foo\unins000.exe".into()),
        ];

        let apps = Apps::with_system(system).scan_installed_apps();
        let ids: Vec<&str> = apps.iter().map(|app| app.app_id.as_str()).collect();
        assert!(ids.contains(&"Code.exe"), "{ids:?}");
        assert!(ids.contains(&"Notepad.exe"), "{ids:?}");
        assert!(ids.contains(&"Zed.exe"), "{ids:?}");
        assert!(ids.contains(&"Microsoft.WindowsTerminal_8wekyb3d8bbwe!App"), "{ids:?}");
        // 安装器 / 被忽略名字 / 目标不存在 / 重名都不进列表。
        assert!(!ids.contains(&"setup.exe"));
        assert!(!ids.contains(&"foo.exe"));
        assert!(!ids.contains(&"gone.exe"));
        assert!(!ids.contains(&"ghost.exe"));
        assert!(!ids.contains(&"unins000.exe"));

        let code = apps.iter().find(|app| app.app_id == "Code.exe").unwrap();
        assert_eq!(code.executable_path.as_deref(), Some(r"C:\Apps\VSCode\Code.exe"));
        assert_eq!(code.launch_target.as_deref(), Some(code_lnk));
        assert_eq!(code.name, "VS Code");

        let notepad = apps.iter().find(|app| app.app_id == "Notepad.exe").unwrap();
        assert_eq!(notepad.name, "记事本", "FileDescription 优先于文件名");

        let uwp = apps
            .iter()
            .find(|app| app.app_id.ends_with("!App"))
            .unwrap();
        assert_eq!(uwp.launch_target.as_deref(), Some("explorer.exe"));
        assert_eq!(
            uwp.arguments.as_deref(),
            Some("shell:AppsFolder\\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App")
        );
    }

    #[test]
    fn running_apps_follow_screen_z_order() {
        let mut system = FakeSystem::default();
        system.window(10, 1, "Code", Some("Code.exe"), "a.rs");
        system.window(20, 2, "Zed", Some("Zed.exe"), "b.rs");
        system.images.insert(10, r"C:\Code\Code.exe".into());
        system.images.insert(20, r"C:\Zed\zed.exe".into());
        // 前面的是 2 号窗口。
        system.z_order = vec![2, 1];

        let running = Apps::with_system(system).running_apps();
        assert_eq!(running.len(), 2);
        assert_eq!(running[0].app.app_id, "Zed.exe");
        assert_eq!(running[1].app.app_id, "Code.exe");
        assert_eq!(running[0].app.executable_path.as_deref(), Some(r"C:\Zed\zed.exe"));
    }

    #[test]
    fn app_control_queries_and_launches() {
        let mut system = FakeSystem::default();
        system.processes = vec![
            ("code".into(), ProcessEntry { pid: 10, has_main_window: false }),
            ("code".into(), ProcessEntry { pid: 11, has_main_window: true }),
        ];
        system.alive = vec![10, 11];
        system.window(10, 101, "Code", Some("Code.exe"), "a.rs");
        system.start_behavior = StartBehavior::Pid(42);
        let apps = Apps::with_system(system);

        let code = AppInfo {
            executable_path: Some(r"C:\Apps\VSCode\Code.exe".into()),
            ..app("Code.exe", "VS Code")
        };
        assert_eq!(apps.pid_of(&code), Some(11), "优先有主窗口的进程");
        assert_eq!(apps.pids("Code.exe"), vec![10, 11]);
        assert!(apps.is_running(10));
        assert!(!apps.is_running(99));

        // 普通 App：直接返回启动进程的 pid。
        assert_eq!(apps.open(&code), Some(42));
        {
            let started = apps.system().started();
            assert_eq!(started[0].0, r"C:\Apps\VSCode\Code.exe");
        }

        // UWP：explorer + shell:AppsFolder 参数，交接后返回 0。
        let handoff = Apps::with_system(FakeSystem {
            start_behavior: StartBehavior::Handoff,
            ..FakeSystem::default()
        });
        let uwp = AppInfo {
            executable_path: None,
            launch_target: Some("explorer.exe".into()),
            arguments: Some("shell:AppsFolder\\A!App".into()),
            ..app("A!App", "UWP")
        };
        assert_eq!(handoff.open(&uwp), Some(0));
        {
            let started = handoff.system().started();
            assert_eq!(started[0].0, "explorer.exe");
            assert_eq!(started[0].1.as_deref(), Some("shell:AppsFolder\\A!App"));
        }

        // 普通 App 被既有实例接管：按 exe 名找回进程。
        let mut handoff = FakeSystem::default();
        handoff.start_behavior = StartBehavior::Handoff;
        handoff.processes = vec![("code".into(), ProcessEntry { pid: 55, has_main_window: true })];
        let apps = Apps::with_system(handoff);
        assert_eq!(apps.open(&code), Some(55));

        // 还是找不到就给 0（C# `found?.Id ?? 0`）。
        let mut handoff = FakeSystem::default();
        handoff.start_behavior = StartBehavior::Handoff;
        let apps = Apps::with_system(handoff);
        assert_eq!(apps.open(&code), Some(0));

        // 启动异常：None。
        let mut failing = FakeSystem::default();
        failing.start_behavior = StartBehavior::Fail;
        let apps = Apps::with_system(failing);
        assert_eq!(apps.open(&code), None);
    }

    #[test]
    fn quit_new_window_and_force_quit() {
        let mut system = FakeSystem::default();
        system.alive = vec![10];
        system.window(10, 101, "Code", Some("Code.exe"), "a.rs");
        system.window(10, 102, "Code", Some("Code.exe"), "b.rs");
        let apps = Apps::with_system(system);

        assert!(apps.quit(10));
        assert!(apps.quit(99), "进程已经没了也算退出成功");
        assert_eq!(*apps.system().closed.lock().unwrap(), vec![101], "只给主窗口发 WM_CLOSE");

        // 新窗口：聚焦第一个窗口，再发 Ctrl+N；Code.exe 这类是 Ctrl+Shift+N。
        assert!(apps.new_window(10));
        assert_eq!(apps.system().focused.lock().unwrap().len(), 1);
        assert_eq!(*apps.system().ctrl_n.lock().unwrap(), vec![true]);

        // 普通应用是 Ctrl+N。
        let mut plain_system = FakeSystem::default();
        plain_system.window(10, 1, "Notepad", Some("notepad.exe"), "a.txt");
        let apps = Apps::with_system(plain_system);
        assert!(apps.new_window(10));
        assert_eq!(*apps.system().ctrl_n.lock().unwrap(), vec![false]);

        // 没有窗口时直接失败，不发快捷键。
        let mut empty = FakeSystem::default();
        empty.window(99, 1, "Other", Some("Other.exe"), "x");
        let apps = Apps::with_system(empty);
        assert!(!apps.new_window(10));
        assert!(apps.system().ctrl_n.lock().unwrap().is_empty());

        assert!(apps.force_quit(10));
        assert_eq!(*apps.system().killed.lock().unwrap(), vec![10]);
    }

    #[test]
    fn app_windows_match_pid_app_id_directory_and_uwp_name() {
        let mut system = FakeSystem::default();
        system.window(10, 1, "Code", Some("Code.exe"), "a.rs");
        system.window(11, 2, "Code Helper", Some("CodeHelper.exe"), "b.rs");
        system.window(12, 3, "Notepad", Some("notepad.exe"), "c.rs");
        system.images.insert(11, r"C:\Apps\VSCode\helper.exe".into());
        let apps = Apps::with_system(system);

        let code = AppInfo {
            executable_path: Some(r"C:\Apps\VSCode\Code.exe".into()),
            ..app("Code.exe", "VS Code")
        };
        let matched = apps.app_windows(&code, None);
        let pids: Vec<i32> = matched.iter().map(|window| window.key.pid).collect();
        assert_eq!(pids, vec![10, 11], "AppId 命中 + 安装目录命中");

        // 启动时拿到的 pid 直接算命中。
        let matched = apps.app_windows(&code, Some(12));
        assert!(matched.iter().any(|window| window.key.pid == 12));

        // UWP 按名字匹配。
        let uwp = AppInfo {
            executable_path: None,
            launch_target: Some("explorer.exe".into()),
            arguments: Some("shell:AppsFolder\\X!App".into()),
            ..app("X!App", "Notepad")
        };
        let matched = apps.app_windows(&uwp, None);
        assert!(matched.iter().any(|window| window.key.pid == 12));
    }

    #[test]
    fn icons_try_shell_then_exe_resource() {
        let icon = IconImage::new(1, 1, vec![1, 2, 3, 4]);

        // shell: 目标优先。
        let mut system = FakeSystem::default();
        system.shell_icons.insert(
            "shell:AppsFolder\\A!App".into(),
            icon.clone(),
        );
        let apps = Apps::with_system(system);
        let uwp = AppInfo {
            executable_path: None,
            launch_target: Some("shell:AppsFolder\\A!App".into()),
            ..app("A!App", "UWP")
        };
        assert_eq!(apps.icon(&uwp, None), Some(icon.clone()));
        assert_eq!(
            *apps.system().icon_requests.lock().unwrap(),
            vec!["shell:shell:AppsFolder\\A!App".to_string()]
        );

        // shell 取不到时退到 exe 资源图标。
        let mut system = FakeSystem::default();
        system
            .exe_icons
            .insert(r"C:\Apps\VSCode\Code.exe".into(), icon.clone());
        let apps = Apps::with_system(system);
        let code = AppInfo {
            executable_path: Some(r"C:\Apps\VSCode\Code.exe".into()),
            ..app("Code.exe", "VS Code")
        };
        assert_eq!(apps.icon(&code, None), Some(icon.clone()));

        // 没有 pid 时，exe 路径来自进程镜像。
        let mut system = FakeSystem::default();
        system.images.insert(10, r"C:\Zed\zed.exe".into());
        system.exe_icons.insert(r"C:\Zed\zed.exe".into(), icon.clone());
        let apps = Apps::with_system(system);
        let running = app("Zed.exe", "Zed");
        assert_eq!(apps.icon(&running, Some(10)), Some(icon.clone()));

        // 什么都取不到：None（C# 画占位图，那是界面层的事）。
        let apps = Apps::with_system(FakeSystem::default());
        assert_eq!(apps.icon(&running, None), None);
    }

    #[test]
    fn empty_system_is_survivable() {
        let apps = Apps::with_system(FakeSystem::default());
        assert!(apps.scan_installed_apps().is_empty());
        assert!(apps.running_apps().is_empty());
        assert_eq!(apps.pid_of(&app("x.exe", "X")), None);
        assert!(apps.pids("x.exe").is_empty());
        assert_eq!(apps.icon(&app("x.exe", "X"), Some(1234)), None);
        assert!(apps.app_windows(&app("x.exe", "X"), None).is_empty());
        assert!(apps.quit(1234));
        assert!(!apps.new_window(1234));
    }
}
