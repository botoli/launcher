[CmdletBinding(SupportsShouldProcess, DefaultParameterSetName = 'Bump')]
param(
    [Parameter(Mandatory, Position = 0, ParameterSetName = 'Bump')]
    [ValidateSet('patch', 'minor', 'major')]
    [string]$Bump,
    [Parameter(Mandatory, ParameterSetName = 'Check')]
    [switch]$Check
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$utf8 = New-Object System.Text.UTF8Encoding($false)
$paths = @('Cargo.toml', 'Cargo.lock', 'README.md', 'packaging/README.txt')
$originals = @{}
foreach ($path in $paths) {
    $originals[$path] = [IO.File]::ReadAllBytes((Join-Path $repositoryRoot $path))
}
$texts = @{}
foreach ($path in $paths) {
    $texts[$path] = [IO.File]::ReadAllText((Join-Path $repositoryRoot $path))
}

function Find-One([string]$Text, [string]$Pattern, [string]$Label) {
    $found = [regex]::Matches($Text, $Pattern)
    if ($found.Count -ne 1) { throw "Expected one $Label; found $($found.Count)" }
    return $found[0]
}

$versionPattern = '(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)'
$workspace = Find-One $texts['Cargo.toml'] '(?ms)^\[workspace\.package\]\s*\r?\n(?:(?!^\[).)*' 'workspace.package section'
$versionMatch = Find-One $workspace.Value ('(?m)^version\s*=\s*"(?<version>' + $versionPattern + ')"\s*$') 'stable workspace version'
$version = $versionMatch.Groups['version'].Value
$parts = @($version.Split('.') | ForEach-Object { [int]$_ })
foreach ($part in $parts) {
    if ($part -gt 65535) { throw 'Windows version components must be at most 65535' }
}

Push-Location $repositoryRoot
try {
    $metadataText = & cargo metadata --offline --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'Unable to read Cargo workspace metadata' }
    $metadata = ($metadataText -join "`n") | ConvertFrom-Json
}
finally { Pop-Location }
$packages = @($metadata.packages | Where-Object { $_.id -in $metadata.workspace_members })
if ($packages.Count -eq 0) { throw 'No workspace packages found' }
$lockMatches = @()
foreach ($package in $packages) {
    if ($package.version -ne $version) { throw "Package $($package.name) must inherit workspace version $version" }
    $manifest = [IO.File]::ReadAllText($package.manifest_path)
    $null = Find-One $manifest '(?m)^version\.workspace\s*=\s*true\s*$' "$($package.name) version inheritance"
    $block = Find-One $texts['Cargo.lock'] ('(?ms)^\[\[package\]\]\r?\nname = "' + [regex]::Escape($package.name) + '"\r?\n(?:(?!^\[\[).)*') "$($package.name) lock entry"
    if ($block.Value -match '(?m)^source\s*=') { throw "Unexpected remote package: $($package.name)" }
    $lockedVersion = Find-One $block.Value '(?m)^version = "(?<version>[^"]+)"' 'lock version'
    if ($lockedVersion.Groups['version'].Value -ne $version) { throw "Cargo.lock version mismatch for $($package.name)" }
    $lockMatches += $block
}
$readmeMatch = Find-One $texts['README.md'] ('(?m)^Версия: \*\*(?<version>' + $versionPattern + ')\*\*\.?\r?$') 'README version'
$packageReadmeMatch = Find-One $texts['packaging/README.txt'] ('\AEquestrian Dream Launcher (?<version>' + $versionPattern + ')(?=\r?\n)') 'distribution README version'
foreach ($match in @($readmeMatch, $packageReadmeMatch)) {
    if ($match.Groups['version'].Value -ne $version) { throw 'README version differs from Cargo.toml' }
}
if ($Check) {
    Write-Host "Versions are consistent: $version"
    return
}
switch ($Bump) {
    'patch' { $parts[2]++ }
    'minor' { $parts[1]++; $parts[2] = 0 }
    'major' { $parts[0]++; $parts[1] = 0; $parts[2] = 0 }
}
foreach ($part in $parts) {
    if ($part -gt 65535) { throw 'New version exceeds Windows version component limit (65535)' }
}
$newVersion = $parts -join '.'
if (-not $PSCmdlet.ShouldProcess($repositoryRoot, "Update launcher version $version -> $newVersion")) { return }

# Change only the matched version spans, preserving all unrelated edits and line endings.
function Replace-Version([string]$Text, [int]$Index, [int]$Length) {
    return $Text.Remove($Index, $Length).Insert($Index, $newVersion)
}
$group = $versionMatch.Groups['version']
$texts['Cargo.toml'] = Replace-Version $texts['Cargo.toml'] ($workspace.Index + $group.Index) $group.Length
foreach ($block in ($lockMatches | Sort-Object Index -Descending)) {
    $group = ([regex]::Match($block.Value, '(?m)^version = "(?<version>[^"]+)"')).Groups['version']
    $texts['Cargo.lock'] = Replace-Version $texts['Cargo.lock'] ($block.Index + $group.Index) $group.Length
}
$group = $readmeMatch.Groups['version']
$texts['README.md'] = Replace-Version $texts['README.md'] $group.Index $group.Length
$group = $packageReadmeMatch.Groups['version']
$texts['packaging/README.txt'] = Replace-Version $texts['packaging/README.txt'] $group.Index $group.Length

try {
    foreach ($path in $paths) {
        $absolute = Join-Path $repositoryRoot $path
        # Preserve an existing UTF-8 BOM as well as the original newline style.
        $encoding = $utf8
        if ($originals[$path].Length -ge 3 -and $originals[$path][0] -eq 239 -and
            $originals[$path][1] -eq 187 -and $originals[$path][2] -eq 191) {
            $encoding = New-Object System.Text.UTF8Encoding($true)
        }
        [IO.File]::WriteAllText($absolute, $texts[$path], $encoding)
    }
    & $PSCommandPath -Check
}
catch {
    foreach ($path in $paths) {
        [IO.File]::WriteAllBytes((Join-Path $repositoryRoot $path), $originals[$path])
    }
    throw
}
Write-Host "Launcher version updated: $version -> $newVersion"
Write-Host 'Review the diff, then run scripts/package-release.ps1 to build the release.'
