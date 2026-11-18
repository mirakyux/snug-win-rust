#Requires -Version 7.0
<#
.SYNOPSIS
    照 docs/acceptance.md 核对日志：每组验收该出现的行有没有出现。

.DESCRIPTION
    真机验收时人眼要在几屏日志里找关键字；这个脚本把八组的关键文案列成规则，
    逐条报告「命中 / 未命中」和**首次命中的时间点**，还顺手揪出不该出现的行
    （IPC Notify 失败、建屏失败、清理残留……）。

    它只读日志，不启动程序、不改配置。规则里的文案就是源码里的原文，
    改文案时这里会先变红——那正是提醒你同步 acceptance.md。

.EXAMPLE
    pwsh -File scripts/check-acceptance-log.ps1
    pwsh -File scripts/check-acceptance-log.ps1 -Log C:\Users\me\snug-rs-test\logs\snug-20261119.log
    pwsh -File scripts/check-acceptance-log.ps1 -Since 21:30:00 -Group 1,6
#>
[CmdletBinding()]
param(
    # 日志文件；不给就按 SNUGWIN_HOME / %LOCALAPPDATA%\SnugWin 猜当天的。
    [string]$Log,
    # 数据目录（覆盖自动猜测）。
    [string]$DataDir,
    # 只从某个时刻开始看（`HH:mm:ss`）。
    [string]$Since,
    # 只核对某几组（1..8）。
    [int[]]$Group
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# 定位日志
# ---------------------------------------------------------------------------

if (-not $Log) {
    if (-not $DataDir) {
        if ($env:SNUGWIN_HOME) { $DataDir = $env:SNUGWIN_HOME }
        elseif ($env:LOCALAPPDATA) { $DataDir = Join-Path $env:LOCALAPPDATA 'SnugWin' }
        else { throw '找不到数据目录：设 -DataDir 或 SNUGWIN_HOME，或直接给 -Log' }
    }
    $today = Get-Date
    $name = 'snug-{0:yyyyMMdd}.log' -f $today
    $Log = Join-Path (Join-Path $DataDir 'logs') $name
}
if (-not (Test-Path -LiteralPath $Log)) {
    throw "日志不存在：$Log（程序跑过今天的日志吗？或换 -Log 指到那天的文件）"
}
$logPath = (Resolve-Path -LiteralPath $Log).Path
Write-Host "日志：$logPath" -ForegroundColor Cyan

$lines = Get-Content -LiteralPath $logPath
if ($Since) {
    if ($Since -notmatch '^\d{2}:\d{2}:\d{2}$') { throw "-Since 要写成 HH:mm:ss（收到 '$Since'）" }
    $lines = $lines | Where-Object {
        if ($_ -match '^\[(\d{2}:\d{2}:\d{2})\]') { $Matches[1] -ge $Since } else { $true }
    }
}
Write-Host ("行数：{0}" -f @($lines).Count) -ForegroundColor Cyan

# ---------------------------------------------------------------------------
# 规则（文案取自源码，不是猜的）
# ---------------------------------------------------------------------------

$rules = @(
    [pscustomobject]@{ n = 1; title = '该撤的屏会撤'
        must = @(
            @{ label = '手机连入'; text = '远程会话：已连入' }
            @{ label = '屏就绪'; text = '已就绪（' }
            @{ label = '断开'; text = '远程会话：已断开' }
            @{ label = '撤屏'; text = '本次会话结束，撤掉手机屏' }
        ) }
    [pscustomobject]@{ n = 2; title = '建屏期间界面不卡'
        must = @(
            @{ label = '建屏走过'; text = '手机屏 #' }
        )
        forbid = @(
            @{ label = '连续失败放弃建屏'; text = '加手机屏连续失败' }
            @{ label = 'IPC Notify 失败'; text = 'IPC Notify 失败' }
        ) }
    [pscustomobject]@{ n = 3; title = '主题'
        must = @(
            @{ label = '主题切换'; text = '主题：' }
        ) }
    [pscustomobject]@{ n = 4; title = '控制中心式设置'
        # 这一组是纯界面行为：打开设置、切页签、点按钮，源码里**没有对应日志行**
        # （C# 也没有），所以只能手点核对，别在这里编一条假规则。
        must = @()
        note = '纯界面，无日志可核；照 acceptance.md 第 4 组手点。' }
    [pscustomobject]@{ n = 5; title = '多屏与设备'
        must = @(
            @{ label = '屏配置变化'; text = '屏配置变化：' }
            @{ label = '切换屏幕'; text = '切换屏幕：' }
        ) }
    [pscustomobject]@{ n = 6; title = '重入胶囊'
        must = @(
            @{ label = '确认退出'; text = '界面：确认退出手机屏' }
            @{ label = '收起屏'; text = '收起手机屏：' }
            @{ label = '胶囊重入'; text = '重入胶囊：进入手机屏' }
            @{ label = '会话内恢复建屏'; text = '同会话重新进入手机屏：恢复建屏' }
        ) }
    [pscustomobject]@{ n = 7; title = '桌面预览'
        must = @(
            @{ label = '打开预览'; text = '预览：打开桌面预览' }
            @{ label = '只读拦截'; text = '预览：只读模式，忽略这次操作' }
            @{ label = '关闭预览'; text = '预览：关闭' }
        ) }
    [pscustomobject]@{ n = 8; title = '退出与残留'
        must = @(
            @{ label = '日志收尾'; text = '收尾：' }
        )
        forbid = @(
            @{ label = '残留清理（只在强杀后该出现）'; text = '已按归属记录清理残留手机屏' }
        ) }
)

if ($Group) { $rules = $rules | Where-Object { $Group -contains $_.n } }

# ---------------------------------------------------------------------------
# 核对
# ---------------------------------------------------------------------------

function Find-FirstHit {
    param([string[]]$Lines, [string]$Text)
    for ($i = 0; $i -lt $Lines.Count; $i++) {
        if ($Lines[$i].Contains($Text)) {
            $time = if ($Lines[$i] -match '^\[(\d{2}:\d{2}:\d{2})\]') { $Matches[1] } else { '?' }
            return [pscustomobject]@{ Line = $i + 1; Time = $time; Text = $Lines[$i].Trim() }
        }
    }
    return $null
}

$total = 0
$missed = 0
foreach ($rule in $rules) {
    Write-Host ''
    Write-Host ("== 第 {0} 组：{1} ==" -f $rule.n, $rule.title) -ForegroundColor Cyan
    if ($rule.PSObject.Properties.Name -contains 'note' -and $rule.note) {
        Write-Host ("  NOTE  {0}" -f $rule.note) -ForegroundColor DarkCyan
    }
    foreach ($item in $rule.must) {
        $total++
        $hit = Find-FirstHit -Lines $lines -Text $item.text
        if ($hit) {
            Write-Host ("  OK    {0}  [{1}] {2}" -f $item.label, $hit.Time, $item.text) -ForegroundColor Green
        } else {
            $missed++
            Write-Host ("  MISS  {0}  未出现：{1}" -f $item.label, $item.text) -ForegroundColor Yellow
        }
    }
    if ($rule.PSObject.Properties.Name -contains 'forbid' -and $rule.forbid) {
        foreach ($item in $rule.forbid) {
            $hit = Find-FirstHit -Lines $lines -Text $item.text
            if ($hit) {
                Write-Host ("  WARN  {0}  [{1}] {2}" -f $item.label, $hit.Time, $hit.Text) -ForegroundColor Red
            } else {
                Write-Host ("  OK    {0}  未出现（符合预期）" -f $item.label) -ForegroundColor Green
            }
        }
    }
}

# 告警行单独列出来：验收时最容易漏掉的就是它们。
$warnLines = @($lines | Where-Object { $_ -match '\[(Warn|Error)\]' })
Write-Host ''
Write-Host ("== 告警/错误：{0} 行 ==" -f $warnLines.Count) -ForegroundColor Cyan
if ($warnLines.Count -gt 0) {
    $warnLines | Select-Object -First 20 | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkYellow }
    if ($warnLines.Count -gt 20) { Write-Host ("  … 还有 {0} 行，看日志文件" -f ($warnLines.Count - 20)) }
}

Write-Host ''
if ($missed -eq 0) {
    Write-Host ("{0}/{0} 条关键行都出现了。" -f $total) -ForegroundColor Green
    exit 0
}
Write-Host ("{0}/{1} 条关键行出现，缺 {2} 条（没点那步，或功能没生效）。" -f ($total - $missed), $total, $missed) -ForegroundColor Yellow
exit 2
