[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$releaseExecutable = Join-Path $repositoryRoot 'target\release\EquestrianDreamLauncher.exe'
$readmeSource = Join-Path $repositoryRoot 'packaging\README.txt'
$configSource = Join-Path $repositoryRoot 'launcher-config.json'
$distRoot = Join-Path $repositoryRoot 'dist'
$outputDirectory = Join-Path $distRoot 'EquestrianDreamLauncher'
$stagingDirectory = Join-Path $distRoot '.EquestrianDreamLauncher.staging'

function Assert-PackagePaths {
    $allowedRoot = [IO.Path]::GetFullPath($distRoot).TrimEnd('\') + '\'
    foreach ($candidate in @($stagingDirectory, $outputDirectory)) {
        $absolute = [IO.Path]::GetFullPath($candidate)
        if (-not $absolute.StartsWith($allowedRoot, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Package path is outside dist: $absolute"
        }
    }
    foreach ($candidate in @($distRoot, $stagingDirectory, $outputDirectory)) {
        if ((Test-Path -LiteralPath $candidate) -and
            ((Get-Item -LiteralPath $candidate).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "Refusing to replace a package junction or symbolic link: $candidate"
        }
    }
}

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

    Assert-PackagePaths
    New-Item -ItemType Directory -Path $distRoot -Force | Out-Null
    if (Test-Path -LiteralPath $stagingDirectory) {
        Remove-Item -LiteralPath $stagingDirectory -Recurse -Force
    }
    New-Item -ItemType Directory -Path $stagingDirectory | Out-Null
    Copy-Item -LiteralPath $releaseExecutable -Destination $stagingDirectory
    Copy-Item -LiteralPath $readmeSource -Destination $stagingDirectory
    Copy-Item -LiteralPath $configSource -Destination $stagingDirectory

    if (Test-Path -LiteralPath $outputDirectory) {
        Remove-Item -LiteralPath $outputDirectory -Recurse -Force
    }
    Move-Item -LiteralPath $stagingDirectory -Destination $outputDirectory

    $unexpectedFiles = Get-ChildItem -LiteralPath $outputDirectory -File |
        Where-Object Name -NotIn @('EquestrianDreamLauncher.exe', 'README.txt', 'launcher-config.json')
    if ($unexpectedFiles) {
        throw "Unexpected files were added to the distribution folder"
    }

    $metadataText = & cargo metadata --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'Unable to read launcher version' }
    $metadata = $metadataText | ConvertFrom-Json
    $version = ($metadata.packages | Where-Object name -eq 'launcher-desktop').version
    $zipPath = Join-Path $distRoot "EquestrianDreamLauncher-$version-windows-x64.zip"
    # Updates preserve the local backend config; their ZIP still contains only EXE/README.
    Compress-Archive -LiteralPath @(
        (Join-Path $outputDirectory 'EquestrianDreamLauncher.exe'),
        (Join-Path $outputDirectory 'README.txt')
    ) -DestinationPath $zipPath -Force
    Write-Host "Release package created: $outputDirectory"
    Write-Host "Upload ZIP: $zipPath"
    Write-Host "Publish version: $version"
    Write-Host "SHA256: $((Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant())"
}
finally {
    Assert-PackagePaths
    if (Test-Path -LiteralPath $stagingDirectory) {
        Remove-Item -LiteralPath $stagingDirectory -Recurse -Force
    }
    Pop-Location
}
