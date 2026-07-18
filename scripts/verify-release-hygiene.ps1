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
if ($dockerfile -notmatch 'HEALTHCHECK') {
    throw 'Dockerfile has no health check.'
}
if ($dockerfile -notmatch '(?m)^\s*FROM\s+rust:[^\s@]+@sha256:[0-9a-f]{64}\s+AS\s+chef') {
    throw 'Dockerfile builder image must be pinned by immutable digest.'
}
if ($dockerfile -notmatch 'cargo install cargo-chef --version \d+\.\d+\.\d+ --locked') {
    throw 'Dockerfile cargo-chef install must use an explicit locked version.'
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
