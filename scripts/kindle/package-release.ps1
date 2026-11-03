#Requires -Version 5.1
<#
.SYNOPSIS
  Thin Windows wrapper around deployment/kindle/package_release.py.

.DESCRIPTION
  Docker ARM release build (unless -SkipBuild) → Dropbear docker_build.sh →
  shared Python packager → Install Ignite vVERSION.sh + SHA-256.
  Staging, validation, SFX, and Dropbear PINNED.txt checks live in Python.

.PARAMETER Version
  Optional artifact version override (default: ignite/VERSION file).

.PARAMETER SkipBuild
  Use existing ARMv7hf release binaries. Not provenance-safe. Forbidden for
  official / CI releases (Linux wrapper has no equivalent flag).
#>
[CmdletBinding()]
param(
    [string]$Version = "",
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Write-Step([string]$Message) {
    Write-Host "== $Message"
}

function Get-RepoRoot {
    return (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
}

function ConvertTo-DockerUnixPath([string]$WindowsPath) {
    $p = (Resolve-Path -LiteralPath $WindowsPath).Path
    $drive = $p.Substring(0, 1).ToLowerInvariant()
    $rest = $p.Substring(2) -replace "\\", "/"
    return "/$drive$rest"
}

function Get-PinnedImage([string]$RepoRoot, [string]$Name) {
    $file = Join-Path $RepoRoot "tools\pinned-docker-images.txt"
    if (-not (Test-Path -LiteralPath $file)) {
        throw "missing $file"
    }
    foreach ($line in Get-Content -LiteralPath $file) {
        if ($line -match "^$Name=(.+)$") {
            $img = $Matches[1].Trim()
            if ($img -notmatch '@sha256:') {
                throw "$Name must be digest-pinned (got $img)"
            }
            return $img
        }
    }
    throw "$Name pin missing in $file"
}

$RepoRoot = Get-RepoRoot
$python = Get-Command python -ErrorAction SilentlyContinue
if (-not $python) {
    $python = Get-Command py -ErrorAction SilentlyContinue
}
if (-not $python) {
    throw "python not found in PATH (needed for package_release.py)"
}
$PythonExe = $python.Source
$Packager = Join-Path $RepoRoot "deployment\kindle\package_release.py"
if (-not (Test-Path -LiteralPath $Packager)) {
    throw "missing $Packager"
}

if (-not $SkipBuild) {
    $docker = Get-Command docker -ErrorAction SilentlyContinue
    if (-not $docker) {
        throw "docker not found in PATH (required to cross-compile the Kindle release binary). Install Docker or pass -SkipBuild with existing release binaries (not provenance-safe)."
    }
    $rustImage = Get-PinnedImage $RepoRoot "rust_armv7hf"
    $unixRoot = ConvertTo-DockerUnixPath $RepoRoot
    Write-Step "ARM release build via Docker ($rustImage)"
    & docker run --rm -v "${unixRoot}:/src" -w /src $rustImage bash /src/tools/docker_build_armv7hf.sh
    if ($LASTEXITCODE -ne 0) {
        throw "Docker ARM build failed with exit $LASTEXITCODE"
    }
}
else {
    Write-Step "SkipBuild: using existing release binaries (NOT provenance-safe; forbidden for official releases)"
}

# Prefer Git Bash. WindowsApps/WSL `bash.exe` mangles Windows paths and cannot
# run tools/dropbear/docker_build.sh from this wrapper.
# ProgramFiles is unset on Linux pwsh CI hosts — skip those candidates there.
$bashExe = $null
$gitBashCandidates = @()
if (-not [string]::IsNullOrWhiteSpace(${env:ProgramFiles})) {
    $gitBashCandidates += @(
        (Join-Path ${env:ProgramFiles} "Git\bin\bash.exe"),
        (Join-Path ${env:ProgramFiles} "Git\usr\bin\bash.exe")
    )
}
foreach ($candidate in $gitBashCandidates) {
    if (Test-Path -LiteralPath $candidate) {
        $bashExe = $candidate
        break
    }
}
if (-not $bashExe) {
    $bashCmd = Get-Command bash -ErrorAction SilentlyContinue
    if ($bashCmd) {
        $bashExe = $bashCmd.Source
    }
}
if (-not $bashExe) {
    throw "bash not found (needed to run tools/dropbear/docker_build.sh); install Git for Windows"
}
Write-Step "Dropbear ARMv7hf client"
$dropbearScript = Join-Path $RepoRoot "tools\dropbear\docker_build.sh"
$dropbearUnix = ConvertTo-DockerUnixPath $dropbearScript
& $bashExe -lc ("bash '" + ($dropbearUnix -replace "'", "'\''") + "'")
if ($LASTEXITCODE -ne 0) {
    throw "Dropbear docker_build.sh failed with exit $LASTEXITCODE"
}

Write-Step "Shared Python packager"
$pyArgs = @($Packager, "--repo-root", $RepoRoot)
if ($Version) {
    $pyArgs += @("--version", $Version)
}
& $PythonExe @pyArgs
if ($LASTEXITCODE -ne 0) {
    throw "package_release.py failed with exit $LASTEXITCODE"
}
exit 0
