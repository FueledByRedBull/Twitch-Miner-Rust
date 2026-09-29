[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$EvidencePath,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-f]{40}$')]
    [string]$ExpectedRevision,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^sha256:[0-9a-f]{64}$')]
    [string]$ExpectedDigest,

    [ValidatePattern('^v[0-9]+\.[0-9]+\.[0-9]+$')]
    [string]$ExpectedReleaseTag = 'v0.0.0',

    [ValidatePattern('^[a-z0-9._-]+/[a-z0-9._-]+$')]
    [string]$Repository = $env:GITHUB_REPOSITORY,

    [string]$GitHubToken = $env:GITHUB_TOKEN,

    [string]$ImageReference,

    [ValidateRange(1, 365)]
    [int]$MaxEvidenceAgeDays = 14,

    [switch]$ValidateOnly
)

$ErrorActionPreference = 'Stop'

function Get-Property([object]$Object, [string]$Name) {
    if ($null -eq $Object) {
        return $null
    }
    return $Object.PSObject.Properties[$Name].Value
}

function Assert-Text([object]$Value, [string]$Name) {
    if ($null -eq $Value -or [string]::IsNullOrWhiteSpace([string]$Value)) {
        throw "Release evidence field '$Name' is required."
    }
}

function Assert-Digest([object]$Value, [string]$Name) {
    Assert-Text $Value $Name
    if ([string]$Value -notmatch '^sha256:[0-9a-f]{64}$') {
        throw "Release evidence field '$Name' is not an immutable SHA-256 digest."
    }
}

function Assert-Hash([object]$Value, [string]$Name) {
    Assert-Text $Value $Name
    if ([string]$Value -notmatch '^[0-9a-f]{64}$') {
        throw "Release evidence field '$Name' is not a SHA-256 configuration fingerprint."
    }
}

function Assert-Revision([object]$Value, [string]$Name) {
    Assert-Text $Value $Name
    if ([string]$Value -notmatch '^[0-9a-f]{40}$') {
        throw "Release evidence field '$Name' is not a full Git revision."
    }
}

function Assert-Pass([object]$Object, [string]$Name) {
    if ((Get-Property $Object 'result') -ne 'pass') {
        throw "Release evidence field '$Name.result' must be 'pass'."
    }
}

function Parse-Utc([object]$Value, [string]$Name) {
    Assert-Text $Value $Name
    if ([string]$Value -notmatch '^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$') {
        throw "Release evidence field '$Name' is not an RFC3339 timestamp with an explicit offset."
    }
    try {
        return [DateTimeOffset]::Parse(
            [string]$Value,
            [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::AssumeUniversal -bor
                [Globalization.DateTimeStyles]::AdjustToUniversal
        )
    } catch {
        throw "Release evidence field '$Name' is not an RFC3339 timestamp."
    }
}

function Get-FiniteNonNegative([object]$Value, [string]$Name) {
    Assert-Text $Value $Name
    [double]$number = 0
    if (-not [double]::TryParse(
            [string]$Value,
            [Globalization.NumberStyles]::Float,
            [Globalization.CultureInfo]::InvariantCulture,
            [ref]$number
        ) -or [double]::IsNaN($number) -or [double]::IsInfinity($number) -or $number -lt 0) {
        throw "Release evidence field '$Name' must be a finite non-negative number."
    }
    return $number
}

function Get-NonNegativeInteger([object]$Value, [string]$Name) {
    $number = Get-FiniteNonNegative $Value $Name
    if ($number -ne [math]::Truncate($number)) {
        throw "Release evidence field '$Name' must be a non-negative integer."
    }
    return [long]$number
}

function Assert-Window(
    [object]$Object,
    [string]$StartName,
    [string]$EndName,
    [string]$Name,
    [double]$MinimumMonotonicSeconds = 0,
    [double]$MaximumClockSkewSeconds = 300
) {
    $start = Parse-Utc (Get-Property $Object $StartName) "$Name.$StartName"
    $end = Parse-Utc (Get-Property $Object $EndName) "$Name.$EndName"
    $monotonic = Get-FiniteNonNegative (Get-Property $Object 'monotonic_duration_seconds') "$Name.monotonic_duration_seconds"
    $now = [DateTimeOffset]::UtcNow
    $wallSeconds = ($end - $start).TotalSeconds
    $futureLimit = $now.AddSeconds($MaximumClockSkewSeconds)
    if ($end -lt $start -or $start -gt $futureLimit -or $end -gt $futureLimit -or
        $wallSeconds -lt $MinimumMonotonicSeconds -or
        $monotonic -lt $MinimumMonotonicSeconds -or
        [math]::Abs($monotonic - $wallSeconds) -gt $MaximumClockSkewSeconds) {
        throw "Release evidence '$Name' has an invalid or insufficient session window."
    }
    return [pscustomobject]@{ start = $start; end = $end; monotonic = $monotonic; wall_seconds = $wallSeconds }
}

function Assert-CapabilityState([object]$Object, [string]$Name, [string[]]$Allowed) {
    $value = [string](Get-Property $Object $Name)
    if ($Allowed -notcontains $value) {
        throw "Release evidence capability '$Name' must be one of: $($Allowed -join ', ')."
    }
    return $value
}

function Assert-Rejected([scriptblock]$Action, [string]$Name) {
    try {
        & $Action
    } catch {
        return
    }
    throw "Validation regression: invalid $Name evidence was accepted."
}

function Invoke-GitHubRunCheck([object]$Check) {
    $runId = Get-Property $Check 'run_id'
    if ($runId -isnot [int64] -and $runId -isnot [int32] -and
        [string]$runId -notmatch '^[1-9][0-9]*$') {
        throw 'Every required release check must have a positive numeric run_id.'
    }
    $workflow = [string](Get-Property $Check 'workflow')
    Assert-Text $workflow 'required_checks.workflow'
    $event = [string](Get-Property $Check 'event')
    $allowedEvents = if ($workflow -eq 'Windows Release') { @('push') } else { @('push', 'workflow_dispatch') }
    if ($allowedEvents -notcontains $event) {
        throw "Required check '$workflow' must come from a trusted push or workflow_dispatch run."
    }
    $expectedRef = if ($workflow -eq 'Windows Release') {
        "refs/tags/$ExpectedReleaseTag"
    } else {
        'refs/heads/main'
    }
    if ((Get-Property $Check 'ref') -ne $expectedRef) {
        throw "Required check '$workflow' has the wrong source ref."
    }
    $uri = "https://api.github.com/repos/$Repository/actions/runs/$runId"
    try {
        $run = Invoke-RestMethod -Method Get -Uri $uri -Headers @{
            Accept = 'application/vnd.github+json'
            Authorization = "Bearer $GitHubToken"
            'X-GitHub-Api-Version' = '2022-11-28'
        }
    } catch {
        throw "Unable to read required GitHub Actions run $runId."
    }
    $workflowPaths = @{
        'CI' = '.github/workflows/ci.yml'
        'Multiarch Build' = '.github/workflows/multiarch-build.yml'
        'Deep Quality' = '.github/workflows/deep-quality.yml'
        'Windows Release' = '.github/workflows/windows-release.yml'
    }
    $workflowPath = [string](Get-Property $Check 'workflow_path')
    if ($workflowPaths[$workflow] -ne $workflowPath -or
        $run.path -ne $workflowPath -or
        $run.repository.full_name -ne $Repository -or
        $run.head_repository.full_name -ne $Repository -or
        $run.name -ne $workflow -or
        $run.head_sha -ne $ExpectedRevision -or
        $run.event -ne $event -or
        (($workflow -ne 'Windows Release') -and $run.head_branch -ne 'main') -or
        (($workflow -eq 'Windows Release') -and $run.head_branch -ne $ExpectedReleaseTag) -or
        $run.status -ne 'completed' -or
        $run.conclusion -ne 'success') {
        throw "Required GitHub Actions run $runId is not a successful $workflow run for $ExpectedRevision."
    }
}

function Test-ImagePlatformDigests([object]$Evidence) {
    if ([string]::IsNullOrWhiteSpace($ImageReference)) {
        throw 'ImageReference is required for non-validation release evidence checks.'
    }
    if ($ImageReference -notmatch '@sha256:[0-9a-f]{64}$') {
        throw 'ImageReference must use an immutable digest reference.'
    }
    if ($ImageReference.Split('@', 2)[1] -ne $ExpectedDigest) {
        throw 'ImageReference does not use the expected release manifest digest.'
    }
    if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
        throw 'Docker is required to bind release evidence to platform digests.'
    }
    $imageName = $ImageReference.Split('@', 2)[0]
    $raw = docker buildx imagetools inspect $ImageReference --raw 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to inspect the immutable release manifest for evidence binding.'
    }
    try {
        $index = ($raw -join "`n") | ConvertFrom-Json
    } catch {
        throw 'The immutable release manifest was not valid JSON.'
    }
    $platforms = Get-Property $Evidence 'platform_digests'
    foreach ($platform in @('linux/amd64', 'linux/arm64')) {
        $expected = [string](Get-Property $platforms $platform)
        Assert-Digest $expected "platform_digests.$platform"
        $parts = $platform.Split('/')
        $descriptor = @($index.manifests | Where-Object {
                $_.platform.os -eq $parts[0] -and
                $_.platform.architecture -eq $parts[1]
            })
        if ($descriptor.Count -ne 1 -or $descriptor[0].digest -ne $expected) {
            throw "Release evidence platform digest does not match the immutable manifest for $platform."
        }
    }
}

function Test-Evidence([object]$Evidence) {
    if ((Get-Property $Evidence 'schema_version') -ne 2) {
        throw 'Release evidence schema_version must be 2.'
    }
    Assert-Revision (Get-Property $Evidence 'source_revision') 'source_revision'
    if ((Get-Property $Evidence 'source_revision') -ne $ExpectedRevision) {
        throw 'Release evidence source_revision does not match the release revision.'
    }
    Assert-Digest (Get-Property $Evidence 'manifest_digest') 'manifest_digest'
    if ((Get-Property $Evidence 'manifest_digest') -ne $ExpectedDigest) {
        throw 'Release evidence manifest_digest does not match the accepted digest.'
    }

    $platformDigests = Get-Property $Evidence 'platform_digests'
    foreach ($platform in @('linux/amd64', 'linux/arm64')) {
        Assert-Digest (Get-Property $platformDigests $platform) "platform_digests.$platform"
    }

    $checks = @(Get-Property $Evidence 'required_checks')
    $requiredWorkflows = @('CI', 'Multiarch Build', 'Deep Quality', 'Windows Release')
    if ($checks.Count -ne $requiredWorkflows.Count -or
        @($checks | Where-Object { $requiredWorkflows -notcontains (Get-Property $_ 'workflow') }).Count -ne 0) {
        throw 'Release evidence must contain exactly the four canonical required checks.'
    }
    foreach ($workflow in $requiredWorkflows) {
        $matching = @($checks | Where-Object {
                (Get-Property $_ 'workflow') -eq $workflow
            })
        if ($matching.Count -ne 1) {
            throw "Release evidence must contain exactly one required check for '$workflow'."
        }
        Assert-Revision (Get-Property $matching[0] 'head_sha') "required_checks.$workflow.head_sha"
        $expectedEventSet = if ($workflow -eq 'Windows Release') { @('push') } else { @('push', 'workflow_dispatch') }
        $expectedRef = if ($workflow -eq 'Windows Release') {
            "refs/tags/$ExpectedReleaseTag"
        } else {
            'refs/heads/main'
        }
        if ($expectedEventSet -notcontains (Get-Property $matching[0] 'event') -or
            (Get-Property $matching[0] 'ref') -ne $expectedRef) {
            throw "Required check '$workflow' is not a trusted exact-source run."
        }
        $expectedWorkflowPath = @{
            'CI' = '.github/workflows/ci.yml'
            'Multiarch Build' = '.github/workflows/multiarch-build.yml'
            'Deep Quality' = '.github/workflows/deep-quality.yml'
            'Windows Release' = '.github/workflows/windows-release.yml'
        }[$workflow]
        if ((Get-Property $matching[0] 'workflow_path') -ne $expectedWorkflowPath) {
            throw "Required check '$workflow' has the wrong workflow path."
        }
        if ((Get-Property $matching[0] 'head_sha') -ne $ExpectedRevision -or
            (Get-Property $matching[0] 'conclusion') -ne 'success') {
            throw "Required check '$workflow' is not a successful check for the release revision."
        }
        if (-not $ValidateOnly) {
            Invoke-GitHubRunCheck $matching[0]
        }
    }

    $canary = Get-Property $Evidence 'canary'
    Assert-Pass $canary 'canary'
    Assert-Revision (Get-Property $canary 'source_revision') 'canary.source_revision'
    Assert-Digest (Get-Property $canary 'image_digest') 'canary.image_digest'
    if ((Get-Property $canary 'source_revision') -ne $ExpectedRevision -or
        (Get-Property $canary 'image_digest') -ne $ExpectedDigest -or
        (Get-Property $canary 'mutations') -ne 'none') {
        throw 'Read-only canary evidence is not bound to the accepted revision/digest or records mutations.'
    }
    Assert-Hash (Get-Property $canary 'config_fingerprint') 'canary.config_fingerprint'
    $canaryWindow = Assert-Window $canary 'started_at' 'ended_at' 'canary' 1
    $canaryObserved = Parse-Utc (Get-Property $canary 'observed_at') 'canary.observed_at'
    if ($canaryObserved -lt $canaryWindow.start -or $canaryObserved -gt $canaryWindow.end) {
        throw 'Canary observed_at must fall inside the canary session window.'
    }

    $soak = Get-Property $Evidence 'soak'
    Assert-Pass $soak 'soak'
    Assert-Revision (Get-Property $soak 'source_revision') 'soak.source_revision'
    Assert-Digest (Get-Property $soak 'image_digest') 'soak.image_digest'
    if ((Get-Property $soak 'source_revision') -ne $ExpectedRevision -or
        (Get-Property $soak 'image_digest') -ne $ExpectedDigest) {
        throw 'Soak evidence is not bound to the accepted revision and digest.'
    }
    Assert-Hash (Get-Property $soak 'config_fingerprint') 'soak.config_fingerprint'
    if ((Get-Property $soak 'config_fingerprint') -ne (Get-Property $canary 'config_fingerprint')) {
        throw 'Canary and soak configuration fingerprints do not match.'
    }
    $soakWindow = Assert-Window $soak 'started_at' 'ended_at' 'soak' (72 * 60 * 60)
    $now = [DateTimeOffset]::UtcNow
    if (($now - $soakWindow.end).TotalDays -gt $MaxEvidenceAgeDays -or
        $soakWindow.start -lt $canaryWindow.end) {
        throw 'Soak evidence is too short, stale, or has an invalid time window.'
    }
    $capabilities = Get-Property $soak 'capabilities'
    foreach ($capability in @('eventsub', 'pubsub', 'watch', 'drops', 'predictions')) {
        [void](Assert-CapabilityState $capabilities $capability @('pass', 'not-exercised', 'unsupported'))
    }
    foreach ($capability in @('eventsub', 'pubsub', 'watch', 'drops')) {
        if ((Get-Property $capabilities $capability) -ne 'pass') {
            throw "Soak evidence must pass the '$capability' capability."
        }
    }
    $opportunity = Get-Property $soak 'opportunity'
    $eligibleSeconds = Get-FiniteNonNegative (Get-Property $opportunity 'eligible_seconds') 'soak.opportunity.eligible_seconds'
    $liveSeconds = Get-FiniteNonNegative (Get-Property $opportunity 'live_seconds') 'soak.opportunity.live_seconds'
    if ($eligibleSeconds -le 0 -or $liveSeconds -le 0 -or
        $liveSeconds -gt $eligibleSeconds -or
        $eligibleSeconds -gt ($soakWindow.wall_seconds + 300)) {
        throw 'Soak opportunity windows must contain positive live time within the eligible soak interval.'
    }
    $confirmed = Get-Property $soak 'server_confirmed'
    $confirmedCounts = @{}
    foreach ($metric in @('watch_rewards', 'claims', 'drops', 'predictions')) {
        $confirmedCounts[$metric] = Get-NonNegativeInteger `
            (Get-Property $confirmed $metric) "soak.server_confirmed.$metric"
    }
    if ($confirmedCounts['claims'] -lt 1 -or
        $confirmedCounts['watch_rewards'] -lt 1 -or
        $confirmedCounts['drops'] -lt 1) {
        throw 'Soak evidence must include at least one server-confirmed watch reward, claim, and drop while those capabilities are marked pass.'
    }
    if ((Get-Property $capabilities 'predictions') -ne 'pass' -and
        $confirmedCounts['predictions'] -ne 0) {
        throw 'Prediction counts must be zero when predictions are marked not-exercised or unsupported.'
    }
    if ((Get-Property $capabilities 'predictions') -eq 'pass' -and
        $confirmedCounts['predictions'] -lt 1) {
        throw 'Prediction capability marked pass requires a server-confirmed prediction outcome.'
    }
    $recovery = Get-Property $soak 'failure_recovery'
    if ((Get-Property $recovery 'result') -ne 'pass') {
        throw 'Soak evidence must include a passing failure-recovery exercise.'
    }
    $injectedFailures = Get-NonNegativeInteger (Get-Property $recovery 'injected_failures') 'soak.failure_recovery.injected_failures'
    $recoveredFailures = Get-NonNegativeInteger (Get-Property $recovery 'recovered_failures') 'soak.failure_recovery.recovered_failures'
    if ($injectedFailures -lt 1 -or $recoveredFailures -lt $injectedFailures) {
        throw 'Soak evidence must demonstrate recovery from at least one injected failure.'
    }
    $maxRecoverySeconds = Get-FiniteNonNegative `
        (Get-Property $recovery 'max_recovery_seconds') 'soak.failure_recovery.max_recovery_seconds'
    if ($maxRecoverySeconds -le 0) {
        throw 'Soak failure-recovery evidence must record a positive recovery duration.'
    }
    $resources = Get-Property $soak 'resource_use'
    if ((Get-Property $resources 'result') -ne 'pass') {
        throw 'Soak evidence must include a passing resource-use measurement.'
    }
    foreach ($metric in @('max_rss_bytes', 'cpu_seconds', 'disk_bytes')) {
        [void](Get-FiniteNonNegative (Get-Property $resources $metric) "soak.resource_use.$metric")
    }

    $rollback = Get-Property $Evidence 'rollback'
    Assert-Pass $rollback 'rollback'
    Assert-Revision (Get-Property $rollback 'source_revision') 'rollback.source_revision'
    Assert-Digest (Get-Property $rollback 'image_digest') 'rollback.image_digest'
    if ((Get-Property $rollback 'image_digest') -eq $ExpectedDigest -or
        (Get-Property $rollback 'compatibility') -ne 'pass' -or
        (Get-Property $rollback 'state_restore') -ne 'pass' -or
        (Get-Property $rollback 'data_snapshot') -ne 'pass' -or
        (Get-Property $rollback 'candidate_data_preserved') -ne 'pass' -or
        (Get-Property $rollback 'format_policy') -ne 'compatible') {
        throw 'Rollback evidence must use a distinct digest and prove state compatibility/restoration.'
    }

    $approval = Get-Property $Evidence 'approval'
    if ((Get-Property $approval 'approved') -ne $true) {
        throw 'Release evidence requires an explicit approval.'
    }
    Assert-Text (Get-Property $approval 'actor') 'approval.actor'
    $approvedAt = Parse-Utc (Get-Property $approval 'approved_at') 'approval.approved_at'
    if ($approvedAt -lt $soakWindow.end -or $approvedAt -gt [DateTimeOffset]::UtcNow.AddMinutes(5)) {
        throw 'Release approval must occur after soak completion and cannot be future-dated.'
    }

    if (-not $ValidateOnly) {
        Test-ImagePlatformDigests $Evidence
    }
}

if ($ValidateOnly) {
    $revision = 'c' * 40
    $digest = "sha256:$('a' * 64)"
    $otherDigest = "sha256:$('b' * 64)"
    $end = [DateTimeOffset]::UtcNow.AddHours(-1)
    $start = $end.AddHours(-72)
    $canaryEnd = $start.AddMinutes(-1)
    $evidence = [pscustomobject]@{
        schema_version = 2
        source_revision = $revision
        manifest_digest = $digest
        platform_digests = [pscustomobject]@{
            'linux/amd64' = $digest
            'linux/arm64' = $otherDigest
        }
        required_checks = @(
            [pscustomobject]@{ workflow = 'CI'; workflow_path = '.github/workflows/ci.yml'; event = 'push'; ref = 'refs/heads/main'; run_id = 1; head_sha = $revision; conclusion = 'success' }
            [pscustomobject]@{ workflow = 'Multiarch Build'; workflow_path = '.github/workflows/multiarch-build.yml'; event = 'push'; ref = 'refs/heads/main'; run_id = 2; head_sha = $revision; conclusion = 'success' }
            [pscustomobject]@{ workflow = 'Deep Quality'; workflow_path = '.github/workflows/deep-quality.yml'; event = 'push'; ref = 'refs/heads/main'; run_id = 3; head_sha = $revision; conclusion = 'success' }
            [pscustomobject]@{ workflow = 'Windows Release'; workflow_path = '.github/workflows/windows-release.yml'; event = 'push'; ref = 'refs/tags/v0.0.0'; run_id = 4; head_sha = $revision; conclusion = 'success' }
        )
        canary = [pscustomobject]@{
            result = 'pass'; source_revision = $revision; image_digest = $digest
            mutations = 'none'; config_fingerprint = ('e' * 64)
            started_at = $canaryEnd.AddMinutes(-10).ToString('o'); ended_at = $canaryEnd.ToString('o')
            monotonic_duration_seconds = 600; observed_at = $canaryEnd.AddMinutes(-5).ToString('o')
        }
        soak = [pscustomobject]@{
            result = 'pass'; source_revision = $revision; image_digest = $digest
            config_fingerprint = ('e' * 64); started_at = $start.ToString('o'); ended_at = $end.ToString('o')
            monotonic_duration_seconds = 72 * 60 * 60
            capabilities = [pscustomobject]@{ eventsub = 'pass'; pubsub = 'pass'; watch = 'pass'; drops = 'pass'; predictions = 'not-exercised' }
            opportunity = [pscustomobject]@{ eligible_seconds = 1000; live_seconds = 900 }
            server_confirmed = [pscustomobject]@{ watch_rewards = 1; claims = 1; drops = 1; predictions = 0 }
            failure_recovery = [pscustomobject]@{ result = 'pass'; injected_failures = 1; recovered_failures = 1; max_recovery_seconds = 30 }
            resource_use = [pscustomobject]@{ result = 'pass'; max_rss_bytes = 1; cpu_seconds = 1; disk_bytes = 1 }
        }
        rollback = [pscustomobject]@{
            result = 'pass'; source_revision = ('d' * 40); image_digest = $otherDigest
            compatibility = 'pass'; state_restore = 'pass'; data_snapshot = 'pass'
            candidate_data_preserved = 'pass'; format_policy = 'compatible'
        }
        approval = [pscustomobject]@{
            approved = $true; actor = 'validation'; approved_at = $end.ToString('o')
        }
    }
    Test-Evidence $evidence
    $zeroOutcome = $evidence | ConvertTo-Json -Depth 20 | ConvertFrom-Json
    $zeroOutcome.soak.server_confirmed.watch_rewards = 0
    Assert-Rejected { Test-Evidence $zeroOutcome } 'zero server-confirmed watch rewards'
    $unboundedClock = $evidence | ConvertTo-Json -Depth 20 | ConvertFrom-Json
    $unboundedClock.soak.monotonic_duration_seconds = 72 * 60 * 60 + 301
    Assert-Rejected { Test-Evidence $unboundedClock } 'unbounded wall/monotonic clock divergence'
    $oversizedOpportunity = $evidence | ConvertTo-Json -Depth 20 | ConvertFrom-Json
    $oversizedOpportunity.soak.opportunity.eligible_seconds = 72 * 60 * 60 + 301
    Assert-Rejected { Test-Evidence $oversizedOpportunity } 'opportunity interval outside soak'
    $futureWindow = $evidence | ConvertTo-Json -Depth 20 | ConvertFrom-Json
    $futureStart = [DateTimeOffset]::UtcNow.AddMinutes(6)
    $futureWindow.soak.started_at = $futureStart.ToString('o')
    $futureWindow.soak.ended_at = $futureStart.AddHours(72).ToString('o')
    Assert-Rejected { Test-Evidence $futureWindow } 'future soak window'
    Write-Output 'release-evidence-validation-ok'
    return
}

if (-not (Test-Path -LiteralPath $EvidencePath -PathType Leaf)) {
    throw "Release evidence file not found: $EvidencePath"
}
try {
    $evidence = Get-Content -Raw -LiteralPath $EvidencePath | ConvertFrom-Json -Depth 20
} catch {
    throw 'Release evidence file was not valid JSON.'
}
if ([string]::IsNullOrWhiteSpace($Repository) -or [string]::IsNullOrWhiteSpace($GitHubToken)) {
    throw 'Repository and GitHubToken are required for non-validation release evidence checks.'
}
Test-Evidence $evidence
Write-Output "release-evidence-ok: revision=$ExpectedRevision digest=$ExpectedDigest"
