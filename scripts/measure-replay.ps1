param(
    [int]$Iterations = 5,
    [int]$WorkloadRepetitions = 20,
    [string]$OutputPath = "./replay-performance-report.json"
)

$ErrorActionPreference = 'Stop'
if ($Iterations -lt 1 -or $Iterations -gt 20) {
    throw 'Iterations must be between 1 and 20.'
}
if ($WorkloadRepetitions -lt 1 -or $WorkloadRepetitions -gt 100) {
    throw 'WorkloadRepetitions must be between 1 and 100.'
}

cargo build -p tm-integration-tests --example replay_benchmark --release --locked
if ($LASTEXITCODE -ne 0) {
    throw 'Replay benchmark build failed.'
}

$binaryName = if ($env:OS -eq 'Windows_NT') {
    'replay_benchmark.exe'
} else {
    'replay_benchmark'
}
$binary = (Resolve-Path -LiteralPath (Join-Path 'target/release/examples' $binaryName)).Path
$samples = [System.Collections.Generic.List[object]]::new()
$replays = [System.Collections.Generic.List[object]]::new()

for ($iteration = 1; $iteration -le $Iterations; $iteration++) {
    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $binary
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.EnvironmentVariables['TM_REPLAY_REPETITIONS'] = $WorkloadRepetitions.ToString()
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $wallClock = [System.Diagnostics.Stopwatch]::StartNew()
    if (-not $process.Start()) {
        throw 'Replay benchmark process did not start.'
    }
    $peakWorkingSet = 0L
    while (-not $process.WaitForExit(20)) {
        $process.Refresh()
        $peakWorkingSet = [Math]::Max($peakWorkingSet, $process.WorkingSet64)
    }
    $stdout = $process.StandardOutput.ReadToEnd()
    $stderr = $process.StandardError.ReadToEnd()
    $wallClock.Stop()
    if ($process.ExitCode -ne 0) {
        throw "Replay benchmark failed with exit code $($process.ExitCode): $stderr"
    }
    $replay = $stdout | ConvertFrom-Json
    if ($replay.schema -ne 2 -or
        $replay.repetitions -ne $WorkloadRepetitions -or
        @($replay.workloads).Count -ne 4 -or
        @($replay.snapshot_profiles).Count -ne 3) {
        throw "Replay benchmark returned an unexpected report shape in iteration $iteration."
    }
    $replays.Add($replay)
    $samples.Add([pscustomobject]@{
            wall_milliseconds = $wallClock.Elapsed.TotalMilliseconds
            cpu_milliseconds = $process.TotalProcessorTime.TotalMilliseconds
            peak_working_set_mb = $peakWorkingSet / 1MB
        })
    $process.Dispose()
}

function Get-Percentile([double[]]$Values, [int]$Percentile) {
    $ordered = $Values | Sort-Object
    $index = [Math]::Ceiling($ordered.Count * $Percentile / 100.0) - 1
    return $ordered[[Math]::Max(0, [Math]::Min($index, $ordered.Count - 1))]
}

function Get-Distribution([double[]]$Values) {
    return [ordered]@{
        p50 = Get-Percentile $Values 50
        p95 = Get-Percentile $Values 95
        p99 = Get-Percentile $Values 99
    }
}

function Get-ThroughputDistribution([double[]]$Values) {
    return [ordered]@{
        p05 = Get-Percentile $Values 5
        p50 = Get-Percentile $Values 50
        p95 = Get-Percentile $Values 95
    }
}

$workloadSummaries = foreach ($streamerCount in @(1, 10, 50, 200)) {
    $runs = @($replays | ForEach-Object {
            $_.workloads | Where-Object { $_.streamers -eq $streamerCount }
        })
    if ($runs.Count -ne $Iterations) {
        throw "Replay benchmark omitted the $streamerCount-streamer workload."
    }
    [ordered]@{
        streamers = $streamerCount
        process_run_count = $runs.Count
        inner_run_count = ($runs.run_count | Measure-Object -Sum).Sum
        latency_sample_count = ($runs.latency_sample_count | Measure-Object -Sum).Sum
        latency_p50_micros = Get-Distribution @($runs.latency.p50_micros)
        latency_p95_micros = Get-Distribution @($runs.latency.p95_micros)
        latency_p99_micros = Get-Distribution @($runs.latency.p99_micros)
        throughput_commands_per_second = Get-ThroughputDistribution @(
            $runs.throughput_commands_per_second.p50
        )
        recovery_snapshot_p95_micros = Get-Distribution @(
            $runs.recovery_snapshot_latency.p95_micros
        )
        max_queue_depth = ($runs.metrics.max_queue_depth | Measure-Object -Maximum).Maximum
        campaign_pin_present = -not ($runs.campaign_pin_present -contains $false)
    }
}

$snapshotSummaries = foreach ($streamerCount in @(17, 100, 1000)) {
    $runs = @($replays | ForEach-Object {
            $_.snapshot_profiles | Where-Object { $_.streamers -eq $streamerCount }
        })
    if ($runs.Count -ne $Iterations) {
        throw "Replay benchmark omitted the $streamerCount-streamer snapshot profile."
    }
    [ordered]@{
        streamers = $streamerCount
        process_run_count = $runs.Count
        inner_run_count = ($runs.run_count | Measure-Object -Sum).Sum
        sample_count = ($runs.sample_count | Measure-Object -Sum).Sum
        clone_latency_p50_micros = Get-Distribution @($runs.clone_latency.p50_micros)
        clone_latency_p95_micros = Get-Distribution @($runs.clone_latency.p95_micros)
        clone_latency_p99_micros = Get-Distribution @($runs.clone_latency.p99_micros)
    }
}

$revision = (git rev-parse --short=12 HEAD).Trim()
$result = [ordered]@{
    schema = 2
    measured_at_utc = [DateTime]::UtcNow.ToString('o')
    revision = $revision
    worktree_dirty = -not [string]::IsNullOrWhiteSpace((git status --porcelain) -join "`n")
    iterations = $Iterations
    workload_repetitions = $WorkloadRepetitions
    process = [ordered]@{
        wall_milliseconds = [ordered]@{
            p50 = Get-Percentile $samples.wall_milliseconds 50
            p95 = Get-Percentile $samples.wall_milliseconds 95
        }
        cpu_milliseconds = [ordered]@{
            p50 = Get-Percentile $samples.cpu_milliseconds 50
            p95 = Get-Percentile $samples.cpu_milliseconds 95
        }
        peak_working_set_mb = [ordered]@{
            p50 = Get-Percentile $samples.peak_working_set_mb 50
            p95 = Get-Percentile $samples.peak_working_set_mb 95
        }
        allocations = 'Not instrumented: production and benchmark code remain unsafe-free, and no allocator-only dependency is justified by the sub-millisecond 1,000-streamer snapshot profile.'
    }
    replay = [ordered]@{
        schema = 1
        process_run_count = $Iterations
        inner_repetitions_per_process = $WorkloadRepetitions
        total_inner_repetitions = $Iterations * $WorkloadRepetitions
        workloads = @($workloadSummaries)
        snapshot_profiles = @($snapshotSummaries)
    }
    replay_runs = @($replays)
}

$result | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $OutputPath -Encoding utf8
Write-Output "replay-performance-report-written: $OutputPath"
