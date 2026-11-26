function Invoke-AcceptanceNative {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments = @(),
        [Parameter(Mandatory = $true)][string]$LogPath
    )

    if (-not [IO.File]::Exists($FilePath)) { throw "Native executable does not exist: $FilePath" }
    [IO.File]::WriteAllText($LogPath, '')
    $previousErrorAction = $ErrorActionPreference
    try {
        # Windows PowerShell 5.1 wraps redirected native stderr in ErrorRecord.
        # A compiler progress line must not terminate an otherwise successful run.
        $ErrorActionPreference = 'Continue'
        & $FilePath @Arguments 2>&1 | ForEach-Object {
            $line = $_.ToString()
            [IO.File]::AppendAllText($LogPath, $line + [Environment]::NewLine)
            Write-Host $line
        }
        $nativeExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorAction
    }
    return [int]$nativeExitCode
}

function Remove-AcceptanceFixture {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Parent
    )

    $fullPath = [IO.Path]::GetFullPath($Path)
    $fullParent = [IO.Path]::GetFullPath($Parent).TrimEnd('\', '/')
    if (-not [string]::Equals([IO.Path]::GetDirectoryName($fullPath), $fullParent, [StringComparison]::OrdinalIgnoreCase) -or
        -not [IO.Path]::GetFileName($fullPath).StartsWith('fixtures-', [StringComparison]::Ordinal)) {
        throw "Refusing to clean a path outside this run's fixture root: $fullPath"
    }

    function Remove-EntryWithoutFollowingLinks([string]$EntryPath) {
        $attributes = [IO.File]::GetAttributes($EntryPath)
        $isDirectory = ($attributes -band [IO.FileAttributes]::Directory) -ne 0
        if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            if ($isDirectory) { [IO.Directory]::Delete($EntryPath, $false) }
            else { [IO.File]::Delete($EntryPath) }
        } elseif ($isDirectory) {
            foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($EntryPath)) {
                Remove-EntryWithoutFollowingLinks $entry
            }
            [IO.Directory]::Delete($EntryPath, $false)
        } else {
            [IO.File]::Delete($EntryPath)
        }
    }

    # GetAttributes also sees broken links, unlike Exists(). The root is created
    # before this helper is registered, so a missing root is already clean.
    try { $null = [IO.File]::GetAttributes($fullPath) }
    catch [IO.FileNotFoundException] { return }
    catch [IO.DirectoryNotFoundException] { return }
    Remove-EntryWithoutFollowingLinks $fullPath
}
