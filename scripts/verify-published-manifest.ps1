[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$ImageReference,

    [ValidatePattern('^sha256:[0-9a-f]{64}$')]
    [string]$ExpectedDigest,

    [ValidatePattern('^[0-9a-f]{40}$')]
    [string]$ExpectedRevision,

    [string]$Repository,

    [string]$SignerWorkflow,

    [string]$SourceRef = 'refs/heads/main',

    [switch]$VerifyAttestations
)

$ErrorActionPreference = 'Stop'

$tagSeparator = $ImageReference.LastIndexOf(':')
if ($ImageReference.Contains('@')) {
    $imageName = $ImageReference.Split('@', 2)[0]
} elseif ($tagSeparator -gt $ImageReference.LastIndexOf('/')) {
    $imageName = $ImageReference.Substring(0, $tagSeparator)
} else {
    throw "Published image reference ${ImageReference} has no tag or digest."
}

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

$immutableReference = "$imageName@$digest"
$raw = docker buildx imagetools inspect $immutableReference --raw 2>&1
if ($LASTEXITCODE -ne 0) {
    throw "Unable to inspect raw manifest ${immutableReference}: $($raw -join ' ')"
}
try {
    $index = ($raw -join "`n") | ConvertFrom-Json
} catch {
    throw "Published manifest ${immutableReference} was not valid JSON: $($raw -join ' ')"
}

$runtimeManifests = @($index.manifests | Where-Object {
        $_.platform.os -eq 'linux' -and
        $_.platform.architecture -in @('amd64', 'arm64')
    })
if ($runtimeManifests.Count -ne 2) {
    throw "Published manifest ${ImageReference} must contain exactly two Linux child images."
}
$unexpectedRuntimeManifests = @($index.manifests | Where-Object {
        ($_.platform.os -ne 'unknown' -or $_.platform.architecture -ne 'unknown') -and
        -not ($_.platform.os -eq 'linux' -and $_.platform.architecture -in @('amd64', 'arm64'))
    })
if ($unexpectedRuntimeManifests.Count -ne 0) {
    throw "Published manifest ${ImageReference} contains unsupported runtime platform descriptors."
}

foreach ($platform in @('linux/amd64', 'linux/arm64')) {
    $descriptor = switch ($platform) {
        'linux/amd64' {
            @($runtimeManifests | Where-Object {
                    $_.platform.os -eq 'linux' -and $_.platform.architecture -eq 'amd64'
                })
        }
        'linux/arm64' {
            @($runtimeManifests | Where-Object {
                    $_.platform.os -eq 'linux' -and $_.platform.architecture -eq 'arm64'
                })
        }
    }
    if ($descriptor.Count -ne 1 -or $descriptor[0].digest -notmatch '^sha256:[0-9a-f]{64}$') {
        throw "Published manifest ${ImageReference} has no child image for $platform."
    }
    $descriptor = $descriptor[0]

    $attestations = @($index.manifests | Where-Object {
            $_.platform.os -eq 'unknown' -and
            $_.platform.architecture -eq 'unknown' -and
            $_.annotations.'vnd.docker.reference.type' -eq 'attestation-manifest' -and
            $_.annotations.'vnd.docker.reference.digest' -eq $descriptor.digest
        })
    $embeddedPredicateTypes = @()
    foreach ($attestationDescriptor in $attestations) {
        if ($attestationDescriptor.digest -notmatch '^sha256:[0-9a-f]{64}$') {
            throw "Published manifest ${ImageReference} has an invalid attestation descriptor for $platform."
        }
        $attestationReference = "$imageName@$($attestationDescriptor.digest)"
        $attestationRaw = docker buildx imagetools inspect $attestationReference --raw 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "Unable to inspect attestation for ${platform}: $($attestationRaw -join ' ')"
        }
        try {
            $attestation = ($attestationRaw -join "`n") | ConvertFrom-Json
        } catch {
            throw "Attestation for ${platform} was not valid JSON."
        }
        $embeddedPredicateTypes += @($attestation.layers | ForEach-Object {
                $_.annotations.'in-toto.io/predicate-type'
            })
    }
    $requiredEmbeddedPredicates = @(
        'https://slsa.dev/provenance/v1',
        'https://spdx.dev/Document'
    )
    $missingEmbeddedPredicates = @($requiredEmbeddedPredicates | Where-Object {
            $embeddedPredicateTypes -notcontains $_
        })
    if ($missingEmbeddedPredicates.Count -gt 0 -and -not $VerifyAttestations) {
        if ($attestations.Count -eq 0) {
            throw "Published manifest ${ImageReference} has no embedded attestation for $platform."
        }
        throw "Published manifest ${ImageReference} is missing embedded $($missingEmbeddedPredicates -join ', ') evidence for $platform."
    }
    if ($missingEmbeddedPredicates.Count -gt 0 -and $VerifyAttestations) {
        Write-Host "No complete embedded BuildKit attestation was retained for $platform; signed attestations will be checked against the child digest."
    }
    if ($attestations.Count -gt 1) {
        Write-Host "Published manifest retained $($attestations.Count) embedded attestation descriptors for $platform."
    }

    if ($VerifyAttestations) {
        # The final index may be assembled from runtime child manifests, which
        # does not necessarily carry BuildKit's unknown/unknown attestation
        # descriptors forward. GitHub's signed statements below are therefore
        # the authoritative source and subject check.
        if ($missingEmbeddedPredicates.Count -eq 0) {
            Write-Host "Embedded BuildKit provenance and SBOM descriptors found for $platform."
        }
    }

    if ($VerifyAttestations) {
        $platformReference = "$imageName@$($descriptor.digest)"
        if ([string]::IsNullOrWhiteSpace($Repository)) {
            throw 'Repository is required when verifying signed attestations.'
        }
        if ([string]::IsNullOrWhiteSpace($SignerWorkflow)) {
            throw 'SignerWorkflow is required when verifying signed attestations.'
        }
        if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
            throw 'GitHub CLI is required when verifying signed attestations.'
        }
        if ([string]::IsNullOrWhiteSpace($ExpectedRevision)) {
            throw 'ExpectedRevision is required when verifying signed attestations.'
        }
        if ($SourceRef -notmatch '^refs/heads/[A-Za-z0-9._/-]+$') {
            throw 'SourceRef must name a branch ref when verifying signed attestations.'
        }
        function Invoke-GhAttestation([string]$PredicateType) {
            $arguments = @(
                'attestation', 'verify', "oci://$platformReference",
                '--repo', $Repository,
                '--signer-workflow', $SignerWorkflow,
                '--source-digest', $ExpectedRevision,
                '--source-ref', $SourceRef,
                '--signer-digest', $ExpectedRevision,
                '--deny-self-hosted-runners',
                '--predicate-type', $PredicateType,
                '--format', 'json'
            )
            $stdoutPath = [System.IO.Path]::GetTempFileName()
            $stderrPath = [System.IO.Path]::GetTempFileName()
            try {
                gh @arguments 1> $stdoutPath 2> $stderrPath
                $exitCode = $LASTEXITCODE
                if ($exitCode -ne 0) {
                    throw "Signed $PredicateType verification failed for ${platform}."
                }
                $json = Get-Content -Raw -LiteralPath $stdoutPath
                if ([string]::IsNullOrWhiteSpace($json)) {
                    throw "Signed $PredicateType verification returned no JSON for ${platform}."
                }
                try {
                    return $json | ConvertFrom-Json
                } catch {
                    throw "Signed $PredicateType verification returned invalid JSON for ${platform}."
                }
            } finally {
                Remove-Item -LiteralPath $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue
            }
        }
        $verifiedAttestations = Invoke-GhAttestation 'https://slsa.dev/provenance/v1'
        $subjectDigest = $descriptor.digest.Substring('sha256:'.Length)
        $matchingAttestation = @($verifiedAttestations | Where-Object {
                $_.verificationResult.statement.predicateType -eq 'https://slsa.dev/provenance/v1' -and
                @($_.verificationResult.statement.subject | Where-Object {
                        $_.digest.sha256 -eq $subjectDigest
                    }).Count -gt 0
            })
        if ($matchingAttestation.Count -eq 0) {
            throw "Signed provenance did not name the expected ${platform} subject digest."
        }
        if ($ExpectedRevision) {
            $sourceMatch = @($matchingAttestation | Where-Object {
                    $predicate = $_.verificationResult.statement.predicate
                    $workflow = $predicate.buildDefinition.externalParameters.workflow
                    $dependencies = @($predicate.buildDefinition.resolvedDependencies)
                    $expectedRepositoryUri = "git+https://github.com/$Repository@$SourceRef"
                    $workflow.repository -eq "https://github.com/$Repository" -and
                    $workflow.ref -eq $SourceRef -and
                    $workflow.path -eq '.github/workflows/multiarch-build.yml' -and
                    @($dependencies | Where-Object {
                            $_.uri -eq $expectedRepositoryUri -and
                            $_.digest.gitCommit -eq $ExpectedRevision
                        }).Count -eq 1
                })
            if ($sourceMatch.Count -eq 0) {
                throw "Signed provenance did not bind ${platform} to source revision $ExpectedRevision."
            }
        }

        $verifiedSbomAttestations = Invoke-GhAttestation 'https://spdx.dev/Document'
        $matchingSbom = @($verifiedSbomAttestations | Where-Object {
                $_.verificationResult.statement.predicateType -eq 'https://spdx.dev/Document' -and
                @($_.verificationResult.statement.subject | Where-Object {
                        $_.digest.sha256 -eq $subjectDigest
                    }).Count -gt 0
            })
        if ($matchingSbom.Count -eq 0) {
            throw "Signed SBOM did not name the expected ${platform} subject digest."
        }
        foreach ($sbomAttestation in $matchingSbom) {
            $sbom = $sbomAttestation.verificationResult.statement.predicate
            if ($sbom.spdxVersion -notmatch '^SPDX-' -or @($sbom.packages).Count -eq 0) {
                throw "Signed SBOM for ${platform} did not contain a package inventory."
            }
        }
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
