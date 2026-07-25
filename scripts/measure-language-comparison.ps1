param(
    [Parameter(Mandatory = $true)]
    [string]$GoRoot,
    [string]$GoExecutable = "go",
    [int]$Iterations = 2000000,
    [int]$Runs = 5,
    [int]$StartupRuns = 30,
    [string]$OutputPath = "./target/language-comparison.json"
)

$ErrorActionPreference = 'Stop'
if ($Iterations -lt 100000 -or $Iterations -gt 100000000) {
    throw 'Iterations must be between 100,000 and 100,000,000.'
}
if ($Runs -lt 3 -or $Runs -gt 25) {
    throw 'Runs must be between 3 and 25.'
}
if ($StartupRuns -lt 5 -or $StartupRuns -gt 100) {
    throw 'StartupRuns must be between 5 and 100.'
}

$rustRoot = (Resolve-Path -LiteralPath (Split-Path -Parent $PSScriptRoot)).Path
$goRoot = (Resolve-Path -LiteralPath $GoRoot).Path
$goCommand = Get-Command $GoExecutable -ErrorAction Stop
$goMain = Join-Path $goRoot 'main.go'
$goModule = Join-Path $goRoot 'go.mod'
if (-not (Test-Path -LiteralPath $goMain -PathType Leaf) -or
    -not (Test-Path -LiteralPath $goModule -PathType Leaf)) {
    throw "Go baseline root is missing main.go or go.mod: $goRoot"
}

$extension = if ($env:OS -eq 'Windows_NT') { '.exe' } else { '' }
$target = Join-Path $rustRoot 'target'
$goHarnessDirectory = Join-Path $goRoot 'cmd/tm-rust-language-comparison'
$goHarnessDestination = Join-Path $goHarnessDirectory 'main.go'
$goHarnessSource = Join-Path $rustRoot 'tests/performance/go-language-comparison/main.go'
$goBenchmarkBinary = Join-Path $target "go-language-comparison$extension"
$goMinerBinary = Join-Path $target "go-baseline-miner$extension"
$rustBenchmarkBinary = Join-Path $target "release/examples/language_comparison$extension"
$rustMinerBinary = Join-Path $target "release/tm-app$extension"
if ((Test-Path -LiteralPath $goHarnessDirectory) -and
    (Get-ChildItem -LiteralPath $goHarnessDirectory -Force)) {
    throw "Refusing to overwrite non-empty Go benchmark path: $goHarnessDirectory"
}

function Measure-Launch(
    [string]$Path,
    [string]$Argument,
    [int]$Count
) {
    $samples = [System.Collections.Generic.List[double]]::new()
    for ($index = 0; $index -lt $Count; $index++) {
        $watch = [System.Diagnostics.Stopwatch]::StartNew()
        $oldErrorPreference = $ErrorActionPreference
        try {
            $ErrorActionPreference = 'Continue'
            & $Path $Argument *> $null
            $exitCode = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $oldErrorPreference
        }
        $watch.Stop()
        if ($exitCode -ne 0) {
            throw "Startup command failed with exit code ${exitCode}: $Path $Argument"
        }
        $samples.Add($watch.Elapsed.TotalMilliseconds)
    }
    $ordered = @($samples | Sort-Object)
    return [ordered]@{
        runs = $Count
        minimum_ms = $ordered[0]
        median_ms = $ordered[[Math]::Floor(($ordered.Count - 1) / 2)]
        p95_ms = $ordered[[Math]::Ceiling($ordered.Count * 0.95) - 1]
        maximum_ms = $ordered[-1]
    }
}

$oldIterations = $env:TM_LANGUAGE_BENCHMARK_ITERATIONS
$oldRuns = $env:TM_LANGUAGE_BENCHMARK_RUNS
$oldBuildRevision = $env:BUILD_REVISION
try {
    $rustRevision = (git -C $rustRoot rev-parse HEAD).Trim()
    $goRevision = (git -C $goRoot rev-parse HEAD).Trim()
    $rustDirty = -not [string]::IsNullOrWhiteSpace(
        (git -C $rustRoot status --porcelain --untracked-files=normal) -join "`n"
    )
    $goDirty = -not [string]::IsNullOrWhiteSpace(
        (git -C $goRoot status --porcelain --untracked-files=normal) -join "`n"
    )

    $env:BUILD_REVISION = $rustRevision
    Push-Location $rustRoot
    try {
        cargo build -p tm-app --release --locked
        if ($LASTEXITCODE -ne 0) {
            throw "Rust miner release build failed with exit code $LASTEXITCODE."
        }
        cargo build -p tm-integration-tests --example language_comparison --release --locked
        if ($LASTEXITCODE -ne 0) {
            throw "Rust comparison release build failed with exit code $LASTEXITCODE."
        }
    } finally {
        Pop-Location
    }

    New-Item -ItemType Directory -Path $goHarnessDirectory -Force | Out-Null
    Copy-Item -LiteralPath $goHarnessSource -Destination $goHarnessDestination
    Push-Location $goRoot
    try {
        & $goCommand.Source build -trimpath -ldflags "-s -w" -o $goMinerBinary .
        if ($LASTEXITCODE -ne 0) {
            throw "Go miner build failed with exit code $LASTEXITCODE."
        }
        & $goCommand.Source build -trimpath `
            -ldflags "-s -w -X main.revision=$goRevision" `
            -o $goBenchmarkBinary ./cmd/tm-rust-language-comparison
        if ($LASTEXITCODE -ne 0) {
            throw "Go comparison build failed with exit code $LASTEXITCODE."
        }
    } finally {
        Pop-Location
    }

    $env:TM_LANGUAGE_BENCHMARK_ITERATIONS = $Iterations
    $env:TM_LANGUAGE_BENCHMARK_RUNS = $Runs
    $rustBenchmark = (& $rustBenchmarkBinary | ConvertFrom-Json)
    if ($LASTEXITCODE -ne 0) {
        throw "Rust comparison failed with exit code $LASTEXITCODE."
    }
    $goBenchmark = (& $goBenchmarkBinary | ConvertFrom-Json)
    if ($LASTEXITCODE -ne 0) {
        throw "Go comparison failed with exit code $LASTEXITCODE."
    }
    $rustDecision = $rustBenchmark.decision_output | ConvertTo-Json -Compress
    $goDecision = $goBenchmark.decision_output | ConvertTo-Json -Compress
    if ($rustBenchmark.schema -ne 3 -or
        $goBenchmark.schema -ne 3 -or
        $rustBenchmark.workload -ne $goBenchmark.workload -or
        $rustBenchmark.iterations_per_run -ne $goBenchmark.iterations_per_run -or
        $rustBenchmark.runs -ne $goBenchmark.runs -or
        $rustBenchmark.checksum -ne $goBenchmark.checksum -or
        $rustBenchmark.semantic_checksum -ne $goBenchmark.semantic_checksum -or
        $rustDecision -ne $goDecision) {
        throw 'The Rust and Go comparison workloads produced different results.'
    }

    $report = [ordered]@{
        schema = 3
        measured_at_utc = [DateTime]::UtcNow.ToString('o')
        host = [ordered]@{
            os = [System.Runtime.InteropServices.RuntimeInformation]::OSDescription
            architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
            logical_processors = [Environment]::ProcessorCount
        }
        toolchains = [ordered]@{
            rust = ((rustc --version) -join "`n").Trim()
            go = ((& $goCommand.Source version) -join "`n").Trim()
        }
        rust = [ordered]@{
            revision = $rustRevision
            dirty = $rustDirty
            binary_size_bytes = (Get-Item -LiteralPath $rustMinerBinary).Length
            startup = Measure-Launch $rustMinerBinary '--help' $StartupRuns
            prediction_decision = $rustBenchmark
        }
        go = [ordered]@{
            revision = $goRevision
            dirty = $goDirty
            binary_size_bytes = (Get-Item -LiteralPath $goMinerBinary).Length
            startup = Measure-Launch $goMinerBinary '-h' $StartupRuns
            prediction_decision = $goBenchmark
        }
        comparability = [ordered]@{
            equivalent = @(
                'stripped release binary size',
                'CLI parse-and-help process startup',
                'complete production MOST_VOTED decision with identical varying sanitized inputs',
                'full choice, outcome ID, amount, operation checksum, and all-decision semantic checksum'
            )
            not_equivalent = @(
                'Rust is compiled with the production size-oriented opt-level=z/LTO profile; Go uses its default speed optimizer and stripped symbols',
                'Rust uses exact i128 percentage arithmetic; Go uses float64 multiplication and truncation',
                'Rust materializes owned outcome-ID strings; Go copies shallow immutable string headers',
                'No allocation-free kernel is reported because the Go selector is private and exposing or duplicating it would distort production interfaces',
                'Rust actor replay: the Go baseline has no equivalent single-writer actor, bounded queue, or snapshot API',
                'live Twitch mining: concurrent account sessions would interfere and are not a safe benchmark'
            )
        }
    }
    $destination = [System.IO.Path]::GetFullPath((Join-Path $rustRoot $OutputPath))
    $destinationDirectory = Split-Path -Parent $destination
    New-Item -ItemType Directory -Path $destinationDirectory -Force | Out-Null
    $report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $destination -Encoding utf8
    Write-Output "language-comparison-written: $destination"
} finally {
    $env:TM_LANGUAGE_BENCHMARK_ITERATIONS = $oldIterations
    $env:TM_LANGUAGE_BENCHMARK_RUNS = $oldRuns
    $env:BUILD_REVISION = $oldBuildRevision
    if (Test-Path -LiteralPath $goHarnessDestination -PathType Leaf) {
        Remove-Item -LiteralPath $goHarnessDestination -Force
    }
    if ((Test-Path -LiteralPath $goHarnessDirectory -PathType Container) -and
        -not (Get-ChildItem -LiteralPath $goHarnessDirectory -Force)) {
        Remove-Item -LiteralPath $goHarnessDirectory -Force
    }
}
