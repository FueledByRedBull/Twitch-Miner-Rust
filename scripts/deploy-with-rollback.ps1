[CmdletBinding(SupportsShouldProcess)]
param(
    [Parameter(Mandatory)]
    [ValidatePattern('^ghcr\.io/[a-z0-9._/-]+@sha256:[0-9a-f]{64}$')]
    [string]$CandidateImage,

    [Parameter(Mandatory)]
    [ValidatePattern('^ghcr\.io/[a-z0-9._/-]+@sha256:[0-9a-f]{64}$')]
    [string]$RollbackImage,

    [Parameter(Mandatory)]
    [ValidatePattern('^[0-9a-f]{40}$')]
    [string]$CandidateRevision,

    [Parameter(Mandatory)]
    [ValidatePattern('^[0-9a-f]{40}$')]
    [string]$RollbackRevision,

    [string]$ComposeFile = 'deploy/docker-compose.rpi.yml',
    [string]$DataDir = './data',
    [string]$Service = 'twitch-miner',
    [ValidateRange(1, 2147483647)]
    [int]$RuntimeUid = 1000,
    [ValidateRange(1, 2147483647)]
    [int]$RuntimeGid = 1000,
    [ValidateRange(30, 600)]
    [int]$HealthTimeoutSeconds = 180,
    [switch]$ValidateOnly
)

$ErrorActionPreference = 'Stop'

if ($CandidateImage -eq $RollbackImage) {
    throw 'Candidate and rollback images must be different immutable digests.'
}
$candidateRepository = $CandidateImage.Split('@', 2)[0]
$rollbackRepository = $RollbackImage.Split('@', 2)[0]
if ($candidateRepository -ne $rollbackRepository) {
    throw 'Candidate and rollback images must use the same GHCR repository.'
}
if (-not (Test-Path -LiteralPath $ComposeFile -PathType Leaf)) {
    throw "Compose file not found: $ComposeFile"
}
$composeText = Get-Content -Raw -LiteralPath $ComposeFile
if ($composeText -notmatch [regex]::Escape('${TWITCH_MINER_IMAGE:')) {
    throw 'Compose file does not use the required TWITCH_MINER_IMAGE variable.'
}
if ($composeText -notmatch [regex]::Escape('${TWITCH_MINER_DATA_DIR:')) {
    throw 'Compose file does not use the required TWITCH_MINER_DATA_DIR variable.'
}
if ($composeText -notmatch "(?m)^\s+$([regex]::Escape($Service)):\s*$") {
    throw "Compose service not found: $Service"
}

function Test-RuntimeStatusReady(
    [object]$Status,
    [string]$Revision,
    [long]$ContainerStarted,
    [long]$Now
) {
    $tasks = @($Status.tasks)
    $eventSub = $Status.eventsub
    $pubSub = $Status.pubsub
    $pubSubCapabilities = @($pubSub.capabilities)
    $acknowledgedTopics = ($pubSubCapabilities |
            Measure-Object -Property acknowledged_topics -Sum).Sum
    if ($null -eq $acknowledgedTopics) {
        $acknowledgedTopics = 0
    }

    return $Status.state -eq 'ready' -and
        $Status.revision -eq $Revision -and
        $Status.started_at_unix -ge $ContainerStarted -and
        $Status.heartbeat_at_unix -ge $Status.started_at_unix -and
        $Status.heartbeat_at_unix -le ($Now + 5) -and
        ($Now - $Status.heartbeat_at_unix) -le 120 -and
        $tasks.Count -gt 0 -and
        @($tasks | Where-Object {
                $_.consecutive_failures -ne 0 -or
                $null -ne $_.last_error_class
            }).Count -eq 0 -and
        $null -ne $eventSub -and
        $eventSub.active_subscriptions -eq $eventSub.planned_subscriptions -and
        $eventSub.failed_subscriptions -eq 0 -and
        $null -ne $pubSub -and
        $acknowledgedTopics -eq $pubSub.total_topics -and
        @($pubSubCapabilities | Where-Object {
                $null -ne $_.failure_class
            }).Count -eq 0 -and
        $null -eq $Status.counters.last_error_class
}

if ($ValidateOnly) {
    $validationRevision = 'c' * 40
    $readyStatus = [pscustomobject]@{
        state = 'ready'
        revision = $validationRevision
        started_at_unix = 101
        heartbeat_at_unix = 120
        tasks = @([pscustomobject]@{
                consecutive_failures = 0
                last_error_class = $null
            })
        eventsub = [pscustomobject]@{
            active_subscriptions = 1
            planned_subscriptions = 1
            failed_subscriptions = 0
        }
        pubsub = [pscustomobject]@{
            total_topics = 1
            capabilities = @([pscustomobject]@{
                    acknowledged_topics = 1
                    failure_class = $null
                })
        }
        counters = [pscustomobject]@{ last_error_class = $null }
    }
    if (-not (Test-RuntimeStatusReady $readyStatus $validationRevision 100 120)) {
        throw 'Fresh complete deployment status validation failed.'
    }
    $staleStatus = $readyStatus | ConvertTo-Json -Depth 5 | ConvertFrom-Json
    $staleStatus.started_at_unix = 99
    if (Test-RuntimeStatusReady $staleStatus $validationRevision 100 120) {
        throw 'Stale deployment status was accepted.'
    }
    $staleHeartbeat = $readyStatus | ConvertTo-Json -Depth 5 | ConvertFrom-Json
    $staleHeartbeat.heartbeat_at_unix = -1
    if (Test-RuntimeStatusReady $staleHeartbeat $validationRevision 100 120) {
        throw 'Stale deployment heartbeat was accepted.'
    }
    $incompleteStatus = $readyStatus | ConvertTo-Json -Depth 5 | ConvertFrom-Json
    $incompleteStatus.pubsub.capabilities[0].acknowledged_topics = 0
    if (Test-RuntimeStatusReady $incompleteStatus $validationRevision 100 120) {
        throw 'Incomplete transport status was accepted.'
    }
    Write-Output 'deploy-with-rollback-validation-ok'
    return
}

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw 'Docker is required for deployment.'
}
if (-not (Test-Path -LiteralPath $DataDir -PathType Container)) {
    throw "Data directory not found: $DataDir"
}
if (-not (Test-Path -LiteralPath (Join-Path $DataDir 'config.json') -PathType Leaf)) {
    throw "Runtime config not found under data directory: $DataDir"
}

$resolvedCompose = (Resolve-Path -LiteralPath $ComposeFile).Path
$resolvedData = (Resolve-Path -LiteralPath $DataDir).Path
$runtimeUser = "${RuntimeUid}:${RuntimeGid}"
$oldImage = $env:TWITCH_MINER_IMAGE
$oldDataDir = $env:TWITCH_MINER_DATA_DIR
$oldUid = $env:UID
$oldGid = $env:GID
$candidateDeploymentStarted = $false

function Invoke-Docker([string[]]$Arguments, [string]$FailureMessage) {
    & docker @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$FailureMessage (exit code $LASTEXITCODE)."
    }
}

function Test-ImageConfig([string]$Image, [bool]$RequireJson) {
    $arguments = @(
        'run', '--rm', '--platform', 'linux/arm64', '--user', $runtimeUser,
        '--volume', "${resolvedData}:/data:ro", $Image,
        '--data-dir', '/data', '--check-config'
    )
    if ($RequireJson) {
        $arguments += '--json'
    }
    Invoke-Docker $arguments "Config preflight failed for immutable image"
}

function Test-ImageCanary([string]$Image) {
    Invoke-Docker @(
        'run', '--rm', '--platform', 'linux/arm64', '--user', $runtimeUser,
        '--volume', "${resolvedData}:/data:ro", $Image,
        '--data-dir', '/data', '--canary'
    ) 'Read-only candidate canary failed'
}

function Test-ImageRevision([string]$Image, [string]$Revision) {
    $version = (& docker run --rm --platform linux/arm64 $Image --version 2>&1) -join "`n"
    if ($LASTEXITCODE -ne 0 -or $version -notmatch [regex]::Escape($Revision)) {
        throw "Immutable image revision verification failed."
    }
}

function Set-ComposeImage([string]$Image) {
    $env:TWITCH_MINER_IMAGE = $Image
    $env:TWITCH_MINER_DATA_DIR = $resolvedData
    $env:UID = "$RuntimeUid"
    $env:GID = "$RuntimeGid"
}

function Test-DeployedService([string]$Revision, [string]$Label) {
    $deadline = [DateTime]::UtcNow.AddSeconds($HealthTimeoutSeconds)
    do {
        $containerId = (& docker compose -f $resolvedCompose ps -q $Service 2>&1) -join "`n"
        if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($containerId)) {
            $state = (& docker inspect --format '{{.State.Status}}|{{if .State.Health}}{{.State.Health.Status}}{{end}}|{{.RestartCount}}|{{.State.StartedAt}}' $containerId.Trim() 2>&1) -join "`n"
            if ($LASTEXITCODE -eq 0) {
                $stateParts = $state.Trim().Split('|')
                $restartCount = 1
                if ($stateParts.Count -eq 4) {
                    [void][int]::TryParse($stateParts[2], [ref]$restartCount)
                }
                if ($restartCount -ne 0) {
                    throw "$Label restarted before becoming healthy."
                }
                if ($stateParts.Count -eq 4 -and $stateParts[0] -eq 'running') {
                    $version = (& docker compose -f $resolvedCompose exec -T $Service /twitch-miner --version 2>&1) -join "`n"
                    $versionReady = $LASTEXITCODE -eq 0 -and $version -match [regex]::Escape($Revision)
                    & docker compose -f $resolvedCompose exec -T $Service /twitch-miner --health *> $null
                    $healthReady = $LASTEXITCODE -eq 0
                    $statusText = (& docker compose -f $resolvedCompose exec -T $Service /twitch-miner --status --json 2>&1) -join "`n"
                    $statusReady = $false
                    if ($LASTEXITCODE -eq 0) {
                        try {
                            $status = $statusText | ConvertFrom-Json -ErrorAction Stop
                            $containerStarted = [DateTimeOffset]::Parse($stateParts[3]).ToUnixTimeSeconds()
                            $now = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
                            $statusReady = Test-RuntimeStatusReady `
                                $status $Revision $containerStarted $now
                        } catch {
                            $statusReady = $false
                        }
                    }
                    if ($versionReady -and $healthReady -and
                        $stateParts[1] -eq 'healthy' -and $statusReady) {
                        return
                    }
                }
            }
        }
        Start-Sleep -Seconds 5
    } while ([DateTime]::UtcNow -lt $deadline)

    throw "$Label did not reach the expected fresh revision, task, and transport state within $HealthTimeoutSeconds seconds."
}

function Assert-RunningRollbackImage {
    Set-ComposeImage $RollbackImage
    $containerId = (& docker compose -f $resolvedCompose ps -q $Service 2>&1) -join "`n"
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($containerId)) {
        throw 'A running service is required for guarded rollback deployment.'
    }
    $runningImage = (& docker inspect --format '{{.Config.Image}}' $containerId.Trim() 2>&1) -join "`n"
    if ($LASTEXITCODE -ne 0 -or $runningImage.Trim() -ne $RollbackImage) {
        throw 'Rollback image does not match the image reference used by the running service.'
    }
}

try {
    Test-ImageConfig $CandidateImage $true
    Test-ImageConfig $RollbackImage $false
    Test-ImageRevision $CandidateImage $CandidateRevision
    Test-ImageRevision $RollbackImage $RollbackRevision
    Assert-RunningRollbackImage
    Test-ImageCanary $CandidateImage
    Set-ComposeImage $CandidateImage
    Invoke-Docker @('compose', '-f', $resolvedCompose, 'config', '--quiet') 'Compose validation failed'

    $timestamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ')
    $backup = "$resolvedCompose.pre-${timestamp}.bak"
    if ($PSCmdlet.ShouldProcess($resolvedCompose, "Back up Compose to $backup")) {
        Copy-Item -LiteralPath $resolvedCompose -Destination $backup -ErrorAction Stop
    }
    if (-not $PSCmdlet.ShouldProcess($Service, "Deploy candidate $CandidateImage")) {
        return
    }

    Invoke-Docker @('compose', '-f', $resolvedCompose, 'pull', $Service) 'Candidate pull failed'
    $candidateDeploymentStarted = $true
    Invoke-Docker @(
        'compose', '-f', $resolvedCompose, 'up', '-d', '--force-recreate', $Service
    ) 'Candidate deployment failed'

    Test-DeployedService $CandidateRevision 'Candidate'
    Write-Output "candidate-deployment-ok: revision=$CandidateRevision backup=$backup"
} catch {
    $candidateFailure = $_
    if (-not $candidateDeploymentStarted) {
        throw "Candidate preflight failed; the running service was unchanged. $($candidateFailure.Exception.Message)"
    }
    if ($PSCmdlet.ShouldProcess($Service, "Restore rollback image $RollbackImage")) {
        Set-ComposeImage $RollbackImage
        try {
            Invoke-Docker @('compose', '-f', $resolvedCompose, 'pull', $Service) 'Rollback pull failed'
            Invoke-Docker @(
                'compose', '-f', $resolvedCompose, 'up', '-d', '--force-recreate', $Service
            ) 'Rollback deployment failed'
            Test-DeployedService $RollbackRevision 'Rollback'
        } catch {
            throw "Candidate failed and rollback health verification also failed. Candidate failure: $($candidateFailure.Exception.Message)"
        }
    }
    throw "Candidate verification failed; rollback was requested. $($candidateFailure.Exception.Message)"
} finally {
    $env:TWITCH_MINER_IMAGE = $oldImage
    $env:TWITCH_MINER_DATA_DIR = $oldDataDir
    $env:UID = $oldUid
    $env:GID = $oldGid
}
