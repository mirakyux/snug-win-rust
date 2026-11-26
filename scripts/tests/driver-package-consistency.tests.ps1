#Requires -Version 7.0
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path (Split-Path -Parent $PSScriptRoot) 'driver-package-consistency.ps1')
Add-Type -AssemblyName System.IO.Compression.FileSystem

$capabilities = @('SnugReserve', 'SnugCreate', 'SnugReplace', 'SnugRelease', 'SnugCheck')
$passed = 0

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function New-Fixture {
    $root = Join-Path ([IO.Path]::GetTempPath()) ("snug-driver-consistency-{0}" -f [guid]::NewGuid().ToString('N'))
    $vendor = Join-Path $root 'vendor/virtual-display-rs'
    $ipcDir = Join-Path $vendor 'driver-ipc/src'
    $driverDir = Join-Path $vendor 'virtual-display-driver'
    $handlerDir = Join-Path $driverDir 'src'
    $repoDriver = Join-Path $root 'driver'
    New-Item -ItemType Directory -Path $ipcDir,$handlerDir,$repoDriver -Force | Out-Null

    $capabilityLines = $capabilities | ForEach-Object { "    $_ { owner: String }," }
    [IO.File]::WriteAllText((Join-Path $ipcDir 'core.rs'), "pub enum RequestCommand {`n$($capabilityLines -join "`n")`n}`n")
    $handlerLines = $capabilities | ForEach-Object { "RequestCommand::$_ => {}" }
    [IO.File]::WriteAllText((Join-Path $handlerDir 'ipc.rs'), ($handlerLines -join "`n"))
    [IO.File]::WriteAllText((Join-Path $driverDir 'Cargo.toml'), "[package]`nname = `"virtual-display-driver`"`nversion = `"0.4.0`"`n")
    [IO.File]::WriteAllText((Join-Path $driverDir 'build.rs'), @'
winres
    .set("FileVersion", "0.4.0.12")
    .set("ProductVersion", "0.4.0.12")
'@)
    $msiName = 'virtual-display-driver-0.4.0.12-x86_64.msi'
    $msiBytes = [byte[]](0x4D,0x53,0x49,0x21)
    $rsa = [System.Security.Cryptography.RSA]::Create(2048)
    $request = [System.Security.Cryptography.X509Certificates.CertificateRequest]::new(
        'CN=Snug offline fixture', $rsa,
        [System.Security.Cryptography.HashAlgorithmName]::SHA256,
        [System.Security.Cryptography.RSASignaturePadding]::Pkcs1)
    $certificate = $request.CreateSelfSigned([DateTimeOffset]::UtcNow.AddDays(-1), [DateTimeOffset]::UtcNow.AddDays(1))
    $certificateBytes = $certificate.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Cert)
    $certificateInfo = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new($certificateBytes)
    $zipPath = Join-Path $repoDriver 'virtual-desktop-driver-installer-x64.zip'
    $archive = [System.IO.Compression.ZipFile]::Open($zipPath, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($entry in @(
            @{ Name = $msiName; Bytes = $msiBytes },
            @{ Name = 'DriverCertificate.cer'; Bytes = $certificateBytes }
        )) {
            $zipEntry = $archive.CreateEntry($entry.Name)
            $stream = $zipEntry.Open()
            try { $stream.Write($entry.Bytes, 0, $entry.Bytes.Length) } finally { $stream.Dispose() }
        }
    } finally {
        $archive.Dispose()
        $certificateInfo.Dispose()
        $certificate.Dispose()
        $rsa.Dispose()
    }

    $archiveHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash
    $msiHash = Get-BytesSha256 $msiBytes
    $certificateHash = Get-BytesSha256 $certificateBytes
    $thumbprint = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new($certificateBytes).Thumbprint
    $sourceHash = Get-DriverSourceTreeSha256 -SourceRoot $vendor
    $lock = [ordered]@{
        repository = 'https://example.invalid/virtual-display-rs'
        commit = '0123456789abcdef0123456789abcdef01234567'
        license = 'MIT'
        cargoPackage = 'virtual-display-driver'
        driverVersion = '0.4.0.12'
        minimumDriverVersion = '0.4.0.9'
        driverDate = '11/26/2026'
        sourceArchiveSha256 = ('A' * 64)
        sourceTreeSha256 = $sourceHash
        requiredProtocolCapabilities = $capabilities
        releasePackage = [ordered]@{
            driverVersion = '0.4.0.12'
            archiveName = 'virtual-desktop-driver-installer-x64.zip'
            archiveSha256 = $archiveHash
            msiSha256 = $msiHash
            certificateSha256 = $certificateHash
            certificateThumbprint = $thumbprint
            protocolCapabilities = $capabilities
        }
    }
    $lockPath = Join-Path $repoDriver 'source.lock.json'
    $lock | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $lockPath -Encoding utf8
    $installer = @"
`$ExpectedVersion = `"0.4.0.12`"
`$ExpectedZipSha256 = `"$archiveHash`"
`$ExpectedMsiSha256 = `"$msiHash`"
`$ExpectedCertSha256 = `"$certificateHash`"
`$ExpectedThumbprint = `"$thumbprint`"
"@
    Set-Content -LiteralPath (Join-Path $repoDriver 'Install-SnugDriver.ps1') -Value $installer -Encoding utf8

    return [pscustomobject]@{ Root = $root; Vendor = $vendor; Zip = $zipPath; LockPath = $lockPath }
}

function Invoke-FixtureIssues($Fixture, [switch]$SourceOnly) {
    Get-DriverPackageConsistencyIssues -RepositoryRoot $Fixture.Root -ArchivePath $Fixture.Zip -CheckProtocolSource -SourceOnly:$SourceOnly
}

function Invoke-Test([string]$Name, [scriptblock]$Body) {
    & $Body
    $script:passed++
    Write-Host "PASS $Name"
}

$fixtures = [System.Collections.Generic.List[object]]::new()
try {
    $fixture = New-Fixture
    $fixtures.Add($fixture)
    Invoke-Test 'valid source, installer, ZIP, MSI, certificate, and protocol pass' {
        $issues = @(Invoke-FixtureIssues $fixture)
        Assert-True ($issues.Count -eq 0) ($issues -join "`n")
    }
    Invoke-Test 'source-only candidate check works before building a driver' {
        $issues = @(Invoke-FixtureIssues $fixture -SourceOnly)
        Assert-True ($issues.Count -eq 0) ($issues -join "`n")
    }
    Invoke-Test 'source hash ignores checkout line-ending conversion' {
        $ipcPath = Join-Path $fixture.Vendor 'driver-ipc/src/core.rs'
        $text = [IO.File]::ReadAllText($ipcPath).Replace("`r`n", "`n")
        [IO.File]::WriteAllText($ipcPath, $text.Replace("`n", "`r`n"), [Text.UTF8Encoding]::new($false))
        $issues = @(Invoke-FixtureIssues $fixture -SourceOnly)
        Assert-True ($issues.Count -eq 0) ($issues -join "`n")
    }

    Invoke-Test 'old release package version is rejected' {
        $lock = Get-Content -LiteralPath $fixture.LockPath -Raw | ConvertFrom-Json
        $lock.releasePackage.driverVersion = '0.3.1'
        $lock | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $fixture.LockPath -Encoding utf8
        $issues = @(Invoke-FixtureIssues $fixture)
        Assert-True (@($issues | Where-Object { $_ -match '发行包版本|低于所需协议版本' }).Count -ge 1) ($issues -join "`n")
    }
    $fixture = New-Fixture
    $fixtures.Add($fixture)
    Invoke-Test 'archive hash mismatch is rejected' {
        $lock = Get-Content -LiteralPath $fixture.LockPath -Raw | ConvertFrom-Json
        $lock.releasePackage.archiveSha256 = ('0' * 64)
        $lock | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $fixture.LockPath -Encoding utf8
        $issues = @(Invoke-FixtureIssues $fixture)
        Assert-True (@($issues | Where-Object { $_ -match '归档 SHA-256' }).Count -eq 1) ($issues -join "`n")
    }
    $fixture = New-Fixture
    $fixtures.Add($fixture)
    Invoke-Test 'missing owned protocol capability is rejected' {
        $lock = Get-Content -LiteralPath $fixture.LockPath -Raw | ConvertFrom-Json
        $lock.releasePackage.protocolCapabilities = @($lock.releasePackage.protocolCapabilities | Where-Object { $_ -ne 'SnugCheck' })
        $lock | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $fixture.LockPath -Encoding utf8
        $issues = @(Invoke-FixtureIssues $fixture)
        Assert-True (@($issues | Where-Object { $_ -match '缺少源码要求的协议能力：SnugCheck' }).Count -eq 1) ($issues -join "`n")
    }
    $fixture = New-Fixture
    $fixtures.Add($fixture)
    Invoke-Test 'changed vendored source tree is rejected' {
        Add-Content -LiteralPath (Join-Path $fixture.Vendor 'virtual-display-driver/Cargo.toml') -Value '# modified'
        $issues = @(Invoke-FixtureIssues $fixture -SourceOnly)
        Assert-True (@($issues | Where-Object { $_ -match '源码树哈希' }).Count -eq 1) ($issues -join "`n")
    }

    $workspaceRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
    Invoke-Test 'current shipping archive is rejected as incompatible with the locked source' {
        $issues = @(Get-DriverPackageConsistencyIssues -RepositoryRoot $workspaceRoot -CheckProtocolSource)
        Assert-True (@($issues | Where-Object { $_ -match '低于所需协议版本' }).Count -eq 1) ($issues -join "`n")
        Assert-True (@($issues | Where-Object { $_ -match '发行包缺少源码要求的协议能力' }).Count -eq 5) ($issues -join "`n")
    }
} finally {
    foreach ($fixture in $fixtures) {
        if (Test-Path -LiteralPath $fixture.Root) { Remove-Item -LiteralPath $fixture.Root -Recurse -Force }
    }
}

Write-Host "All $passed offline driver package consistency tests passed."
