#Requires -Version 7.0
<#
.SYNOPSIS
    把 target\release\snug.exe 打成更新器期待的一套发布资产（纯本地，不联网、不上传）。

.DESCRIPTION
    产物（默认写到 dist\）：
      Snug-v<版本>-win-x64-selfcontained.zip   —— 根目录 snug.exe + driver\（安装脚本与官方驱动包）
      checksums.txt                            —— `<64 位小写十六进制><两个空格><zip 文件名>`

    版本号只认 exe 的版本资源（VersionInfo.FileVersion），不从 Cargo.toml 再解析一遍：
    发出去的是 exe，exe 里的那个数才是真版本。

    资产名与 Snug-v<ver>-win-x64-selfcontained.zip 对齐（crates/snug-win 的
    update::release_asset_name(tag, SelfContained)），TARGET_EXE = "snug.exe"。

    脚本会做两件完整性的事：
      1. 打开生成的 zip 检查条目结构，确保根部只有 snug.exe、外加 driver\ 里的
         安装脚本与官方驱动包；
      2. 用 Rust 侧的实现（sha256_hex）对同一个文件再算一遍 SHA-256，和
         Get-FileHash 的结果比对——两套实现各算各的，最容易在发布链上悄悄分叉。
         不想跑 cargo 可以加 -SkipRustCrossCheck。

.EXAMPLE
    pwsh -File scripts/build-release.ps1
    pwsh -File scripts/build-release.ps1 -SkipRustCrossCheck
#>
[CmdletBinding()]
param(
    # 要打包的 exe（默认 release 产物）。
    [string]$Exe,
    # 输出目录（会被创建；同名 zip 先删再写）。
    [string]$OutDir,
    # 跳过 SHA-256 交叉校验（只在明确知道自己在干什么时用）。
    [switch]$SkipRustCrossCheck,
    # 期望固化在 exe 里的更新源（`owner/name`）。默认取构建时的
    # SNUG_RELEASES_REPO；给了就核对 exe 里真的有它，避免发出去的包"检查更新"不可用。
    [string]$ReleasesRepo = $env:SNUG_RELEASES_REPO
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# 辅助函数（PowerShell 按执行顺序定义，必须放在调用之前）
# ---------------------------------------------------------------------------

function Find-VcVars64 {
    if ($env:VSINSTALLDIR) {
        $candidate = Join-Path $env:VSINSTALLDIR 'VC\Auxiliary\Build\vcvars64.bat'
        if (Test-Path -LiteralPath $candidate) { return $candidate }
    }
    $roots = @()
    if ($env:ProgramFiles) { $roots += (Join-Path $env:ProgramFiles 'Microsoft Visual Studio') }
    if (${env:ProgramFiles(x86)}) { $roots += (Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio') }

    foreach ($root in $roots) {
        if (-not (Test-Path -LiteralPath $root)) { continue }
        foreach ($year in Get-ChildItem -LiteralPath $root -Directory -ErrorAction SilentlyContinue) {
            foreach ($edition in Get-ChildItem -LiteralPath $year.FullName -Directory -ErrorAction SilentlyContinue) {
                $candidate = Join-Path $edition.FullName 'VC\Auxiliary\Build\vcvars64.bat'
                if (Test-Path -LiteralPath $candidate) { return $candidate }
            }
        }
    }
    return $null
}

function Assert-RustSha256 {
    param(
        [Parameter(Mandatory)][string]$File,
        [Parameter(Mandatory)][string]$Expected,
        [Parameter(Mandatory)][string]$VcVars
    )

    # 用 cmd + vcvars 跑 cargo（链接需要 MSVC 环境）。输出直接透传不重定向：
    # 重定向在某些受限/CI 环境里会走管道，出问题时反而更难查；失败时让上面的输出说话。
    $command = '"' + $VcVars + '" >nul'
    $command += ' && set "SNUG_SHA256_FILE=' + $File + '"'
    $command += ' && set "SNUG_SHA256_EXPECTED=' + $Expected + '"'
    $command += ' && cargo test -p snug-win --offline --test sha256_cross -q -- --nocapture'

    & cmd.exe /c $command
    if ($LASTEXITCODE -ne 0) {
        throw "Rust 侧 SHA-256 交叉校验失败（$File，cargo 退出码 $LASTEXITCODE），详见上面的输出"
    }
}

$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $Exe) { $Exe = Join-Path $repoRoot 'target\release\snug.exe' }
if (-not $OutDir) { $OutDir = Join-Path $repoRoot 'dist' }

if (-not (Test-Path -LiteralPath $Exe)) {
    throw "找不到 $Exe。先构建：cmd /c 'vcvars64.bat && cargo build --release -p snug-app --offline'"
}
$exePath = (Resolve-Path -LiteralPath $Exe).Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$outPath = (Resolve-Path -LiteralPath $OutDir).Path

# ---------------------------------------------------------------------------
# 版本号：只读 exe 的版本资源
# ---------------------------------------------------------------------------

$rawVersion = (Get-Item -LiteralPath $exePath).VersionInfo.FileVersion
if ([string]::IsNullOrWhiteSpace($rawVersion)) {
    throw "exe 没有版本资源（FileVersion 为空），拒绝打包"
}
$version = $rawVersion.Trim()
# 资源里写成 1.2.0；若将来改成四段且最后一段是 0，按三段发布。
if ($version -match '^(\d+\.\d+\.\d+)\.0$') { $version = $Matches[1] }
if ($version -notmatch '^\d+\.\d+\.\d+$') {
    throw "exe 的 FileVersion '$rawVersion' 不是 major.minor.patch，拒绝用它拼资产名"
}

$assetName = "Snug-v$version-win-x64-selfcontained.zip"
$zipPath = Join-Path $outPath $assetName
$checksumsPath = Join-Path $outPath 'checksums.txt'

# ---------------------------------------------------------------------------
# 打包：stage 里放 snug.exe + driver\，再压成 zip
#
# 驱动必须一起发：用户拿到 zip 解压就能用，不该先去别处装一遍驱动。
# driver\ 里只有官方安装包（含哈希校验）和安装脚本，安装由脚本或设置页按钮触发。
# ---------------------------------------------------------------------------

$driverDir = Join-Path $repoRoot 'driver'
$driverZip = Join-Path $driverDir 'virtual-desktop-driver-installer-x64.zip'
if (-not (Test-Path -LiteralPath $driverZip)) {
    throw "缺少 driver\$([System.IO.Path]::GetFileName($driverZip))。先跑 pwsh -File scripts\fetch-driver.ps1（会核对官方 SHA-256）。"
}
$driverScript = Join-Path $driverDir 'Install-SnugDriver.ps1'
if (-not (Test-Path -LiteralPath $driverScript)) {
    throw "缺少 driver\Install-SnugDriver.ps1（设置页的「安装驱动」按钮靠它）。"
}

$stage = Join-Path $outPath '.stage'
if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item -LiteralPath $exePath -Destination (Join-Path $stage 'snug.exe') -Force
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'driver') | Out-Null
Copy-Item -LiteralPath $driverScript -Destination (Join-Path $stage 'driver') -Force
Copy-Item -LiteralPath (Join-Path $driverDir 'install-driver.cmd') -Destination (Join-Path $stage 'driver') -Force
Copy-Item -LiteralPath $driverZip -Destination (Join-Path $stage 'driver') -Force

# 已有的同名 zip 必须先删：Compress-Archive 对已存在的包会往里追加，而不是替换。
if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zipPath -CompressionLevel Optimal
Remove-Item -LiteralPath $stage -Recurse -Force

# 结构自检：根目录只有 snug.exe，加上 driver\ 下的安装脚本与官方安装包。
# 多一个目录都不行——更新器只认根部的 snug.exe，其余杂项会让"解压后到底跑哪个"变模糊。
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
try {
    $entries = @($archive.Entries | ForEach-Object { $_.FullName })
} finally {
    $archive.Dispose()
}
$expected = @(
    'snug.exe',
    'driver/Install-SnugDriver.ps1',
    'driver/install-driver.cmd',
    'driver/virtual-desktop-driver-installer-x64.zip'
)
$missing = @($expected | Where-Object { $entries -notcontains $_ })
$extra = @($entries | Where-Object { $expected -notcontains $_ })
if ($missing.Count -gt 0 -or $extra.Count -gt 0) {
    throw ("zip 结构不对：缺少 [$($missing -join ', ')]，多出 [$($extra -join ', ')]")
}

# ---------------------------------------------------------------------------
# 校验和：zip 的 SHA-256，按 sha256sum 文本模式（小写 hex + 两个空格 + 文件名）
# ---------------------------------------------------------------------------

$zipHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
$exeHash = (Get-FileHash -LiteralPath $exePath -Algorithm SHA256).Hash.ToLowerInvariant()

# 更新器拿这份 checksums.txt 校验下载下来的 zip，所以哈希必须是 zip 的。
$checksumsText = "$zipHash  $assetName`n"
# ASCII + LF + 无 BOM：try_find_sha256 按行切、按第 65 个字符判断空白，BOM 会让首行对不上。
[System.IO.File]::WriteAllText($checksumsPath, $checksumsText, [System.Text.Encoding]::ASCII)

# ---------------------------------------------------------------------------
# 交叉校验：Rust 侧 sha256_hex 必须对同一个文件给出同一串
# ---------------------------------------------------------------------------

if (-not $SkipRustCrossCheck) {
    $vcvars = Find-VcVars64
    if (-not $vcvars) {
        throw "找不到 vcvars64.bat，无法跑 Rust 交叉校验；确认装过 VS Build Tools，或加 -SkipRustCrossCheck"
    }
    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path -LiteralPath $cargoBin) { $env:PATH = "$cargoBin;$env:PATH" }

    Assert-RustSha256 -File $exePath -Expected $exeHash -VcVars $vcvars
    Assert-RustSha256 -File $zipPath -Expected $zipHash -VcVars $vcvars
}

# ---------------------------------------------------------------------------
# 更新源：仓库坐标是编译期 SNUG_RELEASES_REPO，没配的话"检查更新"不可用
# ---------------------------------------------------------------------------

if ($ReleasesRepo) {
    # 编译期常量在二进制里就是一段 ASCII，按字节找即可。
    $bytes = [System.IO.File]::ReadAllBytes($exePath)
    $haystack = [System.Text.Encoding]::Latin1.GetString($bytes)
    if ($haystack.Contains($ReleasesRepo)) {
        Write-Host ("更新源       : {0}（已固化在 exe 里）" -f $ReleasesRepo)
    } else {
        throw "exe 里找不到更新源 '$ReleasesRepo'：构建时没设 SNUG_RELEASES_REPO？" +
            "（这样发出去的包「检查更新」会不可用；要么重新构建，要么去掉 -ReleasesRepo 明确接受）"
    }
} else {
    Write-Host '更新源       : 未配置（检查更新不可用；见 docs/release.md）' -ForegroundColor Yellow
}

# ---------------------------------------------------------------------------
# 摘要
# ---------------------------------------------------------------------------

$exeInfo = Get-Item -LiteralPath $exePath
$zipInfo = Get-Item -LiteralPath $zipPath

Write-Host ''
Write-Host '=== SnugWin 发布资产 ==='
Write-Host ("exe          : {0}  ({1:N0} B)" -f $exePath, $exeInfo.Length)
Write-Host ("zip          : {0}  ({1:N0} B)" -f $zipInfo.FullName, $zipInfo.Length)
Write-Host ("zip 条目     : {0}" -f ($entries -join ', '))
Write-Host ("版本（资源） : {0}" -f $version)
Write-Host ("exe SHA-256  : {0}" -f $exeHash)
Write-Host ("zip  SHA-256 : {0}" -f $zipHash)
Write-Host ("checksums.txt: {0}" -f $checksumsPath)
Write-Host '--- checksums.txt ---'
Write-Host $checksumsText.TrimEnd()
