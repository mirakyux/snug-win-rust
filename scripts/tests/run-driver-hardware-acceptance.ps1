param(
    [ValidateSet('Prepare', 'Preflight', 'Smoke', 'Cleanup')]
    [string]$Mode = 'Prepare',
    [string]$OutputDirectory = 'target/driver-hardware-acceptance',
    [string]$RunDirectory,
    [string]$ExpectedDriverVersion = '0.4.0.12',
    [string]$ExpectedDriverSha256,
    [switch]$Execute
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$output = [IO.Path]::GetFullPath((Join-Path $repo $OutputDirectory))
$crate = Join-Path $output 'harness'
$binary = Join-Path $output 'cargo/debug/snug-driver-hardware-acceptance.exe'
$preparedPath = Join-Path $output 'prepared.json'
$template = Join-Path $PSScriptRoot 'driver-hardware-acceptance.template.rs'

function Get-SourceEvidence {
    $paths = @($template, $PSCommandPath, (Join-Path $repo 'Cargo.toml'),
        (Join-Path $repo 'Cargo.lock'), (Join-Path $repo '.cargo/config.toml'))
    foreach ($name in @('snug-core', 'snug-win')) {
        $paths += Join-Path $repo "crates/$name/Cargo.toml"
        $paths += Get-ChildItem -LiteralPath (Join-Path $repo "crates/$name/src") -Filter '*.rs' -Recurse -File |
            ForEach-Object { $_.FullName }
    }
    @($paths | Sort-Object -Unique | ForEach-Object {
        [ordered]@{ path = [IO.Path]::GetFullPath($_); sha256 = (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash }
    })
}

function Assert-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Use an already elevated terminal. This script never elevates, installs drivers, or starts snug.exe.'
    }
}

function Get-DriverPipeServer {
    $pipe = $null
    try {
        if (-not ('SnugHardwarePipeNative' -as [type])) {
            Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class SnugHardwarePipeNative {
    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool GetNamedPipeServerProcessId(SafePipeHandle pipe, out uint processId);
}
'@
        }
        # Open the default pipe for reading only. No request bytes are sent.
        $pipe = [IO.Pipes.NamedPipeClientStream]::new('.', 'virtualdisplaydriver',
            [IO.Pipes.PipeDirection]::In, [IO.Pipes.PipeOptions]::Asynchronous,
            [Security.Principal.TokenImpersonationLevel]::Anonymous)
        $pipe.Connect(2500)
        [uint32]$serverId = 0
        if (-not [SnugHardwarePipeNative]::GetNamedPipeServerProcessId($pipe.SafePipeHandle, [ref]$serverId)) {
            throw [ComponentModel.Win32Exception]::new([Runtime.InteropServices.Marshal]::GetLastWin32Error())
        }
        $process = Get-Process -Id $serverId -ErrorAction Stop
        try {
            [ordered]@{ pipeName = 'virtualdisplaydriver'; access = 'read-only; no request bytes sent'
                processId = $process.Id; processName = $process.ProcessName
                processStartTimeUtc = $process.StartTime.ToUniversalTime().ToString('o'); error = $null }
        } finally { $process.Dispose() }
    } catch {
        [ordered]@{ pipeName = 'virtualdisplaydriver'; processId = $null; processName = $null
            processStartTimeUtc = $null; error = $_.Exception.Message }
    } finally { if ($null -ne $pipe) { $pipe.Dispose() } }
}

function Get-DriverInventory {
    $devices = @()
    foreach ($device in @(Get-PnpDevice -Class Display -PresentOnly -ErrorAction Stop)) {
        $ids = @((Get-PnpDeviceProperty -InstanceId $device.InstanceId -KeyName 'DEVPKEY_Device_HardwareIds' -ErrorAction Stop).Data)
        if ($ids -notcontains 'Root\VirtualDisplayDriver') { continue }
        $inf = (Get-PnpDeviceProperty -InstanceId $device.InstanceId -KeyName 'DEVPKEY_Device_DriverInfPath' -ErrorAction Stop).Data
        $version = (Get-PnpDeviceProperty -InstanceId $device.InstanceId -KeyName 'DEVPKEY_Device_DriverVersion' -ErrorAction Stop).Data
        $devices += [ordered]@{
            instanceId = $device.InstanceId; status = $device.Status; friendlyName = $device.FriendlyName
            hardwareIds = $ids; inf = $inf; version = $version
        }
    }
    $modules = @()
    $moduleErrors = @()
    foreach ($process in @(Get-Process -Name WUDFHost -ErrorAction SilentlyContinue)) {
        try {
            foreach ($module in @($process.Modules | Where-Object { $_.ModuleName -ieq 'VirtualDisplayDriver.dll' })) {
                $signature = Get-AuthenticodeSignature -LiteralPath $module.FileName
                $modules += [ordered]@{
                    processId = $process.Id; processName = $process.ProcessName
                    processStartTimeUtc = $process.StartTime.ToUniversalTime().ToString('o'); path = $module.FileName
                    fileSha256 = (Get-FileHash -LiteralPath $module.FileName -Algorithm SHA256).Hash
                    fileVersion = $module.FileVersionInfo.FileVersion
                    signatureStatus = $signature.Status.ToString()
                }
            }
        } catch { $moduleErrors += "PID $($process.Id): $($_.Exception.Message)" }
        finally { $process.Dispose() }
    }
    [ordered]@{ capturedAt = [DateTimeOffset]::Now.ToString('o'); devices = $devices
        loadedModuleFiles = $modules; moduleEnumerationErrors = $moduleErrors; pipeServer = Get-DriverPipeServer
        hashMeaning = 'SHA256 of the file backing an observed loaded module; not a hash of process memory' }
}

function Assert-CandidateDriver($Inventory, [string]$Version, [string]$Sha256) {
    if (@($Inventory.devices).Count -ne 1 -or $Inventory.devices[0].status -ne 'OK' -or
        $Inventory.devices[0].version -ne $Version) {
        throw "Expected one healthy Root\VirtualDisplayDriver at version $Version"
    }
    if (@($Inventory.moduleEnumerationErrors).Count -ne 0) {
        throw 'WUDFHost module enumeration was incomplete; refusing candidate acceptance'
    }
    $server = $Inventory.pipeServer
    if ($null -eq $server -or $server.error -or $server.processName -ine 'WUDFHost' -or
        -not $server.processId -or -not $server.processStartTimeUtc) {
        throw 'Cannot identify the default driver pipe server as a WUDFHost process'
    }
    $modules = @($Inventory.loadedModuleFiles)
    if ($modules.Count -eq 0 -or @($modules | Where-Object {
        $_.fileSha256 -ine $Sha256 -or $_.fileVersion -ne $Version
    }).Count -gt 0) {
        throw 'Observed driver module files do not all match the expected candidate version and SHA256'
    }
    $serverModules = @($modules | Where-Object {
        $_.processId -eq $server.processId -and $_.processName -ieq 'WUDFHost' -and
        $_.processStartTimeUtc -ceq $server.processStartTimeUtc
    })
    if ($serverModules.Count -ne 1) {
        throw 'The actual default pipe server is not bound to exactly one observed candidate module'
    }
}

function Invoke-HardwareNative([string]$FilePath, [string[]]$Arguments, [string]$LogPath) {
    $command = (Get-Command -Name $FilePath -CommandType Application -ErrorAction Stop).Source
    $savedPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $PSNativeCommandUseErrorActionPreference = $false
    try {
        & $command @Arguments 2>&1 | ForEach-Object { $_.ToString() } |
            Tee-Object -FilePath $LogPath | ForEach-Object { Write-Host $_ }
        return $LASTEXITCODE
    } finally { $ErrorActionPreference = $savedPreference }
}

function Set-HarnessProcessState([string]$Directory, $State) {
    $bytes = [Text.UTF8Encoding]::new($false).GetBytes(($State | ConvertTo-Json -Depth 6))
    $file = [IO.File]::Open((Join-Path $Directory 'harness-process.json'), [IO.FileMode]::Create,
        [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try { $file.Write($bytes, 0, $bytes.Length); $file.Flush($true) }
    finally { $file.Dispose() }
}

function Get-HarnessProcessIdentity([int]$ProcessId) {
    try { $process = [Diagnostics.Process]::GetProcessById($ProcessId) }
    catch [ArgumentException] { return $null }
    try {
        [ordered]@{ processId = $process.Id; processStartTimeUtc = $process.StartTime.ToUniversalTime().ToString('o') }
    } finally { $process.Dispose() }
}

function Assert-HarnessIdle([string]$Directory) {
    $path = Join-Path $Directory 'harness-process.json'
    if (-not (Test-Path -LiteralPath $path)) {
        if (Test-Path -LiteralPath (Join-Path $Directory 'owner-journal.json')) {
            throw 'Owner journal has no harness process evidence; refuse cleanup until the original process exit can be established'
        }
        return
    }
    try { $state = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json }
    catch { throw 'Cannot read prior harness process evidence; retain the owner journal and resolve the unknown process before cleanup' }
    if ($state.schema -ne 'snug-hardware-process-v1' -or $state.exitConfirmed -isnot [bool]) {
        throw 'Invalid prior harness process evidence; refusing concurrent cleanup'
    }
    if ($state.exitConfirmed) { return }
    if (-not $state.processId -or -not $state.processStartTimeUtc) {
        throw 'Prior harness launch has no confirmed process identity; retain the owner journal and do not start cleanup'
    }
    $current = Get-HarnessProcessIdentity $state.processId
    if ($null -ne $current -and
        ([DateTimeOffset]$current.processStartTimeUtc).UtcDateTime.Ticks -eq ([DateTimeOffset]$state.processStartTimeUtc).UtcDateTime.Ticks) {
        throw 'Prior harness process is still running; refusing concurrent cleanup and retaining the owner journal'
    }
}

function Stop-HarnessProcess($Process, [int]$TimeoutMilliseconds) {
    try {
        if ($Process.WaitForExit(0)) { return $true }
        try { $Process.Kill() } catch { } # It may have exited between the check and Kill.
        return [bool]$Process.WaitForExit($TimeoutMilliseconds)
    } catch { return $false }
}

function Invoke-Harness([string]$Operation, [string]$Directory, [string[]]$Tokens, [int]$TimeoutSeconds) {
    $stamp = [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss-ffff')
    $stdout = Join-Path $Directory "$Operation-$stamp.stdout.log"
    $stderr = Join-Path $Directory "$Operation-$stamp.stderr.log"
    # Keep cooperating wrappers mutually exclusive, including explicit cleanup after a wrapper crash.
    $lock = [IO.File]::Open((Join-Path $Directory 'harness.lock'), [IO.FileMode]::OpenOrCreate,
        [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $process = $null
    $failure = $null
    $exitConfirmed = $false
    $stopAttempted = $false
    $state = $null
    try {
        Assert-HarnessIdle $Directory
        $exitConfirmed = $true
        $state = [ordered]@{ schema = 'snug-hardware-process-v1'; operation = $Operation
            launchedAt = [DateTimeOffset]::UtcNow.ToString('o'); processId = $null
            processStartTimeUtc = $null; exitConfirmed = $false; state = 'launch-planned' }
        # Persist intent first: a wrapper crash during launch must fail closed on later cleanup.
        Set-HarnessProcessState $Directory $state
        # CRT parsing treats backslashes before a closing quote as escapes.
        $quotedDirectory = '"' + ($Directory -replace '(\\+)$', '$1$1') + '"'
        $arguments = @($Operation.ToLowerInvariant(), $quotedDirectory) + $Tokens
        $exitConfirmed = $false
        # Launch the executable directly; no shell or elevation verb.
        $process = Start-Process -FilePath $binary -ArgumentList $arguments -PassThru -WindowStyle Hidden `
            -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        # Windows PowerShell otherwise loses ExitCode for a short-lived child.
        $null = $process.Handle
        $state.processId = $process.Id
        $state.processStartTimeUtc = $process.StartTime.ToUniversalTime().ToString('o')
        $state.state = 'running'
        Set-HarnessProcessState $Directory $state
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $stopAttempted = $true
            $exitConfirmed = Stop-HarnessProcess $process 5000
            throw "$Operation exceeded ${TimeoutSeconds}s; process exit confirmed: $exitConfirmed. Retained owner journal. Logs: $stdout ; $stderr"
        }
        $exitConfirmed = $true
        $code = $process.ExitCode
        if (Test-Path -LiteralPath $stdout) { Get-Content -LiteralPath $stdout | Write-Host }
        if (Test-Path -LiteralPath $stderr) { Get-Content -LiteralPath $stderr | Write-Host }
        if ($code -ne 0) { throw "$Operation failed with exit code $code. Logs: $stdout ; $stderr" }
    } catch { $failure = $_ }
    finally {
        if ($null -ne $process -and -not $exitConfirmed -and -not $stopAttempted) {
            $exitConfirmed = Stop-HarnessProcess $process 5000
        }
        if ($null -ne $state) {
            $state.exitConfirmed = $exitConfirmed
            $state.state = if ($exitConfirmed) { 'exited' } else { 'termination-unconfirmed' }
            try { Set-HarnessProcessState $Directory $state }
            catch { Write-Warning "Process evidence update failed; retain the owner journal. $($_.Exception.Message)" }
        }
        if ($null -ne $process) { $process.Dispose() }
        $lock.Dispose()
    }
    if ($null -ne $failure) {
        $failure.Exception.Data['HarnessExitConfirmed'] = $exitConfirmed
        throw $failure
    }
}

if ($Mode -eq 'Prepare') {
    [IO.Directory]::CreateDirectory((Join-Path $crate 'src')) | Out-Null
    $sourcesBefore = Get-SourceEvidence
    $snugWin = (Join-Path $repo 'crates/snug-win').Replace('\', '/')
    # serde_json is already used by the workspace; no production dependency changes.
    $lock = [IO.File]::ReadAllText((Join-Path $repo 'Cargo.lock'))
    $serdeVersion = [regex]::Match($lock, '(?m)^name = "serde_json"\r?\nversion = "([^"]+)"').Groups[1].Value
    if (-not $serdeVersion) { throw 'serde_json version is missing from workspace Cargo.lock' }
    [IO.File]::WriteAllText((Join-Path $crate 'Cargo.toml'), @"
[package]
name = "snug-driver-hardware-acceptance"
version = "0.0.0"
edition = "2024"
[workspace]
[dependencies]
snug-win = { path = "$snugWin" }
serde_json = "=$serdeVersion"
"@)
    [IO.File]::Copy($template, (Join-Path $crate 'src/main.rs'), $true)
    $common = @('--manifest-path', (Join-Path $crate 'Cargo.toml'), '--offline', '--target-dir', (Join-Path $output 'cargo'))
    foreach ($command in @('test', 'build')) {
        $code = Invoke-HardwareNative 'cargo' (@($command) + $common) (Join-Path $output "prepare-$command.log")
        if ($code -ne 0) { throw "Hardware harness $command failed with exit code $code; no real-driver operation was run" }
    }
    $sourcesAfter = Get-SourceEvidence
    if (($sourcesBefore | ConvertTo-Json -Depth 5 -Compress) -cne ($sourcesAfter | ConvertTo-Json -Depth 5 -Compress)) {
        throw 'Source changed during preparation; rerun Prepare before hardware testing'
    }
    & $binary --help
    if ($LASTEXITCODE -ne 0) { throw 'Prepared binary help failed' }
    [ordered]@{ schema = 'snug-hardware-prepared-v1'; binary = $binary
        binarySha256 = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash
        preparedAt = [DateTimeOffset]::Now.ToString('o'); sources = $sourcesAfter } |
        ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $preparedPath -Encoding UTF8
    Write-Host "Prepared and unit-tested: $binary"
    Write-Host 'No driver pipe, installation, elevation, key injection, or topology write was performed.'
    exit 0
}

Assert-Elevated
if (-not (Test-Path -LiteralPath $preparedPath) -or -not (Test-Path -LiteralPath $binary)) {
    throw 'Run -Mode Prepare first. No driver operation was attempted.'
}
$prepared = Get-Content -LiteralPath $preparedPath -Raw | ConvertFrom-Json
if ((Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash -cne $prepared.binarySha256) {
    throw 'Prepared binary hash changed; refusing to run it'
}
if ($Mode -ne 'Cleanup') {
    $currentSources = Get-SourceEvidence | ConvertTo-Json -Depth 5 -Compress
    $previousSources = @($prepared.sources) | ConvertTo-Json -Depth 5 -Compress
    if ($currentSources -cne $previousSources) { throw 'Source changed since Prepare; rerun Prepare first' }
}
if ($Mode -eq 'Cleanup') {
    if (-not $RunDirectory) { throw '-RunDirectory must identify the original smoke directory' }
    $run = [IO.Path]::GetFullPath((Join-Path $repo $RunDirectory))
    if (-not (Test-Path -LiteralPath (Join-Path $run 'owner-journal.json') -PathType Leaf)) {
        throw 'No original owner journal exists; no cleanup command was sent'
    }
    Invoke-Harness 'cleanup' $run @() 60
    Write-Host "Owner-scoped cleanup verified: $run"
    exit 0
}

if ($Mode -eq 'Smoke') {
    if (-not $Execute) { throw 'Smoke changes real display topology. Invoke -Mode Smoke -Execute explicitly.' }
    if ($ExpectedDriverSha256 -notmatch '^[0-9a-fA-F]{64}$') {
        throw '-ExpectedDriverSha256 must be the SHA256 of the independently verified installed candidate DLL (after any signing)'
    }
}
if ($RunDirectory) {
    $run = [IO.Path]::GetFullPath((Join-Path $repo $RunDirectory))
    if (Test-Path -LiteralPath $run) { throw 'Use a new RunDirectory to preserve previous evidence' }
} else {
    $run = Join-Path $output ('runs/' + [DateTime]::Now.ToString('yyyyMMdd-HHmmss') + '-' + [Guid]::NewGuid().ToString('N'))
}
[IO.Directory]::CreateDirectory($run) | Out-Null
$inventory = Get-DriverInventory
$inventory | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $run 'inventory-before.json') -Encoding UTF8
if ($Mode -eq 'Smoke') {
    Assert-CandidateDriver $inventory $ExpectedDriverVersion $ExpectedDriverSha256
}
$owner = [Guid]::NewGuid().ToString('N')
$wrong = [Guid]::NewGuid().ToString('N')
try {
    $tokens = @($owner)
    if ($Mode -eq 'Smoke') { $tokens += $wrong }
    Invoke-Harness $Mode $run $tokens 180
} catch {
    $originalError = $_
    if ($Mode -eq 'Smoke' -and (Test-Path -LiteralPath (Join-Path $run 'owner-journal.json'))) {
        if ($originalError.Exception.Data['HarnessExitConfirmed'] -eq $true) {
            try { Invoke-Harness 'cleanup' $run @() 60 }
            catch { Write-Warning "Scoped cleanup also failed. Retain $run and use -Mode Cleanup -RunDirectory `"$run`". $($_.Exception.Message)" }
        } else {
            Write-Warning "Harness exit was not confirmed; no concurrent cleanup was started. Retain $run and its owner journal."
        }
    }
    throw $originalError
} finally {
    try { Get-DriverInventory | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $run 'inventory-after.json') -Encoding UTF8 }
    catch { Write-Warning "Final inventory failed: $($_.Exception.Message)" }
}
Write-Host "$Mode passed. Evidence: $run"
