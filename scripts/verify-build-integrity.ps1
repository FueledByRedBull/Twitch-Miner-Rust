$ErrorActionPreference = 'Stop'

$revision = (git rev-parse --short=12 HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($revision)) {
    throw 'Unable to determine the source revision.'
}
$buildTime = (git show -s --format=%cI HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($buildTime)) {
    throw 'Unable to determine the deterministic source timestamp.'
}

$metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) {
    throw 'Cargo metadata validation failed.'
}
if (-not (Test-Path -LiteralPath 'Cargo.lock' -PathType Leaf)) {
    throw 'Cargo.lock is missing.'
}

$oldRevision = $env:BUILD_REVISION
$oldBuildTime = $env:BUILD_TIME
try {
    $env:BUILD_REVISION = $revision
    $env:BUILD_TIME = $buildTime
    cargo build --locked --release -p tm-app
    if ($LASTEXITCODE -ne 0) {
        throw 'Release build failed.'
    }
} finally {
    $env:BUILD_REVISION = $oldRevision
    $env:BUILD_TIME = $oldBuildTime
}

$isWindowsHost = $env:OS -eq 'Windows_NT' -or
    [System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT
$binaryName = if ($isWindowsHost) { 'tm-app.exe' } else { 'tm-app' }
$binary = Join-Path 'target/release' $binaryName
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
    throw "Release binary was not produced: $binary"
}
$binaryPath = (Resolve-Path -LiteralPath $binary).Path
$version = (& $binaryPath --version 2>&1) -join "`n"
if ($LASTEXITCODE -ne 0 -or $version -notmatch [regex]::Escape($revision) -or $version -notmatch 'built ') {
    throw 'Release binary metadata does not identify the source revision and build timestamp.'
}

Write-Output "build-integrity-ok: revision=$revision packages=$($metadata.packages.Count)"
