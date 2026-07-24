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
$stageRoot = [System.IO.Path]::GetFullPath((Join-Path $targetRoot "offline-bundle-$PID"))
$sourceArchive = [System.IO.Path]::GetFullPath((Join-Path $targetRoot "offline-source-$PID.tar"))
foreach ($path in @($stageRoot, $sourceArchive)) {
    if (-not $path.StartsWith($targetRoot + [System.IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Unsafe offline bundle staging path: $path"
    }
}

try {
    New-Item -ItemType Directory -Path $stageRoot -Force | Out-Null
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
            cargo vendor --locked --versioned-dirs vendor *> $null
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
        } finally {
            $ErrorActionPreference = 'Stop'
        }
        if ($metadataExitCode -ne 0) {
            throw 'Vendored source tree failed locked offline metadata validation.'
        }
    } finally {
        Pop-Location
    }

    $outputDirectory = Split-Path -Parent $outputFullPath
    New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null
    tar -czf $outputFullPath -C $stageRoot .
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $outputFullPath -PathType Leaf)) {
        throw "Unable to create offline source bundle: $outputFullPath"
    }
    $hash = (Get-FileHash -LiteralPath $outputFullPath -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $([System.IO.Path]::GetFileName($outputFullPath))" |
        Set-Content -LiteralPath "$outputFullPath.sha256" -Encoding ascii
    if ($ValidateOnly) {
        Remove-Item -LiteralPath $outputFullPath -Force
        Remove-Item -LiteralPath "$outputFullPath.sha256" -Force
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
