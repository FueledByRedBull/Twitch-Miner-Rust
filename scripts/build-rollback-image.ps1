param(
    [string]$Revision = '1c10f11',
    [string]$Image = '',
    [ValidateSet('linux/amd64', 'linux/arm64')]
    [string]$Platform = 'linux/arm64',
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
$resolved = (git rev-parse "$Revision^{commit}").Trim()
if ($LASTEXITCODE -ne 0 -or $resolved -notmatch '^[0-9a-f]{40}$') {
    throw "Unable to determine the full rollback revision for $Revision"
}
$tag = "rollback-$resolved"
$reference = "$Image`:$tag"
$sourceDateEpoch = (git show -s --format=%ct "$Revision^{commit}").Trim()
if ($LASTEXITCODE -ne 0 -or $sourceDateEpoch -notmatch '^\d+$') {
    throw "Unable to determine rollback SOURCE_DATE_EPOCH for $resolved"
}
$archiveToken = "$PID-$([Guid]::NewGuid().ToString('N'))"
$archive = Join-Path $env:TEMP "twitch-miner-rollback-$archiveToken.tar"
try {
    if (Test-Path -LiteralPath $archive -PathType Any -ErrorAction SilentlyContinue) {
        throw "Rollback source archive path unexpectedly already exists: $archive"
    }
    git archive --format=tar --output=$archive "$Revision^{commit}"
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $archive -PathType Leaf)) {
        throw "Unable to create source archive for $Revision"
    }

    $args = @(
        'buildx', 'build', '--platform', $Platform, '--tag', $reference,
        '--build-arg', "BUILD_REVISION=$resolved",
        '--build-arg', "SOURCE_DATE_EPOCH=$sourceDateEpoch"
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
