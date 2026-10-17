#Requires -Version 5.1
<#
.SYNOPSIS
  Build the customer Install Ignite vVERSION.sh (self-extracting Scriptlet) plus SHA-256.

.DESCRIPTION
  Docker ARM release build (unless -SkipBuild) → stage payload → validate →
  self-extracting Install Ignite vVERSION.sh → validate SFX → SHA-256.
  Also emits a developer/support ZIP (README + the same installer).
  Profile: full-feature open-source (includes sync/pairing capability).

.PARAMETER Version
  Optional artifact version override (default: ignite/VERSION file).

.PARAMETER SkipBuild
  Use existing release binaries under target/armv7-unknown-linux-gnueabihf/release/.
  Not provenance-safe: stale ELFs may not match the current source tree.
  Prefer a full Docker rebuild for customer-facing Scriptlets.
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
    $here = $PSScriptRoot
    return (Resolve-Path (Join-Path $here "..\..")).Path
}

function ConvertTo-DockerUnixPath([string]$WindowsPath) {
    $p = (Resolve-Path -LiteralPath $WindowsPath).Path
    $drive = $p.Substring(0, 1).ToLowerInvariant()
    $rest = $p.Substring(2) -replace "\\", "/"
    return "/$drive$rest"
}

function Write-Utf8Lf([string]$Path, [string]$Content) {
    $normalized = $Content -replace "`r`n", "`n" -replace "`r", "`n"
    $utf8NoBom = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllText($Path, $normalized, $utf8NoBom)
}

function Copy-TextLf([string]$Source, [string]$Destination) {
    $text = [System.IO.File]::ReadAllText($Source)
    Write-Utf8Lf -Path $Destination -Content $text
}

function Copy-BinaryFile([string]$Source, [string]$Destination) {
    $destDir = Split-Path -Parent $Destination
    if (-not (Test-Path -LiteralPath $destDir)) {
        New-Item -ItemType Directory -Path $destDir -Force | Out-Null
    }
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
}

function New-DeterministicZip([string]$SourceDir, [string]$ZipPath) {
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    if (Test-Path -LiteralPath $ZipPath) {
        Remove-Item -LiteralPath $ZipPath -Force
    }
    $zip = [System.IO.Compression.ZipFile]::Open($ZipPath, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        $rootName = Split-Path -Leaf $SourceDir
        $files = Get-ChildItem -LiteralPath $SourceDir -Recurse -File | Sort-Object FullName
        foreach ($file in $files) {
            $rel = $file.FullName.Substring($SourceDir.Length).TrimStart("\", "/")
            $entryName = ($rootName + "/" + ($rel -replace "\\", "/"))
            [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                $zip,
                $file.FullName,
                $entryName,
                [System.IO.Compression.CompressionLevel]::Optimal
            )
        }
    }
    finally {
        $zip.Dispose()
    }
}

$RepoRoot = Get-RepoRoot
$VersionFile = Join-Path $RepoRoot "ignite\VERSION"
if (-not (Test-Path -LiteralPath $VersionFile)) {
    throw "missing VERSION file at $VersionFile"
}
$FileVersion = (Get-Content -LiteralPath $VersionFile -Raw).Trim()
if (-not $FileVersion) {
    throw "VERSION file is empty"
}

if ($Version) {
    if ($Version -ne $FileVersion) {
        Write-Warning "Version override '$Version' differs from VERSION file '$FileVersion'. Artifact will use '$Version'; binary embeds compile-time VERSION."
    }
}
else {
    $Version = $FileVersion
}

if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$') {
    throw "invalid version '$Version'"
}

$ReleaseBinDir = Join-Path $RepoRoot "target\armv7-unknown-linux-gnueabihf\release"
$AppBin = Join-Path $ReleaseBinDir "scribe-app"
$LifeBin = Join-Path $ReleaseBinDir "scribe-lifecycle"
$OutRoot = Join-Path $RepoRoot "target\package\kindle"
$StagingRoot = Join-Path $OutRoot "staging"
$FriendlyName = "Ignite Kindle $Version"
$PackageName = "Ignite-Kindle-$Version"
$PayloadPkg = Join-Path $StagingRoot $PackageName
$SupportDir = Join-Path $StagingRoot $FriendlyName
$SfxName = "Install Ignite v$Version.sh"
$SfxPath = Join-Path $OutRoot $SfxName
$SfxShaPath = Join-Path $OutRoot "Install Ignite v$Version.sha256"
$ZipPath = Join-Path $OutRoot "$PackageName.zip"
$ZipShaPath = Join-Path $OutRoot "$PackageName.sha256"
$ValidatePy = Join-Path $RepoRoot "deployment\kindle\validate_stage.py"
$BuildSfxPy = Join-Path $RepoRoot "deployment\kindle\sfx\build_sfx.py"
$DeployKindle = Join-Path $RepoRoot "deployment\kindle"

Write-Step "Ignite Kindle package version=$Version profile=full-feature"

# --- early prereqs ---
if (-not (Test-Path -LiteralPath $ValidatePy)) {
    throw "missing $ValidatePy"
}
if (-not (Test-Path -LiteralPath $BuildSfxPy)) {
    throw "missing $BuildSfxPy"
}
$python = Get-Command python -ErrorAction SilentlyContinue
if (-not $python) {
    $python = Get-Command py -ErrorAction SilentlyContinue
}
if (-not $python) {
    throw "python not found in PATH (needed for package validation)"
}
$PythonExe = $python.Source

if (-not $SkipBuild) {
    $docker = Get-Command docker -ErrorAction SilentlyContinue
    if (-not $docker) {
        throw "docker not found in PATH (required to cross-compile the Kindle release binary). Install Docker or pass -SkipBuild with existing release binaries."
    }
    $unixRoot = ConvertTo-DockerUnixPath $RepoRoot
    Write-Step "ARM release build via Docker (mount $unixRoot -> /src)"
    & docker run --rm -v "${unixRoot}:/src" -w /src rust:1-bullseye bash /src/tools/docker_build_armv7hf.sh
    if ($LASTEXITCODE -ne 0) {
        throw "Docker ARM build failed with exit $LASTEXITCODE"
    }
}
else {
    Write-Step "SkipBuild: using existing release binaries (NOT provenance-safe; prefer full Docker rebuild)"
}

foreach ($bin in @($AppBin, $LifeBin)) {
    if (-not (Test-Path -LiteralPath $bin)) {
        throw "missing release binary: $bin"
    }
    # Refuse packaging a debug path
    if ($bin -match '\\debug\\') {
        throw "refusing to package debug binary: $bin"
    }
    if ((Get-Item -LiteralPath $bin).Length -lt 1024) {
        throw "binary suspiciously small: $bin"
    }
}

# --- clean staging ---
Write-Step "Clean staging directory"
if (Test-Path -LiteralPath $StagingRoot) {
    Remove-Item -LiteralPath $StagingRoot -Recurse -Force
}
New-Item -ItemType Directory -Path $PayloadPkg -Force | Out-Null

function Ensure-Dir([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) {
        New-Item -ItemType Directory -Path $Path -Force | Out-Null
    }
}

Write-Step "Stage payload $PackageName"
Ensure-Dir (Join-Path $PayloadPkg "bin")
Ensure-Dir (Join-Path $PayloadPkg "host")
Ensure-Dir (Join-Path $PayloadPkg "launcher")
Ensure-Dir (Join-Path $PayloadPkg "registration")
Ensure-Dir (Join-Path $PayloadPkg "assets\licenses\noto-emoji")
Ensure-Dir (Join-Path $PayloadPkg "assets\licenses\source-sans-3")
Ensure-Dir (Join-Path $PayloadPkg "lib")

Copy-BinaryFile $AppBin (Join-Path $PayloadPkg "bin\scribe-app")
Copy-BinaryFile $LifeBin (Join-Path $PayloadPkg "bin\scribe-lifecycle")
Copy-TextLf (Join-Path $DeployKindle "scribe-lifecycle-host.sh") (Join-Path $PayloadPkg "host\scribe-lifecycle-host.sh")
Copy-TextLf (Join-Path $RepoRoot "tools\kindle_launcher\Scribe.sh") (Join-Path $PayloadPkg "launcher\Scribe.sh")
Copy-TextLf (Join-Path $RepoRoot "tools\kindle_app\registration\ignite.json") (Join-Path $PayloadPkg "registration\ignite.json")
Copy-BinaryFile (Join-Path $RepoRoot "tools\kindle_launcher\assets\scribe-icon.png") (Join-Path $PayloadPkg "assets\scribe-icon.png")
Copy-TextLf (Join-Path $RepoRoot "ignite\licenses\noto-emoji\OFL.txt") (Join-Path $PayloadPkg "assets\licenses\noto-emoji\OFL.txt")
Copy-TextLf (Join-Path $RepoRoot "ignite\assets\typography\OFL.txt") (Join-Path $PayloadPkg "assets\licenses\source-sans-3\OFL.txt")

$oflDesc = Join-Path $RepoRoot "ignite\licenses\noto-emoji\DESCRIPTION.en_us.html"
if (Test-Path -LiteralPath $oflDesc) {
    Copy-TextLf $oflDesc (Join-Path $PayloadPkg "assets\licenses\noto-emoji\DESCRIPTION.en_us.html")
}

$DropbearDir = Join-Path $RepoRoot "target\dropbear-kindle"
$DropbearBin = Join-Path $DropbearDir "dropbearmulti"
$DropbearSum = Join-Path $DropbearDir "elf.sha256"
$DropbearProv = Join-Path $DropbearDir "provenance.txt"
if (-not (Test-Path -LiteralPath $DropbearBin)) {
    throw "missing verified Dropbear client at $DropbearBin (run tools/dropbear/build-armv7hf-client.sh)"
}
if (-not (Test-Path -LiteralPath $DropbearSum)) {
    throw "missing Dropbear digest at $DropbearSum"
}
Ensure-Dir (Join-Path $PayloadPkg "recovery\bin")
Ensure-Dir (Join-Path $PayloadPkg "recovery\manifest")
Copy-BinaryFile $DropbearBin (Join-Path $PayloadPkg "recovery\bin\dropbearmulti")
Copy-TextLf $DropbearSum (Join-Path $PayloadPkg "recovery\manifest\elf.sha256")
$packagedDropbear = Join-Path $PayloadPkg "recovery\bin\dropbearmulti"
$wantDropbear = (Get-Content -LiteralPath $DropbearSum -Raw).Trim().Split()[0].ToLowerInvariant()
$haveDropbear = (Get-FileHash -LiteralPath $packagedDropbear -Algorithm SHA256).Hash.ToLowerInvariant()
if ($haveDropbear -ne $wantDropbear) {
    throw "packaged Dropbear digest $haveDropbear != manifest $wantDropbear"
}
if (Test-Path -LiteralPath $DropbearProv) {
    Copy-TextLf $DropbearProv (Join-Path $PayloadPkg "recovery\manifest\provenance.txt")
}

foreach ($helper in @(
        "install-runtime.sh",
        "uninstall-runtime.sh",
        "backup-runtime.sh",
        "register-helpers.sh",
        "install-recovery.sh"
    )) {
    $helperSrc = Join-Path $DeployKindle "lib\$helper"
    $helperDst = Join-Path $PayloadPkg "lib\$helper"
    if ($helper -eq "uninstall-runtime.sh") {
        $helperText = [System.IO.File]::ReadAllText($helperSrc)
        if ($helperText -notmatch [regex]::Escape("@@IGNITE_PACKAGE_VERSION@@")) {
            throw "uninstall-runtime.sh missing @@IGNITE_PACKAGE_VERSION@@ stamp"
        }
        Write-Utf8Lf -Path $helperDst -Content ($helperText.Replace("@@IGNITE_PACKAGE_VERSION@@", $Version))
    }
    else {
        Copy-TextLf $helperSrc $helperDst
    }
}

Copy-TextLf (Join-Path $DeployKindle "Uninstall-Ignite.sh") (Join-Path $PayloadPkg "Uninstall-Ignite.sh")
Copy-TextLf (Join-Path $DeployKindle "Backup-Ignite-Data.sh") (Join-Path $PayloadPkg "Backup-Ignite-Data.sh")
Copy-TextLf (Join-Path $DeployKindle "Diagnose-Ignite.sh") (Join-Path $PayloadPkg "Diagnose-Ignite.sh")

Write-Step "Validate payload"
& $PythonExe $ValidatePy --payload $PayloadPkg --version $Version
if ($LASTEXITCODE -ne 0) {
    throw "payload validation failed"
}

Write-Step "Build customer $SfxName"
Ensure-Dir $OutRoot
& $PythonExe $BuildSfxPy --payload $PayloadPkg --version $Version --out $SfxPath
if ($LASTEXITCODE -ne 0) {
    throw "SFX build failed"
}

Write-Step "Validate $SfxName"
& $PythonExe $ValidatePy --sfx $SfxPath --version $Version
if ($LASTEXITCODE -ne 0) {
    throw "SFX validation failed"
}

Write-Step "Write customer SHA-256"
$sfxHash = (Get-FileHash -LiteralPath $SfxPath -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Utf8Lf -Path $SfxShaPath -Content ("$sfxHash  $SfxName`n")

Write-Step "Stage developer/support ZIP (README + installer)"
if (Test-Path -LiteralPath $SupportDir) {
    Remove-Item -LiteralPath $SupportDir -Recurse -Force
}
New-Item -ItemType Directory -Path $SupportDir -Force | Out-Null
Copy-Item -LiteralPath $SfxPath -Destination (Join-Path $SupportDir $SfxName) -Force
$readme = Get-Content -LiteralPath (Join-Path $DeployKindle "README.txt") -Raw
$readme = $readme -replace "Version: see the folder name of this package", "Version: $Version"
Write-Utf8Lf -Path (Join-Path $SupportDir "README.txt") -Content $readme

Write-Step "Validate support folder"
& $PythonExe $ValidatePy --root $SupportDir --version $Version
if ($LASTEXITCODE -ne 0) {
    throw "support-folder validation failed"
}

Write-Step "Create developer/support ZIP"
New-DeterministicZip -SourceDir $SupportDir -ZipPath $ZipPath

Write-Step "Validate support ZIP"
& $PythonExe $ValidatePy --zip $ZipPath --version $Version
if ($LASTEXITCODE -ne 0) {
    throw "support ZIP validation failed"
}

$zipHash = (Get-FileHash -LiteralPath $ZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Utf8Lf -Path $ZipShaPath -Content ("$zipHash  $PackageName.zip`n")

$sfxSize = (Get-Item -LiteralPath $SfxPath).Length
$zipSize = (Get-Item -LiteralPath $ZipPath).Length

Write-Host ""
Write-Host "======== Ignite Kindle release ========"
Write-Host "version:              $Version"
Write-Host "profile:              full-feature (sync-dir + device-dir enabled; optional to use)"
Write-Host "CUSTOMER ARTIFACT:    $SfxPath"
Write-Host "customer_size_bytes:  $sfxSize"
Write-Host "customer_sha256:      $sfxHash"
Write-Host "customer_checksum:    $SfxShaPath"
Write-Host "support ZIP (optional, not for end users): $ZipPath"
Write-Host "support_size_bytes:   $zipSize"
Write-Host "support_sha256:       $zipHash"
Write-Host "======================================="
exit 0
