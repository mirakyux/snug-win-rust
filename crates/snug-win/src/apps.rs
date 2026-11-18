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

use snug_core::apps::sorted as sorted_apps;
use snug_core::windows::{AppInfo, RunningApp, WindowInfo};

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
    pub fn add_apps_folder(&mut self, name: &str, target: &str) -> bool {
        if is_ignored_shortcut_name(name) || self.contains_name(name) {
            return false;
        }
        if target.to_lowercase().ends_with(".exe") {
            let exe = file_name(target).to_string();
            if self.contains_id(&exe) || is_installer_or_helper(&exe, name) {
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
        assert!(!collector.add_apps_folder("vs code", r"C:\Other\Code.exe"));
        // 被忽略的名字。
        assert!(!collector.add_apps_folder("Uninstall Foo", r"C:\Foo\unins000.exe"));
        // UWP 目标。
        assert!(collector.add_apps_folder(
            "Windows Terminal",
            "Microsoft.WindowsTerminal_8wekyb3d8bbwe!App"
        ));
        // AUMID 已存在。
        assert!(!collector.add_apps_folder(
            "Windows Terminal 2",
            "Microsoft.WindowsTerminal_8wekyb3d8bbwe!App"
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

}
