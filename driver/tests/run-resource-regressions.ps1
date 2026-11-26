#Requires -Version 7.0
[CmdletBinding()]
param([string]$ArtifactDirectory)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$sourceDirectory = Join-Path $repo 'vendor/virtual-display-rs/virtual-display-driver/src'
$contextSource = Join-Path $sourceDirectory 'context.rs'
$swapchainSource = Join-Path $sourceDirectory 'swap_chain_processor.rs'
if (-not $ArtifactDirectory) { $ArtifactDirectory = Join-Path $repo 'target/driver-resource-tests' }
$artifacts = (New-Item -ItemType Directory -Path $ArtifactDirectory -Force).FullName

. (Join-Path $PSScriptRoot 'rust-source-extraction.ps1')

# Test the extractor itself against delimiters inside literals and nested comments.
$specimen = @'
fn specimen<'a>() {
    let raw = r###"} /* not a comment */"###;
    let escaped = "\" }";
    let character = '}';
    // }
    /* } /* nested { */ } */
}
'@
if ((Get-RustItem ($specimen + "`nfn unrelated() {}") "fn specimen<'a>") -cne $specimen.TrimEnd()) {
    throw 'Rust item boundary regression failed'
}

& rustfmt +nightly-2024-07-26 --check --edition 2021 $contextSource $swapchainSource
if ($LASTEXITCODE -ne 0) { throw 'Production Rust parsing/format check failed' }
$source = Get-Content -LiteralPath $contextSource -Raw
$monitor = Get-RustItem $source 'impl MonitorContext'
$items = [Collections.Generic.List[string]]::new()
$items.Add((Get-RustItem $source 'pub struct MonitorContext'))
$items.Add((Get-RustItem $source 'unsafe impl Send for MonitorContext'))
$items.Add((Get-RustItem $source 'unsafe impl Sync for MonitorContext'))
$items.Add("#[repr(C)]`n" + (Get-RustItem $source 'struct MonitorContextStorage'))
$items.Add((Get-RustItem $source 'unsafe impl Sync for MonitorContextStorage'))
$items.Add((Get-RustItem $source "struct MonitorContextStorageGuard<'a>"))
$items.Add((Get-RustItem $source "impl Drop for MonitorContextStorageGuard<'_>"))
$items.Add((Get-RustItem $source 'impl MonitorContextStorage'))
$items.Add("#[repr(transparent)]`n" + (Get-RustItem $source 'struct MonitorContextTypeInfo'))
$items.Add((Get-RustItem $source 'unsafe impl Sync for MonitorContextTypeInfo'))
$items.Add((Get-RustItem $source 'static MONITOR_CONTEXT_TYPE_INFO'))
$items.Add((Get-RustItem $source 'unsafe extern "C-unwind" fn monitor_context_cleanup'))
$methods = foreach ($head in @('pub fn new', 'fn object_attributes', 'unsafe fn get_type_info', 'unsafe fn storage', 'pub unsafe fn init', 'pub unsafe fn get_mut', 'pub unsafe fn drop', 'pub fn setup_hw_cursor')) {
    Get-RustItem $monitor $head
}
$items.Add("impl MonitorContext {`n" + ($methods -join "`n") + "`n}")
$template = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'cursor-harness.template.rs') -Raw
$cursorHarness = Join-Path $artifacts 'cursor-harness.rs'
$template.Replace('// ACTUAL_PRODUCTION_ITEMS', ($items -join "`n")) | Set-Content -LiteralPath $cursorHarness -Encoding utf8

$priorSwapchainSource = $env:SNUG_SWAPCHAIN_SOURCE
try {
    $env:SNUG_SWAPCHAIN_SOURCE = $swapchainSource
    foreach ($kind in 'cursor', 'swapchain') {
        $testSource = if ($kind -eq 'cursor') { $cursorHarness } else { Join-Path $PSScriptRoot 'swapchain-harness.rs' }
        $executable = Join-Path $artifacts "$kind-harness.exe"
        & rustc +nightly-2024-07-26 --edition 2021 --test -D warnings $testSource -o $executable
        if ($LASTEXITCODE -ne 0) { throw "$kind harness compilation failed" }
        & $executable --test-threads=1 2>&1 | Tee-Object -FilePath (Join-Path $artifacts "$kind-results.log")
        if ($LASTEXITCODE -ne 0) { throw "$kind behavioral regressions failed" }
    }
} finally {
    $env:SNUG_SWAPCHAIN_SOURCE = $priorSwapchainSource
}
Get-FileHash -LiteralPath $contextSource,$swapchainSource -Algorithm SHA256 |
    Select-Object Path,Hash | ConvertTo-Json |
    Set-Content -LiteralPath (Join-Path $artifacts 'source-hashes.json') -Encoding utf8
Write-Output "Resource regression results: $artifacts"
