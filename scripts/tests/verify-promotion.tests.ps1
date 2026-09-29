$ErrorActionPreference = 'Stop'
$workflow = Get-Content -Raw "$PSScriptRoot/../../.github/workflows/promote-release.yml"
$offset = $workflow.IndexOf('          foreach ($tag in @($release, $latest)) {')
if ($offset -lt 0) { throw 'Promoted-tag verification block missing.' }
$verify = [scriptblock]::Create($workflow.Substring($offset))
$release = 'example:v0.2.0'
$latest = 'example:latest'
$previousDigest = $env:MANIFEST_DIGEST
$previousExitCode = $global:LASTEXITCODE
try {
    $env:MANIFEST_DIGEST = 'sha256:' + ('a' * 64)
    function docker {
        $script:calls++
        $global:LASTEXITCODE = $script:inspectExit
        if ($args[-1] -eq $latest) { return $script:latestOutput }
        return "Digest: $env:MANIFEST_DIGEST"
    }
    foreach ($case in @('matching', 'wrong', 'missing', 'failed')) {
        $script:calls = 0
        $script:inspectExit = if ($case -eq 'failed') { 1 } else { 0 }
        $script:latestOutput = switch ($case) {
            'wrong' { 'Digest: sha256:' + ('b' * 64) }
            'missing' { 'No digest returned' }
            default { "Digest: $env:MANIFEST_DIGEST" }
        }
        $rejected = $false
        try { & $verify } catch { $rejected = $true }
        if ($rejected -ne ($case -ne 'matching')) {
            throw "Unexpected promoted-tag verification result: $case"
        }
        if ($case -eq 'matching' -and $script:calls -ne 2) {
            throw 'Both stable aliases must be checked.'
        }
    }
} finally {
    $env:MANIFEST_DIGEST = $previousDigest
    $global:LASTEXITCODE = $previousExitCode
}
Write-Output 'promotion-verifier-tests-ok'
