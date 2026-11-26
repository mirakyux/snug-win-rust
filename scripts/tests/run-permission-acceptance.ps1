param([string]$OutputDirectory = ('target/permission-acceptance-' + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss') + '-' + [Guid]::NewGuid().ToString('N')))

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
. (Join-Path $PSScriptRoot 'permission-acceptance.helpers.ps1')
$output = if ([IO.Path]::IsPathRooted($OutputDirectory)) {
    [IO.Path]::GetFullPath($OutputDirectory)
} else { [IO.Path]::GetFullPath((Join-Path $repo $OutputDirectory)) }
$target = [IO.Path]::GetFullPath((Join-Path $repo 'target')).TrimEnd('\', '/')
if (-not $output.StartsWith($target + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'OutputDirectory must be a new directory beneath this repository target directory'
}
# Reject reused evidence and reparse ancestors before writing or executing as admin.
$cursor = $output
while ($cursor) {
    try {
        $attributes = [IO.File]::GetAttributes($cursor)
        if ($cursor -eq $output) { throw "Output directory already exists; choose a new run directory: $output" }
        if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Output path has a reparse ancestor: $cursor"
        }
    } catch [IO.FileNotFoundException] {
    } catch [IO.DirectoryNotFoundException] {
    }
    $cursor = [IO.Path]::GetDirectoryName($cursor)
}
[IO.Directory]::CreateDirectory($output) | Out-Null
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$elevated = ([Security.Principal.WindowsPrincipal]::new($identity)).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
$evidence = [ordered]@{
    schemaVersion = 2; startedAtUtc = [DateTime]::UtcNow.ToString('o'); completedAtUtc = $null
    status = 'running'; elevated = $elevated; outputDirectory = $output
    acl = @(); symlink = $null; fixtureCleanup = 'not-created'; error = $null
}
$fixture = $null
$scriptExit = 1
try {
    $rustc = (Get-Command rustc -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $cargo = (Get-Command cargo -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $whoami = Join-Path ([Environment]::SystemDirectory) 'whoami.exe'
    $tokenExit = Invoke-AcceptanceNative -FilePath $whoami -Arguments @('/all') -LogPath (Join-Path $output 'token.log')
    if ($tokenExit -ne 0) { throw "Token evidence failed: $tokenExit" }

    # Import the entire production module. Its administrator check queries the
    # real enabled-administrator membership of this process token.
    $systemSource = (Join-Path $repo 'crates/snug-win/src/system_paths.rs').Replace('\', '/')
    $aclHarness = @"
#![allow(dead_code)]
#[path = "$systemSource"]
mod system_paths;
mod elevation {
    #[link(name = "shell32")]
    unsafe extern "system" { fn IsUserAnAdmin() -> i32; }
    pub fn is_elevated() -> bool { unsafe { IsUserAnAdmin() != 0 } }
}
"@
    $aclSource = Join-Path $output 'production-acl.rs'
    [IO.File]::WriteAllText($aclSource, $aclHarness)
    foreach ($profile in @('debug', 'release')) {
        $result = [ordered]@{
            profile = $profile; compileExitCode = $null; ordinaryExitCode = $null; elevatedExitCode = $null
            elevatedStatus = 'not-run'; productionSourceSha256 = (Get-FileHash -LiteralPath $systemSource -Algorithm SHA256).Hash
        }
        $evidence.acl += $result
        $binary = Join-Path $output "acl-$profile.exe"
        $flags = @('--edition=2024', '--test', $aclSource, '-o', $binary)
        if ($profile -eq 'release') { $flags += '-O' }
        $result.compileExitCode = Invoke-AcceptanceNative -FilePath $rustc -Arguments $flags -LogPath (Join-Path $output "acl-compile-$profile.log")
        if ($result.compileExitCode -ne 0) { throw "ACL harness compilation failed ($profile)" }
        $result.ordinaryExitCode = Invoke-AcceptanceNative -FilePath $binary -Arguments @('--nocapture') -LogPath (Join-Path $output "acl-$profile.log")
        if ($result.ordinaryExitCode -ne 0) { throw "Ordinary ACL regressions failed ($profile)" }
        $result.elevatedExitCode = Invoke-AcceptanceNative -FilePath $binary -Arguments @('--ignored', '--exact', 'system_paths::tests::elevated_atomic_staging_inherits_production_acl', '--nocapture') -LogPath (Join-Path $output "acl-elevated-$profile.log")
        $result.elevatedStatus = if ($result.elevatedExitCode -eq 0) { 'passed' } elseif (-not $elevated) { 'blocked-by-medium-token' } else { 'failed' }
        if ($elevated -and $result.elevatedExitCode -ne 0) { throw "Elevated ACL acceptance failed ($profile)" }
    }

    if (-not ('SnugSymlinkProbe' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class SnugSymlinkProbe {
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    [return: MarshalAs(UnmanagedType.U1)]
    private static extern bool CreateSymbolicLinkW(string link, string target, uint flags);
    public static int Create(string link, string target, bool directory, bool allowUnprivileged) {
        uint flags = (allowUnprivileged ? 2u : 0u) | (directory ? 1u : 0u);
        if (CreateSymbolicLinkW(link, target, flags)) return 0;
        return Marshal.GetLastWin32Error();
    }
}
'@
    }
    $fixture = Join-Path $output ('fixtures-' + [Guid]::NewGuid().ToString('N'))
    [IO.Directory]::CreateDirectory($fixture) | Out-Null
    $evidence.fixtureCleanup = 'pending'
    $fileTarget = Join-Path $fixture 'target.json'
    $dirTarget = Join-Path $fixture 'directory-target'
    [IO.File]::WriteAllText($fileTarget, '"retained"')
    [IO.Directory]::CreateDirectory($dirTarget) | Out-Null
    $fileError = [SnugSymlinkProbe]::Create((Join-Path $fixture 'file-link.json'), $fileTarget, $false, $true)
    $dirError = [SnugSymlinkProbe]::Create((Join-Path $fixture 'directory-link'), $dirTarget, $true, $true)
    # Flags 0/1 additionally exercise the privileged path even if Developer Mode
    # permits flags 2/3. No system policy or persistent token setting is changed.
    $privilegedFileError = [SnugSymlinkProbe]::Create((Join-Path $fixture 'privileged-file-link.json'), $fileTarget, $false, $false)
    $privilegedDirError = [SnugSymlinkProbe]::Create((Join-Path $fixture 'privileged-directory-link'), $dirTarget, $true, $false)
    $canRunSymlinks = $fileError -eq 0 -and $dirError -eq 0
    $evidence.symlink = [ordered]@{
        fileCreateWin32Error = $fileError; directoryCreateWin32Error = $dirError
        flags = 'ALLOW_UNPRIVILEGED_CREATE: file=2 directory=3'
        privilegedFileCreateWin32Error = $privilegedFileError; privilegedDirectoryCreateWin32Error = $privilegedDirError
        privilegedFlags = 'file=0 directory=1'
        status = 'blocked'; debugCompileExitCode = $null; releaseCompileExitCode = $null
        debugExitCode = $null; releaseExitCode = $null
        productionSourceSha256 = (Get-FileHash -LiteralPath (Join-Path $repo 'crates/snug-core/src/store.rs') -Algorithm SHA256).Hash
    }

    # Compile real-store tests even when this token cannot create their fixtures.
    $crate = Join-Path $output 'store-symlink-harness'
    [IO.Directory]::CreateDirectory((Join-Path $crate 'src')) | Out-Null
    $core = (Join-Path $repo 'crates/snug-core').Replace('\', '/')
    [IO.File]::WriteAllText((Join-Path $crate 'Cargo.toml'), @"
[package]
name = "snug-store-symlink-acceptance"
version = "0.0.0"
edition = "2024"
[workspace]
[dev-dependencies]
snug-core = { path = "$core" }
"@)
    [IO.File]::Copy((Join-Path $PSScriptRoot 'store-symlink-harness.template.rs'), (Join-Path $crate 'src/lib.rs'), $true)
    $previousRoot = [Environment]::GetEnvironmentVariable('SNUG_SYMLINK_FIXTURE_ROOT', 'Process')
    try {
        $env:SNUG_SYMLINK_FIXTURE_ROOT = $fixture
        foreach ($profile in @('debug', 'release')) {
            $cargoArgs = @('test', '--manifest-path', (Join-Path $crate 'Cargo.toml'), '--offline', '--target-dir', (Join-Path $output 'cargo'))
            if ($profile -eq 'release') { $cargoArgs += '--release' }
            $cargoExit = Invoke-AcceptanceNative -FilePath $cargo -Arguments ($cargoArgs + '--no-run') -LogPath (Join-Path $output "symlink-compile-$profile.log")
            $evidence.symlink["$($profile)CompileExitCode"] = $cargoExit
            if ($cargoExit -ne 0) { throw "Store symlink harness compilation failed ($profile)" }
            if ($canRunSymlinks) {
                $cargoExit = Invoke-AcceptanceNative -FilePath $cargo -Arguments ($cargoArgs + @('--', '--nocapture')) -LogPath (Join-Path $output "symlink-$profile.log")
                $evidence.symlink["$($profile)ExitCode"] = $cargoExit
                if ($cargoExit -ne 0) { throw "Store symlink harness execution failed ($profile)" }
            }
        }
    } finally {
        [Environment]::SetEnvironmentVariable('SNUG_SYMLINK_FIXTURE_ROOT', $previousRoot, 'Process')
    }
    if ($canRunSymlinks) { $evidence.symlink.status = 'passed' }
    if ($elevated -and ($privilegedFileError -ne 0 -or $privilegedDirError -ne 0)) {
        throw 'Elevated token could not create both privileged symlink fixtures (flags 0/1)'
    }
    if ($elevated -and $canRunSymlinks) { $evidence.status = 'passed'; $scriptExit = 0 }
    else { $evidence.status = 'blocked'; $scriptExit = 77 }
} catch {
    $evidence.status = 'failed'
    $evidence.error = $_.ToString()
    Write-Host ("FAIL: " + $_.ToString()) -ForegroundColor Red
} finally {
    if ($null -ne $fixture) {
        try {
            Remove-AcceptanceFixture -Path $fixture -Parent $output
            $evidence.fixtureCleanup = 'passed'
        } catch {
            $evidence.fixtureCleanup = 'failed: ' + $_.ToString()
            $evidence.status = 'failed'; $scriptExit = 1
            Write-Host ("Fixture cleanup failed: " + $_.ToString()) -ForegroundColor Red
        }
    }
    $evidence.completedAtUtc = [DateTime]::UtcNow.ToString('o')
    $json = $evidence | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText((Join-Path $output 'verified-results.json'), $json)
    Write-Host $json
}
exit $scriptExit
