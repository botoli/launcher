[CmdletBinding()]
param()
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$scriptSource = Join-Path $PSScriptRoot '../release.ps1'
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) ('launcher-version-tests-' + [guid]::NewGuid().ToString('N') + '/repo')
$utf8 = New-Object System.Text.UTF8Encoding($false)
function Write-Fixture([string]$Path, [string]$Text) {
    [IO.File]::WriteAllText((Join-Path $fixtureRoot $Path), $Text, $utf8)
}
function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Assert-Rejected([scriptblock]$Action) {
    $rejected = $false
    try { & $Action } catch { $rejected = $true }
    Assert-True $rejected 'Invalid release operation was accepted'
}
try {
    foreach ($dir in @('', 'src', 'scripts', 'packaging', '../vendor', '../vendor/src')) {
        New-Item -ItemType Directory -Path (Join-Path $fixtureRoot $dir) -Force | Out-Null
    }
    Copy-Item -LiteralPath $scriptSource -Destination (Join-Path $fixtureRoot 'scripts/release.ps1')
    Write-Fixture 'Cargo.toml' @"
[workspace]
members = ["."]
exclude = ["../vendor"]
resolver = "3"
[workspace.package]
version = "0.1.3"
[package]
name = "launcher-fixture"
version.workspace = true
edition = "2024"
[dependencies]
fixture-dependency = { path = "../vendor" }
"@
    Write-Fixture 'src/lib.rs' ''
    Write-Fixture '../vendor/Cargo.toml' @"
[package]
name = "fixture-dependency"
version = "0.1.3"
edition = "2024"
"@
    Write-Fixture '../vendor/src/lib.rs' ''
    Write-Fixture 'README.md' "# Fixture`r`n`r`nВерсия: **0.1.3**`r`n`r`nExample: 0.1.3`r`n"
    Write-Fixture 'packaging/README.txt' "Equestrian Dream Launcher 0.1.3`r`nUser instructions`r`n"
    Push-Location $fixtureRoot
    try {
        & cargo generate-lockfile --offline
        if ($LASTEXITCODE -ne 0) { throw 'Fixture lockfile generation failed' }
    }
    finally { Pop-Location }
    $release = Join-Path $fixtureRoot 'scripts/release.ps1'
    $files = @('Cargo.toml', 'Cargo.lock', 'README.md', 'packaging/README.txt')
    $baseline = @{}
    foreach ($file in $files) { $baseline[$file] = [IO.File]::ReadAllBytes((Join-Path $fixtureRoot $file)) }
    function Reset-Fixture {
        foreach ($file in $files) { [IO.File]::WriteAllBytes((Join-Path $fixtureRoot $file), $baseline[$file]) }
    }
    function Assert-Unchanged {
        foreach ($file in $files) {
            Assert-True ([Convert]::ToBase64String([IO.File]::ReadAllBytes((Join-Path $fixtureRoot $file))) -eq
                [Convert]::ToBase64String($baseline[$file])) "Unexpected change to $file"
        }
    }
    & $release -Check
    Assert-Unchanged
    & $release patch -WhatIf
    Assert-Unchanged
    foreach ($case in @(@('patch', '0.1.4'), @('minor', '0.2.0'), @('major', '1.0.0'))) {
        Reset-Fixture
        & $release $case[0]
        $expected = $case[1]
        & $release -Check
        $manifest = [IO.File]::ReadAllText((Join-Path $fixtureRoot 'Cargo.toml'))
        Assert-True ($manifest.Contains('version = "' + $expected + '"')) 'Wrong workspace version'
        $readme = [IO.File]::ReadAllText((Join-Path $fixtureRoot 'README.md'))
        Assert-True ($readme.Contains("Версия: **$expected**")) 'Wrong README version'
        Assert-True ($readme.Contains("Example: 0.1.3`r`n")) 'Unrelated README text or CRLF changed'
        $lock = [IO.File]::ReadAllText((Join-Path $fixtureRoot 'Cargo.lock'))
        Assert-True ($lock -match 'name = "fixture-dependency"\r?\nversion = "0.1.3"') 'Dependency version changed'
        Push-Location $fixtureRoot
        try {
            & cargo metadata --offline --locked --format-version 1 | Out-Null
            if ($LASTEXITCODE -ne 0) { throw 'Cargo rejected updated lockfile' }
        }
        finally { Pop-Location }
    }
    Reset-Fixture
    Write-Fixture 'README.md' 'Missing version marker'
    Assert-Rejected { & $release patch }
    Assert-True ([IO.File]::ReadAllText((Join-Path $fixtureRoot 'Cargo.toml')) -eq $utf8.GetString($baseline['Cargo.toml'])) 'Manifest changed after validation failure'
    Reset-Fixture
    $lock = [IO.File]::ReadAllText((Join-Path $fixtureRoot 'Cargo.lock'))
    Write-Fixture 'Cargo.lock' ([regex]::Replace($lock, '(name = "launcher-fixture"\r?\nversion = ")0\.1\.3', '${1}0.1.2'))
    Assert-Rejected { & $release -Check }
    Reset-Fixture
    foreach ($file in $files) {
        $text = [IO.File]::ReadAllText((Join-Path $fixtureRoot $file))
        Write-Fixture $file ($text.Replace('0.1.3', '0.1.65535'))
    }
    Assert-Rejected { & $release patch }
    Write-Host 'PASS: patch/minor/major, read-only checks, WhatIf, Cargo lockfile, dependency preservation, README validation, mismatched lockfile, Windows version limit.'
}
finally {
    $absolute = [IO.Path]::GetFullPath((Join-Path $fixtureRoot '..'))
    $allowed = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
    if (-not $absolute.StartsWith($allowed, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetFileName($absolute) -notlike 'launcher-version-tests-*') {
        throw "Unsafe fixture cleanup path: $absolute"
    }
    if (Test-Path -LiteralPath $absolute) { Remove-Item -LiteralPath $absolute -Recurse -Force }
}
