param(
    [string]$Image,
    [string]$Tag,
    [switch]$Push
)

if ([string]::IsNullOrWhiteSpace($Image)) {
    $Image = if ($Push) {
        "ghcr.io/fueledbyredbull/twitch-miner-rust"
    } else {
        "twitch-miner-rust"
    }
}
$publishPlatforms = "linux/amd64,linux/arm64"
$buildRevision = (git rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $buildRevision -notmatch '^[0-9a-f]{40}$') {
    throw "Unable to determine the full source revision for build metadata."
}
$sourceDateEpoch = (git show -s --format=%ct HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $sourceDateEpoch -notmatch '^\d+$') {
    throw "Unable to determine SOURCE_DATE_EPOCH."
}
if ([string]::IsNullOrWhiteSpace($Tag)) {
    $Tag = if ($Push) { "candidate-$buildRevision" } else { "local" }
}
if ($Push) {
    $candidateTagPattern = '^candidate-' + [regex]::Escape($buildRevision) + '(?:-[A-Za-z0-9][A-Za-z0-9._-]*)?$'
    if ($Tag -notmatch $candidateTagPattern) {
        throw 'Pushed local images must use a candidate tag scoped to the exact full source revision. Stable image tags are published only by the protected Promote Release workflow.'
    }
}

function Get-LocalLinuxPlatform {
    $dockerPlatform = docker info --format '{{.OSType}}/{{.Architecture}}'
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($dockerPlatform)) {
        throw "Unable to determine the local Docker platform."
    }

    switch ($dockerPlatform.Trim()) {
        "linux/amd64" { return "linux/amd64" }
        "linux/x86_64" { return "linux/amd64" }
        "linux/arm64" { return "linux/arm64" }
        "linux/aarch64" { return "linux/arm64" }
        default {
            throw "Unsupported local Docker platform '$dockerPlatform'. Switch Docker to Linux containers or use -Push from a Linux builder."
        }
    }
}

$args = @(
    "buildx", "build",
    "--tag", "$Image`:$Tag",
    "--build-arg", "BUILD_REVISION=$buildRevision",
    "--build-arg", "SOURCE_DATE_EPOCH=$sourceDateEpoch"
)

if ($Push) {
    $args += "--platform", $publishPlatforms
    $args += "--provenance", "mode=max"
    $args += "--sbom", "true"
    $args += "--push"
} else {
    $args += "--platform", (Get-LocalLinuxPlatform)
    $args += "--load"
}

$args += "."

docker @args
