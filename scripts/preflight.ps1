#Requires -Version 7.0
<#
.SYNOPSIS
    发版前的静态自检：把「不用启动程序就能判定」的发布条件一次跑完。

.DESCRIPTION
    这不是验收，是**发版门禁**。真机行为（建屏、撤屏、托盘、主题…）仍然要照
    docs/acceptance.md 手点一遍；这里只保证发出去的那个 exe 是发布形态：

      1. 版本：exe 资源里的 FileVersion/ProductVersion 与根 Cargo.toml 的
         [workspace.package] version 一致，且是三段数字（更新器按它跟 tag 比）；
      2. 体积：应用 8 MiB + 内嵌驱动 4 MiB，总预算 12 MiB；
      3. 清单：requestedExecutionLevel = asInvoker（GUI/更新普通权限，驱动任务单独 UAC）；
         顺带核对 PerMonitorV2 / longPathAware / activeCodePage=UTF-8；
      4. 依赖：dumpbin /dependents 里只有系统 DLL（零运行时依赖的实证，
         不应出现 VCRUNTIME140 / MSVCP140 之类的可再发行组件）；
      5. 单文件：完整官方驱动 zip 与带 BOM 的安装脚本逐字节内嵌在 exe；
      6. 发行包：dist/snug.exe、仅含 snug.exe 的更新 zip 与输入 exe 哈希一致，
         checksums.txt 同时记录裸 exe 与 zip；
      7. 更新源（给了 -ReleasesRepo 才查）：exe 里确实固化了那个仓库坐标。

    第 3、4 项需要 MSVC 的 dumpbin.exe 与 SDK 的 mt.exe；找不到就记 SKIP 并说明，
    不伪装成通过。

.EXAMPLE
    pwsh -File scripts/preflight.ps1
    pwsh -File scripts/preflight.ps1 -Exe target\release-check\release\snug.exe
#>
[CmdletBinding()]
param(
    # 要检查的 exe（默认 dist 用的那个 target\release\snug.exe）。
    [string]$Exe,
    # 允许的体积上限（字节）：应用 8 MiB + 内嵌驱动 4 MiB。
    [long]$MaxBytes = 12582912,
    # 发行包目录。
    [string]$DistDir,
    # 期望固化在 exe 里的更新源（`owner/name`）；给了才检查这一项。
    [string]$ReleasesRepo
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $Exe) { $Exe = Join-Path $repoRoot 'target\release\snug.exe' }
if (-not $DistDir) { $DistDir = Join-Path $repoRoot 'dist' }

$failures = [System.Collections.Generic.List[string]]::new()
$skips = [System.Collections.Generic.List[string]]::new()
$passes = 0

function Pass([string]$text) { Write-Host "  PASS  $text" -ForegroundColor Green; $script:passes++ }
function Fail([string]$text) { Write-Host "  FAIL  $text" -ForegroundColor Red; $failures.Add($text) }
function Skip([string]$text) { Write-Host "  SKIP  $text" -ForegroundColor Yellow; $skips.Add($text) }
function Head([string]$text) { Write-Host ''; Write-Host $text -ForegroundColor Cyan }

function Get-StreamSha256([System.IO.Stream]$stream) {
    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    try {
        return [System.BitConverter]::ToString($sha256.ComputeHash($stream)).Replace('-', '').ToLowerInvariant()
    } finally {
        $sha256.Dispose()
    }
}

# ---------------------------------------------------------------------------
# 工具定位：dumpbin 来自 VS，mt 来自 Windows SDK
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

# 在 vcvars 环境里跑一条命令，返回 Combined 输出（失败返回 $null）。
function Invoke-InVcVars {
    param(
        [Parameter(Mandatory)][string]$VcVars,
        [Parameter(Mandatory)][string[]]$Arguments
    )
    $quoted = $Arguments | ForEach-Object { '"' + $_ + '"' }
    $commandLine = '"' + $VcVars + '" >nul 2>nul && ' + ($quoted -join ' ')
    $output = & cmd.exe /c $commandLine 2>&1
    if ($LASTEXITCODE -ne 0) { return $null }
    return ($output | Out-String)
}

# ---------------------------------------------------------------------------
# 0. exe 存在
# ---------------------------------------------------------------------------

Head '== 产物 =='
if (-not (Test-Path -LiteralPath $Exe)) {
    Fail "找不到 $Exe（先构建：cargo build --release -p snug-app --offline）"
    Write-Host ''
    Write-Host '自检终止：产物不存在。' -ForegroundColor Red
    exit 1
}
$exePath = (Resolve-Path -LiteralPath $Exe).Path
$exeInfo = Get-Item -LiteralPath $exePath
$exeHash = (Get-FileHash -LiteralPath $exePath -Algorithm SHA256).Hash.ToLowerInvariant()
$exeContent = [System.Text.Encoding]::Latin1.GetString([System.IO.File]::ReadAllBytes($exePath))
Write-Host ("  exe   : {0}" -f $exePath)
Write-Host ("  大小  : {0:N0} B（{1:N2} MiB）" -f $exeInfo.Length, ($exeInfo.Length / 1MB))

# ---------------------------------------------------------------------------
# 1. 版本
# ---------------------------------------------------------------------------

Head '== 版本 =='
$cargoToml = Join-Path $repoRoot 'Cargo.toml'
$tomlText = Get-Content -LiteralPath $cargoToml -Raw
$workspaceVersion = $null
if ($tomlText -match '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') {
    $workspaceVersion = $Matches[1]
}
if (-not $workspaceVersion) {
    Fail "Cargo.toml 里没找到 [workspace.package] version"
} else {
    Write-Host ("  Cargo.toml : {0}" -f $workspaceVersion)
}

$versionInfo = $exeInfo.VersionInfo
$fileVersion = "$($versionInfo.FileVersion)".Trim()
$productVersion = "$($versionInfo.ProductVersion)".Trim()
Write-Host ("  exe 资源   : FileVersion={0}  ProductVersion={1}" -f $fileVersion, $productVersion)

if ([string]::IsNullOrWhiteSpace($fileVersion)) {
    Fail 'exe 没有版本资源（FileVersion 为空）——build.rs 没找到 rc.exe？'
} else {
    # 资源里可能是 1.3.0 或 1.3.0.0，发布名按三段。
    $normalized = $fileVersion
    if ($normalized -match '^(\d+\.\d+\.\d+)\.0$') { $normalized = $Matches[1] }
    if ($normalized -notmatch '^\d+\.\d+\.\d+$') {
        Fail "FileVersion '$fileVersion' 不是 major.minor.patch"
    } elseif ($workspaceVersion -and $normalized -ne $workspaceVersion) {
        Fail "exe 版本 $normalized 与 Cargo.toml 的 $workspaceVersion 不一致（产物过期，重新构建）"
    } else {
        Pass "版本 $normalized 与 Cargo.toml 一致"
    }
    if ($productVersion -and $productVersion -ne $fileVersion) {
        Fail "ProductVersion '$productVersion' 与 FileVersion '$fileVersion' 不一致"
    }
}

# ---------------------------------------------------------------------------
# 2. 体积
# ---------------------------------------------------------------------------

Head '== 体积 =='
if ($exeInfo.Length -gt $MaxBytes) {
    Fail ("{0:N0} B 超过预算 {1:N0} B（文档 docs/size-budget.md）" -f $exeInfo.Length, $MaxBytes)
} else {
    $percent = [math]::Round($exeInfo.Length * 100.0 / $MaxBytes, 1)
    Pass ("{0:N0} B，占预算 {1}%" -f $exeInfo.Length, $percent)
}

# ---------------------------------------------------------------------------
# 2b. 驱动完整内嵌（只读字节，不运行 exe、不释放或安装驱动）
# ---------------------------------------------------------------------------

Head '== 内嵌驱动 =='
$driverDir = Join-Path $repoRoot 'driver'
$driverZipName = 'virtual-desktop-driver-installer-x64.zip'
$fetchScript = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'fetch-driver.ps1') -Raw
$driverHashMatch = [regex]::Match($fetchScript, '(?m)^\$ExpectedSha256\s*=\s*''([0-9A-Fa-f]{64})''')
foreach ($name in @($driverZipName, 'Install-SnugDriver.ps1')) {
    $source = Join-Path $driverDir $name
    if (-not (Test-Path -LiteralPath $source)) {
        Fail "缺少内嵌内容的比对源 driver\$name（先运行 scripts/fetch-driver.ps1）"
        continue
    }
    $sourceBytes = [System.IO.File]::ReadAllBytes($source)
    if ($name -eq $driverZipName) {
        $driverHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
        if ($driverHashMatch.Success -and $driverHash -eq $driverHashMatch.Groups[1].Value) {
            Pass '驱动安装包 SHA-256 与 fetch-driver.ps1 固定的官方哈希一致'
        } else {
            Fail '驱动安装包与 fetch-driver.ps1 的官方 SHA-256 不符，或固定哈希无法读取'
        }
    } elseif ($sourceBytes.Length -ge 3 -and $sourceBytes[0] -eq 0xEF -and $sourceBytes[1] -eq 0xBB -and $sourceBytes[2] -eq 0xBF) {
        Pass '内嵌安装脚本的比对源是 UTF-8 with BOM'
    } else {
        Fail 'driver/Install-SnugDriver.ps1 缺 UTF-8 BOM，powershell.exe 5.1 会按 ANSI 解析'
    }
    # Latin1 一字节对应一个字符；Ordinal 查找完整内容，不能只匹配文件名或魔数。
    if ($sourceBytes.Length -gt 0 -and $exeContent.Contains([System.Text.Encoding]::Latin1.GetString($sourceBytes), [System.StringComparison]::Ordinal)) {
        Pass ("exe 完整内嵌 {0}（{1:N0} B，逐字节相同）" -f $name, $sourceBytes.Length)
    } else {
        Fail "exe 未完整内嵌 driver/$name（不是含驱动的单文件产物，或源文件已改变）"
    }
}

# ---------------------------------------------------------------------------
# 3/4. 清单与依赖（需要 vcvars）
# ---------------------------------------------------------------------------

$vcvars = Find-VcVars64
if (-not $vcvars) {
    Skip '找不到 vcvars64.bat：清单与依赖两项未检查（装 VS Build Tools 或设 VSINSTALLDIR）'
} else {
    Head '== 清单 =='
    $manifestPath = Join-Path ([System.IO.Path]::GetTempPath()) ("snug-manifest-{0}.xml" -f ([guid]::NewGuid().ToString('N')))
    try {
        # mt.exe 的参数里分号要紧贴在资源 id 前：-inputresource:exe;#1
        $manifest = Invoke-InVcVars -VcVars $vcvars -Arguments @(
            'mt.exe', "-inputresource:$exePath;#1", "-out:$manifestPath"
        )
        if (-not (Test-Path -LiteralPath $manifestPath)) {
            Fail "mt.exe 抽不出清单（输出：$manifest）"
        } else {
            $xml = Get-Content -LiteralPath $manifestPath -Raw
            if ($xml -match 'level\s*=\s*"asInvoker"') {
                Pass 'requestedExecutionLevel = asInvoker'
            } else {
                Fail '清单必须使用 asInvoker，GUI 不应默认请求管理员权限'
            }
            foreach ($needle in @('PerMonitorV2', 'longPathAware', 'activeCodePage')) {
                if ($xml -match [regex]::Escape($needle)) { Pass "清单含 $needle" }
                else { Fail "清单缺少 $needle" }
            }
        }
    } finally {
        if (Test-Path -LiteralPath $manifestPath) { Remove-Item -LiteralPath $manifestPath -Force }
    }

    Head '== 依赖 =='
    $dependents = Invoke-InVcVars -VcVars $vcvars -Arguments @('dumpbin.exe', '/nologo', '/dependents', $exePath)
    if (-not $dependents) {
        Skip 'dumpbin /dependents 失败：依赖未检查'
    } else {
        $dlls = [regex]::Matches($dependents, '(?im)^\s+([A-Za-z0-9_.-]+\.dll)\s*$') |
            ForEach-Object { $_.Groups[1].Value.ToLowerInvariant() } |
            Sort-Object -Unique
        if (-not $dlls -or $dlls.Count -eq 0) {
            Fail 'dumpbin 没解析出任何依赖 DLL（输出异常）'
        } else {
            Write-Host ("  依赖 : {0}" -f ($dlls -join ', '))
            # 可再发行组件意味着目标机器要先装运行库，违背"单 exe 零依赖"。
            $redist = $dlls | Where-Object { $_ -match '^(vcruntime|msvcp|msvcr|concrt|ucrtbased)' }
            if ($redist) {
                Fail ("出现了可再发行运行库依赖：{0}" -f ($redist -join ', '))
            } else {
                Pass '只依赖系统 DLL'
            }
            # 系统 DLL 必须来自 System32：dumpbin 只给名字，无路径，这一条只能靠白名单人工判断。
            $known = @(
                'advapi32.dll', 'api-ms-win-core-synch-l1-2-0.dll', 'bcrypt.dll',
                'bcryptprimitives.dll', 'combase.dll', 'comctl32.dll', 'comdlg32.dll',
                'crypt32.dll', 'd3d11.dll', 'dwmapi.dll', 'dwrite.dll', 'dxgi.dll',
                'gdi32.dll', 'gdi32full.dll', 'imm32.dll', 'iphlpapi.dll', 'kernel32.dll',
                'msimg32.dll', 'ntdll.dll', 'ole32.dll', 'oleaut32.dll', 'opengl32.dll',
                'psapi.dll', 'rpcrt4.dll', 'secur32.dll', 'setupapi.dll', 'shcore.dll',
                'shell32.dll', 'shlwapi.dll', 'user32.dll', 'userenv.dll', 'uxtheme.dll',
                'version.dll', 'winmm.dll', 'winspool.drv', 'wintrust.dll', 'ws2_32.dll',
                'wtsapi32.dll', 'dcomp.dll', 'dinput8.dll', 'hid.dll', 'propsys.dll',
                'oleacc.dll', 'uiautomationcore.dll', 'cfgmgr32.dll', 'powrprof.dll',
                'shdwmapi.dll'
            )
            # `api-ms-win-*` 是 Windows 的 API set 转发层，永远是系统的。
            $unknown = $dlls | Where-Object { $known -notcontains $_ -and $_ -notlike 'api-ms-win-*' }
            if ($unknown) { Skip ("未在白名单里的依赖，人工确认是否为系统 DLL：{0}" -f ($unknown -join ', ')) }
            else { Pass '依赖名都能对上系统 DLL 白名单（api-ms-win-* 是 API set 转发层）' }
        }
    }
}

# ---------------------------------------------------------------------------
# 5. 发行包
# ---------------------------------------------------------------------------

Head '== 发行包 =='
$assetName = "Snug-v$($workspaceVersion)-win-x64-selfcontained.zip"
$zipPath = Join-Path $DistDir $assetName
$checksumsPath = Join-Path $DistDir 'checksums.txt'
$distExePath = Join-Path $DistDir 'snug.exe'
if (-not (Test-Path -LiteralPath $distExePath)) {
    Fail 'dist 里没有可直接交付的 snug.exe'
} elseif ((Get-FileHash -LiteralPath $distExePath -Algorithm SHA256).Hash.ToLowerInvariant() -eq $exeHash) {
    Pass 'dist/snug.exe 与输入 exe 的 SHA-256 一致'
} else {
    Fail 'dist/snug.exe 与输入 exe 的 SHA-256 不一致（打包产物过期？）'
}

if (-not (Test-Path -LiteralPath $zipPath)) {
    Fail "dist 里没有 $assetName（先运行 scripts/build-release.ps1）"
} else {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        $entries = @($archive.Entries | ForEach-Object { $_.FullName })
        $packedExeHash = $null
        $exeEntry = $archive.GetEntry('snug.exe')
        if ($null -ne $exeEntry) {
            $stream = $exeEntry.Open()
            try {
                $packedExeHash = Get-StreamSha256 $stream
            } finally {
                $stream.Dispose()
            }
        }
    } finally {
        $archive.Dispose()
    }
    if ($entries.Count -ne 1 -or $entries[0] -cne 'snug.exe') {
        Fail ("更新 zip 必须只有根目录 snug.exe，实际为 [{0}]" -f ($entries -join ', '))
    } else {
        Pass '更新 zip 只有根目录 snug.exe（驱动已内嵌）'
    }
    if ($packedExeHash -eq $exeHash) {
        Pass '更新 zip 内 snug.exe 与输入 exe 的 SHA-256 一致'
    } else {
        Fail '更新 zip 内 snug.exe 与输入 exe 的 SHA-256 不一致或条目缺失'
    }

    if (-not (Test-Path -LiteralPath $checksumsPath)) {
        Fail 'dist 里没有 checksums.txt'
    } else {
        $zipHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
        $text = Get-Content -LiteralPath $checksumsPath -Raw
        # 格式：<64 位小写 hex><两个空格><文件名>，与 sha256sum 文本模式一致。
        if ($text -match ('(?m)^' + [regex]::Escape($zipHash) + '  ' + [regex]::Escape($assetName) + '\r?$')) {
            Pass 'checksums.txt 里的 SHA-256 与 zip 一致'
        } else {
            Fail "checksums.txt 与 $assetName 的 SHA-256 对不上（zip 重打过但没更新校验和？）"
        }
        if ($text -match ('(?m)^' + [regex]::Escape($exeHash) + '  snug\.exe\r?$')) {
            Pass 'checksums.txt 里的 SHA-256 与裸 snug.exe 一致'
        } else {
            Fail 'checksums.txt 里裸 snug.exe 的 SHA-256 缺失或不一致'
        }
    }
    if ($zipPath) { Write-Host ("  zip  : {0:N0} B" -f (Get-Item -LiteralPath $zipPath).Length) }
}

# ---------------------------------------------------------------------------
# 6. 更新源
# ---------------------------------------------------------------------------

Head '== 更新源 =='
if (-not $ReleasesRepo) {
    Skip '未给 -ReleasesRepo：没核对 exe 里的更新源（没配时"检查更新"按钮不可用，见 docs/release.md）'
} else {
    # 仓库坐标是编译期 option_env!("SNUG_RELEASES_REPO") 固化的普通字符串，
    # 在二进制里就是一段 ASCII，直接按字节找即可（不用解析 PE 数据段）。
    if ($exeContent.Contains($ReleasesRepo, [System.StringComparison]::Ordinal)) {
        Pass "exe 里固化了更新源 $ReleasesRepo"
    } else {
        Fail "exe 里找不到更新源 '$ReleasesRepo'——构建时没设 SNUG_RELEASES_REPO？"
    }
}

# ---------------------------------------------------------------------------
# 汇总
# ---------------------------------------------------------------------------

Head '== 汇总 =='
if ($failures.Count -eq 0) {
    Write-Host ("  {0} 项通过" -f $passes) -ForegroundColor Green
    if ($skips.Count -gt 0) { Write-Host ("  另有 {0} 项跳过（见上）" -f $skips.Count) -ForegroundColor Yellow }
    Write-Host ''
    Write-Host '静态门禁通过。真机行为仍要照 docs/acceptance.md 手工验收。' -ForegroundColor Green
    exit 0
}
Write-Host ("  失败 {0} 项：" -f $failures.Count) -ForegroundColor Red
foreach ($failure in $failures) { Write-Host "    - $failure" -ForegroundColor Red }
exit 1
