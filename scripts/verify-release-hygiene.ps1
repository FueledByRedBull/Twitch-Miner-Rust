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

$multiarchWorkflow = Get-Content -Raw .github/workflows/multiarch-build.yml
if ($multiarchWorkflow -notmatch 'candidate-\$\{\{ github\.sha \}\}-\$\{\{ github\.run_id \}\}' -or
    $multiarchWorkflow -notmatch 'Publish consumer aliases after verification' -or
    $multiarchWorkflow -notmatch 'resolve-platform-digest\.ps1' -or
    $multiarchWorkflow -notmatch 'steps\.runtime\.outputs\.digest') {
    throw 'Multiarch publication must verify a candidate-only manifest before publishing aliases.'
}
if ($multiarchWorkflow -match 'tags:\s*\r?\n\s*-\s*v\*') {
    throw 'Stable release tags must be promoted by the protected manual workflow.'
}
if ($multiarchWorkflow -match 'type=raw,value=latest|type=ref,event=branch|type=ref,event=tag') {
    throw 'Multiarch candidates must expose only the immutable SHA discovery tag before acceptance.'
}
$localBuildScript = Get-Content -Raw scripts/build-multiarch.ps1
if ($localBuildScript -notmatch 'candidate-\$buildRevision' -or
    $localBuildScript -notmatch 'candidateTagPattern' -or
    $localBuildScript -notmatch "Stable image tags are published only by the protected Promote Release workflow") {
    throw 'Local multiarch pushes must use revision-scoped candidate tags and reject stable aliases.'
}

$promotionWorkflow = Get-Content -Raw .github/workflows/promote-release.yml
if ($promotionWorkflow -notmatch 'workflow_dispatch' -or
    $promotionWorkflow -notmatch 'environment:\s*release' -or
    $promotionWorkflow -notmatch 'evidence_json' -or
    $promotionWorkflow -notmatch 'verify-published-manifest\.ps1' -or
    $promotionWorkflow -notmatch 'VerifyAttestations') {
    throw 'Stable promotion workflow must require external evidence and the protected release environment.'
}
$windowsWorkflow = Get-Content -Raw .github/workflows/windows-release.yml
$ciWindowsWorkflow = Get-Content -Raw .github/workflows/ci.yml
$installer = Get-Content -Raw installer/Product.wxs
$windowsBuildScript = Get-Content -Raw scripts/build-windows-release.ps1
if ($installer -match 'Guid="\*"' -or
    $installer -notmatch 'ProgramFiles64Folder' -or
    $installer -notmatch 'config\.example\.json') {
    throw 'WiX installer must use a stable component identity and keep runtime data out of Program Files.'
}
foreach ($workflowContent in @($windowsWorkflow, $ciWindowsWorkflow)) {
    if ($workflowContent -notmatch 'dotnet tool install wix .*--version 4\.0\.6 .*--allow-roll-forward' -or
        $workflowContent -notmatch 'DOTNET_ROLL_FORWARD:\s*Major') {
        throw 'Windows WiX jobs must pin 4.0.6 and allow roll-forward on the Windows 2025 runner.'
    }
    if ($workflowContent -match 'twitch-miner-\*\.(?:zip|msi)' -or
        $workflowContent -notmatch 'steps\.package\.outputs\.base' -or
        $workflowContent -notmatch 'Start-Process msiexec\.exe.*-WindowStyle Hidden' -or
        $workflowContent -notmatch '--version' -or
        $workflowContent -notmatch 'verify-windows-installer\.ps1') {
        throw 'Windows packaging jobs must select current version outputs, hide MSI operations, and smoke-test the installed and portable executables.'
    }
}
if ($windowsBuildScript -notmatch 'LastWriteTimeUtc' -or
    $windowsBuildScript -notmatch 'SOURCE_DATE_EPOCH' -or
    $windowsBuildScript -notmatch 'msi validate') {
    throw 'Windows packaging must normalize portable archive timestamps and validate the MSI database.'
}

if (-not (Test-Path -LiteralPath 'fuzz/Cargo.lock' -PathType Leaf) -or
    -not (Test-Path -LiteralPath 'fuzz/Cargo.toml' -PathType Leaf)) {
    throw 'The isolated fuzz workspace must retain its own manifest and lockfile.'
}
$offlineBundleScript = Get-Content -Raw scripts/create-offline-source-bundle.ps1
if ($offlineBundleScript -notmatch 'cargo vendor --locked --versioned-dirs --sync fuzz/Cargo\.toml vendor' -or
    $offlineBundleScript -notmatch '--manifest-path fuzz/Cargo\.toml --locked --offline' -or
    $offlineBundleScript -notmatch '\[Guid\]::NewGuid' -or
    $offlineBundleScript -notmatch 'OutputPath must not be inside the temporary bundle staging directory') {
    throw 'Offline source bundles must vendor and validate the fuzz workspace and isolate staging from stale or self-including output paths.'
}
$deploymentScript = Get-Content -Raw scripts/deploy-with-rollback.ps1
if ($deploymentScript -notmatch 'ConvertTo-ComposeLiteral' -or
    $deploymentScript -notmatch 'Runtime data directory paths cannot contain newline characters') {
    throw 'Deployment state must preserve literal Compose data paths and reject newline values.'
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

& "$PSScriptRoot/verify-release-evidence.ps1" `
    -EvidencePath (Join-Path $env:TEMP 'release-evidence-validation.json') `
    -ExpectedRevision ('c' * 40) `
    -ExpectedDigest "sha256:$candidateDigest" `
    -ValidateOnly
if (-not $?) {
    throw 'Release evidence verifier validation failed.'
}

& "$PSScriptRoot/build-windows-release.ps1" -ValidateOnly
if (-not $?) {
    throw 'Windows release packaging validation failed.'
}

git check-ignore -q FINISHING_TOUCHES.md
if ($LASTEXITCODE -ne 0) {
    throw 'FINISHING_TOUCHES.md must remain ignored.'
}

Write-Output 'release-hygiene-ok'
