param(
    [Parameter(Mandatory)]
    [string]$ReportPath,
    [Parameter(Mandatory)]
    [string]$BaselinePath
)

$ErrorActionPreference = 'Stop'

function Read-JsonFile([string]$Path, [string]$Label) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "$Label does not exist: $Path"
    }
    try {
        return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    } catch {
        throw "$Label is not valid JSON: $Path"
    }
}

function Get-ReplayRow([object[]]$Rows, [int]$StreamerCount, [string]$Label) {
    $matches = @($Rows | Where-Object { $_.streamers -eq $StreamerCount })
    if ($matches.Count -ne 1) {
        throw "$Label must contain exactly one $StreamerCount-streamer row."
    }
    return $matches[0]
}

function Assert-PositiveNumber([object]$Value, [string]$Label) {
    if ($null -eq $Value -or [double]$Value -le 0 -or
        [double]::IsNaN([double]$Value) -or
        [double]::IsInfinity([double]$Value)) {
        throw "$Label must be a finite positive number."
    }
}

$report = Read-JsonFile $ReportPath 'Replay report'
$baseline = Read-JsonFile $BaselinePath 'Replay baseline'
if ($report.schema -ne 2 -or $report.replay.schema -ne 1) {
    throw 'Replay report schema is not supported.'
}
if ($baseline.schema -ne 1 -or
    [string]::IsNullOrWhiteSpace($baseline.environment) -or
    [string]::IsNullOrWhiteSpace($baseline.source_revision)) {
    throw 'Replay baseline schema or provenance is invalid.'
}
foreach ($name in @('hardware_class', 'operating_system', 'architecture')) {
    if ([string]::IsNullOrWhiteSpace($baseline.host.$name) -or
        $report.host.$name -ne $baseline.host.$name) {
        throw "Replay report host field $name does not match the baseline."
    }
}
if ($report.worktree_dirty) {
    throw 'Replay regression evidence must come from a clean worktree.'
}

$workload = Get-ReplayRow @($report.replay.workloads) 200 'Replay workloads'
$snapshot = Get-ReplayRow @($report.replay.snapshot_profiles) 1000 'Snapshot profiles'
$measurements = [ordered]@{
    workload_200_latency_p95_micros = [double]$workload.latency_p95_micros.p95
    workload_200_throughput_commands_per_second = [double]$workload.throughput_commands_per_second.p50
    snapshot_1000_clone_p95_micros = [double]$snapshot.clone_latency_p95_micros.p95
    process_peak_working_set_mb = [double]$report.process.peak_working_set_mb.p95
}

foreach ($entry in $measurements.GetEnumerator()) {
    Assert-PositiveNumber $entry.Value "Report metric $($entry.Key)"
    Assert-PositiveNumber $baseline.measurements.($entry.Key) "Baseline metric $($entry.Key)"
}
foreach ($name in @('latency_max_multiplier', 'throughput_min_multiplier', 'rss_max_multiplier')) {
    Assert-PositiveNumber $baseline.tolerances.$name "Baseline tolerance $name"
}

$limits = [ordered]@{
    workload_200_latency_p95_micros = [double]$baseline.measurements.workload_200_latency_p95_micros *
        [double]$baseline.tolerances.latency_max_multiplier
    workload_200_throughput_commands_per_second = [double]$baseline.measurements.workload_200_throughput_commands_per_second *
        [double]$baseline.tolerances.throughput_min_multiplier
    snapshot_1000_clone_p95_micros = [double]$baseline.measurements.snapshot_1000_clone_p95_micros *
        [double]$baseline.tolerances.latency_max_multiplier
    process_peak_working_set_mb = [double]$baseline.measurements.process_peak_working_set_mb *
        [double]$baseline.tolerances.rss_max_multiplier
}

$failures = [System.Collections.Generic.List[string]]::new()
if ($measurements.workload_200_latency_p95_micros -gt $limits.workload_200_latency_p95_micros) {
    $failures.Add('200-streamer p95 command latency regressed')
}
if ($measurements.workload_200_throughput_commands_per_second -lt
    $limits.workload_200_throughput_commands_per_second) {
    $failures.Add('200-streamer throughput regressed')
}
if ($measurements.snapshot_1000_clone_p95_micros -gt
    $limits.snapshot_1000_clone_p95_micros) {
    $failures.Add('1,000-streamer p95 snapshot latency regressed')
}
if ($measurements.process_peak_working_set_mb -gt $limits.process_peak_working_set_mb) {
    $failures.Add('peak working set regressed')
}
if ($failures.Count -gt 0) {
    throw "Replay regression gate failed: $($failures -join '; ')."
}

Write-Output (
    "replay-regression-ok: environment=$($baseline.environment) " +
    "baseline_revision=$($baseline.source_revision) report_revision=$($report.revision)"
)
