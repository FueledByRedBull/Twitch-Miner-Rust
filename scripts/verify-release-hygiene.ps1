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

# A job without a timeout can hold a runner for the six-hour default. Calls of a
# reusable workflow take their timeouts from the jobs inside it.
foreach ($workflow in $workflowFiles) {
    $jobsSection = [regex]::Match((Get-Content -Raw $workflow.FullName), '(?ms)^jobs:\r?\n(.*)\z').Groups[1].Value
    foreach ($job in [regex]::Matches($jobsSection, '(?ms)^  (?<name>[A-Za-z0-9_-]+):\r?\n(?<body>.*?)(?=^  [A-Za-z0-9_-]+:\r?$|\z)')) {
        $body = $job.Groups['body'].Value
        if ($body -notmatch '(?m)^    uses:' -and $body -notmatch '(?m)^    timeout-minutes:\s*[0-9]+\s*$') {
            throw "Workflow job $($job.Groups['name'].Value) in $($workflow.Name) has no timeout-minutes."
        }
    }
    foreach ($cacheTo in [regex]::Matches((Get-Content -Raw $workflow.FullName), '(?m)^\s*cache-to:\s*(type=gha[^\r\n]*)')) {
        if ($cacheTo.Groups[1].Value -notmatch 'ignore-error=true') {
            throw "A cache outage must not fail an image build: add ignore-error=true to the type=gha cache-to in $($workflow.Name)."
        }
    }
}

# Path filters would leave a required check pending when they skip a workflow.
foreach ($name in @('ci', 'rust-qa', 'deep-quality', 'windows-release', 'multiarch-build')) {
    if ((Get-Content -Raw ".github/workflows/$name.yml") -match '(?m)^\s+paths(?:-ignore)?:') {
        throw "Workflow $name.yml feeds a required check and must not use a paths filter; gate jobs from the changes job instead."
    }
}

function Get-WorkflowJob([string]$Workflow, [string]$Name) {
    $match = [regex]::Match($Workflow, "(?ms)^  $([regex]::Escape($Name)):\r?\n(.*?)(?=^  [A-Za-z0-9_-]+:\r?`$|\z)")
    if (-not $match.Success) {
        throw "Workflow job $Name was not found."
    }
    return $match.Groups[1].Value
}

# Docs-only pull requests skip the jobs below. required-checks is the single
# aggregate that stays required, so it alone decides what a skip means.
$gatedJobs = @('rust-qa', 'go-baseline', 'coverage', 'deep-quality', 'windows-package')
$changesJob = Get-WorkflowJob $ciWorkflow 'changes'
$changesActions = @([regex]::Matches($changesJob, '(?m)^\s*(?:-\s+)?uses:\s*(\S+)') |
        Where-Object { $_.Groups[1].Value -notmatch '^actions/checkout@' })
if ($changesJob -notmatch 'git diff --name-status' -or
    $changesJob -notmatch '"\$GITHUB_EVENT_NAME" = pull_request' -or
    $changesActions.Count -ne 0) {
    throw 'The changes job must classify files with git diff on pull requests and use no action besides checkout.'
}
foreach ($job in $gatedJobs) {
    $body = Get-WorkflowJob $ciWorkflow $job
    if ($body -notmatch '(?m)^    needs:\s*changes\s*$' -or
        $body -notmatch "(?m)^    if:\s*\`$\{\{\s*needs\.changes\.outputs\.code == 'true'\s*\}\}\s*$") {
        throw "CI job $job must wait for the changes job and run only when it reports code changes."
    }
}
$ciJobNames = @([regex]::Matches(
        [regex]::Match($ciWorkflow, '(?ms)^jobs:\r?\n(.*)\z').Groups[1].Value,
        '(?m)^  ([A-Za-z0-9_-]+):\s*$') | ForEach-Object { $_.Groups[1].Value } | Where-Object { $_ -ne 'required-checks' })
$requiredChecks = Get-WorkflowJob $ciWorkflow 'required-checks'
foreach ($job in $ciJobNames) {
    if ($requiredChecks -notmatch "(?m)^      - $([regex]::Escape($job))\s*$") {
        throw "required-checks does not wait for CI job $job."
    }
}
$gate = [regex]::Match($requiredChecks, '(?ms)- name: Require every canonical check\r?\n.*?^        run: \|\r?\n(?<body>.*?)(?=^      - name:|\z)')
if (-not $gate.Success) {
    throw 'required-checks must keep its aggregate step.'
}
$gateScript = [scriptblock]::Create((($gate.Groups['body'].Value -split '\r?\n') | ForEach-Object { $_ -replace '^ {10}', '' }) -join "`n")
function Test-RequiredChecks([string]$Code, [hashtable]$Results = @{}) {
    $needs = [ordered]@{}
    foreach ($job in $ciJobNames) {
        $result = 'success'
        if ($Results.ContainsKey($job)) { $result = $Results[$job] }
        $needs[$job] = [ordered]@{ result = $result; outputs = @{} }
    }
    $needs['changes'].outputs = @{ code = $Code }
    $previous = $env:NEEDS
    try {
        $env:NEEDS = $needs | ConvertTo-Json -Depth 5 -Compress
        & $gateScript
        return $true
    } catch {
        return $false
    } finally {
        $env:NEEDS = $previous
    }
}
$skippedGated = @{}
foreach ($job in $gatedJobs) { $skippedGated[$job] = 'skipped' }
if (-not (Test-RequiredChecks 'true') -or
    -not (Test-RequiredChecks 'false') -or
    -not (Test-RequiredChecks 'false' $skippedGated)) {
    throw 'required-checks must accept all-success runs and gated skips on documentation-only changes.'
}
if ((Test-RequiredChecks 'true' $skippedGated) -or
    (Test-RequiredChecks '' $skippedGated)) {
    throw 'required-checks must reject a skip unless the changes job reported documentation-only changes.'
}
foreach ($job in $ciJobNames) {
    foreach ($result in @('failure', 'cancelled')) {
        foreach ($code in @('true', 'false')) {
            if (Test-RequiredChecks $code @{ $job = $result }) {
                throw "required-checks must reject $result of $job."
            }
        }
    }
    if ($gatedJobs -notcontains $job -and (Test-RequiredChecks 'false' @{ $job = 'skipped' })) {
        throw "required-checks must not accept a skipped $job."
    }
}

$multiarchWorkflow = Get-Content -Raw .github/workflows/multiarch-build.yml
if ($multiarchWorkflow -match 'tags:\s*\r?\n\s*-\s*v\*') {
    throw 'Stable release tags must be promoted by the protected manual workflow.'
}
if ($multiarchWorkflow -match 'type=raw,value=latest|type=ref,event=branch|type=ref,event=tag') {
    throw 'Multiarch candidates must expose only the immutable SHA discovery tag before acceptance.'
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
$installer = Get-Content -Raw installer/Product.wxs
$windowsBuildScript = Get-Content -Raw scripts/build-windows-release.ps1
if ($installer -match 'Guid="\*"' -or
    $installer -notmatch 'ProgramFiles64Folder' -or
    $installer -notmatch 'config\.example\.json') {
    throw 'WiX installer must use a stable component identity and keep runtime data out of Program Files.'
}
if ($windowsWorkflow -notmatch 'dotnet tool install wix .*--version 4\.0\.6 .*--allow-roll-forward' -or
    $windowsWorkflow -notmatch 'DOTNET_ROLL_FORWARD:\s*Major') {
    throw 'Windows WiX jobs must pin 4.0.6 and allow roll-forward on the Windows 2025 runner.'
}
if ($windowsBuildScript -notmatch 'LastWriteTimeUtc' -or
    $windowsBuildScript -notmatch 'SOURCE_DATE_EPOCH' -or
    $windowsBuildScript -notmatch 'msi validate') {
    throw 'Windows packaging must normalize portable archive timestamps and validate the MSI database.'
}

$binariesWorkflow = Get-Content -Raw .github/workflows/release-binaries.yml
foreach ($target in @(
        'x86_64-unknown-linux-musl',
        'aarch64-unknown-linux-musl',
        'armv7-unknown-linux-musleabihf',
        'x86_64-apple-darwin',
        'aarch64-apple-darwin'
    )) {
    if ($binariesWorkflow -notmatch "(?m)^\s+-\s+target:\s*$([regex]::Escape($target))\s*$") {
        throw "Release binaries workflow does not build $target."
    }
}
if ($binariesWorkflow -notmatch 'cargo zigbuild --locked --release' -or
    $binariesWorkflow -notmatch 'cargo build --locked --release' -or
    $binariesWorkflow -notmatch '(?m)^  ZIG_VERSION:\s*\d+\.\d+\.\d+\s*$' -or
    $binariesWorkflow -notmatch '(?m)^  CARGO_ZIGBUILD_VERSION:\s*\d+\.\d+\.\d+\s*$' -or
    [regex]::Matches($binariesWorkflow, 'zig_sha256:\s*[0-9a-f]{64}\s').Count -lt 2 -or
    $binariesWorkflow -notmatch 'sha256sum --check') {
    throw 'Release binaries must be built with --locked --release and exactly pinned, checksum-verified Zig and cargo-zigbuild.'
}
$attestJob = [regex]::Match($binariesWorkflow, '(?ms)^  attest:\r?\n(.*?)(?=^  [A-Za-z0-9_-]+:\r?$|\z)')
if (-not $attestJob.Success -or
    $attestJob.Groups[1].Value -notmatch "(?m)^    if:\s*github\.event_name != 'pull_request'\s*$" -or
    $attestJob.Groups[1].Value -notmatch '(?m)^      id-token:\s*write\s*$' -or
    $attestJob.Groups[1].Value -notmatch '(?m)^      attestations:\s*write\s*$' -or
    $attestJob.Groups[1].Value -notmatch 'actions/attest-build-provenance@[0-9a-f]{40}' -or
    [regex]::Matches($binariesWorkflow, 'attest-build-provenance@').Count -ne 1 -or
    [regex]::Matches($binariesWorkflow, 'id-token:\s*write').Count -ne 1 -or
    [regex]::Matches($binariesWorkflow, 'attestations:\s*write').Count -ne 1 -or
    $binariesWorkflow -notmatch '(?m)^permissions:\r?\n  contents:\s*read\s*$') {
    throw 'Release binary attestations must run only on tag and manual runs, in one job that alone holds id-token and attestations write access.'
}
if ($binariesWorkflow -notmatch "(?ms)^on:.*?^  push:\r?\n    tags:\r?\n      - 'v\*'\s*\r?\n" -or
    $binariesWorkflow -match '(?m)^  push:\r?\n    branches:' -or
    $binariesWorkflow -notmatch '(?m)^  pull_request:\r?\n    paths:' -or
    $binariesWorkflow -match 'contents:\s*write|gh release|action-gh-release') {
    throw 'Release binaries must build on v* tags, manual runs and path-filtered pull requests, and must not publish a GitHub Release.'
}
# Tag and manual release builds stay cold: any cache step is limited to pull requests.
$cacheSteps = [regex]::Matches($binariesWorkflow, '(?m)^\s*(?:-\s+)?uses:\s*(?:Swatinem/rust-cache|actions/cache)\b')
$pullRequestCacheSteps = [regex]::Matches(
    $binariesWorkflow,
    "(?m)^        if:\s*github\.event_name == 'pull_request'\s*\r?\n\s+uses:\s*Swatinem/rust-cache@[0-9a-f]{40}")
if ($cacheSteps.Count -ne $pullRequestCacheSteps.Count) {
    throw 'Release binary caches must be restored only on pull request runs so tag builds stay cold.'
}
# The Windows lane runs on tags too, so its one cache step must skip tag refs
# and may be saved only from main.
$windowsCacheSteps = [regex]::Matches($windowsWorkflow, '(?m)^\s*(?:-\s+)?uses:\s*(?:Swatinem/rust-cache|actions/cache)\b')
$windowsTagGuardedCache = [regex]::Matches(
    $windowsWorkflow,
    '(?m)^        if:\s*\$\{\{\s*!startsWith\(github\.ref,\s*''refs/tags/''\)\s*\}\}\s*\r?\n\s+uses:\s*Swatinem/rust-cache@[0-9a-f]{40}')
if ($windowsCacheSteps.Count -ne 1 -or
    $windowsTagGuardedCache.Count -ne 1 -or
    $windowsWorkflow -notmatch '(?m)^          save-if:\s*\$\{\{\s*github\.ref == ''refs/heads/main''\s*\}\}\s*$') {
    throw 'The Windows release cache must be skipped on tag refs and saved only from main so tag builds stay cold.'
}

# Deep Quality fuzzing keeps its per-target budget, fails when either concurrent
# target fails, and saves the corpus from main only.
$deepQualityWorkflow = Get-Content -Raw .github/workflows/deep-quality.yml
if ([regex]::Matches($deepQualityWorkflow, '-max_total_time=120\b').Count -ne 1 -or
    $deepQualityWorkflow -notmatch '(?m)^\s+targets=\(transport_parsers structured_input\)\s*$' -or
    $deepQualityWorkflow -notmatch 'if ! wait "\$\{pids\[\$i\]\}"' -or
    $deepQualityWorkflow -notmatch '(?m)^\s+exit "\$status"\s*$') {
    throw 'Deep Quality must fuzz both targets for 120 seconds each and fail when either target fails.'
}
$corpusSaves = [regex]::Matches($deepQualityWorkflow, '(?m)^\s*(?:-\s+)?uses:\s*actions/cache(?:/save)?@')
$mainOnlyCorpusSaves = [regex]::Matches(
    $deepQualityWorkflow,
    '(?m)^        if:\s*\$\{\{\s*!cancelled\(\) && github\.ref == ''refs/heads/main''\s*\}\}\s*\r?\n\s+uses:\s*actions/cache/save@[0-9a-f]{40}')
if ($corpusSaves.Count -ne 1 -or $mainOnlyCorpusSaves.Count -ne 1) {
    throw 'The fuzz corpus must be saved only from main.'
}

if (-not (Test-Path -LiteralPath 'fuzz/Cargo.lock' -PathType Leaf) -or
    -not (Test-Path -LiteralPath 'fuzz/Cargo.toml' -PathType Leaf)) {
    throw 'The isolated fuzz workspace must retain its own manifest and lockfile.'
}
$offlineBundleScript = Get-Content -Raw scripts/create-offline-source-bundle.ps1
if ($offlineBundleScript -notmatch 'cargo vendor --locked --versioned-dirs --sync fuzz/Cargo\.toml vendor' -or
    $offlineBundleScript -notmatch '--manifest-path fuzz/Cargo\.toml --locked --offline' -or
    $offlineBundleScript -notmatch 'OutputPath must not be inside the temporary bundle staging directory') {
    throw 'Offline source bundles must vendor and validate the fuzz workspace and isolate staging from stale or self-including output paths.'
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
    -EvidencePath (Join-Path ([System.IO.Path]::GetTempPath()) 'release-evidence-validation.json') `
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

Write-Output 'release-hygiene-ok'
