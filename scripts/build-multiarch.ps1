param(
    [string]$Image = "ghcr.io/fueledbyredbull/twitch-miner-rust",
    [string]$Tag = "latest",
    [switch]$Push
)

$publishPlatforms = "linux/amd64,linux/arm64,linux/arm/v7"

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
    "--tag", "$Image`:$Tag"
)

if ($Push) {
    $args += "--platform", $publishPlatforms
    $args += "--push"
} else {
    $args += "--platform", (Get-LocalLinuxPlatform)
    $args += "--load"
}

$args += "."

docker @args
