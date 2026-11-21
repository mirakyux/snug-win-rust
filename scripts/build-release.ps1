#Requires -Version 7.0
<#
.SYNOPSIS
    把 target\release\snug.exe 打成更新器期待的一套发布资产（纯本地，不联网、不上传）。

.DESCRIPTION
    产物（默认写到 dist\）：
      snug.exe                                 —— 可直接交付的单文件，驱动已内嵌
      Snug-v<版本>-win-x64-selfcontained.zip   —— 更新器兼容包，只有根目录 snug.exe
      checksums.txt                            —— 裸 exe 与 zip 的 SHA-256

    版本号只认 exe 的版本资源（VersionInfo.FileVersion），不从 Cargo.toml 再解析一遍：
    发出去的是 exe，exe 里的那个数才是真版本。

    资产名与 Snug-v<ver>-win-x64-selfcontained.zip 对齐（crates/snug-win 的
    update::release_asset_name(tag, SelfContained)），TARGET_EXE = "snug.exe"。

    脚本会做两件完整性的事：
      1. 打开生成的 zip 检查条目结构，确保只有根目录 snug.exe；
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
    # 期望固化在 exe 里的更新源（`owner/name`）。默认取 SNUG_RELEASES_REPO，
    # 传入时核对 EXE 是否包含该仓库坐标。
    [string]$ReleasesRepo = $(if ($env:SNUG_RELEASES_REPO) { $env:SNUG_RELEASES_REPO } else { 'mirakyux/snug-win-rust' })
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
$distExePath = Join-Path $outPath 'snug.exe'
$zipPath = Join-Path $outPath $assetName
$checksumsPath = Join-Path $outPath 'checksums.txt'

# ---------------------------------------------------------------------------
# 单文件直接交付；zip 仅保留给旧资产名匹配的自动更新器。
# 驱动安装包和脚本在编译时内嵌到 exe，不再分发外部 driver\ 目录。
# ---------------------------------------------------------------------------

$driverDir = Join-Path $repoRoot 'driver'
$driverZip = Join-Path $driverDir 'virtual-desktop-driver-installer-x64.zip'
if (-not (Test-Path -LiteralPath $driverZip)) {
    throw "缺少 driver\$([System.IO.Path]::GetFileName($driverZip))。先跑 pwsh -File scripts\fetch-driver.ps1（会核对官方 SHA-256）。"
}
$driverScript = Join-Path $driverDir 'Install-SnugDriver.ps1'
if (-not (Test-Path -LiteralPath $driverScript)) {
    throw "缺少 driver\Install-SnugDriver.ps1（编译时需要内嵌完整安装脚本）。"
}
# 必须是 **UTF-8 带 BOM**：脚本里有中文，而程序是用 `powershell.exe`（5.1）拉它的，
# 5.1 对没有 BOM 的文件按系统 ANSI 代码页读，中文注释会把语法读崩——真机上第一次
# 自动安装就是这么失败的（ParserError: switch 语句缺少代码块）。
$scriptBytes = [System.IO.File]::ReadAllBytes($driverScript)
$hasBom = $scriptBytes.Length -ge 3 -and $scriptBytes[0] -eq 0xEF -and $scriptBytes[1] -eq 0xBB -and $scriptBytes[2] -eq 0xBF
if (-not $hasBom) {
    throw "driver\Install-SnugDriver.ps1 缺 UTF-8 BOM：powershell.exe 5.1 会按 ANSI 读，中文注释会让脚本解析失败。用编辑器存成「UTF-8 with BOM」再打包。"
}

if ($exePath -ne $distExePath) {
    Copy-Item -LiteralPath $exePath -Destination $distExePath -Force
}

# 已有的同名 zip 必须先删：Compress-Archive 对已存在的包会往里追加，而不是替换。
if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
Compress-Archive -LiteralPath $distExePath -DestinationPath $zipPath -CompressionLevel Optimal

# 结构自检：只有根目录 snug.exe。
# 多一个目录都不行——更新器只认根部的 snug.exe，其余杂项会让"解压后到底跑哪个"变模糊。
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
try {
    $entries = @($archive.Entries | ForEach-Object { $_.FullName })
} finally {
    $archive.Dispose()
}
if ($entries.Count -ne 1 -or $entries[0] -cne 'snug.exe') {
    throw ("zip 结构不对：必须只有根目录 snug.exe，实际为 [{0}]" -f ($entries -join ', '))
}

# ---------------------------------------------------------------------------
# 校验和：exe 与 zip 的 SHA-256，按 sha256sum 文本模式
# ---------------------------------------------------------------------------

$zipHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
$exeHash = (Get-FileHash -LiteralPath $exePath -Algorithm SHA256).Hash.ToLowerInvariant()
if ((Get-FileHash -LiteralPath $distExePath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $exeHash) {
    throw 'dist\snug.exe 与输入 exe 的 SHA-256 不一致'
}

# 更新器按文件名查 zip 的哈希；另列裸 exe，供直接下载时校验。
$checksumsText = "$zipHash  $assetName`n$exeHash  snug.exe`n"
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
# 更新源：核对预期的仓库坐标
# ---------------------------------------------------------------------------

if ($ReleasesRepo) {
    # 编译期常量在二进制里就是一段 ASCII，按字节找即可。
    $bytes = [System.IO.File]::ReadAllBytes($exePath)
    $haystack = [System.Text.Encoding]::Latin1.GetString($bytes)
    if ($haystack.Contains($ReleasesRepo)) {
        Write-Host ("更新源       : {0}（已固化在 exe 里）" -f $ReleasesRepo)
    } else {
        throw "exe 里找不到预期更新源 '$ReleasesRepo'，请检查构建时的仓库配置"
    }
} else {
    Write-Host '更新源       : 未检查（传入 -ReleasesRepo 可核对 EXE 的更新源）' -ForegroundColor Yellow
}

# ---------------------------------------------------------------------------
# 摘要
# ---------------------------------------------------------------------------

$exeInfo = Get-Item -LiteralPath $exePath
$zipInfo = Get-Item -LiteralPath $zipPath

Write-Host ''
Write-Host '=== SnugWin 发布资产 ==='
Write-Host ("exe          : {0}  ({1:N0} B)" -f $distExePath, $exeInfo.Length)
Write-Host ("zip          : {0}  ({1:N0} B)" -f $zipInfo.FullName, $zipInfo.Length)
Write-Host ("zip 条目     : {0}" -f ($entries -join ', '))
Write-Host ("版本（资源） : {0}" -f $version)
Write-Host ("exe SHA-256  : {0}" -f $exeHash)
Write-Host ("zip  SHA-256 : {0}" -f $zipHash)
Write-Host ("checksums.txt: {0}" -f $checksumsPath)
Write-Host '--- checksums.txt ---'
Write-Host $checksumsText.TrimEnd()
