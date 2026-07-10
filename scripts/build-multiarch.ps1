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
if ([string]::IsNullOrWhiteSpace($Tag)) {
    $Tag = if ($Push) { "latest" } else { "local" }
}

$publishPlatforms = "linux/amd64,linux/arm64,linux/arm/v7"
$buildRevision = (git rev-parse --short=12 HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($buildRevision)) {
    throw "Unable to determine the source revision for build metadata."
}
$buildTime = [DateTime]::UtcNow.ToString("o")

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
        "linux/arm" { return "linux/arm/v7" }
        default {
            throw "Unsupported local Docker platform '$dockerPlatform'. Switch Docker to Linux containers or use -Push from a Linux builder."
        }
    }
}

$args = @(
    "buildx", "build",
    "--tag", "$Image`:$Tag",
    "--build-arg", "BUILD_REVISION=$buildRevision",
    "--build-arg", "BUILD_TIME=$buildTime"
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
