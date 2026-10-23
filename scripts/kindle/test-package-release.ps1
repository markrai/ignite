#Requires -Version 5.1
<#
.SYNOPSIS
  Wrapper regressions for scripts/kindle/package-release.ps1 (no Docker daemon).

.DESCRIPTION
  Official Linux/CI packaging has no SkipBuild. The Windows wrapper may skip the
  ARM Docker rebuild for local developers; that path is not provenance-safe and
  must still invoke Dropbear docker_build.sh and package_release.py.
#>
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$Wrapper = Join-Path $PSScriptRoot "package-release.ps1"
if (-not (Test-Path -LiteralPath $Wrapper)) {
  throw "missing package-release.ps1"
}

$src = Get-Content -LiteralPath $Wrapper -Raw
if ($src -notmatch '(?m)\[switch\]\s*\$SkipBuild') {
  throw "package-release.ps1 must expose -SkipBuild for local Windows developers"
}
if ($src -notmatch 'NOT provenance-safe') {
  throw "package-release.ps1 -SkipBuild must warn that it is not provenance-safe"
}
if ($src -notmatch 'package_release\.py') {
  throw "package-release.ps1 must call the shared Python packager"
}
if ($src -notmatch 'docker_build\.sh') {
  throw "package-release.ps1 must still run tools/dropbear/docker_build.sh"
}
if ($src -notmatch 'Get-PinnedImage') {
  throw "package-release.ps1 must read digest-pinned images"
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("ignite-ps1-pkg-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$fakeBin = Join-Path $tmp "bin"
$logDir = Join-Path $tmp "log"
New-Item -ItemType Directory -Force -Path $fakeBin, $logDir | Out-Null
$dockerLog = Join-Path $logDir "docker.log"
$pythonLog = Join-Path $logDir "python.log"
$bashLog = Join-Path $logDir "bash.log"

function Write-Utf8NoBom([string]$Path, [string]$Text) {
  $utf8 = New-Object System.Text.UTF8Encoding $false
  [System.IO.File]::WriteAllText($Path, $Text.Replace("`r`n", "`n"), $utf8)
}

function New-LogShim([string]$Name, [string]$LogPath, [string]$ExtraBash) {
  $unix = Join-Path $fakeBin $Name
  $body = @"
#!/bin/sh
echo "`$0 `$*" >> "$($LogPath.Replace('\', '/'))"
$ExtraBash
"@
  Write-Utf8NoBom $unix $body
  if (Get-Command chmod -ErrorAction SilentlyContinue) {
    & chmod +x $unix
  }
  $cmd = Join-Path $fakeBin ($Name + ".cmd")
  @(
    "@echo off"
    "echo $Name %*>> `"$LogPath`""
    if ($Name -eq "bash") {
      "echo %*| findstr /C:`"docker_build.sh`" >nul && exit 0"
      "bash.exe %*"
    }
    else {
      "exit 0"
    }
  ) | Set-Content -LiteralPath $cmd -Encoding ascii
}

New-LogShim "docker" $dockerLog 'exit ${IGNITE_FAKE_DOCKER_RC:-0}'
New-LogShim "python" $pythonLog "exit 0"
New-LogShim "py" $pythonLog "exit 0"
New-LogShim "bash" $bashLog @"
case " `$* " in
  *docker_build.sh*) exit 0 ;;
esac
exec /bin/bash "`$@"
"@

function Reset-Logs {
  foreach ($p in @($dockerLog, $pythonLog, $bashLog)) {
    if (Test-Path -LiteralPath $p) { Remove-Item -LiteralPath $p -Force }
  }
}

function Invoke-Wrapper {
  param([hashtable]$Named = @{})
  Reset-Logs
  $env:IGNITE_FAKE_DOCKER_RC = "0"
  $oldPath = $env:PATH
  $env:PATH = "$fakeBin$([IO.Path]::PathSeparator)$oldPath"
  try {
    & $Wrapper @Named
    return $LASTEXITCODE
  }
  finally {
    $env:PATH = $oldPath
  }
}

$rc = Invoke-Wrapper
if ($rc -ne 0) { throw "package-release.ps1 without -SkipBuild exited $rc" }
$d = Get-Content -LiteralPath $dockerLog -ErrorAction SilentlyContinue
$b = Get-Content -LiteralPath $bashLog -Raw -ErrorAction SilentlyContinue
$p = Get-Content -LiteralPath $pythonLog -Raw -ErrorAction SilentlyContinue
if (-not ($d -match "docker_build_armv7hf.sh")) {
  throw "without -SkipBuild, docker must run tools/docker_build_armv7hf.sh"
}
if (-not ($d -match "@sha256:")) {
  throw "without -SkipBuild, docker must use a digest-pinned rust image"
}
if (-not ($b -match "docker_build.sh")) {
  throw "package-release.ps1 must run dropbear docker_build.sh"
}
if (-not ($p -match "package_release.py")) {
  throw "package-release.ps1 must run package_release.py"
}

$rc = Invoke-Wrapper -Named @{ SkipBuild = $true }
if ($rc -ne 0) { throw "package-release.ps1 -SkipBuild exited $rc" }
$d = Get-Content -LiteralPath $dockerLog -ErrorAction SilentlyContinue
$b = Get-Content -LiteralPath $bashLog -Raw -ErrorAction SilentlyContinue
$p = Get-Content -LiteralPath $pythonLog -Raw -ErrorAction SilentlyContinue
if ($d) {
  throw "-SkipBuild must not invoke docker for the ARM rebuild (log: $d)"
}
if (-not ($b -match "docker_build.sh")) {
  throw "-SkipBuild must still run dropbear docker_build.sh"
}
if (-not ($p -match "package_release.py")) {
  throw "-SkipBuild must still run package_release.py"
}

Reset-Logs
Remove-Item -LiteralPath (Join-Path $fakeBin "docker") -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath (Join-Path $fakeBin "docker.cmd") -Force -ErrorAction SilentlyContinue
$oldPath = $env:PATH
$env:PATH = $fakeBin
try {
  $failed = $false
  try {
    & $Wrapper
  }
  catch {
    $failed = $true
    if ("$_" -notmatch "docker not found") {
      throw "expected docker-not-found without -SkipBuild, got: $_"
    }
  }
  if (-not $failed) { throw "package-release.ps1 without docker and without -SkipBuild must refuse" }
}
finally {
  $env:PATH = $oldPath
}

Write-Host "PASS: package-release.ps1 official path rebuilds; -SkipBuild is local-only"
Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
exit 0
