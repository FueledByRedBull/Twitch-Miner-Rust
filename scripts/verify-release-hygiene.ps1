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

git check-ignore -q FINISHING_TOUCHES.md
if ($LASTEXITCODE -ne 0) {
    throw 'FINISHING_TOUCHES.md must remain ignored.'
}

Write-Output 'release-hygiene-ok'
