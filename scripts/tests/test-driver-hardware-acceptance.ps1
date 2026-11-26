param([string]$OutputDirectory = ('target/hardware-script-regressions-' + [Guid]::NewGuid().ToString('N')))

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$output = [IO.Path]::GetFullPath((Join-Path $repo $OutputDirectory))
[IO.Directory]::CreateDirectory($output) | Out-Null
# Load only helper definitions: never execute the entry point or connect to a real pipe.
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'run-driver-hardware-acceptance.ps1'), [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -ne 0) { throw ($parseErrors | Out-String) }
foreach ($definition in $ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] }, $false)) {
    . ([scriptblock]::Create($definition.Extent.Text))
}
$script:checks = 0
function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
    $script:checks++
}
function Assert-Refused([scriptblock]$Action, [string]$Message) {
    $refused = $false
    try { & $Action } catch { $refused = $true }
    Assert-True $refused $Message
}
$nativeType = @($ast.FindAll({ param($node)
    $node -is [Management.Automation.Language.StringConstantExpressionAst] -and
    $node.Value.Contains('public static class SnugHardwarePipeNative')
}, $true))
Assert-True ($nativeType.Count -eq 1) 'Pipe identity P/Invoke declaration must be uniquely present'
Add-Type -TypeDefinition $nativeType[0].Value
Assert-True ($null -ne ('SnugHardwarePipeNative' -as [type])) 'Pipe identity P/Invoke declaration must compile without connecting'
function New-Inventory {
    [ordered]@{
        devices = @([ordered]@{ status = 'OK'; version = '0.4.0.9' })
        loadedModuleFiles = @([ordered]@{ processId = 41; processName = 'WUDFHost'; processStartTimeUtc = '2026-11-26T01:00:00.0000000Z'; fileVersion = '0.4.0.9'; fileSha256 = ('A' * 64) })
        moduleEnumerationErrors = @()
        pipeServer = [ordered]@{ processId = 41; processName = 'WUDFHost'; processStartTimeUtc = '2026-11-26T01:00:00.0000000Z'; error = $null }
    }
}
$good = New-Inventory
Assert-CandidateDriver $good '0.4.0.9' ('A' * 64)
Assert-True $true 'Matching pipe server and candidate must be accepted'
foreach ($defect in @('enumeration', 'server-pid', 'server-name', 'server-start', 'pipe-error', 'module-version', 'module-hash', 'empty-modules', 'extra-module', 'unhealthy-device')) {
    $inventory = New-Inventory
    switch ($defect) {
        'enumeration' { $inventory.moduleEnumerationErrors = @('access denied') }
        'server-pid' { $inventory.pipeServer.processId = 42 }
        'server-name' { $inventory.pipeServer.processName = 'unrelated' }
        'server-start' { $inventory.pipeServer.processStartTimeUtc = '2026-11-26T02:00:00.0000000Z' }
        'pipe-error' { $inventory.pipeServer.error = 'could not connect' }
        'module-version' { $inventory.loadedModuleFiles[0].fileVersion = '0.4.0.8' }
        'module-hash' { $inventory.loadedModuleFiles[0].fileSha256 = ('B' * 64) }
        'empty-modules' { $inventory.loadedModuleFiles = @() }
        'extra-module' { $inventory.loadedModuleFiles += [ordered]@{ processId = 42; fileVersion = '0.4.0.9'; fileSha256 = ('B' * 64) } }
        'unhealthy-device' { $inventory.devices[0].status = 'ERROR' }
    }
    Assert-Refused { Assert-CandidateDriver $inventory '0.4.0.9' ('A' * 64) } "Candidate gate accepted $defect"
}

function New-FakeProcess([bool]$ExitAfterKill) {
    $fake = [pscustomobject]@{ Id = 123; StartTime = [DateTime]::UtcNow; ExitCode = 0; KillCount = 0; DisposeCount = 0; ExitAfterKill = $ExitAfterKill; Waits = [Collections.Generic.List[int]]::new() }
    $fake | Add-Member ScriptMethod WaitForExit { param([int]$Milliseconds) $this.Waits.Add($Milliseconds); return ($this.KillCount -gt 0 -and $this.ExitAfterKill) }
    $fake | Add-Member ScriptMethod Kill { $this.KillCount++ }
    $fake | Add-Member ScriptMethod Dispose { $this.DisposeCount++ }
    $fake
}
$fake = New-FakeProcess $false
Assert-True (-not (Stop-HarnessProcess $fake 125)) 'Unconfirmed process termination must remain unconfirmed'
Assert-True (($fake.Waits -join ',') -eq '0,125') 'Termination must use only bounded waits'
$fake = New-FakeProcess $true
Assert-True (Stop-HarnessProcess $fake 125) 'Confirmed process termination must be reported'
$fake = New-FakeProcess $true
$fake | Add-Member ScriptMethod Kill { $this.KillCount++; throw 'already exited' } -Force
Assert-True (Stop-HarnessProcess $fake 125) 'Kill race must still verify process exit with a bounded wait'
$fake = New-FakeProcess $false
$fake | Add-Member ScriptMethod WaitForExit { param([int]$Milliseconds) throw 'query failed' } -Force
Assert-True (-not (Stop-HarnessProcess $fake 125)) 'Process query errors must not claim confirmed termination'

$binary = 'never-executed.exe'
function Start-Process { param($FilePath, $ArgumentList, [switch]$PassThru, $WindowStyle, $RedirectStandardOutput, $RedirectStandardError) $script:NextHarnessProcess }
foreach ($confirmed in @($false, $true)) {
    $run = Join-Path $output ('timeout-' + $confirmed)
    [IO.Directory]::CreateDirectory($run) | Out-Null
    [IO.File]::WriteAllText((Join-Path $run 'owner-journal.json'), '{"retained":true}')
    Set-HarnessProcessState $run ([ordered]@{ schema = 'snug-hardware-process-v1'; exitConfirmed = $true })
    $script:NextHarnessProcess = New-FakeProcess $confirmed
    $caught = $null
    try { Invoke-Harness 'Smoke' $run @('owner') 1 } catch { $caught = $_ }
    Assert-True ($null -ne $caught) 'Timeout must fail'
    Assert-True ($caught.Exception.Data['HarnessExitConfirmed'] -eq $confirmed) 'Timeout must expose whether termination was confirmed'
    Assert-True (($script:NextHarnessProcess.Waits -join ',') -eq '1000,0,5000') 'Timeout path must never perform an unbounded or duplicate stop wait'
    Assert-True ($script:NextHarnessProcess.DisposeCount -eq 1) 'Process handle must be disposed'
    $marker = Get-Content -LiteralPath (Join-Path $run 'harness-process.json') -Raw | ConvertFrom-Json
    Assert-True ($marker.exitConfirmed -eq $confirmed) 'Process marker must preserve the termination result'
    Assert-True ([IO.File]::ReadAllText((Join-Path $run 'owner-journal.json')).Contains('retained')) 'Timeout must retain owner journal'
}
$run = Join-Path $output 'locked-run'
[IO.Directory]::CreateDirectory($run) | Out-Null
$heldLock = [IO.File]::Open((Join-Path $run 'harness.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
try { Assert-Refused { Invoke-Harness 'Cleanup' $run @() 1 } 'Concurrent wrapper must be refused while the run lock is held' }
finally { $heldLock.Dispose() }
Assert-True (-not (Test-Path -LiteralPath (Join-Path $run 'harness-process.json'))) 'Concurrent refusal must not overwrite process evidence'
[IO.File]::WriteAllText((Join-Path $run 'owner-journal.json'), '{"retained":true}')
Assert-Refused { Assert-HarnessIdle $run } 'Legacy owner journal without process evidence must not start potentially concurrent cleanup'
function Start-Process { param($FilePath, $ArgumentList, [switch]$PassThru, $WindowStyle, $RedirectStandardOutput, $RedirectStandardError) throw 'launch outcome unknown' }
$run = Join-Path $output 'unknown-launch'
[IO.Directory]::CreateDirectory($run) | Out-Null
$caught = $null
try { Invoke-Harness 'Smoke' $run @('owner') 1 } catch { $caught = $_ }
Assert-True ($null -ne $caught -and $caught.Exception.Data['HarnessExitConfirmed'] -eq $false) 'Unknown launch failure must not allow concurrent cleanup'
Assert-Refused { Assert-HarnessIdle $run } 'Unknown launch evidence must refuse a subsequent cleanup'
Remove-Item Function:\Start-Process

$identity = Get-HarnessProcessIdentity $PID
Assert-True ($identity.processId -eq $PID) 'Real process identity query must preserve the PID'
Assert-True ($null -eq (Get-HarnessProcessIdentity ([int]::MaxValue))) 'Missing process must be recognized as exited'
$script:OriginalProcessIdentity = (Get-Item Function:\Get-HarnessProcessIdentity).ScriptBlock
function Get-HarnessProcessIdentity { param([int]$ProcessId) $script:MockProcessIdentity }
$run = Join-Path $output 'idle-gate'
[IO.Directory]::CreateDirectory($run) | Out-Null
$marker = [ordered]@{ schema = 'snug-hardware-process-v1'; processId = 123; processStartTimeUtc = '2026-11-26T01:00:00.0000000Z'; exitConfirmed = $false }
Set-HarnessProcessState $run $marker
$script:MockProcessIdentity = [ordered]@{ processId = 123; processStartTimeUtc = $marker.processStartTimeUtc }
Assert-Refused { Assert-HarnessIdle $run } 'Cleanup must refuse a still-running original process'
$script:MockProcessIdentity = [ordered]@{ processId = 123; processStartTimeUtc = '2026-11-26T02:00:00.0000000Z' }
Assert-HarnessIdle $run
Assert-True $true 'PID reuse must not block cleanup after original process exited'
$script:MockProcessIdentity = $null
Assert-HarnessIdle $run
Assert-True $true 'Absent original process must allow scoped cleanup'
$marker.processId = $null
Set-HarnessProcessState $run $marker
Assert-Refused { Assert-HarnessIdle $run } 'Unknown launch state must retain journal and refuse cleanup'
[IO.File]::WriteAllText((Join-Path $run 'harness-process.json'), 'incomplete')
Assert-Refused { Assert-HarnessIdle $run } 'Unreadable process evidence must fail closed'
Set-Item Function:\Get-HarnessProcessIdentity $script:OriginalProcessIdentity

$shell = Join-Path $env:windir 'System32/WindowsPowerShell/v1.0/powershell.exe'
$native = Join-Path $output 'native.ps1'
$log = Join-Path $output 'native.log'
[IO.File]::WriteAllText($native, '[Console]::Error.WriteLine(''native progress''); exit 0')
$code = Invoke-HardwareNative $shell @('-NoProfile', '-File', $native) $log
Assert-True ($code -eq 0 -and [IO.File]::ReadAllText($log).Contains('native progress')) 'Native stderr progress must not fail Prepare'
[IO.File]::WriteAllText($native, '[Console]::Error.WriteLine(''native failure''); exit 23')
$code = Invoke-HardwareNative $shell @('-NoProfile', '-File', $native) $log
Assert-True ($code -eq 23 -and [IO.File]::ReadAllText($log).Contains('native failure')) 'Native failure exit code must be preserved'
Assert-True ($ErrorActionPreference -eq 'Stop') 'Native helper must restore error preference'

# Use a real, harmless native process to check Unicode and CRT quoting end to end.
$unicodeDirectory = Join-Path $output ([string][char]0x9A8C + [char]0x6536 + ' native path')
[IO.Directory]::CreateDirectory($unicodeDirectory) | Out-Null
$source = Join-Path $unicodeDirectory 'argument-probe.cs'
[IO.File]::WriteAllText($source, 'using System; using System.Text; class ArgumentProbe { static void Main(string[] args) { if (args[0] == "sleep") { System.Threading.Thread.Sleep(60000); return; } foreach (string arg in args) Console.WriteLine(Convert.ToBase64String(Encoding.UTF8.GetBytes(arg))); } }')
$binary = Join-Path $unicodeDirectory (([string][char]0x53C2) + [char]0x6570 + ' probe.exe')
$compiler = Join-Path $env:windir 'Microsoft.NET/Framework64/v4.0.30319/csc.exe'
$code = Invoke-HardwareNative $compiler @('/nologo', ('/out:' + $binary), $source) (Join-Path $output 'compile-argument-probe.log')
Assert-True ($code -eq 0) 'Native argument probe compilation must pass'
$run = (Join-Path $unicodeDirectory 'run directory') + '\'
[IO.Directory]::CreateDirectory($run) | Out-Null
$token = ([string][char]0x6807) + [char]0x8BB0
Invoke-Harness 'probe' $run @($token) 10
$stdout = @(Get-ChildItem -LiteralPath $run -Filter 'probe-*.stdout.log')
Assert-True ($stdout.Count -eq 1) 'Argument probe must produce exactly one stdout log'
$arguments = [IO.File]::ReadAllLines($stdout[0].FullName) | ForEach-Object { [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($_)) }
Assert-True ($arguments.Count -eq 3 -and $arguments[0] -ceq 'probe' -and $arguments[1] -ceq $run -and $arguments[2] -ceq $token) 'Unicode executable/run paths, spaces and a trailing backslash must preserve all native arguments'
$run = Join-Path $unicodeDirectory 'real timeout'
[IO.Directory]::CreateDirectory($run) | Out-Null
$watch = [Diagnostics.Stopwatch]::StartNew()
$caught = $null
try { Invoke-Harness 'sleep' $run @() 1 } catch { $caught = $_ }
$watch.Stop()
Assert-True ($null -ne $caught -and $caught.Exception.Data['HarnessExitConfirmed'] -eq $true) 'Real timed-out native process must be terminated and confirmed'
Assert-True ($watch.Elapsed.TotalSeconds -lt 12) 'Real timeout and termination must complete within a bounded interval'
Write-Host "PASS: $script:checks hardware wrapper regression checks; no driver connection or topology write"
