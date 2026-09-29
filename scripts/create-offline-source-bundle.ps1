param(
    [string]$Revision = 'HEAD',
    [string]$OutputPath = '',
    [switch]$ValidateOnly
)

$ErrorActionPreference = 'Stop'
$resolved = (git rev-parse --verify "$Revision^{commit}").Trim()
if ($LASTEXITCODE -ne 0 -or $resolved -notmatch '^[0-9a-f]{40}$') {
    throw "Revision does not resolve to a commit: $Revision"
}
if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $OutputPath = "./target/twitch-miner-source-$($resolved.Substring(0, 12)).tar.gz"
}

$repositoryRoot = (Resolve-Path -LiteralPath '.').Path
$targetRoot = [System.IO.Path]::GetFullPath((Join-Path $repositoryRoot 'target'))
$outputCandidate = if ([System.IO.Path]::IsPathRooted($OutputPath)) {
    $OutputPath
} else {
    Join-Path $repositoryRoot $OutputPath
}
$outputFullPath = [System.IO.Path]::GetFullPath($outputCandidate)
if ($ValidateOnly -and
    -not $outputFullPath.StartsWith(
        $targetRoot + [System.IO.Path]::DirectorySeparatorChar,
        [StringComparison]::OrdinalIgnoreCase
    )) {
    throw 'ValidateOnly output must remain under target/.'
}
$bundleToken = "$PID-$([Guid]::NewGuid().ToString('N'))"
$stageRoot = [System.IO.Path]::GetFullPath((Join-Path $targetRoot "offline-bundle-$bundleToken"))
$sourceArchive = [System.IO.Path]::GetFullPath((Join-Path $targetRoot "offline-source-$bundleToken.tar"))
foreach ($path in @($stageRoot, $sourceArchive)) {
    if (-not $path.StartsWith($targetRoot + [System.IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Unsafe offline bundle staging path: $path"
    }
}
if ($outputFullPath.Equals($sourceArchive, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'OutputPath must not be the temporary source archive path.'
}
if ($outputFullPath.Equals($stageRoot, [StringComparison]::OrdinalIgnoreCase) -or
    $outputFullPath.StartsWith(
        $stageRoot + [System.IO.Path]::DirectorySeparatorChar,
        [StringComparison]::OrdinalIgnoreCase
    )) {
    throw 'OutputPath must not be inside the temporary bundle staging directory.'
}
$archiveOutputPath = if ($ValidateOnly) {
    Join-Path (Split-Path -Parent $outputFullPath) ".offline-validation-$bundleToken.tar.gz"
} else {
    $outputFullPath
}

try {
    New-Item -ItemType Directory -Path $targetRoot -Force | Out-Null
    if ((Test-Path -LiteralPath $stageRoot -PathType Any -ErrorAction SilentlyContinue) -or
        (Test-Path -LiteralPath $sourceArchive -PathType Any -ErrorAction SilentlyContinue)) {
        throw 'Offline bundle staging paths unexpectedly already exist.'
    }
    New-Item -ItemType Directory -Path $stageRoot | Out-Null
    git archive --format=tar --output=$sourceArchive $resolved
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to archive revision $resolved."
    }
    tar -xf $sourceArchive -C $stageRoot
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to extract source archive.'
    }

    Push-Location $stageRoot
    try {
        try {
            $ErrorActionPreference = 'Continue'
            cargo vendor --locked --versioned-dirs --sync fuzz/Cargo.toml vendor *> $null
            $vendorExitCode = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = 'Stop'
        }
        if ($vendorExitCode -ne 0) {
            throw 'Unable to vendor locked Cargo sources.'
        }
        New-Item -ItemType Directory -Path '.cargo' -Force | Out-Null
        @'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"

[net]
offline = true
'@ | Set-Content -LiteralPath '.cargo/config.toml' -Encoding utf8
        $resolved | Set-Content -LiteralPath 'SOURCE_REVISION' -Encoding ascii
        try {
            $ErrorActionPreference = 'Continue'
            cargo metadata --locked --offline --format-version 1 *> $null
            $metadataExitCode = $LASTEXITCODE
            cargo metadata --manifest-path fuzz/Cargo.toml --locked --offline --format-version 1 *> $null
            $fuzzMetadataExitCode = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = 'Stop'
        }
        if ($metadataExitCode -ne 0 -or $fuzzMetadataExitCode -ne 0) {
            throw 'Vendored source tree failed locked offline metadata validation for the root or fuzz workspace.'
        }
    } finally {
        Pop-Location
    }

    $outputDirectory = Split-Path -Parent $archiveOutputPath
    New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null
    tar -czf $archiveOutputPath -C $stageRoot .
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $archiveOutputPath -PathType Leaf)) {
        throw "Unable to create offline source bundle: $archiveOutputPath"
    }
    $hash = (Get-FileHash -LiteralPath $archiveOutputPath -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $([System.IO.Path]::GetFileName($archiveOutputPath))" |
        Set-Content -LiteralPath "$archiveOutputPath.sha256" -Encoding ascii
    if ($ValidateOnly) {
        Remove-Item -LiteralPath $archiveOutputPath -Force
        Remove-Item -LiteralPath "$archiveOutputPath.sha256" -Force
        Write-Output "offline-source-bundle-validation-ok: revision=$resolved sha256=$hash"
    } else {
        Write-Output "offline-source-bundle: revision=$resolved sha256=$hash path=$outputFullPath"
    }
} finally {
    foreach ($path in @($stageRoot, $sourceArchive)) {
        if (Test-Path -LiteralPath $path) {
            Remove-Item -LiteralPath $path -Recurse -Force
        }
    }
}
