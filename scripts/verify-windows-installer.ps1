[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$MsiPath,

    [Parameter(Mandatory)]
    [string]$BuiltBinaryPath,

    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$ExpectedVersion,

    [Parameter(Mandatory)]
    [ValidatePattern('^[0-9a-f]{40}$')]
    [string]$ExpectedRevision,

    [string]$LogDirectory = (Join-Path $env:TEMP 'twitch-miner-msi-verification')
)

$ErrorActionPreference = 'Stop'

function Invoke-Msi([string[]]$Arguments, [string]$LogPath) {
    $quotedLogPath = '"' + $LogPath + '"'
    $process = Start-Process msiexec.exe -Wait -PassThru -WindowStyle Hidden -ArgumentList @(
        $Arguments + @('/l*v', $quotedLogPath)
    )
    return $process.ExitCode
}

if (-not (Test-Path -LiteralPath $MsiPath -PathType Leaf)) {
    throw "MSI package was not found: $MsiPath"
}
if (-not (Test-Path -LiteralPath $BuiltBinaryPath -PathType Leaf)) {
    throw "Release executable was not found: $BuiltBinaryPath"
}

$resolvedMsi = (Resolve-Path -LiteralPath $MsiPath).Path
$resolvedBinary = (Resolve-Path -LiteralPath $BuiltBinaryPath).Path
$logRoot = [System.IO.Path]::GetFullPath($LogDirectory)
New-Item -ItemType Directory -Path $logRoot -Force | Out-Null
$installLog = Join-Path $logRoot 'install.log'
$uninstallLog = Join-Path $logRoot 'uninstall.log'
$programFiles = [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
if ([string]::IsNullOrWhiteSpace($programFiles)) {
    throw 'Unable to resolve the 64-bit Program Files directory.'
}
$installRoot = Join-Path $programFiles 'TwitchMiner'
$installedExe = Join-Path $installRoot 'twitch-miner.exe'

if (Test-Path -LiteralPath $installRoot) {
    throw "Refusing MSI verification because the expected install directory already exists: $installRoot"
}

$expectedHash = (Get-FileHash -LiteralPath $resolvedBinary -Algorithm SHA256).Hash
$testFailure = $null
$cleanupFailure = $null
try {
    $installCode = Invoke-Msi @('/i', ('"' + $resolvedMsi + '"'), '/qn', '/norestart') $installLog
    if ($installCode -ne 0) {
        throw "MSI quiet install failed with exit code $installCode."
    }

    if (-not (Test-Path -LiteralPath $installedExe -PathType Leaf)) {
        throw "MSI install did not place the executable at $installedExe."
    }
    $installedHash = (Get-FileHash -LiteralPath $installedExe -Algorithm SHA256).Hash
    if ($installedHash -ine $expectedHash) {
        throw 'Installed executable bytes do not match the release executable.'
    }
    $version = (& $installedExe --version 2>&1) -join "`n"
    if ($LASTEXITCODE -ne 0 -or
        $version -notmatch [regex]::Escape($ExpectedVersion) -or
        $version -notmatch [regex]::Escape($ExpectedRevision)) {
        throw "Installed executable version smoke test failed: $version"
    }
    foreach ($runtimePath in @(
            'config.json',
            'cookies',
            'runtime-status.json',
            'log'
        )) {
        if (Test-Path -LiteralPath (Join-Path $installRoot $runtimePath)) {
            throw "MSI install created active runtime state under Program Files: $runtimePath"
        }
    }
} catch {
    $testFailure = $_
} finally {
    $uninstallCode = Invoke-Msi @('/x', ('"' + $resolvedMsi + '"'), '/qn', '/norestart') $uninstallLog
    if ($uninstallCode -ne 0) {
        $cleanupFailure = "MSI quiet uninstall failed with exit code $uninstallCode."
    }
}

if ($null -ne $testFailure -and $null -ne $cleanupFailure) {
    throw "$($testFailure.Exception.Message) $cleanupFailure"
}
if ($null -ne $testFailure) {
    throw $testFailure
}
if ($null -ne $cleanupFailure) {
    throw $cleanupFailure
}
if (Test-Path -LiteralPath $installedExe -PathType Leaf) {
    throw "MSI uninstall left the executable behind: $installedExe"
}

Write-Output "windows-msi-install-uninstall-ok: version=$ExpectedVersion revision=$ExpectedRevision"
