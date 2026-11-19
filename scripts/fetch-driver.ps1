#Requires -Version 7.0
<#
.SYNOPSIS
    取一次 virtual-display-rs 的官方安装包，校验 SHA-256 后放到 driver\。

.DESCRIPTION
    驱动不重编、不改动，只下载官方 release 里的安装包并核对哈希：
      driver\virtual-desktop-driver-installer-x64.zip

    版本与哈希写死在本脚本（和 driver\Install-SnugDriver.ps1 里的一致）。
    升级驱动 = 同时改这两处的版本号与三个哈希，并重新跑一遍真机验收。

    打包脚本（build-release.ps1）要求这个 zip 存在，不存在就报错，不会悄悄
    发出一个"没有驱动"的包。

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

$Version = '0.3.1'
$Url = "https://github.com/MolotovCherry/virtual-display-rs/releases/download/v$Version/virtual-desktop-driver-installer-x64.zip"
$ExpectedSha256 = 'B3E3A5AB9B49BD56A7E753120CDDB1D913479BC5C0DFA781C3D43392DDE2FB75'

$repoRoot = Split-Path -Parent $PSScriptRoot
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
$size = (Get-Item -LiteralPath $zipPath).Length
Write-Host ("[OK] virtual-display-rs v{0} 已就绪（{1:N0} B，SHA-256 已核对）" -f $Version, $size)
