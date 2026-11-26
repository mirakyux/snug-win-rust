param([string]$OutputDirectory = 'target/settings-persistence-regressions')

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
. (Join-Path $repo 'driver/tests/rust-source-extraction.ps1')
$output = [IO.Path]::GetFullPath((Join-Path $repo $OutputDirectory))
[IO.Directory]::CreateDirectory($output) | Out-Null
$source = [IO.File]::ReadAllText((Join-Path $repo 'crates/snug-app/src/ui.rs'))
$template = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'settings-persistence-harness.template.rs'))
$methods = @('fn save_config', 'fn apply_settings_change') | ForEach-Object { Get-RustItem $source $_ }
$generated = $template.Replace('/* PRODUCTION_METHODS */', ($methods -join "`n"))
$generated = $generated.Replace('/* PRODUCTION_SELECTION_HELPER */', (Get-RustItem $source 'fn repair_removed_screen_selection'))
$harness = Join-Path $output 'settings-persistence.rs'
[IO.File]::WriteAllText($harness, $generated)
foreach ($profile in @('debug', 'release')) {
    $binary = Join-Path $output "$profile.exe"
    $flags = @('--edition=2024', '--test', $harness, '-o', $binary)
    if ($profile -eq 'release') { $flags += '-O' }
    & rustc @flags
    if ($LASTEXITCODE -ne 0) { throw "Settings harness compilation failed ($profile)" }
    & $binary --nocapture 2>&1 | Tee-Object -FilePath (Join-Path $output "$profile-results.log")
    if ($LASTEXITCODE -ne 0) { throw "Settings regressions failed ($profile)" }
}
