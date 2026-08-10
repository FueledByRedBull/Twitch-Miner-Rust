$ErrorActionPreference = 'Stop'

$revision = (git rev-parse --short=12 HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($revision)) {
    throw 'Unable to determine the source revision.'
}
$sourceDateEpoch = (git show -s --format=%ct HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $sourceDateEpoch -notmatch '^\d+$') {
    throw 'Unable to determine SOURCE_DATE_EPOCH.'
}

$metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) {
    throw 'Cargo metadata validation failed.'
}
if (-not (Test-Path -LiteralPath 'Cargo.lock' -PathType Leaf)) {
    throw 'Cargo.lock is missing.'
}

$oldRevision = $env:BUILD_REVISION
$oldSourceDateEpoch = $env:SOURCE_DATE_EPOCH
$oldCargoIncremental = $env:CARGO_INCREMENTAL
$oldRustFlags = $env:RUSTFLAGS
$repositoryRoot = (Resolve-Path -LiteralPath '.').Path
$isWindowsHost = $env:OS -eq 'Windows_NT' -or
    [System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT
$targetRoot = [System.IO.Path]::GetFullPath((Join-Path $repositoryRoot 'target'))
$buildRoots = @(
    [System.IO.Path]::GetFullPath((Join-Path $targetRoot "reproducible-$PID-a")),
    [System.IO.Path]::GetFullPath((Join-Path $targetRoot "reproducible-$PID-b"))
)
foreach ($buildRoot in $buildRoots) {
    if (-not $buildRoot.StartsWith($targetRoot + [System.IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Unsafe reproducibility target path: $buildRoot"
    }
}

try {
    $env:BUILD_REVISION = $revision
    $env:SOURCE_DATE_EPOCH = $sourceDateEpoch
    $env:CARGO_INCREMENTAL = '0'
    $remapRoot = $repositoryRoot.Replace('\', '/')
    $baseRustFlags = @($oldRustFlags, "--remap-path-prefix=$remapRoot=.")
    if ($isWindowsHost) {
        # MSVC otherwise embeds the absolute PDB path and volatile PE metadata.
        $baseRustFlags += @('-Clink-arg=/DEBUG:NONE', '-Clink-arg=/Brepro')
    }

    $hashes = foreach ($buildRoot in $buildRoots) {
        $remapBuildRoot = $buildRoot.Replace('\', '/')
        $env:RUSTFLAGS = (($baseRustFlags + @("--remap-path-prefix=$remapBuildRoot=./target")) |
                Where-Object { -not [string]::IsNullOrWhiteSpace($_) }) -join ' '
        cargo build --locked --release -p tm-app --target-dir $buildRoot
        if ($LASTEXITCODE -ne 0) {
            throw "Release build failed in $buildRoot."
        }
        $candidate = Join-Path $buildRoot $(if ($env:OS -eq 'Windows_NT') {
                'release/tm-app.exe'
            } else {
                'release/tm-app'
            })
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            throw "Release binary was not produced: $candidate"
        }
        (Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash
    }
    if ($hashes[0] -ne $hashes[1]) {
        throw "Release builds are not reproducible: $($hashes[0]) != $($hashes[1])"
    }
} catch {
    foreach ($buildRoot in $buildRoots) {
        if (Test-Path -LiteralPath $buildRoot) {
            Remove-Item -LiteralPath $buildRoot -Recurse -Force
        }
    }
    throw
} finally {
    $env:BUILD_REVISION = $oldRevision
    $env:SOURCE_DATE_EPOCH = $oldSourceDateEpoch
    $env:CARGO_INCREMENTAL = $oldCargoIncremental
    $env:RUSTFLAGS = $oldRustFlags
}

try {
    $binaryName = if ($isWindowsHost) { 'tm-app.exe' } else { 'tm-app' }
    $binary = Join-Path $buildRoots[0] "release/$binaryName"
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw "Release binary was not produced: $binary"
    }
    $binaryPath = (Resolve-Path -LiteralPath $binary).Path
    $version = (& $binaryPath --version 2>&1) -join "`n"
    if ($LASTEXITCODE -ne 0 -or $version -notmatch [regex]::Escape($revision)) {
        throw 'Release binary metadata does not identify the source revision.'
    }

    Write-Output "build-integrity-ok: revision=$revision packages=$($metadata.packages.Count) sha256=$($hashes[0])"
} finally {
    foreach ($buildRoot in $buildRoots) {
        if (Test-Path -LiteralPath $buildRoot) {
            Remove-Item -LiteralPath $buildRoot -Recurse -Force
        }
    }
}
