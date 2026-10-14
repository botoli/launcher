[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$releaseExecutable = Join-Path $repositoryRoot 'target\release\EquestrianDreamLauncher.exe'
$readmeSource = Join-Path $repositoryRoot 'packaging\README.txt'
$distRoot = Join-Path $repositoryRoot 'dist'
$outputDirectory = Join-Path $distRoot 'EquestrianDreamLauncher'
$stagingDirectory = Join-Path $distRoot '.EquestrianDreamLauncher.staging'

function Invoke-CargoCommand {
    param([Parameter(Mandatory)][string[]]$Arguments)

    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

Push-Location $repositoryRoot
try {
    Invoke-CargoCommand @('fmt', '--all', '--', '--check')
    Invoke-CargoCommand @('check', '--workspace', '--all-targets')
    Invoke-CargoCommand @(
        'clippy', '--workspace', '--all-targets', '--all-features',
        '--', '-D', 'warnings'
    )
    Invoke-CargoCommand @('test', '--workspace')

    $previousRustFlags = [Environment]::GetEnvironmentVariable('RUSTFLAGS', 'Process')
    try {
        $staticCrtFlag = '-C target-feature=+crt-static'
        $env:RUSTFLAGS = if ([string]::IsNullOrWhiteSpace($previousRustFlags)) {
            $staticCrtFlag
        }
        else {
            "$previousRustFlags $staticCrtFlag"
        }
        Invoke-CargoCommand @('build', '-p', 'launcher-desktop', '--release')
    }
    finally {
        if ($null -eq $previousRustFlags) {
            Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
        }
        else {
            $env:RUSTFLAGS = $previousRustFlags
        }
    }

    if (-not (Test-Path -LiteralPath $releaseExecutable -PathType Leaf)) {
        throw "Release executable was not produced: $releaseExecutable"
    }
    if (-not (Test-Path -LiteralPath $readmeSource -PathType Leaf)) {
        throw "Distribution README is missing: $readmeSource"
    }

    New-Item -ItemType Directory -Path $distRoot -Force | Out-Null
    if (Test-Path -LiteralPath $stagingDirectory) {
        Remove-Item -LiteralPath $stagingDirectory -Recurse -Force
    }
    New-Item -ItemType Directory -Path $stagingDirectory | Out-Null
    Copy-Item -LiteralPath $releaseExecutable -Destination $stagingDirectory
    Copy-Item -LiteralPath $readmeSource -Destination $stagingDirectory

    if (Test-Path -LiteralPath $outputDirectory) {
        Remove-Item -LiteralPath $outputDirectory -Recurse -Force
    }
    Move-Item -LiteralPath $stagingDirectory -Destination $outputDirectory

    $unexpectedFiles = Get-ChildItem -LiteralPath $outputDirectory -File |
        Where-Object Name -NotIn @('EquestrianDreamLauncher.exe', 'README.txt')
    if ($unexpectedFiles) {
        throw "Unexpected files were added to the distribution folder"
    }

    Write-Host "Release package created: $outputDirectory"
}
finally {
    if (Test-Path -LiteralPath $stagingDirectory) {
        Remove-Item -LiteralPath $stagingDirectory -Recurse -Force
    }
    Pop-Location
}
