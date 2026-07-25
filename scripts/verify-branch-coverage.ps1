param(
    [Parameter(Mandatory)]
    [string]$ReportPath,
    [ValidateRange(0, 100)]
    [double]$MinimumPercent = 60
)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path -LiteralPath $ReportPath -PathType Leaf)) {
    throw "Branch coverage report does not exist: $ReportPath"
}
try {
    $report = Get-Content -LiteralPath $ReportPath -Raw | ConvertFrom-Json
} catch {
    throw "Branch coverage report is not valid JSON: $ReportPath"
}

$branches = $report.data[0].totals.branches
$count = [long]$branches.count
$covered = [long]$branches.covered
if ($count -le 0 -or $covered -lt 0 -or $covered -gt $count) {
    throw 'Branch coverage totals are missing or invalid.'
}
$percent = 100.0 * $covered / $count
if ($percent -lt $MinimumPercent) {
    throw (
        "Branch coverage $($percent.ToString('F2'))% is below the " +
        "$($MinimumPercent.ToString('F2'))% floor ($covered/$count)."
    )
}

Write-Output (
    "branch-coverage-ok: covered=$covered total=$count " +
    "percent=$($percent.ToString('F2')) minimum=$($MinimumPercent.ToString('F2'))"
)
