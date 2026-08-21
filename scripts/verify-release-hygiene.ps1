$ErrorActionPreference = 'Stop'

$workflowFiles = Get-ChildItem .github/workflows -Filter '*.yml' -File
foreach ($workflow in $workflowFiles) {
    $content = Get-Content -Raw $workflow.FullName
    foreach ($match in [regex]::Matches($content, '(?m)^\s*uses:\s*([^\s#]+)')) {
        $reference = $match.Groups[1].Value
        if ($reference.StartsWith('./')) {
            continue
        }
        if ($reference -notmatch '@[0-9a-f]{40}$') {
            throw "Unpinned GitHub Action in $($workflow.FullName): $reference"
        }
    }
}

foreach ($compose in @('docker-compose.yml', 'deploy/docker-compose.bind-mount.yml', 'deploy/docker-compose.volume.yml')) {
    $content = Get-Content -Raw $compose
    if ($content -match ':latest') {
        throw "Mutable image tag found in $compose"
    }
    if ($compose -ne 'docker-compose.yml' -and $content -notmatch 'TWITCH_MINER_IMAGE') {
        throw "Digest image variable missing from $compose"
    }
    if ($content -match '(?ms)healthcheck:\s*\r?\n\s+disable:\s*true') {
        throw "Health check is disabled in $compose"
    }
    if ($content -notmatch '(?ms)healthcheck:\s*\r?\n\s+test:\s*\[\s*"CMD"\s*,\s*"/twitch-miner"\s*,\s*"--health"\s*\]') {
        throw "Explicit Twitch miner health check missing from $compose"
    }
}

$dockerfile = Get-Content -Raw Dockerfile
if ($dockerfile -notmatch '(?m)^# syntax=docker/dockerfile:[^@\s]+@sha256:[0-9a-f]{64}\r?$') {
    throw 'Dockerfile frontend must be pinned by immutable digest.'
}
if ($dockerfile -notmatch 'HEALTHCHECK') {
    throw 'Dockerfile has no health check.'
}
if ($dockerfile -notmatch '(?m)^\s*FROM\s+rust:[^\s@]+@sha256:[0-9a-f]{64}\b') {
    throw 'Dockerfile builder image must be pinned by immutable digest.'
}
# Immutability invariants only. The specific build tool (currently cargo-chef) is an
# implementation choice and is deliberately not asserted here.
if ($dockerfile -notmatch 'snapshot\.debian\.org/archive/debian/\d{8}T\d{6}Z' -or
    $dockerfile -notmatch 'musl-tools=\d+\.\d+\.\d+-\d+') {
    throw 'Dockerfile system package inputs must be pinned to an immutable snapshot and version.'
}

$ciWorkflow = Get-Content -Raw .github/workflows/ci.yml
if ($ciWorkflow -match 'gitleaks/gitleaks-action' -or
    $ciWorkflow -notmatch 'GITLEAKS_VERSION:\s*\d+\.\d+\.\d+' -or
    $ciWorkflow -notmatch 'GITLEAKS_ARCHIVE_SHA256:\s*[0-9a-f]{64}' -or
    $ciWorkflow -notmatch 'sha256sum --check') {
    throw 'CI secret scanning must use a checksum-verified native Gitleaks release.'
}

if (-not (Test-Path -LiteralPath 'fuzz/Cargo.lock' -PathType Leaf) -or
    -not (Test-Path -LiteralPath 'fuzz/Cargo.toml' -PathType Leaf)) {
    throw 'The isolated fuzz workspace must retain its own manifest and lockfile.'
}

$sentinelPath = Join-Path (Resolve-Path -LiteralPath '.').Path "offline-bundle-validate-only-sentinel-$PID.txt"
$sentinelContent = 'must-not-be-overwritten'
try {
    Set-Content -LiteralPath $sentinelPath -Value $sentinelContent -NoNewline
    $rejected = $false
    try {
        & "$PSScriptRoot/create-offline-source-bundle.ps1" `
            -Revision HEAD `
            -OutputPath $sentinelPath `
            -ValidateOnly
    } catch {
        if ($_.Exception.Message -notmatch 'ValidateOnly output must remain under target/') {
            throw
        }
        $rejected = $true
    }
    if (-not $rejected) {
        throw 'ValidateOnly accepted an output path outside target/.'
    }
    if ((Get-Content -LiteralPath $sentinelPath -Raw) -ne $sentinelContent) {
        throw 'ValidateOnly modified an output path before rejecting it.'
    }
} finally {
    if (Test-Path -LiteralPath $sentinelPath) {
        Remove-Item -LiteralPath $sentinelPath -Force
    }
}

$candidateDigest = 'a' * 64
$rollbackDigest = 'b' * 64
& "$PSScriptRoot/deploy-with-rollback.ps1" `
    -CandidateImage "ghcr.io/example/twitch-miner@sha256:$candidateDigest" `
    -RollbackImage "ghcr.io/example/twitch-miner@sha256:$rollbackDigest" `
    -CandidateRevision ('c' * 40) `
    -RollbackRevision ('d' * 40) `
    -ValidateOnly
if (-not $?) {
    throw 'Guarded deployment helper validation failed.'
}

git check-ignore -q FINISHING_TOUCHES.md
if ($LASTEXITCODE -ne 0) {
    throw 'FINISHING_TOUCHES.md must remain ignored.'
}

Write-Output 'release-hygiene-ok'
