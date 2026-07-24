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
$lastReplay = $null

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
    $lastReplay = $stdout | ConvertFrom-Json
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

$revision = (git rev-parse --short=12 HEAD).Trim()
$result = [ordered]@{
    schema = 1
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
    replay = $lastReplay
}

$result | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $OutputPath -Encoding utf8
Write-Output "replay-performance-report-written: $OutputPath"
