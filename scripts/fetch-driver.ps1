#Requires -Version 7.0
<#
.SYNOPSIS
    取一次 virtual-display-rs 的官方安装包，校验 SHA-256 后放到 driver\。

.DESCRIPTION
    驱动不重编、不改动，只下载官方 release 里的安装包并核对哈希：
      driver\virtual-desktop-driver-installer-x64.zip

    版本与哈希来自 driver\source.lock.json；一致性门禁会拒绝与当前源码协议不兼容的包。
    升级驱动 = 更新源码清单、重新生成兼容的 releasePackage，并重新跑一遍真机验收。

    必须在 cargo test/build 前运行：应用用 include_bytes! 把原版 zip 内嵌到单 exe。
    发布时不用再附带 driver\ 目录，静态门禁会核对 exe 里完整内嵌的字节。

.EXAMPLE
    pwsh -File scripts/fetch-driver.ps1
    pwsh -File scripts/fetch-driver.ps1 -Force   # 重新下载
#>
[CmdletBinding()]
param(
    # 下载到哪个目录（默认 repo 根的 driver\）。
    [string]$Destination,
    # 文件已存在且哈希正确时也重新下载。
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$consistencyScript = Join-Path $PSScriptRoot 'driver-package-consistency.ps1'
. $consistencyScript
$consistencyIssues = @(Get-DriverPackageConsistencyIssues -RepositoryRoot $repoRoot -CheckProtocolSource)
if ($consistencyIssues.Count -gt 0) {
    throw "source.lock.json 中的驱动发行包与源码/应用不兼容，拒绝下载：`n - $($consistencyIssues -join "`n - ")"
}
$lock = Get-Content -LiteralPath (Join-Path $repoRoot 'driver/source.lock.json') -Raw | ConvertFrom-Json
$package = $lock.releasePackage
$Version = [string]$package.driverVersion
$Url = "https://github.com/MolotovCherry/virtual-display-rs/releases/download/v$Version/$($package.archiveName)"
$ExpectedSha256 = [string]$package.archiveSha256
if (-not $Destination) { $Destination = Join-Path $repoRoot 'driver' }
$zipPath = Join-Path $Destination 'virtual-desktop-driver-installer-x64.zip'

New-Item -ItemType Directory -Force -Path $Destination | Out-Null

function Test-Hash([string]$path) {
    (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToUpperInvariant()
}

if ((Test-Path -LiteralPath $zipPath) -and -not $Force) {
    $current = Test-Hash $zipPath
    if ($current -eq $ExpectedSha256) {
        Write-Host "[OK] 驱动包已存在且哈希正确：$zipPath"
        return
    }
    Write-Host "驱动包哈希不符（$current），重新下载"
}

Write-Host "下载 virtual-display-rs v$Version …"
Write-Host "  $Url"
$tmp = "$zipPath.download"
if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Force }
Invoke-WebRequest -Uri $Url -OutFile $tmp -UseBasicParsing

$actual = Test-Hash $tmp
if ($actual -ne $ExpectedSha256) {
    Remove-Item -LiteralPath $tmp -Force
    throw "驱动包 SHA-256 校验失败：期望 $ExpectedSha256，实际 $actual"
}

Move-Item -LiteralPath $tmp -Destination $zipPath -Force
$archiveIssues = @(Get-DriverPackageConsistencyIssues -RepositoryRoot $repoRoot -ArchivePath $zipPath -CheckProtocolSource)
if ($archiveIssues.Count -gt 0) {
    Remove-Item -LiteralPath $zipPath -Force
    throw "下载的驱动发行包未通过一致性门禁：`n - $($archiveIssues -join "`n - ")"
}
$size = (Get-Item -LiteralPath $zipPath).Length
Write-Host ("[OK] virtual-display-rs v{0} 已就绪（{1:N0} B，SHA-256 已核对）" -f $Version, $size)
