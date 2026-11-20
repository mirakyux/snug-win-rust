//! 版本比较、校验和解析、GitHub Release 解析与开机启动任务 XML。
//!
//! 移植自 C# 文件 `SnugWin.Core/Support/UpdateAndStartup.cs`：
//! `AppVersionHelper`、`ChecksumParser`、`GitHubReleaseParser`、`StartupTaskDefinition`
//! 以及 `ReleasePackageKind` / `UpdateAssetInfo` / `UpdateReleaseInfo`。
//! C# 里它们是静态类，Rust 里按职责拆成本模块的公开函数。

use serde_json::Value;

/// 发布包的两种形态（对应 C# `ReleasePackageKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasePackageKind {
    SelfContained,
    FrameworkDependent,
}

impl ReleasePackageKind {
    /// 资源名里的后缀，见 [`preferred_asset_name`]。
    pub fn suffix(self) -> &'static str {
        match self {
            Self::SelfContained => "selfcontained",
            Self::FrameworkDependent => "frameworkdependent",
        }
    }
}

/// 一个发布附件（对应 C# `UpdateAssetInfo`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAssetInfo {
    pub name: String,
    pub download_url: String,
    pub size: i64,
}

impl UpdateAssetInfo {
    pub fn new(name: impl Into<String>, download_url: impl Into<String>, size: i64) -> Self {
        Self { name: name.into(), download_url: download_url.into(), size }
    }
}

/// 一次最新发布（对应 C# `UpdateReleaseInfo`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReleaseInfo {
    pub tag_name: String,
    pub normalized_version: String,
    pub html_url: String,
    pub body: String,
    pub assets: Vec<UpdateAssetInfo>,
}

/// 规范化版本号（对应 C# `AppVersionHelper.Normalize`）。
///
/// 去空白、去前导 `v`/`V`、截掉 `+` 之后的构建元数据、截掉 `-` 之后的预发布标记；
/// 结果为空时返回 `"0.0.0"`。
pub fn normalize(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "0.0.0".to_string();
    }
    let mut text = trimmed;
    if let Some(rest) = text.strip_prefix(['v', 'V']) {
        text = rest;
    }
    if let Some(index) = text.find('+') {
        text = &text[..index];
    }
    if let Some(index) = text.find('-') {
        text = &text[..index];
    }
    if text.trim().is_empty() {
        "0.0.0".to_string()
    } else {
        text.to_string()
    }
}

/// 解析成 `(major, minor, build, revision)`；缺的段补 0，负数不存在，
/// 对齐 C# `Version`（至少两段，最多四段）。
pub fn try_parse_semver(raw: &str) -> Option<(i64, i64, i64, i64)> {
    let normalized = normalize(raw);
    let parts: Vec<&str> = normalized.split('.').collect();
    if parts.len() < 2 || parts.len() > 4 {
        return None;
    }
    let mut numbers = [0i64; 4];
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            return None;
        }
        let value: i64 = part.parse().ok()?;
        if value < 0 {
            return None;
        }
        numbers[index] = value;
    }
    Some((numbers[0], numbers[1], numbers[2], numbers[3]))
}

/// 候选版本是否比当前版本新（对应 C# `AppVersionHelper.IsNewer`）。
///
/// 候选解析失败 → `false`；当前版本解析失败 → `true`（任何能解析的都比"读不出版本"新）。
pub fn is_newer(candidate_tag: &str, current_version: &str) -> bool {
    let Some(candidate) = try_parse_semver(candidate_tag) else {
        return false;
    };
    match try_parse_semver(current_version) {
        Some(current) => candidate > current,
        None => true,
    }
}

/// 发布资源文件名（对应 C# `AppVersionHelper.PreferredAssetName`）。
pub fn preferred_asset_name(tag_or_version: &str, kind: ReleasePackageKind) -> String {
    let version = normalize(tag_or_version);
    format!("SnugWin-v{version}-win-x64-{}.zip", kind.suffix())
}

/// 从 `checksums.txt` 里找某个文件名的 SHA-256（对应 C# `ChecksumParser.TryFindSha256`）。
///
/// 每行形状是 `64 位十六进制 + 空白 + 文件名`（GitHub 的 `sha256sum` 格式，
/// 文件名前可能带 `*`，也可能带目录）；找到时返回大写的哈希。
pub fn try_find_sha256(checksums_text: &str, file_name: &str) -> Option<String> {
    if checksums_text.trim().is_empty() || file_name.trim().is_empty() {
        return None;
    }
    let target = path_file_name(file_name.trim());

    for raw_line in checksums_text.split('\n') {
        let line = raw_line.trim();
        if line.len() < 66 {
            continue;
        }
        // 哈希必须是前 64 个 ASCII 字符，第 65 个字符是空格或制表符。
        let separator = line.find([' ', '\t']);
        if separator != Some(64) {
            continue;
        }
        let hash = &line[..64];
        if !is_hex_64(hash) {
            continue;
        }
        let name = line[65..].trim().trim_start_matches('*');
        if path_file_name(name).eq_ignore_ascii_case(target) {
            return Some(hash.to_ascii_uppercase());
        }
    }
    None
}

fn is_hex_64(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// `Path.GetFileName` 的等价物（Windows 路径，`/` 也当分隔符）。
fn path_file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or("")
}

/// 解析 GitHub "latest release" JSON（对应 C# `GitHubReleaseParser.ParseLatestRelease`）。
///
/// 解析失败、不是对象、`tag_name` 为空都返回 `None`；附件缺名字或缺下载地址时跳过。
pub fn parse_latest_release(json: &str) -> Option<UpdateReleaseInfo> {
    if json.trim().is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(json).ok()?;
    let object = value.as_object()?;

    let tag = object.get("tag_name").and_then(Value::as_str).unwrap_or("").trim();
    if tag.is_empty() {
        return None;
    }
    let html_url = object.get("html_url").and_then(Value::as_str).unwrap_or("");
    let body = object.get("body").and_then(Value::as_str).unwrap_or("");

    let mut assets = Vec::new();
    if let Some(items) = object.get("assets").and_then(Value::as_array) {
        for item in items {
            let Some(item) = item.as_object() else {
                continue;
            };
            let name = item.get("name").and_then(Value::as_str).unwrap_or("");
            let download_url =
                item.get("browser_download_url").and_then(Value::as_str).unwrap_or("");
            // C# 只在 TryGetInt64 成功时取 size，否则按 0。
            let size = item.get("size").and_then(Value::as_i64).unwrap_or(0);
            if !name.is_empty() && !download_url.is_empty() {
                assets.push(UpdateAssetInfo::new(name, download_url, size));
            }
        }
    }

    Some(UpdateReleaseInfo {
        tag_name: tag.to_string(),
        normalized_version: normalize(tag),
        html_url: html_url.to_string(),
        body: body.to_string(),
        assets,
    })
}

/// 开机启动任务（对应 C# `StartupTaskDefinition`）。
pub const TASK_NAME: &str = "SnugWinAutoStart";
/// 静默启动参数。
pub const MINIMIZED_ARG: &str = "--minimized";

/// 生成计划任务的 XML（对应 C# `StartupTaskDefinition.BuildTaskXml`）。
///
/// `working_directory` 缺省时取可执行文件所在目录；XML 特殊字符会转义。
pub fn build_task_xml(exe_path: &str, working_directory: Option<&str>) -> String {
    let safe_exe = xml_escape(exe_path);
    let directory = working_directory.map(str::to_string).unwrap_or_else(|| parent_dir(exe_path));
    let safe_dir = xml_escape(&directory);
    let safe_args = xml_escape(MINIMIZED_ARG);

    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>SnugWin UU远程手机竖屏伴侣开机静默启动</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{safe_exe}</Command>
      <Arguments>{safe_args}</Arguments>
      <WorkingDirectory>{safe_dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>"#
    )
}

/// 对应 C# `SecurityElement.Escape`。
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// `Path.GetDirectoryName` 的等价物（Windows 路径）。
fn parent_dir(path: &str) -> String {
    match path.rfind(['\\', '/']) {
        None => String::new(),
        Some(0) => path[..1].to_string(),
        Some(index) => {
            let head = &path[..index];
            if head.ends_with(':') {
                // "C:\a.exe" → "C:\"
                path[..index + 1].to_string()
            } else {
                head.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 转写自 C# `UpdateAndStartupTests.AppVersionHelper_ComparesSemVerCleanly`。
    #[test]
    fn app_version_helper_compares_semver_cleanly() {
        let cases = [
            ("v1.1.2", "1.1.1+8fc71d4", true),
            ("v1.2.0", "1.1.9", true),
            ("v1.1.1", "1.1.1+abc", false),
            ("v1.1.0", "1.1.1", false),
            ("invalid", "1.1.0", false),
        ];
        for (candidate, current, expected) in cases {
            assert_eq!(is_newer(candidate, current), expected, "IsNewer({candidate:?}, {current:?})");
        }
    }

    #[test]
    fn normalize_strips_prefix_and_suffixes() {
        assert_eq!(normalize(""), "0.0.0");
        assert_eq!(normalize("   "), "0.0.0");
        assert_eq!(normalize("v1.2.3"), "1.2.3");
        assert_eq!(normalize("V1.2.3"), "1.2.3");
        assert_eq!(normalize(" 1.2.3 "), "1.2.3");
        assert_eq!(normalize("1.2.3+build.9"), "1.2.3");
        assert_eq!(normalize("v1.2.3-beta+abc"), "1.2.3");
        assert_eq!(normalize("v"), "0.0.0", "只有前缀时按空处理");
    }

    #[test]
    fn semver_parsing_follows_version_rules() {
        assert_eq!(try_parse_semver("v1.2"), Some((1, 2, 0, 0)));
        assert_eq!(try_parse_semver("1.2.3.4"), Some((1, 2, 3, 4)));
        assert_eq!(try_parse_semver("1"), None, "C# Version 至少两段");
        assert_eq!(try_parse_semver("1.2.3.4.5"), None);
        assert_eq!(try_parse_semver("1.x"), None);
        // C# 的 Normalize 会把空串变成 "0.0.0"，所以空输入也算能解析。
        assert_eq!(try_parse_semver(""), Some((0, 0, 0, 0)));

        // 当前版本读不出来时，能读出来的候选算"更新"。
        assert!(is_newer("1.0.0", "not-a-version"));
    }

    /// 转写自 C# `UpdateAndStartupTests.AppVersionHelper_FormatsAssetName`。
    #[test]
    fn app_version_helper_formats_asset_name() {
        assert_eq!(
            preferred_asset_name("v1.2.0", ReleasePackageKind::SelfContained),
            "SnugWin-v1.2.0-win-x64-selfcontained.zip"
        );
        assert_eq!(
            preferred_asset_name("1.2.0", ReleasePackageKind::FrameworkDependent),
            "SnugWin-v1.2.0-win-x64-frameworkdependent.zip"
        );
    }

    /// 转写自 C# `UpdateAndStartupTests.ChecksumParser_FindsMatchingHashCaseInsensitive`。
    #[test]
    fn checksum_parser_finds_matching_hash_case_insensitive() {
        let text = "643AD866A8BA65A214BD8AE7239EA338174BD127629013055195B418BC6E598F  SnugWin-v1.1.1-win-x64-frameworkdependent.zip\n\
                    a6e8201c5d361a05cd5003e309891c8edc64c59d6e06d1603abff7dfba7a0214  dist/SnugWin-v1.1.1-win-x64-selfcontained.zip\n";

        assert_eq!(
            try_find_sha256(text, "snugwin-v1.1.1-win-x64-selfcontained.zip"),
            Some("A6E8201C5D361A05CD5003E309891C8EDC64C59D6E06D1603ABFF7DFBA7A0214".to_string())
        );
        assert_eq!(try_find_sha256(text, "missing.zip"), None);
        // `sha256sum` 的二进制模式标记（*）与反斜杠路径也要能认出来。
        let starred = "0000000000000000000000000000000000000000000000000000000000000000 *C:\\dist\\SnugWin.zip\n";
        assert_eq!(
            try_find_sha256(starred, "SnugWin.zip"),
            Some("0".repeat(64))
        );
        // 不是 64 位十六进制的行直接跳过。
        let malformed = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz  SnugWin.zip\n";
        assert_eq!(try_find_sha256(malformed, "SnugWin.zip"), None);
    }

    /// 转写自 C# `UpdateAndStartupTests.GitHubReleaseParser_ParsesLatestReleaseJson`。
    #[test]
    fn github_release_parser_parses_latest_release_json() {
        let json = r###"{
          "tag_name": "v1.2.0",
          "html_url": "https://github.com/nicocart/snug-win/releases/tag/v1.2.0",
          "body": "## Changes\n- Added auto update",
          "assets": [
            {
              "name": "SnugWin-v1.2.0-win-x64-selfcontained.zip",
              "browser_download_url": "https://example.invalid/self.zip",
              "size": 76000000
            },
            {
              "name": "checksums.txt",
              "browser_download_url": "https://example.invalid/checksums.txt",
              "size": 325
            }
          ]
        }"###;

        let info = parse_latest_release(json).expect("应能解析");
        assert_eq!(info.tag_name, "v1.2.0");
        assert_eq!(info.normalized_version, "1.2.0");
        assert_eq!(info.assets.len(), 2);
        assert_eq!(info.assets[1].name, "checksums.txt");
        assert_eq!(info.assets[0].size, 76_000_000);
    }

    #[test]
    fn github_release_parser_rejects_bad_input() {
        assert_eq!(parse_latest_release(""), None);
        assert_eq!(parse_latest_release("not json"), None);
        assert_eq!(parse_latest_release("[]"), None, "根节点必须是对象");
        assert_eq!(parse_latest_release("{\"tag_name\":\"  \"}"), None, "tag 为空");
        // 没有 assets 时返回空列表。
        let info = parse_latest_release("{\"tag_name\":\"v2.0.0\"}").expect("parse");
        assert!(info.assets.is_empty());
        assert_eq!(info.normalized_version, "2.0.0");
    }

    /// Unlike the upstream task, the GUI is never elevated automatically at logon.
    #[test]
    fn startup_task_definition_builds_expected_xml() {
        let xml = build_task_xml(r"C:\Tools\Snug & Win\SnugWin.App.exe", None);

        assert!(xml.starts_with(r#"<?xml version="1.0" encoding="UTF-16"?>"#));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(!xml.contains("HighestAvailable"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        // & 要转义，否则 XML 不合法。
        assert!(xml.contains(r"<Command>C:\Tools\Snug &amp; Win\SnugWin.App.exe</Command>"));
        assert!(xml.contains(&format!("<Arguments>{MINIMIZED_ARG}</Arguments>")));
        // 工作目录默认取可执行文件所在目录。
        assert!(xml.contains(r"<WorkingDirectory>C:\Tools\Snug &amp; Win</WorkingDirectory>"));
        assert_eq!(TASK_NAME, "SnugWinAutoStart");
    }

    #[test]
    fn startup_task_definition_honours_working_directory_and_escapes_quotes() {
        let xml = build_task_xml("C:\\a\"b\\app.exe", Some("D:\\it's"));
        assert!(xml.contains("C:\\a&quot;b\\app.exe"));
        assert!(xml.contains("D:\\it&apos;s"));
    }

    #[test]
    fn parent_dir_matches_windows_get_directory_name() {
        assert_eq!(parent_dir(r"C:\Tools\app.exe"), r"C:\Tools");
        assert_eq!(parent_dir(r"C:\app.exe"), r"C:\");
        assert_eq!(parent_dir("app.exe"), "");
        assert_eq!(parent_dir(r"C:\a/b.exe"), r"C:\a");
    }
}
