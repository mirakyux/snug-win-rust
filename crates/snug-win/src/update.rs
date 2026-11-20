//! 自动更新，移植自 C# 版 `SnugWin.Windows.Support.AppUpdateService`。
//!
//! 流程与 C# 一致：
//! 1. `GET https://api.github.com/repos/{owner}/{name}/releases/latest`（仓库来自
//!    [`ReleaseRepo`]），解出 tag 与附件；API 不行就退到 releases/latest 的
//!    **重定向最终地址**里抠 tag；
//! 2. 按当前包形态拼出资产名（`Snug-v{ver}-win-x64-{selfcontained|frameworkdependent}.zip`，
//!    上游 C# 的 `SnugWin-v…` 布局只作参考：Rust 版是并行重写版，用自己的发行名），
//!    顺带找 `checksums.txt`；
//! 3. 要求 `checksums.txt` 含有更新包的 SHA-256，再下载并校验 zip；
//! 4. 在 System32 下创建受保护 staging 目录，解压并确认其中有普通文件 `snug.exe`；
//! 5. 写一个 `apply-update.ps1`：等待当前进程退出、原子替换 EXE、校验并重启。
//!
//! **更新源可配置**：仓库坐标由编译期 `SNUG_RELEASES_REPO=owner/name` 决定
//! （见 [`DEFAULT_RELEASES_REPO`]），API/网页/下载地址全部由 [`ReleaseRepo`] 拼；
//! 没配置时 [`AppUpdateService::is_configured`] 为 `false`，`check_for_update()`
//! 直接返回 [`UpdateError::NotConfigured`]，**一个请求都不发**。
//!
//! 网络、解压、文件系统都经 trait 注入，单测只跑纯逻辑（版本比较、资产匹配、
//! 校验和、脚本转义、错误分支），**不会真的下载或替换任何文件**。
//!
//! 与 C# 的差别：
//! - 解压用系统自带 `tar.exe`（Win10 1803+ 起是 bsdtar，认 zip），省掉一个 zip crate；
//! - SHA-256 自己实现（见 [`sha256`]），不引入加密库，保证单 exe 零依赖；
//! - 当前版本编译期取 `CARGO_PKG_VERSION`；
//! - 发布仓库不写死：C# 里 URL 是常量，这里全部由 `owner/name` 推导。
//! - 更新暂存目录直接创建在 System32 下，使用受限 ACL；失败时拒绝在普通临时目录降级。
//!
//! 手动验证（真机）：
//! 1. 把版本号改小后启动，应提示有新版本；
//! 2. 点更新 → System32 下的受保护暂存目录出现 zip 与 payload，校验通过；
//! 3. 程序退出后 PowerShell 脚本原子替换 EXE 并重新启动新版本；
//! 4. 断网时应提示"无法获取最新版本信息"而不是崩溃。

use std::path::{Path, PathBuf};

use crate::system_paths::{create_protected_directory, powershell, system_directory};
use snug_core::update::{
    is_newer, normalize, parse_latest_release, try_find_sha256, ReleasePackageKind,
    UpdateReleaseInfo,
};

/// 编译期配置的发布仓库（`owner/name`）。
///
/// 用法：`SNUG_RELEASES_REPO=owner/name cargo build --release`。
/// 没配置时更新器不会去猜仓库，也不会发任何请求——[`UpdateError::NotConfigured`] 直接返回。
/// 这样换首发仓库、或 fork 后自建发布，都只改环境变量，不用改源码。
pub const DEFAULT_RELEASES_REPO: Option<&str> = option_env!("SNUG_RELEASES_REPO");
/// 校验和文件名。
pub const CHECKSUMS_ASSET: &str = "checksums.txt";
/// 更新包里必须有的可执行文件。
///
/// 用我们自己的发行名 `snug.exe`：上游 C# 版的 `SnugWin.App.exe` 只是参考。
/// Rust 版是**并行**的重写版，不替换用户机器上已装的 C# 正式版，所以资产名、
/// 覆盖目标、重启命令一律按 `snug.exe` 走，不去兼容 C# 的目录布局。
pub const TARGET_EXE: &str = "snug.exe";
/// 发行资产名前缀。C# 是 `SnugWin-v…`，我们是 `Snug-v…`。
pub const ASSET_PREFIX: &str = "Snug-v";
/// **应用版本**，编译期取 crate 版本。
///
/// 对应 C# `Directory.Build.props` 的 `<Version>1.2.0</Version>`：自动更新拿它跟
/// GitHub 的 tag 比。Rust 这边版本号只出现在根 `Cargo.toml` 的
/// `[workspace.package] version` 一处——crate 版本、exe 资源版本（build.rs 读
/// `CARGO_PKG_VERSION`）、跟 tag 比的版本都从这里来，所以用 `env!` 而不是再写一个常量。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
/// 下载时带的 User-Agent（C# `SnugWin-Updater/1.1`）。
pub const USER_AGENT: &str = "SnugWin-Updater/1.1";
/// staging 目录名前缀。
pub const STAGING_PREFIX: &str = "SnugWin-Update";

/// 本地发行资产名：`Snug-v{ver}-win-x64-{kind}.zip`。
///
/// 对应 C# `AppVersionHelper.PreferredAssetName`，但换了产品名。`snug-core` 里的
/// [`snug_core::update::preferred_asset_name`] 仍按 C# 的 `SnugWin-v…` 拼（core 归别人
/// 维护、且它服务的是上游语义），更新器不能调它，否则会去 GitHub 找一个永远不会存在的资产。
pub fn release_asset_name(tag_or_version: &str, kind: ReleasePackageKind) -> String {
    let version = normalize(tag_or_version);
    format!("{ASSET_PREFIX}{version}-win-x64-{}.zip", kind.suffix())
}

/// 发布仓库坐标（`owner/name`）。更新源里的所有地址都由它拼出来，模块内不再散落 URL。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRepo {
    pub owner: String,
    pub name: String,
}

impl ReleaseRepo {
    /// 解析 `owner/name`；空串、缺斜杠、多斜杠、含非法字符都返回 `None`。
    pub fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim();
        let (owner, name) = spec.split_once('/')?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return None;
        }
        let valid = |part: &str| {
            part.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        };
        if !valid(owner) || !valid(name) {
            return None;
        }
        Some(Self { owner: owner.to_string(), name: name.to_string() })
    }

    /// 取编译期配置的仓库（[`DEFAULT_RELEASES_REPO`]）；没配置或格式不对都是 `None`。
    pub fn from_default() -> Option<Self> {
        Self::parse(DEFAULT_RELEASES_REPO?)
    }

    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// GitHub "latest release" API。
    pub fn api_url(&self) -> String {
        format!("https://api.github.com/repos/{}/releases/latest", self.slug())
    }

    /// 兜底用的网页地址（靠重定向拿 tag）。
    pub fn web_url(&self) -> String {
        format!("https://github.com/{}/releases/latest", self.slug())
    }

    /// 某个 tag 的发布页（API 没给 html_url 时兜底）。
    pub fn tag_url(&self, tag: &str) -> String {
        format!("https://github.com/{}/releases/tag/{tag}", self.slug())
    }

    /// 某个 tag 的下载目录；兜底时据此拼资产下载地址。
    pub fn download_base(&self, tag: &str) -> String {
        format!("https://github.com/{}/releases/download/{tag}", self.slug())
    }
}

// ---------------------------------------------------------------------------
// HTTP 与解压的抽象
// ---------------------------------------------------------------------------

/// 一次 HTTP 响应。`final_url` 是跟随重定向之后的地址（兜底检查要用）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub final_url: String,
}

impl HttpResponse {
    pub fn new(status: u16, body: impl Into<Vec<u8>>, final_url: impl Into<String>) -> Self {
        Self { status, body: body.into(), final_url: final_url.into() }
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// 网络访问。真机用 [`CurlHttpClient`]，测试用假实现。
pub trait HttpClient: Send + Sync {
    fn get(&self, url: &str) -> Result<HttpResponse, String>;
    /// 下载到本地，按已收字节/总字节回报进度（0.0..=1.0）。
    fn download(
        &self,
        url: &str,
        destination: &Path,
        progress: &mut dyn FnMut(f64),
    ) -> Result<(), String>;
}

/// 真机 HTTP 客户端：用 Windows 自带的 `curl.exe`（Win10 1803 起在 System32）。
///
/// 选它的理由和 `tar.exe`、`schtasks.exe` 一样：系统自带，不用引一个 HTTP 库，
/// 也不给"单 exe 零依赖"添包袱。拿不到最终 URL 的兜底检查用 `-w %{url_effective}`。
#[derive(Debug, Clone, Copy)]
pub struct CurlHttpClient {
    /// 小请求（JSON、checksums）的超时秒数。
    pub timeout_seconds: u32,
    /// 下载超时秒数。
    pub download_timeout_seconds: u32,
}

impl Default for CurlHttpClient {
    fn default() -> Self {
        Self { timeout_seconds: 12, download_timeout_seconds: 600 }
    }
}

impl CurlHttpClient {
    fn run(args: &[String]) -> Result<(i32, String, String), String> {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let executable = system_directory().map_err(|error| error.to_string())?.join("curl.exe");
        let output = Command::new(executable)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| format!("无法启动 curl.exe：{error}"))?;
        Ok((
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        ))
    }

    fn base_args(timeout_seconds: u32) -> Vec<String> {
        vec![
            // curl only ignores the user's config file when -q comes first.
            "-q".to_string(),
            "-sS".to_string(),
            "-L".to_string(),
            "--proto".to_string(),
            "=https".to_string(),
            "--proto-redir".to_string(),
            "=https".to_string(),
            "--max-time".to_string(),
            timeout_seconds.to_string(),
            "-A".to_string(),
            USER_AGENT.to_string(),
            "-H".to_string(),
            "Accept: application/vnd.github+json".to_string(),
        ]
    }
}

impl HttpClient for CurlHttpClient {
    fn get(&self, url: &str) -> Result<HttpResponse, String> {
        let mut args = Self::base_args(self.timeout_seconds);
        // 末尾这一行就是 `%{http_code}\t%{url_effective}`。
        args.extend(["-w".to_string(), "\n%{http_code}\t%{url_effective}".to_string()]);
        args.push(url.to_string());

        let (code, stdout, stderr) = Self::run(&args)?;
        if code != 0 {
            return Err(format!("curl 退出码 {code}：{}", stderr.trim()));
        }
        let Some(index) = stdout.rfind('\n') else {
            return Err("curl 输出里没有状态行".to_string());
        };
        let (body, trailer) = stdout.split_at(index);
        let mut parts = trailer.trim().splitn(2, '\t');
        let status = parts
            .next()
            .and_then(|value| value.trim().parse::<u16>().ok())
            .ok_or_else(|| format!("读不出 HTTP 状态码：{}", trailer.trim()))?;
        let final_url = parts.next().unwrap_or(url).trim().to_string();

        Ok(HttpResponse { status, body: body.as_bytes().to_vec(), final_url })
    }

    fn download(
        &self,
        url: &str,
        destination: &Path,
        progress: &mut dyn FnMut(f64),
    ) -> Result<(), String> {
        let mut args = Self::base_args(self.download_timeout_seconds);
        args.extend([
            "-o".to_string(),
            destination.to_string_lossy().to_string(),
            "-w".to_string(),
            "%{http_code}".to_string(),
            url.to_string(),
        ]);

        progress(0.0);
        let (code, stdout, stderr) = Self::run(&args)?;
        if code != 0 {
            return Err(format!("下载失败（curl 退出码 {code}）：{}", stderr.trim()));
        }
        let status = stdout
            .trim()
            .parse::<u16>()
            .map_err(|_| format!("读不出下载的 HTTP 状态码：{}", stdout.trim()))?;
        if !(200..300).contains(&status) {
            return Err(format!("下载返回 HTTP {status}"));
        }
        progress(1.0);
        Ok(())
    }
}

/// 解压。真机用 [`TarExtractor`]（系统自带 bsdtar 认 zip）。
pub trait PackageExtractor: Send + Sync {
    fn extract(&self, archive: &Path, destination: &Path) -> Result<(), String>;
}

/// 用 `tar.exe -xf` 解 zip。
#[derive(Debug, Default, Clone, Copy)]
pub struct TarExtractor;

impl PackageExtractor for TarExtractor {
    fn extract(&self, archive: &Path, destination: &Path) -> Result<(), String> {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        std::fs::create_dir_all(destination).map_err(|error| error.to_string())?;
        let executable = system_directory().map_err(|error| error.to_string())?.join("tar.exe");
        let output = Command::new(executable)
            .args([
                "-xf",
                &archive.to_string_lossy(),
                "-C",
                &destination.to_string_lossy(),
                "--",
                TARGET_EXE,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| format!("无法启动 tar.exe：{error}"))?;
        if !output.status.success() {
            return Err(format!(
                "解压失败（tar 退出码 {:?}）：{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 结果类型（对应 C# 的两个 record）
// ---------------------------------------------------------------------------

/// 一次检查的结果（C# `UpdateCheckResult`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheckResult {
    pub has_update: bool,
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
    pub release_notes: String,
    pub package_kind: ReleasePackageKind,
    pub package_asset_name: String,
    pub package_download_url: Option<String>,
    pub checksums_download_url: Option<String>,
}

impl UpdateCheckResult {
    /// An update is installable only when both the package and its checksum manifest exist.
    pub fn is_downloadable(&self) -> bool {
        self.package_download_url.is_some() && self.checksums_download_url.is_some()
    }
}

/// 下载并解压好的更新（C# `PreparedUpdate`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedUpdate {
    pub version: String,
    staging_root: PathBuf,
    exe_sha256: String,
}

/// 更新流程里的失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    /// 没配置发布仓库（编译期 `SNUG_RELEASES_REPO` 或显式传入）。此时**不发任何请求**。
    NotConfigured,
    /// 网络或 HTTP 层失败。
    Network(String),
    /// API 与兜底都拿不到发布信息。
    NoRelease,
    /// 这个版本没有对应资产的下载地址。
    MissingPackage { asset: String },
    /// 缺少发布校验和或目标文件条目，不安装更新。
    MissingChecksum { asset: String },
    /// 校验和文件读取失败，不安装更新。
    ChecksumUnavailable(String),
    /// SHA-256 对不上，已中止。
    ChecksumMismatch { expected: String, actual: String },
    /// 解压成功但包里没有目标 exe。
    MissingPayload { exe: String },
    ExtractFailed(String),
    Io(String),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => f.write_str(
                "未配置更新源：请在编译期设置 SNUG_RELEASES_REPO=owner/name（例如 acme/snug-rs）。",
            ),
            Self::Network(message) => write!(f, "网络请求失败：{message}"),
            Self::NoRelease => f.write_str("无法获取最新版本信息，请检查网络连接或稍后再试。"),
            Self::MissingPackage { asset } => {
                write!(f, "当前版本资产 {asset} 尚未提供直接下载地址。")
            }
            Self::MissingChecksum { asset } => write!(f, "更新缺少 {asset} 的 SHA-256，已中止更新。"),
            Self::ChecksumUnavailable(message) => write!(f, "无法读取更新校验和：{message}，已中止更新。"),
            Self::ChecksumMismatch { expected, actual } => write!(
                f,
                "安装包 SHA-256 校验失败（期望 {expected}，实际 {actual}），已中止更新。"
            ),
            Self::MissingPayload { exe } => write!(f, "更新包内未找到 {exe}。"),
            Self::ExtractFailed(message) => write!(f, "解压更新包失败：{message}"),
            Self::Io(message) => write!(f, "更新过程中读写文件失败：{message}"),
        }
    }
}

impl std::error::Error for UpdateError {}

// ---------------------------------------------------------------------------
// 纯逻辑
// ---------------------------------------------------------------------------

/// 从重定向后的地址里抠 tag（C# `FallbackCheckViaRedirectAsync`）。
pub fn tag_from_release_url(url: &str) -> Option<String> {
    const MARKER: &str = "/releases/tag/";
    let index = url.to_lowercase().find(MARKER)?;
    let tag = url[index + MARKER.len()..].trim_matches('/').trim();
    if tag.is_empty() { None } else { Some(tag.to_string()) }
}

/// 兜底时拼出来的发布信息（不联网，方便单测）。
pub fn fallback_release(repo: &ReleaseRepo, url: &str) -> Option<UpdateReleaseInfo> {
    let final_url = if url.trim().is_empty() { repo.web_url() } else { url.to_string() };
    let tag = tag_from_release_url(&final_url)?;
    let version = normalize(&tag);
    let base = repo.download_base(&tag);
    let self_zip = release_asset_name(&tag, ReleasePackageKind::SelfContained);
    let fdd_zip = release_asset_name(&tag, ReleasePackageKind::FrameworkDependent);
    Some(UpdateReleaseInfo {
        tag_name: tag,
        normalized_version: version,
        html_url: final_url.to_string(),
        body: String::new(),
        assets: vec![
            snug_core::update::UpdateAssetInfo::new(self_zip.clone(), format!("{base}/{self_zip}"), 0),
            snug_core::update::UpdateAssetInfo::new(fdd_zip.clone(), format!("{base}/{fdd_zip}"), 0),
            snug_core::update::UpdateAssetInfo::new(
                CHECKSUMS_ASSET,
                format!("{base}/{CHECKSUMS_ASSET}"),
                0,
            ),
        ],
    })
}

/// 按 tag 与包形态挑资产、算出检查结果（不联网）。
pub fn build_check_result(
    release: &UpdateReleaseInfo,
    current_version: &str,
    package_kind: ReleasePackageKind,
    repo: &ReleaseRepo,
) -> UpdateCheckResult {
    let preferred = release_asset_name(&release.tag_name, package_kind);
    let package = release
        .assets
        .iter()
        .find(|asset| asset.name.eq_ignore_ascii_case(&preferred));
    let checksums = release
        .assets
        .iter()
        .find(|asset| asset.name.eq_ignore_ascii_case(CHECKSUMS_ASSET));

    // html_url 为空时按 tag 拼一个；tag 优先用原始 tag_name，缺了才退回 v{version}。
    let tag = if release.tag_name.trim().is_empty() {
        format!("v{}", release.normalized_version)
    } else {
        release.tag_name.clone()
    };
    let release_url = if release.html_url.trim().is_empty() {
        repo.tag_url(&tag)
    } else {
        release.html_url.clone()
    };

    UpdateCheckResult {
        has_update: is_newer(&release.tag_name, current_version),
        current_version: current_version.to_string(),
        latest_version: release.normalized_version.clone(),
        release_url,
        release_notes: release.body.clone(),
        package_kind,
        package_asset_name: preferred,
        package_download_url: package.map(|asset| asset.download_url.clone()),
        checksums_download_url: checksums.map(|asset| asset.download_url.clone()),
    }
}

/// staging 目录名：`{版本}-{唯一后缀}`，把路径非法字符换掉。
pub fn staging_dir_name(version: &str, unique: u128) -> String {
    let safe_version: String = version
        .chars()
        .map(|ch| if ch.is_alphanumeric() || ch == '.' || ch == '-' || ch == '_' { ch } else { '_' })
        .collect();
    format!("{safe_version}-{unique:032x}")
}

/// 检测当前包形态（C# `CurrentPackageKind`：旁边有 `coreclr.dll` 就是自包含）。
///
/// 说明：Rust 版永远没有 `coreclr.dll`，所以真机调用方应当直接传
/// [`ReleasePackageKind::SelfContained`]；这个函数只为对齐 C# 的行为而保留。
pub fn detect_package_kind(app_directory: &Path) -> ReleasePackageKind {
    if app_directory.join("coreclr.dll").exists() {
        ReleasePackageKind::SelfContained
    } else {
        ReleasePackageKind::FrameworkDependent
    }
}

/// Paths are passed as data in the child environment, never interpolated as PowerShell source.
fn apply_script(target_pid: u32, relaunch_minimized: bool) -> String {
    let args = if relaunch_minimized { "'--minimized'" } else { "''" };
    format!(
        r#"$ErrorActionPreference = 'Stop'
function Assert-Hash([IO.Stream]$Stream, [string]$Expected) {{
    $sha = [Security.Cryptography.SHA256]::Create()
    try {{
        $actual = [BitConverter]::ToString($sha.ComputeHash($Stream)).Replace('-', '')
        if ($actual -ne $Expected) {{ throw 'Updated executable SHA-256 does not match.' }}
    }} finally {{ $sha.Dispose() }}
}}

$src = Join-Path $PSScriptRoot 'payload\snug.exe'
$dst = $env:SNUG_UPDATE_TARGET
$expected = $env:SNUG_UPDATE_SHA256
$backup = Join-Path $PSScriptRoot 'snug-previous.exe'
$workingDir = [IO.Path]::GetDirectoryName($dst)
$launchArgs = {args}
$start = [Diagnostics.ProcessStartInfo]::new($dst, $launchArgs)
$start.UseShellExecute = $false
$start.WorkingDirectory = $workingDir
$replaced = $false
try {{
    try {{ $old = [Diagnostics.Process]::GetProcessById({target_pid}) }}
    catch [ArgumentException] {{ $old = $null }}
    if ($old) {{
        try {{
            if (-not $old.WaitForExit(30000)) {{
                throw 'Snug did not exit within 30 seconds. The original executable was preserved.'
            }}
        }} finally {{ $old.Dispose() }}
    }}

    $file = [IO.File]::OpenRead($src)
    try {{ Assert-Hash $file $expected }} finally {{ $file.Dispose() }}
    [IO.File]::Replace($src, $dst, $backup)
    $replaced = $true

    # Keep the verified file open with read sharing only while starting it.
    $file = [IO.File]::OpenRead($dst)
    try {{
        Assert-Hash $file $expected
        [Diagnostics.Process]::Start($start).Dispose()
    }} finally {{ $file.Dispose() }}
}} catch {{
    $failure = $_.ToString()
    $rollbackFailed = $false
    if ($replaced) {{
        try {{ [IO.File]::Replace($backup, $dst, [NullString]::Value) }}
        catch {{ $failure += "`r`nRollback failed: $_"; $rollbackFailed = $true }}
    }}
    if (-not $rollbackFailed) {{
        $targetRunning = $false
        try {{
            $old = [Diagnostics.Process]::GetProcessById({target_pid})
            $targetRunning = $true
            $old.Dispose()
        }} catch [ArgumentException] {{}}
        if (-not $targetRunning -and $start) {{
            try {{ [Diagnostics.Process]::Start($start).Dispose() }}
            catch {{ $failure += "`r`nRestarting the original executable failed: $_" }}
        }}
    }}
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'apply-error.txt'), $failure)
    try {{
        Add-Type -AssemblyName System.Windows.Forms -ErrorAction Stop
        [Windows.Forms.MessageBox]::Show("Snug update failed. The previous version was restored.`r`n`r`n$($_.Exception.Message)", 'Snug Update') | Out-Null
    }} catch {{}}
    exit 1
}}

Remove-Item -LiteralPath $PSScriptRoot -Recurse -Force -ErrorAction SilentlyContinue
"#
    )
}

/// UTF-8 + BOM（C# 写脚本用 `new UTF8Encoding(true)`，PowerShell 才认中文）。
pub fn utf8_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

// ---------------------------------------------------------------------------
// SHA-256（自实现，避免引入加密库）
// ---------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// 标准 SHA-256。
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_length = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    for block in message.chunks_exact(64) {
        let mut schedule = [0u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            schedule[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(SHA256_K[index])
                .wrapping_add(schedule[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut digest = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// SHA-256 的十六进制大写串（C# `Convert.ToHexString`）。
pub fn sha256_hex(data: &[u8]) -> String {
    sha256(data).iter().map(|byte| format!("{byte:02X}")).collect()
}

fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let bytes = std::fs::read(path).map_err(|error| UpdateError::Io(error.to_string()))?;
    Ok(sha256_hex(&bytes))
}

// ---------------------------------------------------------------------------
// 服务本体
// ---------------------------------------------------------------------------

/// 自动更新服务（C# `AppUpdateService`）。
pub struct AppUpdateService<H: HttpClient, E: PackageExtractor = TarExtractor> {
    http: H,
    extractor: E,
    current_version: String,
    package_kind: ReleasePackageKind,
    /// 发布仓库；`None` 表示没配置，检查更新会直接报 [`UpdateError::NotConfigured`]。
    repo: Option<ReleaseRepo>,
    #[cfg(test)]
    test_staging_parent: Option<PathBuf>,
}

impl<H: HttpClient> AppUpdateService<H, TarExtractor> {
    /// 生产用构造：版本取 [`APP_VERSION`]，包形态按 Rust 版当自包含，
    /// 仓库取编译期 `SNUG_RELEASES_REPO`（没配就是 `None`，更新功能明确不可用）。
    pub fn new(http: H) -> Self {
        Self {
            http,
            extractor: TarExtractor,
            current_version: APP_VERSION.to_string(),
            package_kind: ReleasePackageKind::SelfContained,
            repo: ReleaseRepo::from_default(),
            #[cfg(test)]
            test_staging_parent: None,
        }
    }
}

#[cfg(test)]
impl<H: HttpClient, E: PackageExtractor> AppUpdateService<H, E> {
    fn with_parts(
        http: H,
        extractor: E,
        current_version: impl Into<String>,
        package_kind: ReleasePackageKind,
        repo: Option<ReleaseRepo>,
        temp_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            http,
            extractor,
            current_version: current_version.into(),
            package_kind,
            repo,
            test_staging_parent: Some(temp_root.into()),
        }
    }
}

impl<H: HttpClient, E: PackageExtractor> AppUpdateService<H, E> {
    fn staging_path(&self, version: &str, unique: u128) -> std::io::Result<PathBuf> {
        let name = format!("{STAGING_PREFIX}-{}-{}", std::process::id(), staging_dir_name(version, unique));
        #[cfg(test)]
        if let Some(parent) = &self.test_staging_parent {
            return Ok(parent.join(name));
        }
        Ok(system_directory()?.join(name))
    }

    #[cfg(test)]
    fn test_staging_path(&self, version: &str, unique: u128) -> Option<PathBuf> {
        self.test_staging_parent.as_ref().map(|parent| {
            parent.join(format!(
                "{STAGING_PREFIX}-{}-{}",
                std::process::id(),
                staging_dir_name(version, unique)
            ))
        })
    }

    pub fn current_version(&self) -> &str {
        &self.current_version
    }

    /// 更新源是否可用（决定设置界面要不要显示“检查更新”）。
    pub fn is_configured(&self) -> bool {
        self.repo.is_some()
    }

    pub fn repo(&self) -> Option<&ReleaseRepo> {
        self.repo.as_ref()
    }

    /// 运行期覆盖更新源（`owner/name`）；传 `None` 关掉更新功能。
    pub fn set_repo_spec(&mut self, spec: Option<&str>) -> Result<(), UpdateError> {
        self.repo = match spec {
            Some(spec) => Some(ReleaseRepo::parse(spec).ok_or(UpdateError::NotConfigured)?),
            None => None,
        };
        Ok(())
    }

    /// 检查更新（C# `CheckForUpdateAsync`）。
    ///
    /// 没配置仓库时立刻返回 [`UpdateError::NotConfigured`]，**一个请求都不发**。
    pub fn check_for_update(&self) -> Result<UpdateCheckResult, UpdateError> {
        let Some(repo) = self.repo.as_ref() else {
            return Err(UpdateError::NotConfigured);
        };

        let mut release = None;
        match self.http.get(&repo.api_url()) {
            Ok(response) if response.is_success() => {
                release = parse_latest_release(&response.text());
            }
            Ok(_) | Err(_) => {
                // 网络或状态码不行就走兜底，和 C# 一样只记日志不抛出。
            }
        }

        let release = match release {
            Some(release) => release,
            None => {
                let response = self.http.get(&repo.web_url()).map_err(UpdateError::Network)?;
                fallback_release(repo, &response.final_url).ok_or(UpdateError::NoRelease)?
            }
        };

        Ok(build_check_result(&release, &self.current_version, self.package_kind, repo))
    }

    /// 下载、校验、解压（C# `DownloadAndPrepareAsync`）。
    pub fn download_and_prepare(
        &self,
        check: &UpdateCheckResult,
        progress: &mut dyn FnMut(f64),
    ) -> Result<PreparedUpdate, UpdateError> {
        let Some(download_url) = check.package_download_url.clone() else {
            return Err(UpdateError::MissingPackage { asset: check.package_asset_name.clone() });
        };
        // Asset names become local paths. Require the single safe filename produced for
        // our release package rather than accepting an arbitrary tag-derived path.
        let asset = &check.package_asset_name;
        if asset != &release_asset_name(&check.latest_version, ReleasePackageKind::SelfContained)
            || !asset.bytes().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'.' | b'-'))
        {
            return Err(UpdateError::MissingPackage { asset: asset.clone() });
        }
        let expected = self.read_checksum(check)?;

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        // The directory is created atomically with its final DACL. Do not use
        // create_dir_all here: both the script and verified payload are later consumed
        // by an elevated process, so a user-writable staging path would re-open a TOCTOU.
        let staging_root = self.staging_path(&check.latest_version, unique)
            .map_err(|error| UpdateError::Io(error.to_string()))?;
        #[cfg(test)]
        let create: fn(&Path) -> std::io::Result<()> = if self.test_staging_parent.is_some() {
            |path| std::fs::create_dir(path)
        } else {
            create_protected_directory
        };
        #[cfg(not(test))]
        let create = create_protected_directory;
        create(&staging_root)
            .map_err(|error| UpdateError::Io(format!("无法创建受保护的更新目录：{error}")))?;

        let prepare = || -> Result<PreparedUpdate, UpdateError> {
            let zip_path = staging_root.join(asset);
            let payload_dir = staging_root.join("payload");
            self.http.download(&download_url, &zip_path, progress).map_err(UpdateError::Network)?;
            let actual = sha256_file(&zip_path)?;
            if !actual.eq_ignore_ascii_case(&expected) {
                return Err(UpdateError::ChecksumMismatch { expected, actual });
            }
            self.extractor.extract(&zip_path, &payload_dir).map_err(UpdateError::ExtractFailed)?;
            let target_exe = payload_dir.join(TARGET_EXE);
            use std::os::windows::fs::MetadataExt;
            let is_regular_exe = std::fs::symlink_metadata(&target_exe).is_ok_and(|metadata| {
                // Neither a symlink nor any other Windows reparse point can be an update.
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
                metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
            });
            if !is_regular_exe {
                return Err(UpdateError::MissingPayload { exe: TARGET_EXE.to_string() });
            }
            Ok(PreparedUpdate {
                version: check.latest_version.clone(),
                staging_root: staging_root.clone(),
                exe_sha256: sha256_file(&target_exe)?,
            })
        };
        let result = prepare();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&staging_root);
        }
        result
    }

    fn read_checksum(&self, check: &UpdateCheckResult) -> Result<String, UpdateError> {
        let missing = || UpdateError::MissingChecksum { asset: check.package_asset_name.clone() };
        let url = check.checksums_download_url.as_deref().ok_or_else(missing)?;
        let response = self.http.get(url).map_err(UpdateError::ChecksumUnavailable)?;
        if !response.is_success() {
            return Err(UpdateError::ChecksumUnavailable(format!("HTTP {}", response.status)));
        }
        try_find_sha256(&response.text(), &check.package_asset_name).ok_or_else(missing)
    }
}

/// 写脚本并拉起绝对路径下的 PowerShell。父进程不等待替换结果；脚本保留失败记录。
fn volume_path(path: &Path) -> Result<String, UpdateError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{GetVolumeNameForVolumeMountPointW, GetVolumePathNameW};
    use windows::core::PCWSTR;

    let path: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut mount = vec![0u16; 32_768];
    let mut volume = [0u16; 64];
    unsafe {
        GetVolumePathNameW(PCWSTR(path.as_ptr()), &mut mount)
            .and_then(|()| GetVolumeNameForVolumeMountPointW(PCWSTR(mount.as_ptr()), &mut volume))
            .map_err(|error| UpdateError::Io(error.to_string()))?;
    }
    Ok(crate::wide::from_wide(&volume))
}

pub fn launch_apply_script(
    update: &PreparedUpdate,
    relaunch_minimized: bool,
) -> Result<PathBuf, UpdateError> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let result = (|| {
        let target = std::env::current_exe().map_err(|error| UpdateError::Io(error.to_string()))?;
        // File.Replace requires both files and the backup to be on one volume. Check
        // before exiting; never fall back to copying into the live EXE.
        if volume_path(&target)? != volume_path(&update.staging_root)? {
            return Err(UpdateError::Io("自动更新要求程序与 Windows 位于同一个卷，请手动替换 EXE".into()));
        }
        let executable = powershell().map_err(|error| UpdateError::Io(error.to_string()))?;
        let script_path = update.staging_root.join("apply-update.ps1");
        let script = apply_script(std::process::id(), relaunch_minimized);
        std::fs::write(&script_path, utf8_with_bom(&script))
            .map_err(|error| UpdateError::Io(error.to_string()))?;

        Command::new(executable)
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script_path)
            .env_remove("PSModulePath")
            .env("SNUG_UPDATE_TARGET", &target)
            .env("SNUG_UPDATE_SHA256", &update.exe_sha256)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|error| UpdateError::Io(error.to_string()))?;
        Ok(script_path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&update.staging_root);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// 测试用发布仓库：所有 URL 都由它拼，别在测试里再写死域名。
    fn test_repo() -> ReleaseRepo {
        ReleaseRepo::parse("acme/snug").expect("合法的 owner/name")
    }

    // ----- 纯逻辑 -----

    #[test]
    fn release_repo_parses_and_builds_every_url() {
        let repo = test_repo();
        assert_eq!(repo.slug(), "acme/snug");
        assert_eq!(
            repo.api_url(),
            "https://api.github.com/repos/acme/snug/releases/latest"
        );
        assert_eq!(repo.web_url(), "https://github.com/acme/snug/releases/latest");
        assert_eq!(repo.tag_url("1.2.0"), "https://github.com/acme/snug/releases/tag/1.2.0");
        assert_eq!(
            repo.download_base("v1.2.0"),
            "https://github.com/acme/snug/releases/download/v1.2.0"
        );
        // 前后空白可以容忍。
        assert_eq!(ReleaseRepo::parse("  acme/snug  "), Some(repo));
    }

    #[test]
    fn release_repo_rejects_malformed_specs() {
        for spec in ["", "acme", "acme/", "/snug", "a/b/c", "ac me/snug", "acme/sn ug"] {
            assert_eq!(ReleaseRepo::parse(spec), None, "不该接受 {spec:?}");
        }
    }

    #[test]
    fn default_repo_comes_from_the_build_time_env() {
        // 测试构建通常没设 SNUG_RELEASES_REPO：生产构造必须如实报告“未配置”，
        // 而不是偷偷用一个写死的仓库。
        let service = AppUpdateService::new(FakeHttp::new());
        assert_eq!(
            service.is_configured(),
            DEFAULT_RELEASES_REPO.and_then(ReleaseRepo::parse).is_some()
        );
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"
        );
        // 经典 56 字节向量（跨两块）。
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248D6A61D20638B8E5C026930C3E6039A33CE45964FF2167F6ECEDD419DB06C1"
        );
    }

    #[test]
    fn tag_is_extracted_from_redirect_url() {
        assert_eq!(
            tag_from_release_url("https://github.com/nicocart/snug-win/releases/tag/v1.2.0").as_deref(),
            Some("v1.2.0")
        );
        assert_eq!(
            tag_from_release_url("https://github.com/nicocart/snug-win/releases/tag/v1.2.0/").as_deref(),
            Some("v1.2.0")
        );
        assert_eq!(tag_from_release_url("https://github.com/nicocart/snug-win"), None);
        assert_eq!(tag_from_release_url("https://x/releases/tag/"), None);
        assert_eq!(tag_from_release_url(""), None);
    }

    #[test]
    fn release_asset_name_uses_our_own_product_name() {
        // 不兼容 C# 的 `SnugWin-v…`：Rust 版是并行重写版，用自己的发行名。
        assert_eq!(
            release_asset_name("v1.2.0", ReleasePackageKind::SelfContained),
            "Snug-v1.2.0-win-x64-selfcontained.zip"
        );
        assert_eq!(
            release_asset_name("1.2.0", ReleasePackageKind::FrameworkDependent),
            "Snug-v1.2.0-win-x64-frameworkdependent.zip"
        );
        assert_eq!(TARGET_EXE, "snug.exe", "覆盖/重启的目标就是我们的产物名");
    }

    #[test]
    fn fallback_release_builds_three_assets() {
        let repo = test_repo();
        let release = fallback_release(&repo, "https://github.com/acme/snug/releases/tag/v1.2.0")
            .expect("能拼出来");
        assert_eq!(release.tag_name, "v1.2.0");
        assert_eq!(release.normalized_version, "1.2.0");
        assert_eq!(release.assets.len(), 3);
        assert_eq!(release.assets[2].name, CHECKSUMS_ASSET);
        assert_eq!(
            release.assets[0].download_url,
            "https://github.com/acme/snug/releases/download/v1.2.0/Snug-v1.2.0-win-x64-selfcontained.zip"
        );
        assert!(fallback_release(&repo, "https://example.com/").is_none());
    }

    #[test]
    fn check_result_picks_assets_by_package_kind() {
        let repo = test_repo();
        let release = UpdateReleaseInfo {
            tag_name: "v1.3.0".to_string(),
            normalized_version: "1.3.0".to_string(),
            html_url: String::new(),
            body: "notes".to_string(),
            assets: vec![
                snug_core::update::UpdateAssetInfo::new(
                    "Snug-v1.3.0-win-x64-selfcontained.zip",
                    "https://example.invalid/self.zip",
                    1,
                ),
                snug_core::update::UpdateAssetInfo::new(
                    "Snug-v1.3.0-win-x64-frameworkdependent.zip",
                    "https://example.invalid/fdd.zip",
                    2,
                ),
                snug_core::update::UpdateAssetInfo::new(
                    CHECKSUMS_ASSET,
                    "https://example.invalid/checksums.txt",
                    3,
                ),
            ],
        };

        let self_contained =
            build_check_result(&release, "1.2.0", ReleasePackageKind::SelfContained, &repo);
        assert!(self_contained.has_update);
        assert_eq!(self_contained.package_asset_name, "Snug-v1.3.0-win-x64-selfcontained.zip");
        assert_eq!(
            self_contained.package_download_url.as_deref(),
            Some("https://example.invalid/self.zip")
        );
        assert_eq!(
            self_contained.checksums_download_url.as_deref(),
            Some("https://example.invalid/checksums.txt")
        );
        // html_url 为空时用仓库 + tag 拼一个。
        assert_eq!(
            self_contained.release_url,
            "https://github.com/acme/snug/releases/tag/v1.3.0"
        );

        let framework =
            build_check_result(&release, "1.3.0", ReleasePackageKind::FrameworkDependent, &repo);
        assert!(!framework.has_update, "同版本不算更新");
        assert_eq!(
            framework.package_download_url.as_deref(),
            Some("https://example.invalid/fdd.zip")
        );

        let mut no_checksums = release.clone();
        no_checksums.assets.retain(|asset| !asset.name.eq_ignore_ascii_case(CHECKSUMS_ASSET));
        let missing_checksums =
            build_check_result(&no_checksums, "1.0.0", ReleasePackageKind::SelfContained, &repo);
        assert!(!missing_checksums.is_downloadable());

        // 缺资产时下载地址是 None。
        let missing =
            build_check_result(&release, "1.0.0", ReleasePackageKind::SelfContained, &repo);
        assert!(missing.package_download_url.is_some());
        assert!(missing.is_downloadable(), "包和校验和都存在时应启用下载");
        let no_assets = UpdateReleaseInfo { assets: Vec::new(), ..release };
        let none =
            build_check_result(&no_assets, "1.0.0", ReleasePackageKind::SelfContained, &repo);
        assert_eq!(none.package_download_url, None);
        assert!(!none.is_downloadable());
    }

    #[test]
    fn staging_name_is_sanitized() {
        let name = staging_dir_name("1.2.3-beta/evil", 0xAB);
        assert!(name.starts_with("1.2.3-beta_evil-"));
        assert_eq!(name.len(), "1.2.3-beta_evil-".len() + 32);
        assert!(!name.contains('/'));
    }

    #[test]
    fn test_staging_root_is_isolated_and_uses_production_prefix() {
        let parent = temp_root();
        let service = AppUpdateService::with_parts(
            FakeHttp::new(),
            FakeExtractor { create_exe: true },
            "1.2.0",
            ReleasePackageKind::SelfContained,
            Some(test_repo()),
            parent.clone(),
        );
        let path = service.test_staging_path("1.2.3", 7).expect("test parent");
        assert_eq!(path.parent(), Some(parent.as_path()));
        assert!(path.file_name().unwrap().to_string_lossy().starts_with(STAGING_PREFIX));
    }

    #[test]
    fn production_staging_is_a_direct_system_directory_child() {
        let service = AppUpdateService::new(FakeHttp::new());
        let path = service.staging_path("1.2.3", 7).expect("system staging path");
        let system = system_directory().unwrap();
        assert_eq!(path.parent(), Some(system.as_path()));
        assert!(path.file_name().unwrap().to_string_lossy().starts_with(STAGING_PREFIX));
    }

    #[test]
    fn curl_defaults_ignore_user_config_and_allow_only_https() {
        let args = CurlHttpClient::base_args(12);
        assert_eq!(args.first().map(String::as_str), Some("-q"));
        assert!(args.windows(2).any(|pair| pair == ["--proto", "=https"]));
        assert!(args.windows(2).any(|pair| pair == ["--proto-redir", "=https"]));
    }

    #[test]
    fn apply_script_checks_hash_waits_and_rolls_back() {
        let script = apply_script(4242, true);
        assert!(script.contains("GetProcessById(4242)"));
        assert!(script.contains("WaitForExit(30000)"));
        assert!(script.contains("does not match"));
        assert!(script.contains("[IO.File]::Replace($src, $dst, $backup)"));
        assert!(script.contains("[IO.File]::Replace($backup, $dst"));
        assert!(script.contains("Restarting the original executable failed"));
        assert!(script.contains("apply-error.txt"));
        assert!(script.contains("$env:SNUG_UPDATE_TARGET"));
        assert!(script.contains(r"Join-Path $PSScriptRoot 'payload\snug.exe'"));
        assert!(!script.contains("Copy-Item"));
        assert!(script.contains("$launchArgs = '--minimized'"));

        let plain = apply_script(1, false);
        assert!(plain.contains("$launchArgs = ''"));
        assert!(!plain.contains("--minimized"));
    }

    #[test]
    fn script_is_written_with_a_utf8_bom() {
        assert_eq!(&utf8_with_bom("x")[..3], &[0xEF, 0xBB, 0xBF]);
        assert_eq!(utf8_with_bom("x").len(), 4);
    }

    #[test]
    fn package_kind_detection_matches_csharp() {
        let dir = std::env::temp_dir().join(format!(
            "snug-rs-update-kind-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(detect_package_kind(&dir), ReleasePackageKind::FrameworkDependent);
        std::fs::write(dir.join("coreclr.dll"), b"").unwrap();
        assert_eq!(detect_package_kind(&dir), ReleasePackageKind::SelfContained);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ----- 假实现跑流程 -----

    struct FakeHttp {
        responses: HashMap<String, HttpResponse>,
        download_bytes: Vec<u8>,
        downloads: Arc<Mutex<Vec<(String, PathBuf)>>>,
        /// 记录每一次 `get`，用来断言“未配置时不发请求”。
        gets: Arc<Mutex<Vec<String>>>,
    }

    impl FakeHttp {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
                download_bytes: Vec::new(),
                downloads: Arc::new(Mutex::new(Vec::new())),
                gets: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with(mut self, url: &str, response: HttpResponse) -> Self {
            self.responses.insert(url.to_string(), response);
            self
        }

        /// 拿到请求记录句柄（可以在把 `self` 交给服务之后继续查）。
        fn request_log(&self) -> Arc<Mutex<Vec<String>>> {
            self.gets.clone()
        }
    }

    impl HttpClient for FakeHttp {
        fn get(&self, url: &str) -> Result<HttpResponse, String> {
            self.gets.lock().unwrap().push(url.to_string());
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| format!("没有为 {url} 准备响应"))
        }

        fn download(
            &self,
            url: &str,
            destination: &Path,
            progress: &mut dyn FnMut(f64),
        ) -> Result<(), String> {
            self.downloads.lock().unwrap().push((url.to_string(), destination.to_path_buf()));
            std::fs::write(destination, &self.download_bytes).map_err(|error| error.to_string())?;
            progress(1.0);
            Ok(())
        }
    }

    struct FakeExtractor {
        create_exe: bool,
    }

    impl PackageExtractor for FakeExtractor {
        fn extract(&self, _archive: &Path, destination: &Path) -> Result<(), String> {
            if self.create_exe {
                std::fs::create_dir_all(destination).map_err(|error| error.to_string())?;
                std::fs::write(destination.join(TARGET_EXE), b"exe").map_err(|error| error.to_string())?;
            }
            Ok(())
        }
    }

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "snug-rs-update-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn release_json(tag: &str) -> String {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://github.com/acme/snug/releases/tag/{tag}","body":"notes","assets":[
              {{"name":"Snug-v{}-win-x64-selfcontained.zip","browser_download_url":"https://example.invalid/self.zip","size":10}},
              {{"name":"checksums.txt","browser_download_url":"https://example.invalid/checksums.txt","size":2}}]}}"#,
            tag.trim_start_matches('v')
        )
    }

    fn service_with(http: FakeHttp, create_exe: bool, repo: Option<ReleaseRepo>) -> AppUpdateService<FakeHttp, FakeExtractor> {
        // 这里的 "1.2.0" 是**测试场景里的旧版本**（配合 `release_json("v1.3.0")`
        // 验"有新版本"），不是包版本常量——别改成 APP_VERSION，否则那些用例的
        // "发现更新"前提就没了。要真实版本用 `service_current`。
        AppUpdateService::with_parts(
            http,
            FakeExtractor { create_exe },
            "1.2.0",
            ReleasePackageKind::SelfContained,
            repo,
            temp_root(),
        )
    }

    /// 版本号与生产构造同源的服务实例（只给 `current_version_comes_from_*` 用）。
    fn service_current(http: FakeHttp) -> AppUpdateService<FakeHttp, FakeExtractor> {
        AppUpdateService::with_parts(
            http,
            FakeExtractor { create_exe: true },
            APP_VERSION,
            ReleasePackageKind::SelfContained,
            Some(test_repo()),
            temp_root(),
        )
    }

    fn service(http: FakeHttp, create_exe: bool) -> AppUpdateService<FakeHttp, FakeExtractor> {
        service_with(http, create_exe, Some(test_repo()))
    }

    #[test]
    fn check_uses_the_api_first() {
        let repo = test_repo();
        let http = FakeHttp::new().with(
            &repo.api_url(),
            HttpResponse::new(200, release_json("v1.3.0"), repo.api_url()),
        );
        let check = service(http, true).check_for_update().expect("检查成功");
        assert!(check.has_update);
        assert_eq!(check.latest_version, "1.3.0");
        assert_eq!(check.release_notes, "notes");
        assert_eq!(
            check.package_download_url.as_deref(),
            Some("https://example.invalid/self.zip")
        );
    }

    #[test]
    fn check_falls_back_to_redirect_when_api_fails() {
        // API 没准备 → 网络错误 → 走网页兜底。
        let repo = test_repo();
        let http = FakeHttp::new().with(
            &repo.web_url(),
            HttpResponse::new(200, "", "https://github.com/acme/snug/releases/tag/v1.4.0"),
        );
        let check = service(http, true).check_for_update().expect("兜底成功");
        assert!(check.has_update);
        assert_eq!(check.latest_version, "1.4.0");
        assert_eq!(check.release_notes, "", "兜底没有发布说明");
        assert!(check.package_download_url.is_some());
    }

    #[test]
    fn check_falls_back_when_api_returns_an_error_status() {
        let repo = test_repo();
        let http = FakeHttp::new()
            .with(&repo.api_url(), HttpResponse::new(500, "boom", repo.api_url()))
            .with(
                &repo.web_url(),
                HttpResponse::new(200, "", "https://github.com/acme/snug/releases/tag/v9.9.9"),
            );
        let check = service(http, true).check_for_update().expect("兜底成功");
        assert_eq!(check.latest_version, "9.9.9");
    }

    #[test]
    fn check_without_any_release_is_an_error() {
        let repo = test_repo();
        let http = FakeHttp::new()
            .with(&repo.web_url(), HttpResponse::new(200, "", "https://github.com/acme/snug"));
        let error = service(http, true).check_for_update().expect_err("拿不到版本");
        assert_eq!(error, UpdateError::NoRelease);
        assert!(error.to_string().contains("无法获取最新版本信息"));
    }

    #[test]
    fn unconfigured_update_source_sends_no_request() {
        // 没配 SNUG_RELEASES_REPO 时不能去猜仓库、更不能真的发请求。
        let http = FakeHttp::new();
        let requests = http.request_log();
        let service = service_with(http, true, None);
        assert!(!service.is_configured());
        let error = service.check_for_update().expect_err("未配置就该直接失败");
        assert_eq!(error, UpdateError::NotConfigured);
        assert!(error.to_string().contains("SNUG_RELEASES_REPO"));
        let sent = requests.lock().unwrap();
        assert!(sent.is_empty(), "不该发任何 HTTP 请求：{sent:?}");
    }

    #[test]
    fn set_repo_spec_switches_the_source_or_refuses_malformed_input() {
        let mut service = service(FakeHttp::new(), true);
        assert!(service.is_configured());
        service.set_repo_spec(Some("other-owner/snug-rs")).expect("合法坐标");
        assert_eq!(service.repo().map(ReleaseRepo::slug), Some("other-owner/snug-rs".to_string()));
        assert_eq!(service.set_repo_spec(Some("not-a-spec")), Err(UpdateError::NotConfigured));
        service.set_repo_spec(None).expect("显式关掉");
        assert!(!service.is_configured());
    }

    #[test]
    fn download_requires_a_package_url() {
        let check = UpdateCheckResult {
            has_update: true,
            current_version: "1.2.0".to_string(),
            latest_version: "1.3.0".to_string(),
            release_url: String::new(),
            release_notes: String::new(),
            package_kind: ReleasePackageKind::SelfContained,
            package_asset_name: "pkg.zip".to_string(),
            package_download_url: None,
            checksums_download_url: None,
        };
        let error = service(FakeHttp::new(), true)
            .download_and_prepare(&check, &mut |_| {})
            .expect_err("没有下载地址");
        assert_eq!(error, UpdateError::MissingPackage { asset: "pkg.zip".to_string() });
        assert!(error.to_string().contains("pkg.zip"));
    }

    fn check_with(checksums_url: Option<&str>) -> UpdateCheckResult {
        UpdateCheckResult {
            has_update: true,
            current_version: "1.2.0".to_string(),
            latest_version: "1.3.0".to_string(),
            release_url: String::new(),
            release_notes: String::new(),
            package_kind: ReleasePackageKind::SelfContained,
            package_asset_name: "Snug-v1.3.0-win-x64-selfcontained.zip".to_string(),
            package_download_url: Some("https://example.invalid/pkg.zip".to_string()),
            checksums_download_url: checksums_url.map(str::to_string),
        }
    }

    #[test]
    fn prepare_downloads_verifies_and_extracts() {
        let mut http = FakeHttp::new();
        http.download_bytes = b"payload".to_vec();
        let hash = sha256_hex(b"payload");
        let http = http.with(
            "https://example.invalid/checksums.txt",
            HttpResponse::new(
                200,
                format!("{hash}  {}\n", "Snug-v1.3.0-win-x64-selfcontained.zip"),
                "https://example.invalid/checksums.txt",
            ),
        );

        let service = service(http, true);
        let mut progress = Vec::new();
        let prepared = service
            .download_and_prepare(&check_with(Some("https://example.invalid/checksums.txt")), &mut |value| {
                progress.push(value)
            })
            .expect("准备成功");

        assert_eq!(prepared.version, "1.3.0");
        assert!(prepared.staging_root.exists());
        assert!(prepared.staging_root.join("payload").join(TARGET_EXE).exists());
        assert_eq!(progress, vec![1.0]);
        let _ = std::fs::remove_dir_all(&prepared.staging_root);
    }

    #[test]
    fn checksum_mismatch_aborts_the_update() {
        let mut http = FakeHttp::new();
        http.download_bytes = b"payload".to_vec();
        let http = http.with(
            "https://example.invalid/checksums.txt",
            HttpResponse::new(
                200,
                format!(
                    "{}  Snug-v1.3.0-win-x64-selfcontained.zip\n",
                    "0".repeat(64)
                ),
                "https://example.invalid/checksums.txt",
            ),
        );
        let service = service(http, true);
        let error = service
            .download_and_prepare(&check_with(Some("https://example.invalid/checksums.txt")), &mut |_| {})
            .expect_err("哈希不对要中止");
        match error {
            UpdateError::ChecksumMismatch { expected, actual } => {
                assert_eq!(expected, "0".repeat(64));
                assert_eq!(actual, sha256_hex(b"payload"));
            }
            other => panic!("错误类型不对：{other:?}"),
        }
    }

    #[test]
    fn missing_payload_exe_aborts_and_cleans_up() {
        let hash = sha256_hex(b"payload");
        let mut http = FakeHttp::new();
        http.download_bytes = b"payload".to_vec();
        let http = http.with(
            "https://example.invalid/checksums.txt",
            HttpResponse::new(
                200,
                format!("{hash}  Snug-v1.3.0-win-x64-selfcontained.zip\n"),
                "https://example.invalid/checksums.txt",
            ),
        );
        let service = service(http, false);
        let error = service
            .download_and_prepare(&check_with(Some("https://example.invalid/checksums.txt")), &mut |_| {})
            .expect_err("包里没有 exe 要中止");
        assert_eq!(error, UpdateError::MissingPayload { exe: TARGET_EXE.to_string() });
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn checksums_without_the_asset_name_aborts_before_download() {
        // The package must not download when its own entry is absent from the manifest.
        let mut http = FakeHttp::new();
        http.download_bytes = b"payload".to_vec();
        let http = http.with(
            "https://example.invalid/checksums.txt",
            HttpResponse::new(
                200,
                format!("{}  some-other-file.zip\n", "0".repeat(64)),
                "https://example.invalid/checksums.txt",
            ),
        );
        let downloads = http.downloads.clone();
        let error = service(http, true)
            .download_and_prepare(&check_with(Some("https://example.invalid/checksums.txt")), &mut |_| {})
            .expect_err("没有匹配校验和必须拒绝");
        assert!(matches!(error, UpdateError::MissingChecksum { .. }));
        assert!(downloads.lock().unwrap().is_empty());
    }

    #[test]
    fn checksums_download_failure_aborts_before_download() {
        let mut http = FakeHttp::new();
        http.download_bytes = b"payload".to_vec();
        let downloads = http.downloads.clone();
        let error = service(http, true)
            .download_and_prepare(&check_with(Some("https://example.invalid/missing.txt")), &mut |_| {})
            .expect_err("获取不到校验和必须拒绝");
        assert!(matches!(error, UpdateError::ChecksumUnavailable(_)));
        assert!(downloads.lock().unwrap().is_empty());
    }

    #[test]
    fn missing_checksum_aborts_before_any_download() {
        let mut http = FakeHttp::new();
        http.download_bytes = b"payload".to_vec();
        let downloads = http.downloads.clone();
        let error = service(http, true)
            .download_and_prepare(&check_with(None), &mut |_| {})
            .expect_err("缺少校验和地址必须拒绝");
        assert!(matches!(error, UpdateError::MissingChecksum { .. }));
        assert!(downloads.lock().unwrap().is_empty());
    }

    #[test]
    fn current_version_comes_from_the_cargo_package_version() {
        let service = service_current(FakeHttp::new());
        assert_eq!(service.current_version(), env!("CARGO_PKG_VERSION"));

        // 生产构造与 crate 版本同源（根 Cargo.toml 的 [workspace.package] version）。
        let production = AppUpdateService::new(FakeHttp::new());
        assert_eq!(production.current_version(), APP_VERSION);
        assert_eq!(APP_VERSION, env!("CARGO_PKG_VERSION"), "版本只能有一个来源");

        // 必须是 major.minor.patch 三段数字：exe 资源版本和 GitHub tag 都按这个格式生成。
        let parts: Vec<&str> = APP_VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "版本号必须是三段：{APP_VERSION}");
        assert!(
            parts.iter().all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())),
            "版本号每段都得是数字：{APP_VERSION}"
        );
    }

    #[test]
    fn warn_is_delivered_to_the_installed_sink() {
        // snug-win 是库，告警要靠应用装接收端才会进文件日志；没装时退到 stderr。
        // OnceLock 只认第一次安装，所以这里装一次、断言消息确实送达。
        static WARNED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        crate::set_warn_sink(|message| {
            WARNED.lock().unwrap_or_else(|error| error.into_inner()).push(message.to_string());
        });
        crate::warn("校验被跳过");
        let warned = WARNED.lock().unwrap_or_else(|error| error.into_inner());
        assert!(warned.iter().any(|message| message == "校验被跳过"), "warned={warned:?}");
    }

    #[test]
    fn release_checksums_txt_format_is_accepted_by_try_find_sha256() {
        // scripts/build-release.ps1 写的是 sha256sum 文本模式：
        // `<64 位小写十六进制><两个空格><文件名>`，哈希是 zip 的（更新器验的是下载下来的 zip）。
        let hash = "ab".repeat(32);
        let upper = hash.to_ascii_uppercase();
        let asset = "Snug-v1.2.0-win-x64-selfcontained.zip";

        let canonical = format!("{hash}  {asset}\n");
        assert_eq!(try_find_sha256(&canonical, asset).as_deref(), Some(upper.as_str()));

        // sha256sum -b 的星号变体：C# 会 TrimStart('*')，必须仍指到同一个文件，
        // 而不是被当成"别的文件名"（这行只是容忍，不是发布脚本的写出格式）。
        let binary_mode = format!("{hash} *{asset}\n");
        assert_eq!(try_find_sha256(&binary_mode, asset).as_deref(), Some(upper.as_str()));

        // 单空格变体：解析器按"第 65 个字符是空白"切，所以同样能认出来；
        // 发布脚本仍固定写两个空格，跟 sha256sum 一致。
        let single_space = format!("{hash} {asset}\n");
        assert_eq!(try_find_sha256(&single_space, asset).as_deref(), Some(upper.as_str()));

        // 名字对不上、哈希长度不对、不是十六进制，都必须匹配失败（不能糊里糊涂放行）。
        assert_eq!(
            try_find_sha256(&format!("{hash}  Snug-v9.9.9-win-x64-selfcontained.zip\n"), asset),
            None
        );
        assert_eq!(try_find_sha256(&format!("{}  {asset}\n", &hash[..62]), asset), None);
        assert_eq!(try_find_sha256(&format!("{}  {asset}\n", "zz".repeat(32)), asset), None);
    }

}
