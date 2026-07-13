param(
    [string]$GoRoot
)

$ErrorActionPreference = 'Stop'

$rustRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($GoRoot)) {
    $GoRoot = Join-Path (Split-Path -Parent $rustRoot) 'Twitch-Channel-Points-Miner'
}

$goRoot = (Resolve-Path -LiteralPath $GoRoot).Path
$goConstants = Join-Path $goRoot 'TwitchChannelPointsMiner/constants/constants.go'
$rustOperations = Join-Path $rustRoot 'crates/tm-twitch/src/operations.rs'
if (-not (Test-Path -LiteralPath $goConstants -PathType Leaf)) {
    throw "Go operation source not found: $goConstants"
}
if (-not (Test-Path -LiteralPath $rustOperations -PathType Leaf)) {
    throw "Rust operation source not found: $rustOperations"
}

if (-not (Get-Command go -ErrorAction SilentlyContinue)) {
    throw 'Go is required for the baseline test gate. Install Go 1.21+ or run this script on the prepared Pi/CI environment.'
}

Push-Location $goRoot
try {
    # The pinned Go test seeds exactly two minutes, but the implementation resets
    # when execution overhead makes the elapsed duration greater than two minutes.
    # A deterministic replacement is injected with the parity harness below.
    go test ./... -skip '^TestStreamWatchProgress$'
    if ($LASTEXITCODE -ne 0) {
        throw "Go baseline tests failed with exit code $LASTEXITCODE."
    }
} finally {
    Pop-Location
}

$parityVector = Join-Path $rustRoot 'tests/parity/vectors.json'
if (-not (Test-Path -LiteralPath $parityVector -PathType Leaf)) {
    throw "Parity vector file not found: $parityVector"
}
$parityMappings = @(
    @{ Source = 'go/settings_test.go'; Destination = 'tm_rust_parity_settings_test.go' },
    @{ Source = 'go/prediction_test.go'; Destination = 'TwitchChannelPointsMiner/classes/tm_rust_parity_prediction_test.go' },
    @{ Source = 'go/event_test.go'; Destination = 'TwitchChannelPointsMiner/classes/tm_rust_parity_event_test.go' },
    @{ Source = 'go/watch_test.go'; Destination = 'TwitchChannelPointsMiner/tm_rust_parity_watch_test.go' },
    @{ Source = 'go/entities_watch_progress_test.go'; Destination = 'TwitchChannelPointsMiner/classes/entities/tm_rust_parity_watch_progress_test.go' },
    @{ Source = 'go/points_test.go'; Destination = 'TwitchChannelPointsMiner/tm_rust_parity_points_test.go' }
)
$createdParityFiles = @()
$oldParityVector = $env:TM_PARITY_VECTOR
try {
    foreach ($mapping in $parityMappings) {
        $source = Join-Path $rustRoot "tests/parity/$($mapping.Source)"
        $destination = Join-Path $goRoot $mapping.Destination
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "Parity harness source not found: $source"
        }
        if (Test-Path -LiteralPath $destination) {
            throw "Refusing to overwrite an existing Go parity file: $destination"
        }
        $destinationParent = Split-Path -Parent $destination
        if (-not (Test-Path -LiteralPath $destinationParent -PathType Container)) {
            throw "Go parity destination directory not found: $destinationParent"
        }
        Copy-Item -LiteralPath $source -Destination $destination
        $createdParityFiles += $destination
    }
    $env:TM_PARITY_VECTOR = $parityVector
    Push-Location $goRoot
    try {
        go test ./... -run '^TestParity' -count=1
        if ($LASTEXITCODE -ne 0) {
            throw "Go/Rust normalized parity fixtures failed with exit code $LASTEXITCODE."
        }
    } finally {
        Pop-Location
    }
} finally {
    foreach ($path in $createdParityFiles) {
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            Remove-Item -LiteralPath $path -Force
        }
    }
    $env:TM_PARITY_VECTOR = $oldParityVector
}

$goText = Get-Content -Raw -LiteralPath $goConstants
$rustText = Get-Content -Raw -LiteralPath $rustOperations
$goMatches = [regex]::Matches(
    $goText,
    'newPersistedOperation\("(?<name>[^"]+)",\s*"(?<hash>[0-9a-f]{64})"'
)
$rustMatches = [regex]::Matches(
    $rustText,
    '(?ms)operation_name:\s*"(?<name>[^"]+)"\s*,\s*sha256_hash:\s*"(?<hash>[0-9a-f]{64})"'
)

function Convert-ToContractMap([System.Text.RegularExpressions.MatchCollection]$Matches, [string]$Label) {
    $map = @{}
    foreach ($match in $Matches) {
        $name = $match.Groups['name'].Value
        if ($map.ContainsKey($name)) {
            throw "Duplicate $Label operation: $name"
        }
        $map[$name] = $match.Groups['hash'].Value
    }
    return $map
}

$goMap = Convert-ToContractMap $goMatches 'Go'
$rustMap = Convert-ToContractMap $rustMatches 'Rust'
$allowedGoOnly = @(
    'DropCampaignDetails',
    'ModViewChannelQuery',
    'PersonalSections',
    'PlaybackAccessToken'
)

$missing = @($goMap.Keys | Where-Object { -not $rustMap.ContainsKey($_) } | Sort-Object)
$extra = @($rustMap.Keys | Where-Object { -not $goMap.ContainsKey($_) } | Sort-Object)
$mismatches = @($goMap.Keys | Where-Object {
    $rustMap.ContainsKey($_) -and $goMap[$_] -ne $rustMap[$_]
} | Sort-Object)
$unexpectedMissing = @($missing | Where-Object { $_ -notin $allowedGoOnly })
$unexpectedGoOnly = @($allowedGoOnly | Where-Object { $_ -notin $missing })

if ($unexpectedMissing.Count -or $unexpectedGoOnly.Count -or $extra.Count -or $mismatches.Count) {
    throw @"
Go/Rust contract comparison failed.
Unexpected Go-only: $($unexpectedMissing -join ', ')
Missing documented Go-only: $($unexpectedGoOnly -join ', ')
Extra Rust operations: $($extra -join ', ')
Hash mismatches: $($mismatches -join ', ')
"@
}

Write-Output "go-baseline-ok: $($goMap.Count) Go definitions, $($rustMap.Count) active Rust definitions, $($missing.Count) documented Go-only definitions"
