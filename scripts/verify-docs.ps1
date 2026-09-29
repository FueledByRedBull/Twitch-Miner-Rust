param([string[]]$AdditionalPaths = @())

$ErrorActionPreference = 'Stop'

$root = git rev-parse --show-toplevel
if ($LASTEXITCODE -ne 0) { throw 'Unable to locate the documentation Git root.' }
$root = (Resolve-Path -LiteralPath $root).Path
$tracked = (git -C $root ls-files -z -- '*.md') -join "`n"
if ($LASTEXITCODE -ne 0) { throw 'Unable to list tracked Markdown files.' }
$paths = @($tracked.Split([char]0, [StringSplitOptions]::RemoveEmptyEntries)) + $AdditionalPaths
$markdownFiles = foreach ($relative in $paths | Select-Object -Unique) {
    $path = [IO.Path]::GetFullPath((Join-Path $root $relative))
    if (-not $path.StartsWith($root + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Documentation path must remain inside the repository: $relative"
    }
    Get-Item -LiteralPath $path
}

foreach ($file in $markdownFiles) {
    $lineNumber = 0
    foreach ($line in Get-Content -LiteralPath $file.FullName) {
        $lineNumber++
        foreach ($match in [regex]::Matches($line, '\[[^\]]+\]\(([^)#]+)(?:#[^)]*)?\)')) {
            $target = $match.Groups[1].Value
            if ($target -match '^(https?://|mailto:)') {
                continue
            }
            $path = Join-Path $file.DirectoryName $target
            if (-not (Test-Path -LiteralPath $path)) {
                throw "Broken Markdown link: $($file.FullName):$lineNumber -> $target"
            }
        }
    }
}
