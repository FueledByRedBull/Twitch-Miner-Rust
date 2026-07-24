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

$multiarchWorkflow = Get-Content -Raw .github/workflows/multiarch-build.yml
if ($multiarchWorkflow -notmatch '(?m)^  release-manifest:\s*$' -or
    $multiarchWorkflow -notmatch 'sha-\$\{\{ github\.sha \}\}' -or
    $multiarchWorkflow -notmatch 'verify-published-manifest\.ps1' -or
    $multiarchWorkflow -notmatch 'imagetools create --tag \$release') {
    throw 'Signed releases must verify and promote the existing commit-SHA manifest.'
}
$releaseJob = [regex]::Match(
    $multiarchWorkflow,
    '(?ms)^  release-manifest:\s*$(.*)\z'
).Groups[1].Value
if (-not $releaseJob -or $releaseJob -match 'docker/build-push-action') {
    throw 'Signed release promotion must not rebuild the accepted image.'
}
if ($releaseJob -notmatch '-ExpectedDigest \$digest') {
    throw 'Signed release promotion must require exact digest equality.'
}

foreach ($compose in @('docker-compose.yml', 'deploy/docker-compose.rpi.yml', 'deploy/docker-compose.volume.yml')) {
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
if ($dockerfile -notmatch '(?m)^# syntax=docker/dockerfile:[^@\s]+@sha256:[0-9a-f]{64}$') {
    throw 'Dockerfile frontend must be pinned by immutable digest.'
}
if ($dockerfile -notmatch 'HEALTHCHECK') {
    throw 'Dockerfile has no health check.'
}
if ($dockerfile -notmatch '(?m)^\s*FROM\s+rust:[^\s@]+@sha256:[0-9a-f]{64}\s+AS\s+chef') {
    throw 'Dockerfile builder image must be pinned by immutable digest.'
}
if ($dockerfile -notmatch 'cargo install cargo-chef --version \d+\.\d+\.\d+ --locked') {
    throw 'Dockerfile cargo-chef install must use an explicit locked version.'
}
if ($dockerfile -notmatch 'snapshot\.debian\.org/archive/debian/\d{8}T\d{6}Z' -or
    $dockerfile -notmatch 'musl-tools=\d+\.\d+\.\d+-\d+' -or
    $dockerfile -notmatch 'cargo chef cook --locked') {
    throw 'Dockerfile system and Cargo Chef inputs must be immutable and locked.'
}

$ciWorkflow = Get-Content -Raw .github/workflows/ci.yml
if ($ciWorkflow -notmatch 'cargo-deny@\d+\.\d+\.\d+' -or
    $ciWorkflow -notmatch 'cargo-llvm-cov@\d+\.\d+\.\d+') {
    throw 'CI analysis executables must use explicit versions.'
}

$deepQualityWorkflow = Get-Content -Raw .github/workflows/deep-quality.yml
if ($deepQualityWorkflow -notmatch 'nightly-\d{4}-\d{2}-\d{2}' -or
    $deepQualityWorkflow -notmatch 'cargo-fuzz --version \d+\.\d+\.\d+ --locked' -or
    $deepQualityWorkflow -notmatch 'cargo-mutants --version \d+\.\d+\.\d+ --locked' -or
    $deepQualityWorkflow -notmatch 'cargo-llvm-cov@\d+\.\d+\.\d+' -or
    $deepQualityWorkflow -notmatch '--branch' -or
    $deepQualityWorkflow -notmatch 'verify-branch-coverage\.ps1' -or
    $deepQualityWorkflow -notmatch 'compare-replay-baseline\.ps1') {
    throw 'Deep quality tools, nightly, coverage, and replay comparison must be explicitly pinned.'
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

$releaseProcess = Get-Content -Raw docs/release-process.md
if ($releaseProcess -match '(?m)^docker exec twitch-miner\b') {
    throw 'Release commands must resolve the Compose service instead of assuming a container name.'
}
if ($releaseProcess -notmatch '(?s)-v "\$DATA_DIR:/data:ro".*--data-dir /data --canary') {
    throw 'Release process must run the digest-pinned canary with a read-only data mount.'
}

$deploymentHelper = Get-Content -Raw "$PSScriptRoot/deploy-with-rollback.ps1"
if ($deploymentHelper -match '& docker exec\b') {
    throw 'Guarded deployment helper must use Compose service execution.'
}
if ($deploymentHelper -notmatch 'Test-ImageConfig \$CandidateImage \$true' -or
    $deploymentHelper -notmatch 'Test-ImageConfig \$RollbackImage \$false') {
    throw 'Guarded deployment helper must preserve candidate JSON and rollback-compatible config checks.'
}
if ($deploymentHelper -notmatch 'started_at_unix -ge \$containerStarted' -or
    $deploymentHelper -notmatch 'active_subscriptions -eq \$eventSub\.planned_subscriptions' -or
    $deploymentHelper -notmatch '\$acknowledgedTopics -eq \$pubSub\.total_topics') {
    throw 'Guarded deployment must reject stale status and incomplete transport recovery.'
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
