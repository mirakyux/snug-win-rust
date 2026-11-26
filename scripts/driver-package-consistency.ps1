Set-StrictMode -Version Latest

function Get-DriverPackageConsistencyIssues {
    param(
        [Parameter(Mandatory)][string]$RepositoryRoot,
        [string]$ArchivePath,
        [switch]$CheckProtocolSource,
        [switch]$SourceOnly
    )

    $issues = [System.Collections.Generic.List[string]]::new()
    $lockPath = Join-Path $RepositoryRoot 'driver/source.lock.json'
    try {
        $lock = Get-Content -LiteralPath $lockPath -Raw | ConvertFrom-Json
    } catch {
        return @("读取 driver/source.lock.json 失败：$($_.Exception.Message)")
    }

    if ($lock.driverVersion -notmatch '^\d+\.\d+\.\d+\.\d+$') {
        $issues.Add('source.lock.json 缺少有效的四段 driverVersion')
    }
    if ($lock.sourceArchiveSha256 -notmatch '^[0-9A-Fa-f]{64}$') {
        $issues.Add('source.lock.json 缺少有效的 sourceArchiveSha256')
    }
    if (-not $lock.releasePackage) {
        $issues.Add('source.lock.json 缺少 releasePackage 清单')
        return @($issues)
    }

    $package = $lock.releasePackage
    $required = @($lock.requiredProtocolCapabilities)
    if ($required.Count -eq 0) {
        $issues.Add('source.lock.json 没有声明最低协议能力')
    }

    try {
        $minimumVersion = [version]$lock.minimumDriverVersion
        $sourceVersion = [version]$lock.driverVersion
        if ($sourceVersion -lt $minimumVersion) {
            $issues.Add("源码驱动版本 $sourceVersion 低于要求的最小协议版本 $minimumVersion")
        }
    } catch {
        $issues.Add('source.lock.json 的 minimumDriverVersion 或 driverVersion 无效')
    }

    if ($CheckProtocolSource) {
        $sourceRoot = Join-Path $RepositoryRoot 'vendor/virtual-display-rs'
        $actualSourceHash = Get-DriverSourceTreeSha256 -SourceRoot $sourceRoot
        if (-not $lock.PSObject.Properties['sourceTreeSha256'] -or
            [string]::IsNullOrWhiteSpace([string]$lock.sourceTreeSha256) -or
            $actualSourceHash -ine $lock.sourceTreeSha256) {
            $issues.Add("驱动源码树哈希与 source.lock.json 不一致：$actualSourceHash")
        }
        foreach ($sourceIssue in @(Get-DriverProtocolSourceIssues -SourceRoot $sourceRoot -Lock $lock)) {
            $issues.Add([string]$sourceIssue)
        }
    }

    if (-not $SourceOnly) {
        if ($package.driverVersion -cne $lock.driverVersion) {
            $issues.Add("发行包版本 $($package.driverVersion) 与源码锁定版本 $($lock.driverVersion) 不一致")
        }
        if ($package.archiveSha256 -notmatch '^[0-9A-Fa-f]{64}$' -or
            $package.msiSha256 -notmatch '^[0-9A-Fa-f]{64}$' -or
            $package.certificateSha256 -notmatch '^[0-9A-Fa-f]{64}$' -or
            $package.certificateThumbprint -notmatch '^[0-9A-Fa-f]{40}$' -or
            [string]::IsNullOrWhiteSpace([string]$package.archiveName)) {
            $issues.Add('releasePackage 的归档、MSI、证书哈希或证书指纹格式无效')
        }
        if ($package.archiveName -cne 'virtual-desktop-driver-installer-x64.zip') {
            $issues.Add("releasePackage.archiveName 与 Snug 安装入口约定不一致：$($package.archiveName)")
        }
        try {
            if ([version]$package.driverVersion -lt [version]$lock.minimumDriverVersion) {
                $issues.Add("发行驱动包版本 $($package.driverVersion) 低于所需协议版本 $($lock.minimumDriverVersion)")
            }
        } catch {
            $issues.Add('releasePackage.driverVersion 或 minimumDriverVersion 无效')
        }

        $provided = @($package.protocolCapabilities)
        foreach ($capability in $required) {
            if ($capability -notin $provided) {
                $issues.Add("发行包缺少源码要求的协议能力：$capability")
            }
        }

        $installerPath = Join-Path $RepositoryRoot 'driver/Install-SnugDriver.ps1'
        if (Test-Path -LiteralPath $installerPath -PathType Leaf) {
            $installer = Get-Content -LiteralPath $installerPath -Raw
            $constants = @{
                ExpectedVersion = [string]$package.driverVersion
                ExpectedZipSha256 = [string]$package.archiveSha256
                ExpectedMsiSha256 = [string]$package.msiSha256
                ExpectedCertSha256 = [string]$package.certificateSha256
                ExpectedThumbprint = [string]$package.certificateThumbprint
            }
            foreach ($name in $constants.Keys) {
                $pattern = '(?m)^\$' + [regex]::Escape($name) + '\s*=\s*["'']([^"'']+)["'']'
                $match = [regex]::Match($installer, $pattern)
                if (-not $match.Success -or $match.Groups[1].Value -cne $constants[$name]) {
                    $issues.Add("Install-SnugDriver.ps1 的 $name 与 releasePackage 清单不一致")
                }
            }
        } else {
            $issues.Add('缺少 driver/Install-SnugDriver.ps1')
        }
    }

    if (-not $SourceOnly -and $ArchivePath) {
        if (-not (Test-Path -LiteralPath $ArchivePath -PathType Leaf)) {
            $issues.Add("缺少发行驱动包：$ArchivePath")
        } else {
            if ([IO.Path]::GetFileName($ArchivePath) -cne $package.archiveName) {
                $issues.Add("驱动归档文件名与 releasePackage 不一致：$([IO.Path]::GetFileName($ArchivePath))")
            }
            $actualArchiveHash = (Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash
            if ($actualArchiveHash -ine $package.archiveSha256) {
                $issues.Add("驱动归档 SHA-256 与 releasePackage 清单不一致：$actualArchiveHash")
            }
            try {
                Add-Type -AssemblyName System.IO.Compression.FileSystem
                $archive = [System.IO.Compression.ZipFile]::OpenRead((Resolve-Path -LiteralPath $ArchivePath).Path)
                try {
                    $msiEntries = @($archive.Entries | Where-Object { [IO.Path]::GetExtension($_.FullName) -ieq '.msi' })
                    $certEntries = @($archive.Entries | Where-Object { [IO.Path]::GetFileName($_.FullName) -ieq 'DriverCertificate.cer' })
                    if ($msiEntries.Count -ne 1) {
                        $issues.Add("驱动归档应恰含一个 MSI，实际为 $($msiEntries.Count) 个")
                    } else {
                        $msiName = [IO.Path]::GetFileName($msiEntries[0].FullName)
                        $versionMatch = [regex]::Match($msiName, '^virtual-display-driver-(\d+\.\d+\.\d+(?:\.\d+)?)-x86_64\.msi$')
                        if (-not $versionMatch.Success -or $versionMatch.Groups[1].Value -cne $package.driverVersion) {
                            $issues.Add("MSI 文件名版本与 releasePackage 不一致：$msiName")
                        }
                        $msiHash = Get-ArchiveEntrySha256 $msiEntries[0]
                        if ($msiHash -ine $package.msiSha256) {
                            $issues.Add("归档内 MSI SHA-256 与 releasePackage 清单不一致：$msiHash")
                        }
                    }
                    if ($certEntries.Count -ne 1) {
                        $issues.Add("驱动归档应恰含一个 DriverCertificate.cer，实际为 $($certEntries.Count) 个")
                    } else {
                        $certBytes = Get-ArchiveEntryBytes $certEntries[0]
                        $certHash = Get-BytesSha256 $certBytes
                        if ($certHash -ine $package.certificateSha256) {
                            $issues.Add("归档内证书 SHA-256 与 releasePackage 清单不一致：$certHash")
                        }
                        try {
                            $cert = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new($certBytes)
                            if ($cert.Thumbprint -ine $package.certificateThumbprint) {
                                $issues.Add("归档内证书指纹与 releasePackage 清单不一致：$($cert.Thumbprint)")
                            }
                        } catch {
                            $issues.Add("归档内证书无法解析：$($_.Exception.Message)")
                        }
                    }
                } finally {
                    $archive.Dispose()
                }
            } catch {
                $issues.Add("读取驱动 ZIP 失败：$($_.Exception.Message)")
            }
        }
    }

    return @($issues)
}

function Get-DriverSourceTreeSha256 {
    param([Parameter(Mandatory)][string]$SourceRoot)
    if (-not (Test-Path -LiteralPath $SourceRoot -PathType Container)) {
        throw "驱动源码目录不存在：$SourceRoot"
    }

    $root = (Resolve-Path -LiteralPath $SourceRoot).Path
    $entries = [System.Collections.Generic.List[string]]::new()
    $utf8Strict = [Text.UTF8Encoding]::new($false, $true)
    $utf8NoBom = [Text.UTF8Encoding]::new($false)
    foreach ($file in Get-ChildItem -LiteralPath $root -Recurse -File -Force) {
        $relative = [IO.Path]::GetRelativePath($root, $file.FullName).Replace('\', '/')
        $segments = $relative.Split('/')
        if ($segments -contains 'target' -or $segments -contains '.git') { continue }
        $bytes = [IO.File]::ReadAllBytes($file.FullName)
        $canonicalBytes = $bytes
        if ($bytes -notcontains 0) {
            try {
                $text = $utf8Strict.GetString($bytes)
                $canonicalText = [regex]::Replace($text, "`r`n?", "`n")
                $canonicalBytes = $utf8NoBom.GetBytes($canonicalText)
            } catch {
                # Non-UTF8 payloads remain byte-exact.
            }
        }
        $hash = (Get-BytesSha256 $canonicalBytes).ToLowerInvariant()
        $entries.Add("$relative`t$hash")
    }
    if ($entries.Count -eq 0) { throw "驱动源码目录为空：$SourceRoot" }

    $sorted = $entries.ToArray()
    [Array]::Sort($sorted, [StringComparer]::Ordinal)
    $content = [Text.UTF8Encoding]::new($false).GetBytes(($sorted -join "`n") + "`n")
    return Get-BytesSha256 $content
}

function Get-DriverProtocolSourceIssues {
    param(
        [Parameter(Mandatory)][string]$SourceRoot,
        [Parameter(Mandatory)]$Lock
    )
    $issues = [System.Collections.Generic.List[string]]::new()
    $ipcContractPath = Join-Path $SourceRoot 'driver-ipc/src/core.rs'
    $handlerPath = Join-Path $SourceRoot 'virtual-display-driver/src/ipc.rs'
    $manifestPath = Join-Path $SourceRoot 'virtual-display-driver/Cargo.toml'
    $buildPath = Join-Path $SourceRoot 'virtual-display-driver/build.rs'
    foreach ($path in @($ipcContractPath, $handlerPath, $manifestPath, $buildPath)) {
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            $issues.Add("驱动源码缺少版本/协议输入：$path")
        }
    }
    if ($issues.Count -gt 0) { return @($issues) }

    $ipcContract = Get-Content -LiteralPath $ipcContractPath -Raw
    $handler = Get-Content -LiteralPath $handlerPath -Raw
    foreach ($capability in @($Lock.requiredProtocolCapabilities)) {
        if ($ipcContract -notmatch "\b$([regex]::Escape($capability))\s*\{") {
            $issues.Add("IPC 协议定义缺少 source.lock 要求的能力：$capability")
        }
        if ($handler -notmatch "RequestCommand::$([regex]::Escape($capability))\b") {
            $issues.Add("驱动 IPC handler 缺少 source.lock 要求的能力：$capability")
        }
    }

    $manifest = Get-Content -LiteralPath $manifestPath -Raw
    $packageMatch = [regex]::Match($manifest, '(?ms)^\[package\].*?^name\s*=\s*"([^"]+)".*?^version\s*=\s*"([^"]+)"')
    $packageName = $packageMatch.Groups[1].Value
    $cargoVersion = $packageMatch.Groups[2].Value
    if ($packageName -cne $Lock.cargoPackage) {
        $issues.Add("Cargo 包名与 source.lock 不一致：$packageName")
    }
    $cargoVersionMatch = [regex]::Match([string]$Lock.driverVersion, '^(\d+\.\d+\.\d+)\.\d+$')
    if (-not $packageMatch.Success -or -not $cargoVersionMatch.Success -or $cargoVersion -cne $cargoVersionMatch.Groups[1].Value) {
        $issues.Add("Cargo crate 三段版本与 source.lock 的驱动资源版本不一致：$cargoVersion / $($Lock.driverVersion)")
    }
    $build = Get-Content -LiteralPath $buildPath -Raw
    foreach ($field in @('FileVersion', 'ProductVersion')) {
        $pattern = '\.set\("' + [regex]::Escape($field) + '",\s*"([^"]+)"\)'
        $versionMatch = [regex]::Match($build, $pattern)
        if (-not $versionMatch.Success -or $versionMatch.Groups[1].Value -cne $Lock.driverVersion) {
            $issues.Add("驱动 build.rs 的 $field 与 source.lock 不一致")
        }
    }
    return @($issues)
}

function Get-ArchiveEntryBytes {
    param([Parameter(Mandatory)]$Entry)
    $stream = $Entry.Open()
    try {
        $memory = [IO.MemoryStream]::new()
        try {
            $stream.CopyTo($memory)
            return ,$memory.ToArray()
        } finally {
            $memory.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

function Get-ArchiveEntrySha256 {
    param([Parameter(Mandatory)]$Entry)
    Get-BytesSha256 (Get-ArchiveEntryBytes $Entry)
}

function Get-BytesSha256 {
    param([Parameter(Mandatory)][byte[]]$Bytes)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        return [Convert]::ToHexString($sha.ComputeHash($Bytes))
    } finally {
        $sha.Dispose()
    }
}
