#Requires -Version 7.0
[CmdletBinding()]
param(
    [string]$OutDir = 'target/driver-build',
    [string]$SdkRoot = $env:SNUG_SDK_ROOT,
    [string]$WdkRoot = $env:SNUG_WDK_ROOT,
    [string]$SdkVersion = '10.0.26100.0',
    [string]$LibClang = $env:LIBCLANG_PATH,
    [string]$SignerThumbprint,
    [switch]$MachineSigner,
    [switch]$Offline
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$source = Join-Path $repo 'vendor/virtual-display-rs'
$lock = Get-Content -LiteralPath (Join-Path $repo 'driver/source.lock.json') -Raw | ConvertFrom-Json
if (-not $SdkRoot) {
    $SdkRoot = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots').KitsRoot10
}
if (-not $WdkRoot) { $WdkRoot = $SdkRoot }
$SdkRoot = (Resolve-Path -LiteralPath $SdkRoot).Path
$WdkRoot = (Resolve-Path -LiteralPath $WdkRoot).Path

$required = @(
    (Join-Path $SdkRoot "Include/$SdkVersion/um/Windows.h"),
    (Join-Path $WdkRoot 'Include/wdf/umdf/2.31/wdf.h'),
    (Join-Path $WdkRoot "Include/$SdkVersion/um/iddcx/1.4/IddCx.h"),
    (Join-Path $WdkRoot "bin/$SdkVersion/x64/stampinf.exe"),
    (Join-Path $WdkRoot "bin/$SdkVersion/x86/Inf2Cat.exe")
)
foreach ($file in $required) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { throw "缺少 Windows SDK / WDK 文件 $file" }
}
if ($LibClang) {
    $LibClang = (Resolve-Path -LiteralPath $LibClang).Path
    if (-not (Test-Path -LiteralPath (Join-Path $LibClang 'libclang.dll') -PathType Leaf)) {
        throw "指定目录缺少 libclang.dll：$LibClang"
    }
    $env:LIBCLANG_PATH = $LibClang
}
$env:SNUG_SDK_ROOT = $SdkRoot
$env:SNUG_WDK_ROOT = $WdkRoot
$env:SNUG_SDK_VERSION = $SdkVersion

if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere)) { throw '需要 Visual Studio 的 C++ x64 构建工具' }
    $vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $vs) { throw '未找到 Visual Studio C++ x64 安装' }
    $vcvars = Join-Path $vs 'VC/Auxiliary/Build/vcvars64.bat'
    $devEnv = & cmd.exe /d /c ('call "' + $vcvars + '" >nul && set')
    if ($LASTEXITCODE -ne 0) { throw '初始化 MSVC 环境失败' }
    foreach ($pair in $devEnv) {
        if ($pair -match '^([^=]+)=(.*)$') {
            [Environment]::SetEnvironmentVariable($Matches[1], $Matches[2], 'Process')
        }
    }
}

$out = (New-Item -ItemType Directory -Path $OutDir -Force).FullName
$cargoOut = Join-Path $out 'cargo'
$package = Join-Path $out 'package'
$abiOut = Join-Path $out 'abi'
New-Item -ItemType Directory -Path $package,$abiOut -Force | Out-Null

function Invoke-Checked {
    param([string]$Exe, [string[]]$Arguments)
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Exe 执行失败（$LASTEXITCODE）" }
}

$target = 'x86_64-pc-windows-msvc'
$cargoArgs = @('build','--locked','--release','--target',$target,'--target-dir',$cargoOut,'-p',$lock.cargoPackage)
if ($Offline) { $cargoArgs += '--offline' }
Push-Location -LiteralPath $source
try {
    Invoke-Checked cargo $cargoArgs
    $abiRustArgs = @('run','--locked','--release','--target',$target,'--target-dir',$cargoOut,'-p','wdf-umdf-sys','--example','swapchain-abi')
    if ($Offline) { $abiRustArgs += '--offline' }
    $rustAbi = & cargo @abiRustArgs
    if ($LASTEXITCODE -ne 0) { throw 'Rust ABI 探针执行失败' }
} finally {
    Pop-Location
}

$cAbi = Join-Path $abiOut 'swapchain-abi.exe'
$cSource = Join-Path $repo 'driver/swapchain-abi.cpp'
Invoke-Checked cl.exe @('/nologo','/std:c++17',
    "/I$WdkRoot/Include/wdf/umdf/2.31", "/I$WdkRoot/Include/$SdkVersion/um/iddcx/1.4",
    "/Fo$abiOut/swapchain-abi.obj", "/Fe$cAbi", $cSource)
$wdkAbi = & $cAbi
if ($LASTEXITCODE -ne 0 -or @($wdkAbi).Count -ne 14 -or ($wdkAbi -join "`n") -cne ($rustAbi -join "`n")) {
    throw "WDK 与 Rust ABI 不一致。WDK：$wdkAbi。Rust：$rustAbi"
}
$wdkAbi | Set-Content -LiteralPath (Join-Path $abiOut 'verified.txt') -Encoding ascii
Write-Host '14 项 swapchain 结构尺寸、对齐和字段偏移一致。'

$dll = Join-Path $package 'VirtualDisplayDriver.dll'
$inf = Join-Path $package 'VirtualDisplayDriver.inf'
$cat = Join-Path $package 'VirtualDisplayDriver.cat'
foreach ($file in @($dll,$inf,$cat)) {
    if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file -Force }
}
Copy-Item -LiteralPath (Join-Path $cargoOut "$target/release/virtual_display_driver.dll") -Destination $dll
Copy-Item -LiteralPath (Join-Path $source 'virtual-display-driver/VirtualDisplayDriver.inf') -Destination $inf
Copy-Item -LiteralPath (Join-Path $source 'LICENSE') -Destination (Join-Path $package 'LICENSE.txt') -Force
Copy-Item -LiteralPath (Join-Path $source 'driver-logger/LICENSE') -Destination (Join-Path $package 'DRIVER-LOGGER-LICENSE.txt') -Force
Invoke-Checked (Join-Path $WdkRoot "bin/$SdkVersion/x64/stampinf.exe") @('-f',$inf,'-a','amd64','-u','2.31.0','-d',$lock.driverDate,'-v',$lock.driverVersion)

$sign = Join-Path $SdkRoot "bin/$SdkVersion/x64/signtool.exe"
if ($SignerThumbprint) {
    if ($SignerThumbprint -notmatch '^[a-fA-F0-9]{40}$') { throw '证书指纹必须是 40 位十六进制值' }
    $signArgs = @('sign','/fd','SHA256','/sha1',$SignerThumbprint)
    if ($MachineSigner) { $signArgs += '/sm' }
    Invoke-Checked $sign ($signArgs + $dll)
}
Invoke-Checked (Join-Path $WdkRoot "bin/$SdkVersion/x86/Inf2Cat.exe") @("/driver:$package",'/os:10_VB_X64,10_CO_X64,10_NI_X64,10_GE_X64')
if (-not (Test-Path -LiteralPath $cat -PathType Leaf)) { throw '驱动目录缺少 CAT' }
if ($SignerThumbprint) {
    Invoke-Checked $sign ($signArgs + $cat)
    Invoke-Checked $sign @('verify','/pa','/v',$cat)
    Invoke-Checked $sign @('verify','/pa','/c',$cat,$dll)
    Invoke-Checked $sign @('verify','/pa','/c',$cat,$inf)
}

$files = @($dll,$inf,$cat)
$packageFiles = $files + @((Join-Path $package 'LICENSE.txt'), (Join-Path $package 'DRIVER-LOGGER-LICENSE.txt'))
$archiveKind = if ($SignerThumbprint) { 'signed' } else { 'unsigned' }
$archive = Join-Path $out "snug-virtual-display-$($lock.driverVersion)-x64-$archiveKind.zip"
Compress-Archive -LiteralPath $packageFiles -DestinationPath $archive -Force
Get-FileHash -LiteralPath ($files + $archive) -Algorithm SHA256 |
    ForEach-Object { '{0}  {1}' -f $_.Hash.ToLowerInvariant(), (Split-Path -Leaf $_.Path) } |
    Set-Content -LiteralPath (Join-Path $out 'checksums.txt') -Encoding ascii
Write-Host "驱动 $($lock.driverVersion) 文件已写入 $package"
if (-not $SignerThumbprint) { Write-Host '当前产物为未签名构建，供开发和签名使用。' }
