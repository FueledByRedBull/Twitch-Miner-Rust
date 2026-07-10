$ErrorActionPreference = 'Stop'

$root = (Get-Location).Path
$markdownFiles = Get-ChildItem -Path $root -Recurse -File -Filter '*.md' |
    Where-Object { $_.FullName -notmatch '[\\/]target[\\/]' }

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
