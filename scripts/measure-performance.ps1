param(
    [string]$Binary = "./target/release/tm-app.exe",
    [int]$Iterations = 5,
    [string]$OutputPath = "",
    [int]$ProcessId = 0,
    [int]$SampleSeconds = 0,
    [string]$Label = "unspecified"
)

$ErrorActionPreference = 'Stop'
if ($Iterations -lt 1 -or $Iterations -gt 100) {
    throw 'Iterations must be between 1 and 100.'
}
if ($ProcessId -eq 0 -and $SampleSeconds -ne 0) {
    throw 'SampleSeconds requires ProcessId.'
}
if ($ProcessId -ne 0 -and ($SampleSeconds -lt 1 -or $SampleSeconds -gt 3600)) {
    throw 'SampleSeconds must be between 1 and 3600 when ProcessId is supplied.'
}
if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
    throw "Release binary not found: $Binary. Build with cargo build --workspace --release --locked first."
}

$samples = [System.Collections.Generic.List[double]]::new()
for ($index = 0; $index -lt $Iterations; $index++) {
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    & $Binary --help *> $null
    $exitCode = $LASTEXITCODE
    $watch.Stop()
    if ($exitCode -ne 0) {
        throw "Startup smoke command failed with exit code $exitCode."
    }
    $samples.Add($watch.Elapsed.TotalMilliseconds)
}

$resourceSummary = $null
if ($ProcessId -ne 0) {
    $process = Get-Process -Id $ProcessId -ErrorAction Stop
    $resourceSamples = [System.Collections.Generic.List[object]]::new()
    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    $previousCpuMs = $process.TotalProcessorTime.TotalMilliseconds
    for ($index = 0; $index -lt $SampleSeconds; $index++) {
        Start-Sleep -Seconds 1
        $process.Refresh()
        if ($process.HasExited) {
            throw "Process $ProcessId exited before resource sampling completed."
        }
        $elapsedMs = $clock.Elapsed.TotalMilliseconds
        $cpuMs = $process.TotalProcessorTime.TotalMilliseconds
        $cpuPercent = (($cpuMs - $previousCpuMs) / [Math]::Max($elapsedMs, 1) * 100.0) / [Environment]::ProcessorCount
        $resourceSamples.Add([pscustomobject]@{
                working_set_mb = $process.WorkingSet64 / 1MB
                cpu_percent = [Math]::Max($cpuPercent, 0)
            })
        $previousCpuMs = $cpuMs
        $clock.Restart()
    }

    $resourceSummary = [ordered]@{
        label = $Label
        process_id = $ProcessId
        sample_seconds = $SampleSeconds
        working_set_mb = [ordered]@{
            min = ($resourceSamples.working_set_mb | Measure-Object -Minimum).Minimum
            median = ($resourceSamples.working_set_mb | Sort-Object | Select-Object -Index ([Math]::Floor($resourceSamples.Count / 2)))
            max = ($resourceSamples.working_set_mb | Measure-Object -Maximum).Maximum
        }
        cpu_percent = [ordered]@{
            min = ($resourceSamples.cpu_percent | Measure-Object -Minimum).Minimum
            median = ($resourceSamples.cpu_percent | Sort-Object | Select-Object -Index ([Math]::Floor($resourceSamples.Count / 2)))
            max = ($resourceSamples.cpu_percent | Measure-Object -Maximum).Maximum
        }
    }
}

$revision = (git rev-parse --short=12 HEAD).Trim()
$worktreeDirty = -not [string]::IsNullOrWhiteSpace((git status --porcelain --untracked-files=normal) -join "`n")
$binaryItem = Get-Item -LiteralPath $Binary -ErrorAction Stop
$binaryVersion = (& $Binary --version 2>&1) -join "`n"
if ($LASTEXITCODE -ne 0) {
    throw "Version command failed with exit code $LASTEXITCODE."
}
$rustcVersion = (& rustc --version 2>&1) -join "`n"
if ($LASTEXITCODE -ne 0) {
    throw "Rust compiler version check failed with exit code $LASTEXITCODE."
}
$metadata = cargo metadata --locked --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) {
    throw "Cargo metadata check failed with exit code $LASTEXITCODE."
}
$result = [ordered]@{
    measured_at_utc = [DateTime]::UtcNow.ToString('o')
    revision = $revision
    worktree_dirty = $worktreeDirty
    binary_version = $binaryVersion.Trim()
    binary_size_bytes = $binaryItem.Length
    host = [ordered]@{
        os = [System.Runtime.InteropServices.RuntimeInformation]::OSDescription
        architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        logical_processors = [Environment]::ProcessorCount
        rustc = $rustcVersion.Trim()
    }
    workspace = [ordered]@{
        package_count = $metadata.workspace_members.Count
        resolved_package_count = $metadata.packages.Count
    }
    command = "$Binary --help"
    iterations = $Iterations
    startup_ms = [ordered]@{
        min = ($samples | Measure-Object -Minimum).Minimum
        median = ($samples | Sort-Object | Select-Object -Index ([Math]::Floor($samples.Count / 2)))
        max = ($samples | Measure-Object -Maximum).Maximum
    }
    resource_sampling = $resourceSummary
    runtime_metrics = 'Run a real session and inspect --status for queue depth, command wait, event throughput, and transport-to-state latency.'
    go_comparison = 'Run the same fixture/workload with the adjacent Go baseline when Go 1.21+ is available.'
}

$json = $result | ConvertTo-Json -Depth 5
if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $json
} else {
    $json | Set-Content -LiteralPath $OutputPath -Encoding utf8
    Write-Output "performance-report-written: $OutputPath"
}
