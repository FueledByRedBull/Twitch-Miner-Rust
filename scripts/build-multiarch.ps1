param(
    [string]$Image = "ghcr.io/fueledbyredbull/twitch-miner-rust",
    [string]$Tag = "latest",
    [switch]$Push
)

$platforms = "linux/amd64,linux/arm64,linux/arm/v7"
$args = @(
    "buildx", "build",
    "--platform", $platforms,
    "--tag", "$Image`:$Tag"
)

if ($Push) {
    $args += "--push"
} else {
    $args += "--load"
}

$args += "."

docker @args
