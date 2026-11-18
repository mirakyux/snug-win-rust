//! 自动更新，移植自 C# 版 `SnugWin.Windows.Support.AppUpdateService`。
//!
//! 流程与 C# 一致：
//! 1. `GET https://api.github.com/repos/nicocart/snug-win/releases/latest`，
//!    解出 tag 与附件；API 不行就退到 releases/latest 的**重定向最终地址**里抠 tag；
//! 2. 按当前包形态拼出资产名（`Snug-v{ver}-win-x64-{selfcontained|frameworkdependent}.zip`，
//!    上游 C# 的 `SnugWin-v…` 布局只作参考：Rust 版是并行重写版，用自己的发行名），
//!    顺带找 `checksums.txt`；
//! 3. 下载 zip、用 `checksums.txt` 里的 SHA-256 校验（校验不过就中止并删临时目录）；
//! 4. 解压到 staging 的 `payload/`，确认里面有 [`TARGET_EXE`]（`snug.exe`）；
//! 5. 写一个 `apply-update.ps1`：等本进程退出 → 覆盖文件 → 重新拉起 → 删临时目录。
//!
//! 网络、解压、文件系统都经 trait 注入，单测只跑纯逻辑（版本比较、资产匹配、
//! 校验和、脚本转义、错误分支），**不会真的下载或替换任何文件**。
//!
//! 与 C# 的差别：
//! - 解压用系统自带 `tar.exe`（Win10 1803+ 起是 bsdtar，认 zip），省掉一个 zip crate；
//! - SHA-256 自己实现（见 [`sha256`]），不引入加密库，保证单 exe 零依赖；
//! - 当前版本编译期取 `CARGO_PKG_VERSION`。
//!
//! 手动验证（真机）：
//! 1. 把版本号改小后启动，应提示有新版本；
//! 2. 点更新 → 临时目录里出现 zip 与 payload，校验通过；
//! 3. 程序退出后 PowerShell 脚本覆盖文件并重新启动新版本；
//! 4. 断网时应提示"无法获取最新版本信息"而不是崩溃。

use std::path::{Path, PathBuf};

use snug_core::update::{
    is_newer, normalize, parse_latest_release, try_find_sha256, ReleasePackageKind,
    UpdateReleaseInfo,
};

/// GitHub "latest release" API。
pub const LATEST_API_URL: &str = "https://api.github.com/repos/nicocart/snug-win/releases/latest";
/// 兜底用的网页地址（靠重定向拿 tag）。
pub const LATEST_WEB_URL: &str = "https://github.com/nicocart/snug-win/releases/latest";
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

        let output = Command::new("curl.exe")
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
            "-sS".to_string(),
            "-L".to_string(),
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
        let output = Command::new("tar.exe")
            .args([
                "-xf",
                &archive.to_string_lossy(),
                "-C",
                &destination.to_string_lossy(),
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

/// 下载并解压好的更新（C# `PreparedUpdate`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedUpdate {
    pub version: String,
    pub staging_root: PathBuf,
    pub payload_dir: PathBuf,
    pub target_app_dir: PathBuf,
}

/// 更新流程里的失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    /// 网络或 HTTP 层失败。
    Network(String),
    /// API 与兜底都拿不到发布信息。
    NoRelease,
    /// 这个版本没有对应资产的下载地址。
    MissingPackage { asset: String },
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
            Self::Network(message) => write!(f, "网络请求失败：{message}"),
            Self::NoRelease => f.write_str("无法获取最新版本信息，请检查网络连接或稍后再试。"),
            Self::MissingPackage { asset } => {
                write!(f, "当前版本资产 {asset} 尚未提供直接下载地址。")
            }
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
pub fn fallback_release(url: &str) -> Option<UpdateReleaseInfo> {
    let final_url = if url.trim().is_empty() { LATEST_WEB_URL } else { url };
    let tag = tag_from_release_url(final_url)?;
    let version = normalize(&tag);
    let base = format!("https://github.com/nicocart/snug-win/releases/download/{tag}");
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

    let release_url = if release.html_url.trim().is_empty() {
        format!(
            "https://github.com/nicocart/snug-win/releases/tag/v{}",
            release.normalized_version
        )
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

/// PowerShell 单引号字符串里的转义（C# `EscapePsSingle`）。
pub fn escape_powershell_single(value: &str) -> String {
    value.replace('\'', "''")
}

/// 生成 `apply-update.ps1` 的内容（C# `LaunchApplyScript` 里的 here-string）。
pub fn apply_script(update: &PreparedUpdate, target_pid: u32, relaunch_minimized: bool) -> String {
    let args = if relaunch_minimized {
        format!("'{}'", escape_powershell_single(snug_core::update::MINIMIZED_ARG))
    } else {
        "@()".to_string()
    };
    let payload = escape_powershell_single(&update.payload_dir.to_string_lossy());
    let target = escape_powershell_single(&update.target_app_dir.to_string_lossy());
    let exe = escape_powershell_single(
        &update.target_app_dir.join(TARGET_EXE).to_string_lossy(),
    );
    let staging = escape_powershell_single(&update.staging_root.to_string_lossy());

    format!(
        r#"$ErrorActionPreference = 'Stop'
$targetPid = {target_pid}
$deadline = (Get-Date).AddSeconds(30)
while ((Get-Date) -lt $deadline) {{
    $p = Get-Process -Id $targetPid -ErrorAction SilentlyContinue
    if (-not $p) {{ break }}
    Start-Sleep -Milliseconds 250
}}
Start-Sleep -Milliseconds 350

$src = '{payload}'
$dst = '{target}'
Get-ChildItem -Path $src -Recurse -File | ForEach-Object {{
    $rel = $_.FullName.Substring($src.Length).TrimStart('\', '/')
    $outFile = Join-Path $dst $rel
    $outDir = Split-Path -Parent $outFile
    if (-not (Test-Path $outDir)) {{ New-Item -ItemType Directory -Path $outDir -Force | Out-Null }}
    Copy-Item -Path $_.FullName -Destination $outFile -Force
}}

$launchExe = '{exe}'
$launchArgs = {args}
if ($launchArgs.Count -gt 0) {{
    Start-Process -FilePath $launchExe -ArgumentList $launchArgs -WorkingDirectory $dst
}} else {{
    Start-Process -FilePath $launchExe -WorkingDirectory $dst
}}

Start-Sleep -Seconds 2
Remove-Item -Path '{staging}' -Recurse -Force -ErrorAction SilentlyContinue
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
    /// 当前程序所在目录（覆盖目标）。
    app_dir: PathBuf,
    /// 临时目录根。
    temp_root: PathBuf,
}

impl<H: HttpClient> AppUpdateService<H, TarExtractor> {
    /// 生产用构造：版本取 [`APP_VERSION`]，包形态按 Rust 版当自包含。
    pub fn new(http: H, app_dir: impl Into<PathBuf>, temp_root: impl Into<PathBuf>) -> Self {
        Self {
            http,
            extractor: TarExtractor,
            current_version: APP_VERSION.to_string(),
            package_kind: ReleasePackageKind::SelfContained,
            app_dir: app_dir.into(),
            temp_root: temp_root.into(),
        }
    }
}

impl<H: HttpClient, E: PackageExtractor> AppUpdateService<H, E> {
    pub fn with_parts(
        http: H,
        extractor: E,
        current_version: impl Into<String>,
        package_kind: ReleasePackageKind,
        app_dir: impl Into<PathBuf>,
        temp_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            http,
            extractor,
            current_version: current_version.into(),
            package_kind,
            app_dir: app_dir.into(),
            temp_root: temp_root.into(),
        }
    }

    pub fn current_version(&self) -> &str {
        &self.current_version
    }

    /// 检查更新（C# `CheckForUpdateAsync`）。
    pub fn check_for_update(&self) -> Result<UpdateCheckResult, UpdateError> {
        let mut release = None;
        match self.http.get(LATEST_API_URL) {
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
                let response = self
                    .http
                    .get(LATEST_WEB_URL)
                    .map_err(UpdateError::Network)?;
                fallback_release(&response.final_url).ok_or(UpdateError::NoRelease)?
            }
        };

        Ok(build_check_result(&release, &self.current_version, self.package_kind))
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

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let staging_root = self
            .temp_root
            .join(STAGING_PREFIX)
            .join(staging_dir_name(&check.latest_version, unique));
        std::fs::create_dir_all(&staging_root)
            .map_err(|error| UpdateError::Io(error.to_string()))?;

        let zip_path = staging_root.join(&check.package_asset_name);
        let payload_dir = staging_root.join("payload");

        // 校验和拿不到不算错（C# 只是 Warn，然后继续）。
        let checksums = check
            .checksums_download_url
            .as_ref()
            .and_then(|url| self.http.get(url).ok())
            .filter(HttpResponse::is_success)
            .map(|response| response.text());

        if let Err(message) = self.http.download(&download_url, &zip_path, progress) {
            let _ = std::fs::remove_dir_all(&staging_root);
            return Err(UpdateError::Network(message));
        }

        if let Some(text) = checksums {
            match try_find_sha256(&text, &check.package_asset_name) {
                Some(expected) => {
                    let actual = sha256_file(&zip_path)?;
                    if !actual.eq_ignore_ascii_case(&expected) {
                        let _ = std::fs::remove_dir_all(&staging_root);
                        return Err(UpdateError::ChecksumMismatch { expected, actual });
                    }
                }
                // 有意偏离 C#：`AppUpdateService.cs` 141-151 行在这一支是**静默跳过**
                // （`TryFindSha256` 返回 false 时外层没有 else，连 Warn 都没有）。
                // 行为保持一致（不校验、照常继续），只补一行告警：checksums.txt 改名
                // 或包名对不上时，整条完整性校验会悄无声息地失效，至少让它留痕。
                None => crate::warn(&missing_checksum_warning(&check.package_asset_name)),
            }
        }

        if let Err(message) = self.extractor.extract(&zip_path, &payload_dir) {
            let _ = std::fs::remove_dir_all(&staging_root);
            return Err(UpdateError::ExtractFailed(message));
        }

        let target_exe = payload_dir.join(TARGET_EXE);
        if !target_exe.exists() {
            let _ = std::fs::remove_dir_all(&staging_root);
            return Err(UpdateError::MissingPayload { exe: TARGET_EXE.to_string() });
        }

        Ok(PreparedUpdate {
            version: check.latest_version.clone(),
            staging_root,
            payload_dir,
            target_app_dir: self.app_dir.clone(),
        })
    }
}

/// "checksums.txt 里没有这个包名"的告警文案。
///
/// C# `AppUpdateService.cs:141-151` 在这一支是静默跳过（`TryFindSha256` 返回 false，
/// 外层没有 else）；我们行为一致，只把这行日志补上，文案抽出来单独测。
fn missing_checksum_warning(asset: &str) -> String {
    format!("checksums.txt 里找不到 {asset}，跳过 SHA-256 校验（C# 同语义，此处只补日志）")
}

/// 写脚本并拉起 PowerShell（C# `LaunchApplyScript`）。
///
/// 脚本自己会等本进程退出，所以这里不等它、也不检查退出码。
pub fn launch_apply_script(
    update: &PreparedUpdate,
    relaunch_minimized: bool,
) -> Result<PathBuf, UpdateError> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let script_path = update.staging_root.join("apply-update.ps1");
    let script = apply_script(update, std::process::id(), relaunch_minimized);
    std::fs::write(&script_path, utf8_with_bom(&script))
        .map_err(|error| UpdateError::Io(error.to_string()))?;

    Command::new("powershell.exe")
        .args([
            "-NoProfile".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-WindowStyle".to_string(),
            "Hidden".to_string(),
            "-File".to_string(),
            script_path.to_string_lossy().to_string(),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|error| UpdateError::Io(error.to_string()))?;
    Ok(script_path)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    // ----- 纯逻辑 -----

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
        let release = fallback_release("https://github.com/nicocart/snug-win/releases/tag/v1.2.0")
            .expect("能拼出来");
        assert_eq!(release.tag_name, "v1.2.0");
        assert_eq!(release.normalized_version, "1.2.0");
        assert_eq!(release.assets.len(), 3);
        assert_eq!(release.assets[2].name, CHECKSUMS_ASSET);
        assert_eq!(
            release.assets[0].download_url,
            "https://github.com/nicocart/snug-win/releases/download/v1.2.0/Snug-v1.2.0-win-x64-selfcontained.zip"
        );
        assert!(fallback_release("https://example.com/") .is_none());
    }

    #[test]
    fn check_result_picks_assets_by_package_kind() {
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
            build_check_result(&release, "1.2.0", ReleasePackageKind::SelfContained);
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
        // html_url 为空时按 tag 拼一个。
        assert_eq!(
            self_contained.release_url,
            "https://github.com/nicocart/snug-win/releases/tag/v1.3.0"
        );

        let framework =
            build_check_result(&release, "1.3.0", ReleasePackageKind::FrameworkDependent);
        assert!(!framework.has_update, "同版本不算更新");
        assert_eq!(
            framework.package_download_url.as_deref(),
            Some("https://example.invalid/fdd.zip")
        );

        // 缺资产时下载地址是 None。
        let missing = build_check_result(&release, "1.0.0", ReleasePackageKind::SelfContained);
        assert!(missing.package_download_url.is_some());
        let no_assets = UpdateReleaseInfo { assets: Vec::new(), ..release };
        let none = build_check_result(&no_assets, "1.0.0", ReleasePackageKind::SelfContained);
        assert_eq!(none.package_download_url, None);
    }

    #[test]
    fn staging_name_is_sanitized() {
        let name = staging_dir_name("1.2.3-beta/evil", 0xAB);
        assert!(name.starts_with("1.2.3-beta_evil-"));
        assert_eq!(name.len(), "1.2.3-beta_evil-".len() + 32);
        assert!(!name.contains('/'));
    }

    #[test]
    fn powershell_escaping_doubles_single_quotes() {
        assert_eq!(escape_powershell_single(r"C:\it's"), r"C:\it''s");
        assert_eq!(escape_powershell_single("plain"), "plain");
    }

    #[test]
    fn apply_script_contains_paths_and_args() {
        let update = PreparedUpdate {
            version: "1.3.0".to_string(),
            staging_root: PathBuf::from(r"C:\Temp\SnugWin-Update\1.3.0-ab"),
            payload_dir: PathBuf::from(r"C:\Temp\SnugWin-Update\1.3.0-ab\payload"),
            target_app_dir: PathBuf::from(r"C:\it's\SnugWin"),
        };
        let script = apply_script(&update, 4242, true);
        assert!(script.contains("$targetPid = 4242"));
        assert!(script.contains(r"$src = 'C:\Temp\SnugWin-Update\1.3.0-ab\payload'"));
        assert!(script.contains(r"$dst = 'C:\it''s\SnugWin'"), "单引号要转义：{script}");
        assert!(script.contains(r"$launchExe = 'C:\it''s\SnugWin\snug.exe'"));
        assert!(script.contains("$launchArgs = '--minimized'"));
        assert!(script.contains("Remove-Item -Path 'C:\\Temp\\SnugWin-Update\\1.3.0-ab'"));

        let plain = apply_script(&update, 1, false);
        assert!(plain.contains("$launchArgs = @()"));
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
        downloads: Mutex<Vec<(String, PathBuf)>>,
    }

    impl FakeHttp {
        fn new() -> Self {
            Self { responses: HashMap::new(), download_bytes: Vec::new(), downloads: Mutex::new(Vec::new()) }
        }

        fn with(mut self, url: &str, response: HttpResponse) -> Self {
            self.responses.insert(url.to_string(), response);
            self
        }
    }

    impl HttpClient for FakeHttp {
        fn get(&self, url: &str) -> Result<HttpResponse, String> {
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
            r#"{{"tag_name":"{tag}","html_url":"https://github.com/nicocart/snug-win/releases/tag/{tag}","body":"notes","assets":[
              {{"name":"Snug-v{}-win-x64-selfcontained.zip","browser_download_url":"https://example.invalid/self.zip","size":10}},
              {{"name":"checksums.txt","browser_download_url":"https://example.invalid/checksums.txt","size":2}}]}}"#,
            tag.trim_start_matches('v')
        )
    }

    fn service(http: FakeHttp, create_exe: bool) -> AppUpdateService<FakeHttp, FakeExtractor> {
        AppUpdateService::with_parts(
            http,
            FakeExtractor { create_exe },
            "1.2.0",
            ReleasePackageKind::SelfContained,
            temp_root(),
            temp_root(),
        )
    }

    #[test]
    fn check_uses_the_api_first() {
        let http = FakeHttp::new().with(
            LATEST_API_URL,
            HttpResponse::new(200, release_json("v1.3.0"), LATEST_API_URL),
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
        let http = FakeHttp::new().with(
            LATEST_WEB_URL,
            HttpResponse::new(
                200,
                "",
                "https://github.com/nicocart/snug-win/releases/tag/v1.4.0",
            ),
        );
        let check = service(http, true).check_for_update().expect("兜底成功");
        assert!(check.has_update);
        assert_eq!(check.latest_version, "1.4.0");
        assert_eq!(check.release_notes, "", "兜底没有发布说明");
        assert!(check.package_download_url.is_some());
    }

    #[test]
    fn check_falls_back_when_api_returns_an_error_status() {
        let http = FakeHttp::new()
            .with(LATEST_API_URL, HttpResponse::new(500, "boom", LATEST_API_URL))
            .with(
                LATEST_WEB_URL,
                HttpResponse::new(200, "", "https://github.com/nicocart/snug-win/releases/tag/v9.9.9"),
            );
        let check = service(http, true).check_for_update().expect("兜底成功");
        assert_eq!(check.latest_version, "9.9.9");
    }

    #[test]
    fn check_without_any_release_is_an_error() {
        let http = FakeHttp::new().with(
            LATEST_WEB_URL,
            HttpResponse::new(200, "", "https://github.com/nicocart/snug-win"),
        );
        let error = service(http, true).check_for_update().expect_err("拿不到版本");
        assert_eq!(error, UpdateError::NoRelease);
        assert!(error.to_string().contains("无法获取最新版本信息"));
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
        assert!(prepared.payload_dir.join(TARGET_EXE).exists());
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
        let service = service(FakeHttp::new(), false);
        let error = service
            .download_and_prepare(&check_with(None), &mut |_| {})
            .expect_err("包里没有 exe 要中止");
        assert_eq!(error, UpdateError::MissingPayload { exe: TARGET_EXE.to_string() });
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn checksums_without_the_asset_name_skips_verification_like_csharp() {
        // C# 的 TryFindSha256 找不到包名时返回 false → 整个校验分支跳过（不是放行错误哈希）。
        // 这条和上一条一起把两条分支都钉住：名字对上必须校验，对不上才跳过。
        // 注意：C# `AppUpdateService.cs:141-151` 在这一支连日志都没有（静默跳过），
        // 我们的行为保持一致，只补一行 Warn（见 missing_checksum_warning），所以这里
        // 断言的是"不报错、照常继续"，日志文案由 missing_checksum_warning_names_the_asset 覆盖。
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
        let prepared = service(http, true)
            .download_and_prepare(&check_with(Some("https://example.invalid/checksums.txt")), &mut |_| {})
            .expect("没有该包名的校验和就跳过校验");
        assert!(prepared.payload_dir.join(TARGET_EXE).exists());
        let _ = std::fs::remove_dir_all(&prepared.staging_root);
    }

    #[test]
    fn checksums_download_failure_still_proceeds() {
        // checksums 拿不到 → 跳过校验（C# 只是 Warn）。
        let mut http = FakeHttp::new();
        http.download_bytes = b"payload".to_vec();
        let service = service(http, true);
        let prepared = service
            .download_and_prepare(&check_with(Some("https://example.invalid/missing.txt")), &mut |_| {})
            .expect("没有校验和也能继续");
        assert!(prepared.payload_dir.join(TARGET_EXE).exists());
        let _ = std::fs::remove_dir_all(&prepared.staging_root);
    }

    #[test]
    fn current_version_comes_from_the_cargo_package_version() {
        let service = service(FakeHttp::new(), true);
        assert_eq!(service.current_version(), env!("CARGO_PKG_VERSION"));

        // 生产构造与 crate 版本同源（根 Cargo.toml 的 [workspace.package] version）。
        let production = AppUpdateService::new(FakeHttp::new(), ".", ".");
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
    fn missing_checksum_warning_names_the_asset() {
        // 名字对不上时行为不变（继续、不校验），但必须留一行日志说明"校验没生效"。
        let message = missing_checksum_warning("Snug-v1.3.0-win-x64-selfcontained.zip");
        assert!(message.contains("Snug-v1.3.0-win-x64-selfcontained.zip"), "{message}");
        assert!(message.contains("跳过 SHA-256 校验"), "{message}");
    }
}
