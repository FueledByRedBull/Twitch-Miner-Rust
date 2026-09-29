[CmdletBinding()]
param(
    [string]$OutputDirectory = 'target/windows-release',

    [string]$WixPath = '',

    [switch]$PortableOnly,

    [switch]$ValidateOnly
)

$ErrorActionPreference = 'Stop'

function Get-Metadata {
    $metadata = (cargo metadata --locked --no-deps --format-version 1 2>&1) -join "`n"
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to read Cargo package metadata.'
    }
    try {
        return $metadata | ConvertFrom-Json
    } catch {
        throw 'Cargo metadata was not valid JSON.'
    }
}

function Assert-CleanSourceTree {
    $status = @(git status --porcelain --untracked-files=all)
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to inspect the source worktree before release packaging.'
    }
    if ($status.Count -ne 0) {
        throw 'Windows release packaging requires a clean source worktree; commit or remove local changes first.'
    }
}

function Find-Dumpbin {
    $command = Get-Command dumpbin -ErrorAction SilentlyContinue
    if ($null -ne $command) {
        return $command.Source
    }

    $vswhere = Get-Command vswhere -ErrorAction SilentlyContinue
    if ($null -eq $vswhere) {
        $programFilesX86 = [Environment]::GetEnvironmentVariable('ProgramFiles(x86)')
        if (-not [string]::IsNullOrWhiteSpace($programFilesX86)) {
            $installerVswhere = Join-Path $programFilesX86 'Microsoft Visual Studio/Installer/vswhere.exe'
            if (Test-Path -LiteralPath $installerVswhere -PathType Leaf) {
                $vswhere = Get-Item -LiteralPath $installerVswhere
            }
        }
    }
    if ($null -ne $vswhere) {
        $vswherePath = if ($vswhere.PSObject.Properties.Name -contains 'Source') {
            $vswhere.Source
        } else {
            $vswhere.FullName
        }
        $candidates = @(& $vswherePath -latest -products '*' `
            -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
            -find 'VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe' 2>$null)
        foreach ($candidate in $candidates) {
            $path = ([string]$candidate).Trim()
            if (Test-Path -LiteralPath $path -PathType Leaf) {
                return $path
            }
        }
    }
    return $null
}

function Assert-StaticRuntime([string]$BinaryPath, [string]$TargetTriple) {
    $inspectorPath = Find-Dumpbin
    $mode = 'dumpbin'
    if ([string]::IsNullOrWhiteSpace($inspectorPath)) {
        $rustc = Get-Command rustc -ErrorAction SilentlyContinue
        if ($null -ne $rustc) {
            $sysroot = (& rustc --print sysroot 2>$null).Trim()
            $candidate = Join-Path $sysroot "lib/rustlib/$TargetTriple/bin/llvm-readobj.exe"
            if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                $inspectorPath = $candidate
                $mode = 'llvm-readobj'
            }
        }
    }
    if ([string]::IsNullOrWhiteSpace($inspectorPath)) {
        throw 'A PE import inspector (dumpbin or llvm-readobj) is required to prove the portable CRT contract.'
    }
    $imports = if ($mode -eq 'dumpbin') {
        & $inspectorPath /DEPENDENTS $BinaryPath 2>&1
    } else {
        & $inspectorPath --coff-imports $BinaryPath 2>&1
    }
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to inspect Windows executable imports with $mode."
    }
    if (($imports -join "`n") -match '(?i)\b(?:vcruntime140(?:_1)?|msvcp140(?:_1)?|ucrtbase)\.dll\b') {
        throw 'Portable Windows executable imports the dynamic Visual C++ runtime.'
    }
    Write-Output "windows-crt-imports-ok: inspector=$mode"
}

if ($ValidateOnly) {
    $wxs = Get-Content -Raw -LiteralPath 'installer/Product.wxs'
    if ($wxs -notmatch 'http://wixtoolset.org/schemas/v4/wxs' -or
        $wxs -notmatch '\$\(var\.BinaryPath\)' -or
        $wxs -notmatch '\$\(var\.Version\)') {
        throw 'WiX installer source is missing the pinned schema or required build variables.'
    }
    foreach ($path in @('config.example.json', 'README.md', 'LICENSE')) {
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "Windows release input is missing: $path"
        }
    }
    Write-Output 'windows-release-validation-ok'
    return
}

Assert-CleanSourceTree

$revision = (git rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $revision -notmatch '^[0-9a-f]{40}$') {
    throw 'Unable to determine the full source revision for the Windows release.'
}
$sourceDateEpoch = (git show -s --format=%ct HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $sourceDateEpoch -notmatch '^\d+$') {
    throw 'Unable to determine SOURCE_DATE_EPOCH for the Windows release.'
}
$metadata = Get-Metadata
$appPackage = @($metadata.packages | Where-Object { $_.name -eq 'tm-app' })
if ($appPackage.Count -ne 1 -or $appPackage[0].version -notmatch '^\d+\.\d+\.\d+$') {
    throw 'The tm-app package must expose a three-part release version.'
}
$version = $appPackage[0].version
$targetTriple = 'x86_64-pc-windows-msvc'

$oldRevision = $env:BUILD_REVISION
$oldSourceDateEpoch = $env:SOURCE_DATE_EPOCH
$oldRustFlags = $env:RUSTFLAGS
$oldTargetDirectory = $env:CARGO_TARGET_DIR
$binary = $null
try {
    $env:BUILD_REVISION = $revision
    $env:SOURCE_DATE_EPOCH = $sourceDateEpoch
    $windowsReleaseFlags = @(
        '-C target-feature=+crt-static'
        '-Clink-arg=/DEBUG:NONE'
        '-Clink-arg=/Brepro'
    ) -join ' '
    $env:RUSTFLAGS = if ([string]::IsNullOrWhiteSpace($oldRustFlags)) {
        $windowsReleaseFlags
    } else {
        "$oldRustFlags $windowsReleaseFlags"
    }
    $env:CARGO_TARGET_DIR = [System.IO.Path]::GetFullPath((Join-Path (Get-Location) 'target'))
    cargo build --locked --release -p tm-app --target $targetTriple
    if ($LASTEXITCODE -ne 0) {
        throw 'Windows release binary build failed.'
    }

    $binary = (Resolve-Path -LiteralPath "target/$targetTriple/release/tm-app.exe").Path
    Assert-StaticRuntime $binary $targetTriple
    $outputRoot = [System.IO.Path]::GetFullPath((Join-Path (Get-Location) $OutputDirectory))
    $stage = Join-Path $outputRoot "twitch-miner-$version-windows-x86_64"
    New-Item -ItemType Directory -Path $stage -Force | Out-Null
    Copy-Item -LiteralPath $binary -Destination (Join-Path $stage 'twitch-miner.exe') -Force
    Copy-Item -LiteralPath 'config.example.json', 'README.md', 'LICENSE' -Destination $stage -Force
    @(
        "version=$version"
        "revision=$revision"
        "target=$targetTriple"
        "source_date_epoch=$sourceDateEpoch"
    ) | Set-Content -LiteralPath (Join-Path $stage 'BUILD-METADATA.txt') -Encoding ascii

    # Compress-Archive stores each input's LastWriteTime in the ZIP central
    # directory. Normalize every allowlisted input so a repeated build of the
    # same revision has stable portable-archive metadata.
    $archiveTime = [DateTimeOffset]::FromUnixTimeSeconds([int64]$sourceDateEpoch).UtcDateTime
    Get-ChildItem -LiteralPath $stage -File | ForEach-Object {
        $_.LastWriteTimeUtc = $archiveTime
    }

    $zip = Join-Path $outputRoot "twitch-miner-$version-windows-x86_64.zip"
    if (Test-Path -LiteralPath $zip) {
        Remove-Item -LiteralPath $zip -Force
    }
    $archiveInputs = @(
        'twitch-miner.exe'
        'config.example.json'
        'README.md'
        'LICENSE'
        'BUILD-METADATA.txt'
    ) | ForEach-Object { Join-Path $stage $_ }
    Compress-Archive -LiteralPath $archiveInputs -DestinationPath $zip -CompressionLevel Optimal
    $zipHash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    "$zipHash  $([System.IO.Path]::GetFileName($zip))" |
        Set-Content -LiteralPath "$zip.sha256" -Encoding ascii

    Write-Output "windows-portable: path=$zip sha256=$zipHash"
    if ($PortableOnly) {
        return
    }

    $wixCommand = if ([string]::IsNullOrWhiteSpace($WixPath)) {
        (Get-Command wix -ErrorAction SilentlyContinue).Source
    } else {
        $WixPath
    }
    if ([string]::IsNullOrWhiteSpace($wixCommand) -or
        -not (Test-Path -LiteralPath $wixCommand -PathType Leaf)) {
        throw 'WiX 4+ is required for MSI creation. Pass -WixPath or install the pinned CI tool.'
    }
    $wixVersion = (& $wixCommand --version 2>&1) -join "`n"
    if ($LASTEXITCODE -ne 0 -or $wixVersion -notmatch '(^|\s)4\.0\.6(?:\+|\s|$)') {
        throw "WiX 4.0.6 is required for pinned MSI creation; found: $wixVersion"
    }
    $msi = Join-Path $outputRoot "twitch-miner-$version-windows-x86_64.msi"
    $wixArgs = @(
        'build', '-arch', 'x64',
        '-d', "Version=$version",
        '-d', "BinaryPath=$binary",
        '-d', "ConfigPath=$(Join-Path $stage 'config.example.json')",
        '-d', "ReadmePath=$(Join-Path $stage 'README.md')",
        '-d', "LicensePath=$(Join-Path $stage 'LICENSE')",
        '-d', "MetadataPath=$(Join-Path $stage 'BUILD-METADATA.txt')",
        'installer/Product.wxs', '-o', $msi
    )
    & $wixCommand @wixArgs
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $msi -PathType Leaf)) {
        throw 'WiX MSI build failed.'
    }
    & $wixCommand msi validate $msi
    if ($LASTEXITCODE -ne 0) {
        throw 'WiX MSI validation failed.'
    }
    $msiHash = (Get-FileHash -LiteralPath $msi -Algorithm SHA256).Hash.ToLowerInvariant()
    "$msiHash  $([System.IO.Path]::GetFileName($msi))" |
        Set-Content -LiteralPath "$msi.sha256" -Encoding ascii
    Write-Output "windows-msi: path=$msi sha256=$msiHash"
} finally {
    if ($null -eq $oldRevision) {
        Remove-Item Env:BUILD_REVISION -ErrorAction SilentlyContinue
    } else {
        $env:BUILD_REVISION = $oldRevision
    }
    if ($null -eq $oldSourceDateEpoch) {
        Remove-Item Env:SOURCE_DATE_EPOCH -ErrorAction SilentlyContinue
    } else {
        $env:SOURCE_DATE_EPOCH = $oldSourceDateEpoch
    }
    if ($null -eq $oldRustFlags) {
        Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
    } else {
        $env:RUSTFLAGS = $oldRustFlags
    }
    if ($null -eq $oldTargetDirectory) {
        Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
    } else {
        $env:CARGO_TARGET_DIR = $oldTargetDirectory
    }
}
