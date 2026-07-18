[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$ImageReference,

    [ValidatePattern('^sha256:[0-9a-f]{64}$')]
    [string]$ExpectedDigest,

    [ValidatePattern('^[0-9a-f]{40}$')]
    [string]$ExpectedRevision
)

$ErrorActionPreference = 'Stop'

$description = docker buildx imagetools inspect $ImageReference 2>&1
if ($LASTEXITCODE -ne 0) {
    throw "Unable to inspect published manifest ${ImageReference}: $($description -join ' ')"
}

$digestMatch = [regex]::Match(
    ($description -join "`n"),
    '(?m)^Digest:\s+(sha256:[0-9a-f]{64})\s*$'
)
if (-not $digestMatch.Success) {
    throw "Published manifest ${ImageReference} did not report a digest."
}
$digest = $digestMatch.Groups[1].Value
if ($ExpectedDigest -and $digest -ne $ExpectedDigest) {
    throw "Published manifest ${ImageReference} resolved to $digest instead of $ExpectedDigest."
}

$raw = docker buildx imagetools inspect $ImageReference --raw 2>&1
if ($LASTEXITCODE -ne 0) {
    throw "Unable to inspect raw manifest ${ImageReference}: $($raw -join ' ')"
}
try {
    $index = ($raw -join "`n") | ConvertFrom-Json
} catch {
    throw "Published manifest ${ImageReference} was not valid JSON: $($raw -join ' ')"
}

if ($ImageReference.Contains('@')) {
    $imageName = $ImageReference.Split('@', 2)[0]
} else {
    $tagSeparator = $ImageReference.LastIndexOf(':')
    if ($tagSeparator -le $ImageReference.LastIndexOf('/')) {
        throw "Published image reference ${ImageReference} has no tag or digest."
    }
    $imageName = $ImageReference.Substring(0, $tagSeparator)
}

foreach ($platform in @('linux/amd64', 'linux/arm64', 'linux/arm/v7')) {
    $descriptor = switch ($platform) {
        'linux/amd64' {
            @($index.manifests | Where-Object {
                    $_.platform.os -eq 'linux' -and $_.platform.architecture -eq 'amd64'
                } | Select-Object -First 1)
        }
        'linux/arm64' {
            @($index.manifests | Where-Object {
                    $_.platform.os -eq 'linux' -and $_.platform.architecture -eq 'arm64'
                } | Select-Object -First 1)
        }
        'linux/arm/v7' {
            @($index.manifests | Where-Object {
                    $_.platform.os -eq 'linux' -and
                    $_.platform.architecture -eq 'arm' -and
                    $_.platform.variant -eq 'v7'
                } | Select-Object -First 1)
        }
    }
    if (-not $descriptor -or $descriptor.digest -notmatch '^sha256:[0-9a-f]{64}$') {
        throw "Published manifest ${ImageReference} has no child image for $platform."
    }

    $attestations = @($index.manifests | Where-Object {
            $_.platform.os -eq 'unknown' -and
            $_.platform.architecture -eq 'unknown' -and
            $_.annotations.'vnd.docker.reference.type' -eq 'attestation-manifest' -and
            $_.annotations.'vnd.docker.reference.digest' -eq $descriptor.digest
        })
    if ($attestations.Count -eq 0) {
        throw "Published manifest ${ImageReference} has no attestation for $platform."
    }

    $platformReference = "$imageName@$($descriptor.digest)"
    $help = docker run --rm --platform $platform $platformReference --help 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "Published manifest smoke test failed for ${platform}: $($help -join ' ')"
    }

    $version = docker run --rm --platform $platform $platformReference --version 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "Published manifest version check failed for ${platform}: $($version -join ' ')"
    }
    if ($ExpectedRevision -and ($version -join "`n") -notmatch [regex]::Escape($ExpectedRevision)) {
        throw "Published manifest $platform revision did not match $ExpectedRevision."
    }
    Write-Host "Verified $platform child image and attestation at $($descriptor.digest)."
}

Write-Output $digest
