[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$ImageReference,

    [Parameter(Mandatory = $true)]
    [ValidateSet('linux/amd64', 'linux/arm64')]
    [string]$Platform
)

$ErrorActionPreference = 'Stop'

if (-not $ImageReference.Contains('@')) {
    throw "Image reference must be immutable and include a digest: $ImageReference"
}

$buildDigest = $ImageReference.Split('@', 2)[1]
if ($buildDigest -notmatch '^sha256:[0-9a-f]{64}$') {
    throw "Image reference has an invalid digest: $ImageReference"
}

$raw = docker buildx imagetools inspect $ImageReference --raw 2>&1
if ($LASTEXITCODE -ne 0) {
    throw "Unable to inspect image ${ImageReference}: $($raw -join ' ')"
}
try {
    $document = ($raw -join "`n") | ConvertFrom-Json
} catch {
    throw "Image $ImageReference did not return valid manifest JSON."
}

# BuildKit returns an index when provenance/SBOM attestations are attached. The
# build action's digest is then the index digest, while the runtime image that
# the multiarch manifest and attestations must name is its platform child.
$manifests = @($document.manifests)
if ($null -ne $document.manifests -and $manifests.Count -gt 0) {
    $parts = $Platform.Split('/', 2)
    $candidates = @($manifests | Where-Object {
            $_.platform.os -eq $parts[0] -and
            $_.platform.architecture -eq $parts[1] -and
            $_.digest -match '^sha256:[0-9a-f]{64}$'
        })
    if ($candidates.Count -ne 1) {
        throw "Image $ImageReference must contain exactly one $Platform runtime child; found $($candidates.Count)."
    }
    Write-Output $candidates[0].digest
    return
}

# A build without an attached index is already a single-platform runtime
# manifest. Keep this fallback explicit so a future BuildKit output mode does
# not silently select an attestation descriptor.
if ($document.config.mediaType -and $document.config.digest) {
    Write-Output $buildDigest
    return
}

throw "Image $ImageReference was neither a platform manifest index nor a runtime manifest."
