$ErrorActionPreference = 'Stop'

$root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$metadata = cargo metadata --locked --no-deps --format-version 1 |
    ConvertFrom-Json
if ($LASTEXITCODE -ne 0) {
    throw "cargo metadata failed with exit code $LASTEXITCODE."
}

$expectedInternalDependencies = [ordered]@{
    'tm-app' = @(
        'tm-auth',
        'tm-config',
        'tm-domain',
        'tm-irc',
        'tm-observability',
        'tm-pubsub',
        'tm-runtime',
        'tm-twitch'
    )
    'tm-auth' = @('tm-twitch')
    'tm-config' = @('tm-domain')
    'tm-domain' = @()
    'tm-irc' = @()
    'tm-observability' = @('tm-domain')
    'tm-pubsub' = @('tm-domain')
    'tm-runtime' = @('tm-config', 'tm-domain', 'tm-pubsub')
    'tm-twitch' = @('tm-domain')
}
$productionCrates = @($expectedInternalDependencies.Keys)

foreach ($crate in $productionCrates) {
    $package = $metadata.packages | Where-Object name -eq $crate
    if (@($package).Count -ne 1) {
        throw "Expected exactly one Cargo package named ${crate}."
    }
    $actual = @(
        $package.dependencies |
            Where-Object { $_.name -in $productionCrates } |
            ForEach-Object name |
            Sort-Object -Unique
    )
    $expected = @($expectedInternalDependencies[$crate] | Sort-Object -Unique)
    $difference = @(Compare-Object -ReferenceObject $expected -DifferenceObject $actual)
    if ($difference.Count -ne 0) {
        $detail = ($difference | ForEach-Object {
            "$($_.SideIndicator) $($_.InputObject)"
        }) -join ', '
        throw "Internal dependency boundary changed for ${crate}: ${detail}"
    }
}

$domainPackage = $metadata.packages | Where-Object name -eq 'tm-domain'
$forbiddenDomainDependencies = @(
    'anyhow',
    'clap',
    'futures-util',
    'reqwest',
    'tokio',
    'tokio-tungstenite',
    'tracing',
    'tracing-subscriber'
)
$domainViolations = @(
    $domainPackage.dependencies |
        Where-Object name -in $forbiddenDomainDependencies |
        ForEach-Object name
)
if ($domainViolations.Count -ne 0) {
    throw "tm-domain must remain transport and application agnostic: $($domainViolations -join ', ')"
}

$sourceTestFiles = @(
    Get-ChildItem -LiteralPath (Join-Path $root 'crates') -Recurse -File -Filter '*.rs' |
        Where-Object {
            $_.FullName -match '[\\/]src[\\/]' -and
            $_.Name -match '(^app_tests|_tests)\.rs$'
        }
)
if ($sourceTestFiles.Count -ne 0) {
    $relative = $sourceTestFiles | ForEach-Object {
        $_.FullName.Substring($root.Length).TrimStart('\', '/')
    }
    throw "Substantial test-only files belong outside production src trees: $($relative -join ', ')"
}

Write-Output 'architecture-boundaries-ok'
