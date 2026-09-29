$ErrorActionPreference = 'Stop'
$verify = Join-Path $PSScriptRoot '../verify-docs.ps1'
$target = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../target'))
$root = Join-Path $target "docs-check-$([Guid]::NewGuid().ToString('N'))"

function Assert-Rejected([scriptblock]$Action, [string]$Message) {
    try { & $Action } catch {
        if ($_.Exception.Message -notmatch $Message) { throw }
        return
    }
    throw "Expected rejection: $Message"
}

New-Item -ItemType Directory -Path $root | Out-Null
Push-Location $root
try {
    git init --quiet
    if ($LASTEXITCODE -ne 0) { throw 'Unable to initialize documentation test repository.' }
    Set-Content -LiteralPath .gitignore -Value '/target/'
    Set-Content -LiteralPath README.md -Value '[existing](file.txt)'
    Set-Content -LiteralPath file.txt -Value 'synthetic'
    git add -- README.md
    if ($LASTEXITCODE -ne 0) { throw 'Unable to stage synthetic Markdown.' }
    New-Item -ItemType Directory -Path target | Out-Null
    Set-Content -LiteralPath target/generated.md -Value '[ignored](missing.txt)'
    Set-Content -LiteralPath 'new guide.md' -Value '[new](missing.txt)'
    & $verify
    Assert-Rejected { & $verify -AdditionalPaths 'new guide.md' } 'Broken Markdown link'
    Set-Content -LiteralPath 'new guide.md' -Value '[existing](file.txt)'
    & $verify -AdditionalPaths 'new guide.md'
    Assert-Rejected { & $verify -AdditionalPaths '../outside.md' } 'must remain inside'
    Set-Content -LiteralPath README.md -Value '[broken](missing.txt)'
    Assert-Rejected { & $verify } 'Broken Markdown link'
    Set-Content -LiteralPath README.md -Value '[existing](file.txt)'
    Push-Location target
    try { & $verify } finally { Pop-Location }
    Write-Output 'docs-verifier-tests-ok'
} finally {
    Pop-Location
    $resolved = (Resolve-Path -LiteralPath $root).Path
    if (-not $resolved.StartsWith($target + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Unsafe documentation test cleanup path.'
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
