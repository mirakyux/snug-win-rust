# 虚拟显示器驱动：安装 / 卸载 / 状态
#
# 此脚本安装 MolotovCherry/virtual-display-rs v0.3.1 官方二进制（AGPLv3），
# 只做「校验 + 安装 + 归属记录」三件事：
#   1. 校验安装包与证书的 SHA-256（版本与哈希写死在下面，不接受其他来源）；
#   2. 用 msiexec 静默安装，再等命名管道 \\.\pipe\virtualdisplaydriver 起来；
#   3. 在 ProgramData 留一份归属记录：是我们装的才允许卸载，
#      用户自己装过的驱动一律保留。
#
# 由 snug.exe 的固定管理员驱动入口调用（GUI 默认普通权限），
# 也可以手动双击同目录的 install-driver.cmd。

[CmdletBinding()]
param(
    [ValidateSet("Install", "Uninstall", "Status")]
    [string]$Action = "Install"
)

$ErrorActionPreference = "Stop"
$systemToolsDir = [Environment]::SystemDirectory
$msiexec = Join-Path -Path $systemToolsDir -ChildPath "msiexec.exe"

$ExpectedVersion = "0.3.1"
$ExpectedZipSha256 = "B3E3A5AB9B49BD56A7E753120CDDB1D913479BC5C0DFA781C3D43392DDE2FB75"
$ExpectedMsiSha256 = "036E27CEA782B1297AB7D822CC59694105952CDE5BA885D0B2850D4005D79390"
$ExpectedCertSha256 = "D8C5783083BF892339F5ED4191BBF3BE502CBF85F19FF29EFB00568B5F8462B7"
$ExpectedThumbprint = "5DFA7E68997ED52D4B10420407860D1C29B6F427"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ZipPath = Join-Path $ScriptDir "virtual-desktop-driver-installer-x64.zip"
$InstallerDir = Join-Path $ScriptDir "installer"
$MsiPath = Join-Path $InstallerDir "virtual-display-driver-$ExpectedVersion-x86_64.msi"
$CertPath = Join-Path $InstallerDir "DriverCertificate.cer"

# 与 C# 版（SnugWin）分开放：同一台机器上两版各自记录自己装了什么，
# 不会因为对方卸载而互相拆台。
$StateDir = Join-Path $env:ProgramData "Snug"
$OwnershipPath = Join-Path $StateDir "driver-ownership.json"

function Test-ExistingDriver {
    # 管道在不在：**两种问法都要试**。`GetFiles("\\.\pipe\")` 在有些上下文里列不全，
    # `[IO.File]::Exists` 依赖"有可用实例"；任一个说在就算在（宁可当它装好了，
    # 也别为了个误判去重装一遍驱动）。
    $pipePath = "\\.\pipe\virtualdisplaydriver"
    $pipeExists = [System.IO.File]::Exists($pipePath)
    if (-not $pipeExists) {
        try {
            $pipeExists = [System.IO.Directory]::GetFiles("\\.\pipe\") -contains $pipePath
        } catch {
            $pipeExists = $false
        }
    }
    $svc = Get-Service -Name "vdd-user-session-service" -ErrorAction SilentlyContinue
    $rootKey = "HKLM:\SYSTEM\CurrentControlSet\Enum\ROOT\DISPLAY"
    $hasDevice = $false
    if (Test-Path $rootKey) {
        foreach ($sub in Get-ChildItem $rootKey -ErrorAction SilentlyContinue) {
            $props = Get-ItemProperty $sub.PSPath -ErrorAction SilentlyContinue
            # 认 **HardwareID** 而不是显示名：UU 的 "GameViewer Virtual Display Adapter"
            # 同样挂在 ROOT\DISPLAY 下、名字里也有 "Virtual Display"，
            # 按名字匹配会把它当成我们的驱动（真机实测过），于是"已装好"被误判。
            # 我们的驱动 HardwareID 是 `Root\VirtualDisplayDriver`（没有空格）。
            $hwIds = @($props.HardwareID)
            if ($hwIds -match '(?i)virtualdisplaydriver') {
                $hasDevice = $true
                break
            }
        }
    }
    [pscustomobject]@{
        HasDevice     = $hasDevice
        ServiceStatus = if ($svc) { $svc.Status.ToString() } else { "NotInstalled" }
        PipeAvailable = $pipeExists
    }
}

function Assert-Admin {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "需要管理员权限运行驱动安装或卸载脚本。"
    }
}

# SHA-256 自己算，不用 Get-FileHash。
#
# 真机踩到过：从 PS7 的进程里拉起 `powershell.exe` 时，子进程继承 PS7 的 `PSModulePath`，
# 5.1 会先去 PS7 的模块目录找 `Microsoft.PowerShell.Utility`（PS7 专用、5.1 加载不了），
# 结果 `Get-FileHash` 直接"不是 cmdlet"，安装在校验那一步就挂了。
# .NET 的 SHA256 一定在，绕开模块自动加载这条路。
function Get-FileSha256([string]$Path) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $stream = [System.IO.File]::OpenRead($Path)
        try {
            $hash = $sha.ComputeHash($stream)
        } finally {
            $stream.Dispose()
        }
    } finally {
        $sha.Dispose()
    }
    return ([System.BitConverter]::ToString($hash) -replace '-', '').ToUpperInvariant()
}

# 解压同理：优先 .NET（不依赖 Expand-Archive），失败再退回 cmdlet。
function Expand-ZipPackage([string]$Zip, [string]$Destination) {
    try {
        Add-Type -AssemblyName System.IO.Compression.FileSystem -ErrorAction Stop
        [System.IO.Compression.ZipFile]::ExtractToDirectory($Zip, $Destination, $true)
    } catch {
        if (Get-Command Expand-Archive -ErrorAction SilentlyContinue) {
            Expand-Archive -Path $Zip -DestinationPath $Destination -Force
        } else {
            throw "解压驱动包失败：$($_.Exception.Message)"
        }
    }
}

function Assert-PackageIntegrity {
    if (-not (Test-Path $MsiPath) -or -not (Test-Path $CertPath)) {
        if (Test-Path $ZipPath) {
            $zipHash = Get-FileSha256 $ZipPath
            if ($zipHash -ne $ExpectedZipSha256) {
                throw "驱动压缩包 SHA256 校验失败: $zipHash"
            }
            Expand-ZipPackage $ZipPath $InstallerDir
        } else {
            throw "未找到 virtual-display-rs $ExpectedVersion 安装包，请从 https://github.com/MolotovCherry/virtual-display-rs/releases 下载。"
        }
    }

    $msiHash = Get-FileSha256 $MsiPath
    if ($msiHash -ne $ExpectedMsiSha256) {
        throw "MSI SHA256 校验失败 ($msiHash != $ExpectedMsiSha256)"
    }

    $certHash = Get-FileSha256 $CertPath
    if ($certHash -ne $ExpectedCertSha256) {
        throw "证书 SHA256 校验失败 ($certHash != $ExpectedCertSha256)"
    }

    $cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($CertPath)
    if ($cert.Thumbprint -ne $ExpectedThumbprint) {
        throw "驱动证书指纹不匹配: $($cert.Thumbprint)"
    }

    Write-Host "[OK] SHA256 & 证书指纹校验通过 ($ExpectedThumbprint)"
}

switch ($Action) {
    "Status" {
        $st = Test-ExistingDriver
        $owned = if (Test-Path $OwnershipPath) { Get-Content $OwnershipPath -Raw } else { "none" }
        Write-Host "DeviceInstalled=$($st.HasDevice) Service=$($st.ServiceStatus) PipeAvailable=$($st.PipeAvailable)"
        Write-Host "OwnershipMarker=$owned"
    }

    "Install" {
        Assert-Admin
        $before = Test-ExistingDriver
        if (-not (Test-Path $OwnershipPath)) {
            New-Item -ItemType Directory -Path $StateDir -Force | Out-Null
            $marker = @{
                driver              = "MolotovCherry/virtual-display-rs"
                version             = $ExpectedVersion
                managedBySnug       = (-not $before.HasDevice)
                preExistingUserDriver = $before.HasDevice
                thumbprint          = $ExpectedThumbprint
                recordedAt          = (Get-Date).ToString("o")
            } | ConvertTo-Json
            Set-Content -Path $OwnershipPath -Value $marker -Encoding UTF8
        }

        if ($before.HasDevice -and $before.PipeAvailable) {
            Write-Host "[OK] 检测到已就绪的 virtual-display-rs 驱动与命名管道，保留现有安装。"
            exit 0
        }

        Assert-PackageIntegrity

        Write-Host "[1/3] 安装驱动签名证书至 Root & TrustedPublisher..."
        $certutil = Join-Path -Path $systemToolsDir -ChildPath "certutil.exe"
        if (-not (Test-Path -LiteralPath $certutil -PathType Leaf)) { throw "找不到系统 certutil.exe: $certutil" }
        if (-not (Test-Path -LiteralPath $msiexec -PathType Leaf)) { throw "找不到系统 msiexec.exe: $msiexec" }

        & $certutil -addstore -f root $CertPath | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "导入 Root 证书失败 ($LASTEXITCODE)" }
        & $certutil -addstore -f TrustedPublisher $CertPath | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "导入 TrustedPublisher 证书失败 ($LASTEXITCODE)" }

        Write-Host "[2/3] 安装 virtual-display-driver-$ExpectedVersion-x86_64.msi..."
        $proc = Start-Process -FilePath $msiexec -ArgumentList "/i `"$MsiPath`" /passive /norestart" -Wait -PassThru
        if ($proc.ExitCode -ne 0) { throw "msiexec 失败，退出码 $($proc.ExitCode)" }

        Write-Host "[3/3] 校验驱动服务与命名管道..."
        $deadline = (Get-Date).AddSeconds(15)
        while ((Get-Date) -lt $deadline) {
            $after = Test-ExistingDriver
            if ($after.PipeAvailable) {
                Write-Host "[OK] virtual-display-rs $ExpectedVersion 安装并上线成功。"
                exit 0
            }
            Start-Sleep -Milliseconds 300
        }
        Write-Warning "驱动已安装，但命名管道暂未就绪（可能需要重启 Windows，或在设备管理器里重启 Cherry Virtual Display）。"
    }

    "Uninstall" {
        Assert-Admin
        if (-not (Test-Path $OwnershipPath)) {
            Write-Host "未找到 Snug 的驱动归属记录，为避免误删用户自有驱动，跳过卸载。"
            exit 0
        }
        $marker = Get-Content $OwnershipPath -Raw | ConvertFrom-Json
        if (-not $marker.managedBySnug) {
            Write-Host "驱动在安装 Snug 前已由用户自行安装，保留驱动不被卸载。"
            exit 0
        }
        if (-not (Test-Path -LiteralPath $msiexec -PathType Leaf)) { throw "找不到系统 msiexec.exe: $msiexec" }
        Assert-PackageIntegrity
        $proc = Start-Process -FilePath $msiexec -ArgumentList "/x `"$MsiPath`" /passive /norestart" -Wait -PassThru
        if ($proc.ExitCode -eq 0) {
            Remove-Item $OwnershipPath -Force -ErrorAction SilentlyContinue
            Write-Host "[OK] 已卸载 Snug 管理的 virtual-display-rs 驱动。"
        } else {
            throw "卸载 MSI 失败 ($($proc.ExitCode))"
        }
    }
}
