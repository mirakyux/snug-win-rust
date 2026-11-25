#Requires -Version 7.0
[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs/Snug')
)

$ErrorActionPreference = 'Stop'
$repo = 'mirakyux/snug-win-rust'
$destination = Join-Path ([IO.Path]::GetFullPath($InstallDir)) 'snug.exe'
if (-not $PSCmdlet.ShouldProcess($destination, 'Install the latest Snug GitHub release')) { return }

$release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" -TimeoutSec 30
$exe = @($release.assets | Where-Object name -CEQ 'snug.exe')
$checksums = @($release.assets | Where-Object name -CEQ 'checksums.txt')
if ($release.tag_name -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$' -or $exe.Count -ne 1 -or $checksums.Count -ne 1) {
    throw 'The latest release does not contain the expected Snug assets.'
}
$prefix = "https://github.com/$repo/releases/download/$($release.tag_name)/"
if ($exe[0].browser_download_url -cne ($prefix + 'snug.exe') -or
    $checksums[0].browser_download_url -cne ($prefix + 'checksums.txt')) {
    throw 'Unexpected GitHub asset URL.'
}

$manifest = Invoke-WebRequest -Uri $checksums[0].browser_download_url -TimeoutSec 30
$manifestText = if ($manifest.Content -is [byte[]]) {
    [Text.Encoding]::UTF8.GetString($manifest.Content)
} else {
    $manifest.Content
}
$hashes = [regex]::Matches($manifestText, '(?m)^([0-9a-fA-F]{64})[ \t]+snug\.exe\r?$')
if ($hashes.Count -ne 1) { throw 'Expected one SHA-256 entry for snug.exe.' }

$folder = [IO.Path]::GetDirectoryName($destination)
[IO.Directory]::CreateDirectory($folder) | Out-Null
$staged = Join-Path $folder ('.snug-download-' + [guid]::NewGuid().ToString('N') + '.tmp')
try {
    Write-Host "Downloading Snug $($release.tag_name)"
    Invoke-WebRequest -Uri $exe[0].browser_download_url -OutFile $staged -TimeoutSec 180
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $staged).Hash
    if ($actual -ne $hashes[0].Groups[1].Value) { throw 'Snug download failed SHA-256 verification.' }
    [IO.File]::Move($staged, $destination, $true)
    Write-Host "Installed $destination"
} finally {
    if (Test-Path -LiteralPath $staged -PathType Leaf) { Remove-Item -LiteralPath $staged -Force }
}
