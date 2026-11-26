param([string]$OutputDirectory = ('target/permission-script-regressions-' + [Guid]::NewGuid().ToString('N')))

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
. (Join-Path $PSScriptRoot 'permission-acceptance.helpers.ps1')
$output = [IO.Path]::GetFullPath((Join-Path $repo $OutputDirectory))
[IO.Directory]::CreateDirectory($output) | Out-Null
$shell = Join-Path $env:windir 'System32/WindowsPowerShell/v1.0/powershell.exe'
$native = Join-Path $output 'native.ps1'
$log = Join-Path $output 'native.log'
[IO.File]::WriteAllText($native, '[Console]::Error.WriteLine(''native progress''); exit 0')
$exitCode = Invoke-AcceptanceNative -FilePath $shell -Arguments @('-NoProfile', '-File', $native) -LogPath $log
if ($exitCode -ne 0 -or -not [IO.File]::ReadAllText($log).Contains('native progress')) {
    throw 'Normal native stderr must preserve exit code 0 and remain logged'
}
if ($ErrorActionPreference -ne 'Stop') { throw 'Native capture must restore ErrorActionPreference' }
[IO.File]::WriteAllText($native, '[Console]::Error.WriteLine(''native failure''); exit 23')
$exitCode = Invoke-AcceptanceNative -FilePath $shell -Arguments @('-NoProfile', '-File', $native) -LogPath $log
if ($exitCode -ne 23 -or -not [IO.File]::ReadAllText($log).Contains('native failure')) {
    throw 'Native failure must preserve its nonzero exit code and log'
}

$fixture = Join-Path $output 'fixtures-cleanup'
$outside = Join-Path $output 'retained-target'
[IO.Directory]::CreateDirectory($fixture) | Out-Null
[IO.Directory]::CreateDirectory($outside) | Out-Null
$sentinel = Join-Path $outside 'sentinel.txt'
[IO.File]::WriteAllText($sentinel, 'retained')
[IO.Directory]::CreateDirectory((Join-Path $fixture 'nested')) | Out-Null
[IO.File]::WriteAllText((Join-Path $fixture 'nested/payload.txt'), 'temporary')
New-Item -ItemType Junction -Path (Join-Path $fixture 'outside-link') -Target $outside | Out-Null
Remove-AcceptanceFixture -Path $fixture -Parent $output
if ([IO.Directory]::Exists($fixture)) { throw 'Fixture must be removed' }
if ([IO.File]::ReadAllText($sentinel) -ne 'retained') { throw 'Cleanup followed a junction outside the fixture' }
Remove-AcceptanceFixture -Path $fixture -Parent $output
$refused = $false
try { Remove-AcceptanceFixture -Path $outside -Parent $output }
catch { $refused = $true }
if (-not $refused -or [IO.File]::ReadAllText($sentinel) -ne 'retained') { throw 'Cleanup must refuse unrelated paths' }
$fixture = Join-Path $output 'fixtures-broken-link'
$temporaryTarget = Join-Path $output 'removed-target'
[IO.Directory]::CreateDirectory($fixture) | Out-Null
[IO.Directory]::CreateDirectory($temporaryTarget) | Out-Null
New-Item -ItemType Junction -Path (Join-Path $fixture 'broken-link') -Target $temporaryTarget | Out-Null
[IO.Directory]::Delete($temporaryTarget)
Remove-AcceptanceFixture -Path $fixture -Parent $output
if ([IO.Directory]::Exists($fixture)) { throw 'Broken junction fixture must be removed' }

$entry = Join-Path $PSScriptRoot 'run-permission-acceptance.ps1'
$exitCode = Invoke-AcceptanceNative -FilePath $shell -Arguments @('-NoProfile', '-File', $entry, '-OutputDirectory', $output) -LogPath (Join-Path $output 'refuse-reused-output.log')
if ($exitCode -eq 0 -or [IO.File]::Exists((Join-Path $output 'verified-results.json'))) { throw 'Entry must refuse reused output before writing evidence' }
$linkedOutput = Join-Path $output 'linked-output'
New-Item -ItemType Junction -Path $linkedOutput -Target $outside | Out-Null
$exitCode = Invoke-AcceptanceNative -FilePath $shell -Arguments @('-NoProfile', '-File', $entry, '-OutputDirectory', (Join-Path $linkedOutput 'never-created')) -LogPath (Join-Path $output 'refuse-reparse-output.log')
if ($exitCode -eq 0 -or [IO.Directory]::Exists((Join-Path $outside 'never-created'))) { throw 'Entry must refuse reparse ancestors before writing' }
[IO.Directory]::Delete($linkedOutput)
$template = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'store-symlink-harness.template.rs'))
$entryText = [IO.File]::ReadAllText($entry)
$storeNames = @([regex]::Matches($template, 'fixture\("([^"]+)"\)') | ForEach-Object { $_.Groups[1].Value })
$probeNames = @([regex]::Matches($entryText, 'SnugSymlinkProbe\]::Create\(\(Join-Path \$fixture ''([^'']+)''\)') | ForEach-Object { $_.Groups[1].Value })
if ($storeNames.Count -ne 2 -or $probeNames.Count -ne 4) { throw 'Expected both store fixtures and all four probe fixture names' }
if (@($storeNames | Where-Object { $_ -in $probeNames }).Count -ne 0) { throw 'Store fixtures must not collide with existing symlink probes' }
Write-Host 'PASS: native stderr success, native failure, preference restoration, bounded cleanup, junction target retention, absent fixture, unrelated path refusal, broken junction cleanup, reused output refusal, reparse ancestor refusal, disjoint store and probe fixtures'
