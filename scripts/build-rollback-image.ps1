param(
    [string]$Revision = '1c10f11',
    [string]$Image = '',
    [switch]$Push
)

$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($Image)) {
    $Image = if ($Push) {
        'ghcr.io/fueledbyredbull/twitch-miner-rust'
    } else {
        'twitch-miner-rust'
    }
}

git cat-file -e "$Revision^{commit}"
if ($LASTEXITCODE -ne 0) {
    throw "Revision does not resolve to a commit: $Revision"
}
$resolved = (git rev-parse --short=12 "$Revision^{commit}").Trim()
$tag = "rollback-$resolved"
$reference = "$Image`:$tag"
$buildTime = (git show -s --format=%cI "$Revision^{commit}").Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($buildTime)) {
    throw "Unable to determine the rollback source timestamp for $resolved"
}
$archive = Join-Path $env:TEMP "twitch-miner-rollback-$PID.tar"
try {
    git archive --format=tar --output=$archive "$Revision^{commit}"
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $archive -PathType Leaf)) {
        throw "Unable to create source archive for $Revision"
    }

    $args = @(
        'buildx', 'build', '--platform', 'linux/arm64', '--tag', $reference,
        '--build-arg', "BUILD_REVISION=$resolved",
        '--build-arg', "BUILD_TIME=$buildTime"
    )
    if ($Push) {
        $args += '--provenance', 'mode=max', '--sbom', 'true', '--push'
    } else {
        $args += '--load'
    }
    $args += $archive
    docker @args
    if ($LASTEXITCODE -ne 0) {
        throw "Rollback image build failed for $resolved"
    }
    Write-Output "rollback-image-built: $reference"
    if ($Push) {
        Write-Output "Record the manifest digest from: docker buildx imagetools inspect $reference"
    }
} finally {
    if (Test-Path -LiteralPath $archive -PathType Leaf) {
        Remove-Item -LiteralPath $archive -Force
    }
}
