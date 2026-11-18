<#
SnugWin（Rust 版）真机冒烟脚本。

它做的事：用隔离数据目录启动 `snug.exe`，跑一段时间，把日志里有意义的东西挑出来判定
PASS/FAIL，然后收尾。**它自己不会改显示配置**；改不改由传进去的 -Phase 决定：
  -Phase A  配置里 phone_screen=false，只验证托盘/设置窗/单实例/日志，不建虚拟屏（默认）
  -Phase B  phone_screen=true，真的建虚拟屏、搬窗口（会动显示配置，需先退掉 C# 正式版）

用法（当前会话需要有管理员权限，因为 snug.exe 的清单是 requireAdministrator）：
  pwsh -File scripts\smoke-test.ps1 -Phase A
  pwsh -File scripts\smoke-test.ps1 -Phase B -Seconds 60

如果当前会话不是管理员，脚本会提示你先开一个管理员终端。
#>

[CmdletBinding()]
param(
    [ValidateSet('A', 'B')]
    [string]$Phase = 'A',
    [int]$Seconds = 25,
    [string]$DataDir = (Join-Path $env:USERPROFILE 'snug-rs-test'),
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\snug.exe')
)

$ErrorActionPreference = 'Stop'

function Write-Step($text) { Write-Host "== $text" -ForegroundColor Cyan }
function Fail($text) { Write-Host "FAIL: $text" -ForegroundColor Red; exit 1 }

# ---- 前置检查 ---------------------------------------------------------------
Write-Step "检查运行环境"

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Fail "当前会话不是管理员。snug.exe 的清单要求提权，请用管理员终端再跑这个脚本。"
}

$exePath = [System.IO.Path]::GetFullPath($Exe)
if (-not (Test-Path $exePath)) { Fail "找不到 $exePath（先 cargo build --release -p snug-app --offline）" }

$csharp = Get-Process -Name 'SnugWin.App' -ErrorAction SilentlyContinue
if ($Phase -eq 'B' -and $csharp) {
    Fail "C# 正式版还在跑（pid $($csharp.Id)）。阶段 B 会抢同一个虚拟屏驱动，请先退出它。"
}
if ($csharp) {
    Write-Host "  注意：C# 正式版在运行（pid $($csharp.Id)）。阶段 A 不建虚拟屏，可以并存。" -ForegroundColor Yellow
}

# ---- 准备隔离数据目录 -------------------------------------------------------
Write-Step "准备隔离数据目录 $DataDir"
New-Item -ItemType Directory -Force (Join-Path $DataDir 'logs') | Out-Null
$phoneScreen = if ($Phase -eq 'B') { 'true' } else { 'false' }
$configPath = Join-Path $DataDir 'config.json'
# 保留已有配置里的其它字段，只强制 phoneScreen，避免覆盖掉用户调过的值。
if (Test-Path $configPath) {
    try {
        $cfg = Get-Content $configPath -Raw | ConvertFrom-Json
        $cfg | Add-Member -NotePropertyName phoneScreen -NotePropertyValue ($phoneScreen -eq 'true') -Force
        $cfg | ConvertTo-Json -Depth 8 | Set-Content $configPath -Encoding utf8
    } catch {
        "{`"phoneScreen`":$phoneScreen}" | Set-Content $configPath -Encoding utf8
    }
} else {
    "{`"phoneScreen`":$phoneScreen}" | Set-Content $configPath -Encoding utf8
}
Write-Host "  phoneScreen=$phoneScreen"

# 记一份"跑之前"的显示器拓扑，阶段 B 用来对比是否真的多出虚拟屏。
$beforeMonitors = @(Get-CimInstance -Namespace root\wmi -ClassName WmiMonitorBasicDisplayParams -ErrorAction SilentlyContinue)
Write-Host "  跑之前显示器数：$($beforeMonitors.Count)"

# ---- 启动 -------------------------------------------------------------------
# 不要用 `Start-Process`：它走 ShellExecute，对一个刚编译出来、带 requireAdministrator
# 清单的 exe，会先去问 Shell/提权通道，表现为「打开文件 - 安全警告」或迟迟不起进程。
# 直接从本进程 CreateProcess 启动（UseShellExecute=$false）不经过 shell，行为可预期。
Write-Step "启动 snug.exe"
$env:SNUGWIN_HOME = $DataDir
$startedAt = Get-Date
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exePath
$psi.WorkingDirectory = (Split-Path $exePath)
$psi.UseShellExecute = $false
$proc = [System.Diagnostics.Process]::Start($psi)
if (-not $proc) { Fail "进程没有启动起来" }
Write-Host "  pid=$($proc.Id)"

Start-Sleep -Seconds $Seconds
$alive = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
if (-not $alive) { Write-Host "  进程已退出（可能自己崩了或单实例互斥），继续看日志" -ForegroundColor Yellow }

# ---- 读日志判定 -------------------------------------------------------------
Write-Step "判定"
$logDir = Join-Path $DataDir 'logs'
# 隔离目录里只有我们自己的日志，取最新的那个即可（不要用时间过滤：脚本和进程的
# 时钟/时区口径没必要在这里互相猜）。
$logs = @(Get-ChildItem -Path $logDir -Filter '*.log' -File -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending)
Write-Host "  日志目录：$logDir（$($logs.Count) 个文件）"
$log = $logs | Select-Object -First 1
if (-not $log) { Fail "没有生成日志文件（程序没跑起来？）" }

$text = Get-Content $log.FullName -Raw
Write-Host "  日志：$($log.FullName)（$([math]::Round($log.Length/1KB,1)) KB）"

$checks = [ordered]@{
    '启动行'         = 'SnugWin \(Rust\) 启动 v\d'
    '数据目录隔离'   = [regex]::Escape($DataDir)
    '驱动管道检测'   = '虚拟屏驱动管道：'
    '托盘'           = '托盘(已就绪|创建失败)'
    'UU 探测'        = 'UU 探测：'
    '未出现 panic'   = $null   # 反向检查
}
$failed = @()
foreach ($name in $checks.Keys) {
    if ($name -eq '未出现 panic') { continue }
    if ($text -match $checks[$name]) {
        Write-Host "  [ok] $name" -ForegroundColor Green
    } else {
        Write-Host "  [!!] $name" -ForegroundColor Yellow
        $failed += $name
    }
}
if ($text -match 'panicked|RUST_BACKTRACE') {
    Write-Host "  [!!] 日志里有 panic" -ForegroundColor Red
    $failed += 'panic'
} else {
    Write-Host "  [ok] 未出现 panic" -ForegroundColor Green
}

if ($Phase -eq 'B') {
    $afterMonitors = @(Get-CimInstance -Namespace root\wmi -ClassName WmiMonitorBasicDisplayParams -ErrorAction SilentlyContinue)
    Write-Host "  跑之后显示器数：$($afterMonitors.Count)（之前 $($beforeMonitors.Count)）"
    if ($afterMonitors.Count -gt $beforeMonitors.Count) {
        Write-Host "  [ok] 多出了显示设备（虚拟屏建起来了）" -ForegroundColor Green
    } else {
        Write-Host "  [!!] 没多出显示设备，手机屏没建起来" -ForegroundColor Yellow
        $failed += '虚拟屏'
    }
    if ($text -match '手机屏界面') { Write-Host "  [ok] 界面搬窗有记录" -ForegroundColor Green }
    else { Write-Host "  [!!] 没有界面搬窗记录" -ForegroundColor Yellow; $failed += '搬窗' }
}

# ---- 日志摘录给用户看 -------------------------------------------------------
Write-Step "关键日志（最后 30 行）"
Get-Content $log.FullName -Tail 30 | ForEach-Object { "  $_" }

# ---- 收尾 -------------------------------------------------------------------
Write-Step "收尾"
if (Get-Process -Id $proc.Id -ErrorAction SilentlyContinue) {
    # 真机上 `CloseMainWindow` 会让进程直接消失、不走任何 Rust 收尾（虚拟屏会留在桌面）。
    # 所以先放一个 `quit.request`，让它自己走 on_exit 那条路（撤屏 + 归位窗口 + 存配置）。
    $quitFlag = Join-Path $DataDir 'quit.request'
    Set-Content -Path $quitFlag -Value 'quit' -Encoding ascii
    Write-Host "  已放退出请求：$quitFlag"
    $graceful = $proc.WaitForExit(30000)
    if ($graceful) {
        Write-Host "  已正常退出（收尾日志见上）" -ForegroundColor Green
    } else {
        Write-Host "  30s 内没退出，改发关闭请求" -ForegroundColor Yellow
        $proc.CloseMainWindow() | Out-Null
        if (-not $proc.WaitForExit(10000)) {
            Stop-Process -Id $proc.Id -Force
            Write-Host "  只能强制结束（这次不会走收尾，虚拟屏可能残留，下次启动会自动清理）" -ForegroundColor Yellow
        }
    }
    Remove-Item $quitFlag -ErrorAction SilentlyContinue
} else {
    Write-Host "  进程早已退出"
}
Remove-Item Env:\SNUGWIN_HOME -ErrorAction SilentlyContinue

if ($failed.Count -gt 0) {
    Write-Host ""
    Write-Host "结果：FAIL（$($failed -join '、')）" -ForegroundColor Red
    exit 1
}
Write-Host ""
Write-Host "结果：PASS" -ForegroundColor Green
