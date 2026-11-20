//! 虚拟屏驱动的自动安装。
//!
//! 用户不该为了装个驱动先去别处找文件、也不该知道 `\\\\.\\pipe\\virtualdisplaydriver`
//! 这种东西。程序本身清单就是 `requireAdministrator`，所以**能自己装**：
//!
//! 1. 启动时探一下命名管道（`snug_win::vdd::is_driver_pipe_available`）；
//! 2. 没有，就在后台释放 exe 内嵌的驱动包，跑 `Install-SnugDriver.ps1 -Action Install`；
//! 3. 脚本自己校验安装包与证书的 SHA-256，装完等管道起来，然后把手让给建屏流程。
//! 4. 子进程结束后清理临时脚本、压缩包和解压目录，exe 旁无需 `driver\`。
//!
//! 全程在后台线程：装驱动要几十秒（msiexec），不能卡住界面。
//! 失败了也不拦着程序启动——建屏的重试阶梯还在，设置页里也能手动再来一次。

use std::path::PathBuf;
use std::time::{Duration, Instant};
use snug_win::system_paths::{create_protected_directory, powershell, system_directory};

/// 装完等管道就绪的上限。msiexec 装完到服务起管道通常几秒，给足余量。
const PIPE_WAIT: Duration = Duration::from_secs(40);

const DRIVER_ARCHIVE: &[u8] =
    include_bytes!("../../../driver/virtual-desktop-driver-installer-x64.zip");
const DRIVER_SCRIPT: &[u8] = include_bytes!("../../../driver/Install-SnugDriver.ps1");
const DRIVER_ARCHIVE_NAME: &str = "virtual-desktop-driver-installer-x64.zip";
const DRIVER_SCRIPT_NAME: &str = "Install-SnugDriver.ps1";
static INSTALL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 独占的临时安装目录；只在安装期间存在，不依赖 exe 所在目录可写。
struct BundledDriver {
    directory: PathBuf,
}

impl BundledDriver {
    fn prepare() -> std::io::Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_nanos();
        // 不使用 TEMP/TMP、Windows\Temp 或 ProgramData：这些父目录可能允许普通用户
        // 创建/删除子项。直接放在受系统 ACL 保护的 System32 下，避免父级 DELETE_CHILD 竞态。
        let directory = system_directory()?.join(format!("snug-driver-{}-{nonce}", std::process::id()));
        create_protected_directory(&directory)?;
        Self::write_contents(directory)
    }

    fn write_contents(directory: PathBuf) -> std::io::Result<Self> {
        let bundle = Self { directory };
        std::fs::write(bundle.script(), DRIVER_SCRIPT)?;
        std::fs::write(bundle.directory.join(DRIVER_ARCHIVE_NAME), DRIVER_ARCHIVE)?;
        Ok(bundle)
    }

    fn script(&self) -> PathBuf {
        self.directory.join(DRIVER_SCRIPT_NAME)
    }
}

impl Drop for BundledDriver {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory) {
            crate::warn!("驱动：清理临时安装目录 {} 失败：{error}", self.directory.display());
        }
    }
}

/// 驱动管道在不在（也就是"驱动装好并且服务起着"）。
pub fn pipe_available() -> bool {
    snug_win::vdd::is_driver_pipe_available(snug_win::vdd::DEFAULT_PIPE_NAME)
}

/// 驱动**装没装**（注册表里有没有那块设备），和管道起没起来是两件事。
fn driver_installed() -> bool {
    snug_win::display::virtual_display_instance_id().is_some()
}

/// 等命名管道起来，最多等 `budget`。
fn wait_for_pipe(budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if pipe_available() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    pipe_available()
}

/// 没驱动就自动装一次。返回 `true` 表示最后管道可用（本来就可用，或装成功了）。
///
/// 这个函数会阻塞到安装结束，调用方要放在后台线程里。
pub fn ensure_installed() -> bool {
    if pipe_available() {
        return true;
    }
    // **驱动装着、只是管道还没起来**：登录早期最常见（用户态服务还没把管道拉出来），
    // 开机自启正好卡在这个时间点上。这时候绝不能重装一遍驱动——msiexec 会跑一次
    // 修复安装，慢且没有任何意义。等一小会儿，等不到就交给建屏的重试阶梯
    // （`5/15/30` 秒），或者让用户重启一下设备。
    if driver_installed() {
        crate::info!("驱动：已安装，但命名管道还没就绪（登录早期常见），等一会…");
        if wait_for_pipe(Duration::from_secs(20)) {
            crate::info!("驱动：命名管道已就绪");
            return true;
        }
        crate::warn!(
            "驱动：等 20 秒管道仍没起来，先不重装（驱动是装着的）。\
             手机屏会按重试阶梯继续尝试；实在不行在设备管理器里重启一次 Cherry Virtual Display。"
        );
        return false;
    }
    install_bundled()
}

/// 自动安装与设置页的显式安装共用内嵌包；等待子进程退出后才释放临时目录。
fn install_bundled() -> bool {
    let _install_guard = match INSTALL_LOCK.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            crate::info!("驱动：安装任务已在进行，跳过重复请求");
            return false;
        }
    };
    let bundle = match BundledDriver::prepare() {
        Ok(bundle) => bundle,
        Err(error) => {
            crate::warn!("驱动：释放内嵌安装包失败：{error}");
            return false;
        }
    };
    let script = bundle.script();
    crate::info!("驱动：使用内嵌驱动安装包（{}）", script.display());
    let started = Instant::now();
    // 用 `-Command` 包一层，而不是 `-File`：顺手把子进程的控制台输出编码钉成 UTF-8。
    //
    // 不钉的话输出按系统代码页（简中 936/GBK）编码，我们按 UTF-8 读会整段变成替换符，
    // 日志里就只剩「驱动：����λ��…」——真机上安装失败时用户完全看不出哪里出错。
    let escaped = script.to_string_lossy().replace('\'', "''");
    let command = format!(
        "[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false); \
         & '{escaped}' -Action Install; exit $LASTEXITCODE"
    );
    let powershell = match powershell() {
        Ok(path) => path,
        Err(error) => {
            crate::warn!("驱动：无法定位系统 PowerShell：{error}");
            return false;
        }
    };
    let output = std::process::Command::new(powershell)
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &command])
        // **别把 PowerShell 7 的 `PSModulePath` 传下去。**
        // 真机踩到过：本程序的父进程若是 PS7（开发环境、或从 PS7 里启动的批处理），
        // 子进程 `powershell.exe`（5.1）会先在 PS7 的模块目录里找 `Microsoft.PowerShell.Utility`，
        // 那个模块是 PS7 专用的、5.1 加载不了，于是 `Get-FileHash` 这类 cmdlet 全部"不存在"，
        // 安装脚本在校验哈希那一步直接失败。删掉这个变量，5.1 会用它自己的默认模块路径。
        .env_remove("PSModulePath")
        .creation_flags_hidden()
        .output();
    match output {
        Ok(output) => {
            let ms = started.elapsed().as_millis();
            let text = decode_console(&output.stdout);
            let err = decode_console(&output.stderr);
            for line in text.lines().chain(err.lines()).filter(|line| !line.trim().is_empty()) {
                crate::info!("驱动：{line}");
            }
            if !output.status.success() {
                crate::warn!(
                    "驱动：自动安装失败（退出码 {}，{ms} ms）",
                    output.status.code().unwrap_or(-1)
                );
                return pipe_available();
            }
            crate::info!("驱动：安装脚本结束（{ms} ms），等命名管道就绪…");
        }
        Err(error) => {
            crate::warn!("驱动：拉起安装脚本失败：{error}");
            return false;
        }
    }

    // 脚本自己会等 15 秒；再兜一层，覆盖"装完了但服务还没起管道"的情况。
    let deadline = Instant::now() + PIPE_WAIT;
    while Instant::now() < deadline {
        if pipe_available() {
            crate::info!("驱动：命名管道已就绪，可以建手机屏了");
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    crate::warn!("驱动：装完了但命名管道还没起来（可能重启 Windows 后才生效）");
    false
}

/// 解出子进程的控制台输出。
///
/// `powershell.exe` 的输出按**控制台代码页**编码（简中机器上是 936/GBK），
/// 直接 `from_utf8_lossy` 会把中文变成一串替换符——真机日志里就是这样，
/// 用户根本看不出安装脚本到底报了什么错。先按 UTF-8 严格解码，不成立再按系统
/// ANSI 代码页转一次。
fn decode_console(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    // 兜底：系统 ANSI 与 OEM 代码页都试一次，挑替换符少的那个。
    // 简中机器上两者都是 936，一样；但开了「Beta: 使用 Unicode UTF-8」的机器
    // ANSI 会变成 65001，这时只有 OEM 能解对。
    let ansi = codepage_to_string(bytes, CP_ACP);
    let oem = codepage_to_string(bytes, CP_OEMCP);
    match (ansi, oem) {
        (Some(ansi), Some(oem)) => {
            if replacement_count(&oem) < replacement_count(&ansi) {
                oem
            } else {
                ansi
            }
        }
        (Some(ansi), None) => ansi,
        (None, Some(oem)) => oem,
        (None, None) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

const CP_ACP: u32 = 0;
const CP_OEMCP: u32 = 1;

fn replacement_count(text: &str) -> usize {
    text.chars().filter(|c| *c == '\u{FFFD}').count()
}

/// `MultiByteToWideChar(code_page)`：指定代码页 → `String`。
fn codepage_to_string(bytes: &[u8], code_page: u32) -> Option<String> {
    unsafe extern "system" {
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            multi_byte_str: *const u8,
            cb_multi_byte: i32,
            wide_char_str: *mut u16,
            cch_wide_char: i32,
        ) -> i32;
    }
    if bytes.is_empty() {
        return Some(String::new());
    }
    let needed = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        )
    };
    if needed <= 0 {
        return None;
    }
    let mut wide = vec![0u16; needed as usize];
    let written = unsafe {
        MultiByteToWideChar(code_page, 0, bytes.as_ptr(), bytes.len() as i32, wide.as_mut_ptr(), needed)
    };
    if written <= 0 {
        return None;
    }
    wide.truncate(written as usize);
    Some(String::from_utf16_lossy(&wide))
}

/// 起一个后台线程做 [`ensure_installed`]，不挡启动。
pub fn spawn_ensure_installed() {
    std::thread::Builder::new()
        .name("snug-driver-install".to_string())
        .spawn(|| {
            let _ = ensure_installed();
        })
        .ok();
}

/// 设置页显式请求安装/修复，不套用启动时“已安装就只等管道”的快速路径。
pub fn spawn_install() -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("snug-driver-install".to_string())
        .spawn(|| {
            let _ = install_bundled();
        })
        .map(|_| ())
}

/// `CREATE_NO_WINDOW`：别给用户闪一个蓝框 PowerShell 出来。
trait HiddenWindow {
    fn creation_flags_hidden(&mut self) -> &mut Self;
}

impl HiddenWindow for std::process::Command {
    fn creation_flags_hidden(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        self.creation_flags(CREATE_NO_WINDOW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated_driver_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("snug-driver-test-{label}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn embedded_driver_matches_the_pinned_official_archive() {
        assert_eq!(
            snug_win::update::sha256_hex(DRIVER_ARCHIVE),
            "B3E3A5AB9B49BD56A7E753120CDDB1D913479BC5C0DFA781C3D43392DDE2FB75"
        );
        assert!(DRIVER_SCRIPT.starts_with(b"\xef\xbb\xbf"));
        let script = std::str::from_utf8(DRIVER_SCRIPT).unwrap();
        assert!(script.contains(DRIVER_ARCHIVE_NAME));
        assert!(script.contains("B3E3A5AB9B49BD56A7E753120CDDB1D913479BC5C0DFA781C3D43392DDE2FB75"));
    }

    #[test]
    fn standalone_driver_extracts_without_sidecar_files_and_cleans_up() {
        let directory = isolated_driver_dir("standalone");
        {
            std::fs::create_dir(&directory).unwrap();
            let bundle = BundledDriver::write_contents(directory.clone()).unwrap();
            assert_eq!(bundle.script(), directory.join(DRIVER_SCRIPT_NAME));
            assert_eq!(std::fs::read(bundle.script()).unwrap(), DRIVER_SCRIPT);
            assert_eq!(std::fs::read(directory.join(DRIVER_ARCHIVE_NAME)).unwrap(), DRIVER_ARCHIVE);
            // 模拟安装脚本解压的 MSI；安装进程结束后一起清理。
            let installer = directory.join("installer");
            std::fs::create_dir(&installer).unwrap();
            std::fs::write(installer.join("test.msi"), b"temporary installer").unwrap();
        }
        assert!(!directory.exists(), "安装完成后不应保留驱动临时副本");
    }

    #[test]
    fn pipe_probe_does_not_panic() {
        // 只是探一下，真机有没有驱动都行：不能崩。
        let _ = pipe_available();
    }

    #[test]
    fn device_probe_does_not_panic() {
        // "驱动装没装"的注册表查询：装没装都要能安全回答。
        let _ = driver_installed();
    }

    #[test]
    fn zero_budget_wait_just_reports_the_current_state() {
        // 预算为 0 时不该睡、也不该谎报：结果必须与当前探测一致。
        assert_eq!(wait_for_pipe(Duration::ZERO), pipe_available());
    }

    #[test]
    fn utf8_output_is_kept_as_is() {
        assert_eq!(decode_console("[OK] 安装成功".as_bytes()), "[OK] 安装成功");
    }

    #[test]
    fn non_utf8_output_never_panics() {
        // 系统 ANSI 代码页的输出（简中机器上是 GBK）：解出来成不成字不重要，
        // 重要的是不能 panic、不能整段字符串丢掉。
        let gbk = b"\xd7\xb0\xb1\xb8\xa3\xbaOK";
        let text = decode_console(gbk);
        assert!(!text.is_empty(), "ANSI 输出不该被丢成空串");
    }
}

