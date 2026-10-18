#Requires -Version 5.1
<#
.SYNOPSIS
  True-VM Linux acceptance via native Windows Multipass.

.DESCRIPTION
  Requires Multipass already installed on Windows. Evidence is labeled
  platform_class=true-vm. Does NOT fall back to Docker.

  Records the configured Multipass backend (e.g. hcs, virtualbox) without
  changing it. On guest failure, attempts to pull sanitized evidence before
  deleting the disposable VM.

  Invoke-Multipass takes an explicit [string[]] so Windows PowerShell 5.1
  preserves the literal "--" separator (ValueFromRemainingArguments drops it).

.PARAMETER BundleDir
  Path to dist/ignite-server-linux (or equivalent bundle).

.PARAMETER KeepVmOnFailure
  If set, leave the disposable VM running after a failed guest run so the
  owner can inspect it. Default: always delete the disposable instance.

.PARAMETER MultipassExeOverride
  Optional path to a Multipass-compatible executable (used by regression tests).
#>
param(
  [string]$BundleDir = "dist\ignite-server-linux",
  [switch]$KeepVmOnFailure,
  [string]$MultipassExeOverride = ""
)

$ErrorActionPreference = "Stop"
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..\..\..")).Path
$Bundle = if ([System.IO.Path]::IsPathRooted($BundleDir)) {
  (Resolve-Path $BundleDir).Path
} else {
  (Resolve-Path (Join-Path $Root $BundleDir)).Path
}
$GuestScript = Join-Path $PSScriptRoot "guest-acceptance.sh"
$PrepareScript = Join-Path $PSScriptRoot "prepare-guest.sh"
$Name = "ignite-linux-accept-" + [guid]::NewGuid().ToString("N").Substring(0, 8)
$EvidenceHost = Join-Path $PSScriptRoot ("evidence-" + (Get-Date).ToUniversalTime().ToString("yyyyMMddTHHmmssZ"))
New-Item -ItemType Directory -Force -Path $EvidenceHost | Out-Null
$GuestEvidenceDir = Join-Path $EvidenceHost "guest"
New-Item -ItemType Directory -Force -Path $GuestEvidenceDir | Out-Null

function Resolve-MultipassExe {
  if ($MultipassExeOverride) {
    if (-not (Test-Path -LiteralPath $MultipassExeOverride)) {
      throw "MultipassExeOverride not found: $MultipassExeOverride"
    }
    return (Resolve-Path -LiteralPath $MultipassExeOverride).Path
  }
  $cmd = Get-Command multipass -ErrorAction SilentlyContinue
  if ($cmd) { return $cmd.Source }
  $candidates = @(
    (Join-Path ${env:ProgramFiles} "Multipass\bin\multipass.exe"),
    (Join-Path ${env:ProgramFiles(x86)} "Multipass\bin\multipass.exe")
  )
  foreach ($c in $candidates) {
    if (Test-Path -LiteralPath $c) { return $c }
  }
  throw @"
Multipass executable not found on PATH or under Program Files.
This driver does not fall back to Docker (use run-host.sh for systemd-container evidence only).
"@
}

$MultipassExe = Resolve-MultipassExe

# Explicit string-array argv — required so PowerShell 5.1 keeps literal "--".
function Invoke-Multipass {
  param(
    [Parameter(Mandatory = $true)]
    [string[]]$Arguments
  )
  & $MultipassExe @Arguments
  if ($LASTEXITCODE -ne 0) {
    throw "multipass $($Arguments -join ' ') failed (exit $LASTEXITCODE)"
  }
}

function Try-Pull-Evidence {
  param([string]$InstanceName)
  $probe = & $MultipassExe @('exec', $InstanceName, '--', 'bash', '-lc', 'test -d /home/ubuntu/evidence && echo YES || echo NO')
  $probeText = ($probe | Out-String).Trim()
  if ($probeText -notmatch 'YES') {
    Write-Warning ("Evidence transfer skipped: guest has not created /home/ubuntu/evidence yet " +
      "(typical when failure occurs before the acceptance script writes diagnostics). Primary error is unchanged.")
    return
  }
  try {
    & $MultipassExe @('transfer', '-r', "${InstanceName}:/home/ubuntu/evidence", $GuestEvidenceDir)
    $xferExit = $LASTEXITCODE
    $summaryLanded = $null -ne (
      Get-ChildItem -Path $EvidenceHost -Recurse -Filter summary.txt -ErrorAction SilentlyContinue |
        Select-Object -First 1
    )
    if ($xferExit -eq 0) { return }
    if ($summaryLanded) {
      # Multipass on Windows often returns non-zero from SFTP chmod noise while files still land.
      Write-Warning "Evidence transfer exited $xferExit (Windows SFTP permission noise); summary.txt is present under $EvidenceHost."
      return
    }
    Write-Warning "Evidence transfer returned exit $xferExit and summary.txt is missing; primary guest/host error is unchanged."
  } catch {
    Write-Warning "Could not pull guest evidence: $_. Primary guest/host error is unchanged."
  }
}

if (-not (Test-Path (Join-Path $Bundle "install.sh"))) {
  throw "Bundle missing install.sh: $Bundle"
}
if (-not (Test-Path (Join-Path $Bundle "scribe-server"))) {
  throw "Bundle missing scribe-server: $Bundle"
}
if (-not (Test-Path (Join-Path $Bundle "web\index.html")) -and -not (Test-Path (Join-Path $Bundle "web/index.html"))) {
  throw "Bundle missing web/index.html: $Bundle"
}
if (-not (Test-Path $GuestScript)) {
  throw "Missing guest-acceptance.sh"
}
if (-not (Test-Path $PrepareScript)) {
  throw "Missing prepare-guest.sh"
}

$MpVersion = (& $MultipassExe @('version') 2>&1 | Out-String).Trim()
$MpDriver = (& $MultipassExe @('get', 'local.driver') 2>&1 | Out-String).Trim()
if (-not $MpDriver) { $MpDriver = "unknown" }

# Fail fast with a clear diagnosis; never change local.driver automatically.
if ($MpDriver -eq "virtualbox") {
  $vbox = @(
    (Join-Path ${env:ProgramFiles} "Oracle\VirtualBox\VBoxManage.exe"),
    (Join-Path ${env:ProgramFiles(x86)} "Oracle\VirtualBox\VBoxManage.exe")
  ) | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
  if (-not $vbox) {
    throw @"
Multipass local.driver is 'virtualbox' but VBoxManage.exe was not found.
This script does not change the Multipass driver automatically.
If this host should use the Windows HCS backend instead, the owner can run:
  multipass set local.driver=hcs
Then re-run run-host.ps1. Current driver remains: $MpDriver
"@
  }
}

@"
platform_class=true-vm
platform_driver=multipass-24.04
multipass_backend=$MpDriver
multipass_version<<EOF
$MpVersion
EOF
host_driver=run-host.ps1
"@ | Set-Content -Encoding utf8 (Join-Path $EvidenceHost "platform.txt")

Write-Host "platform_class=true-vm platform_driver=multipass-24.04 multipass_backend=$MpDriver"
Write-Host "multipass_exe=$MultipassExe"
Write-Host "bundle=$Bundle"
Write-Host "evidence=$EvidenceHost"

$guestFailed = $false
$vmCreated = $false
try {
  Invoke-Multipass -Arguments @('launch', '24.04', '--name', $Name, '--cpus', '2', '--memory', '2G', '--disk', '10G')
  $vmCreated = $true

  # Transfer the bundle directory; Multipass places it as .../ignite-server-linux
  # when the source folder is named that way. Normalize in the guest.
  Invoke-Multipass -Arguments @('transfer', '-r', $Bundle, "${Name}:/home/ubuntu/bundle-upload")
  Invoke-Multipass -Arguments @('transfer', $GuestScript, "${Name}:/home/ubuntu/guest-acceptance.sh")
  Invoke-Multipass -Arguments @('transfer', $PrepareScript, "${Name}:/home/ubuntu/prepare-guest.sh")

  # Fresh Multipass/HCS guests can boot with a skewed clock; apt then rejects
  # InRelease files as "not valid yet". Wait for NTP before package ops.
  Invoke-Multipass -Arguments @(
    'exec', $Name, '--', 'bash', '-lc',
    'sudo timedatectl set-ntp true 2>/dev/null || true; for i in $(seq 1 40); do synced=$(timedatectl show -p NTPSynchronized --value 2>/dev/null || echo no); [ "$synced" = yes ] && break; sleep 3; done; date -u; timedatectl status || true'
  )
  Invoke-Multipass -Arguments @('exec', $Name, '--', 'sudo', 'apt-get', 'update', '-qq')
  Invoke-Multipass -Arguments @(
    'exec', $Name, '--',
    'sudo', 'env', 'DEBIAN_FRONTEND=noninteractive',
    'apt-get', 'install', '-y', '-qq', 'curl', 'ca-certificates', 'avahi-utils'
  )

  # Strip CRLF on the helper before exec (transfer from Windows may reintroduce them).
  Invoke-Multipass -Arguments @('exec', $Name, '--', 'sed', '-i', 's/\r$//', '/home/ubuntu/prepare-guest.sh')
  # prepare-guest.sh is transferred as a file so Windows quoting cannot mangle argv.
  Invoke-Multipass -Arguments @(
    'exec', $Name, '--',
    'sudo', 'env',
    'IGNITE_ACCEPT_PLATFORM_CLASS=true-vm',
    'IGNITE_ACCEPT_PLATFORM_DRIVER=multipass-24.04',
    "IGNITE_ACCEPT_MULTIPASS_BACKEND=$MpDriver",
    'bash', '/home/ubuntu/prepare-guest.sh'
  )

  Try-Pull-Evidence -InstanceName $Name
}
catch {
  $guestFailed = $true
  if ($vmCreated) {
    Write-Warning "Guest/host step failed; attempting evidence pull before cleanup."
    Try-Pull-Evidence -InstanceName $Name
  }
  throw
}
finally {
  if ($vmCreated) {
    if ($guestFailed -and $KeepVmOnFailure) {
      Write-Warning "Keeping disposable VM '$Name' for inspection (-KeepVmOnFailure)."
    } else {
      try {
        & $MultipassExe @('delete', '--purge', $Name)
      } catch {
        Write-Warning "VM cleanup warning: $_"
      }
    }
  }
}

$summary = Join-Path $GuestEvidenceDir "summary.txt"
if (-not (Test-Path $summary)) {
  $summary = Join-Path $GuestEvidenceDir "evidence\summary.txt"
}
if (-not (Test-Path $summary)) {
  # transfer -r may nest as guest/evidence/summary.txt or guest/summary.txt
  $found = Get-ChildItem -Path $EvidenceHost -Recurse -Filter summary.txt -ErrorAction SilentlyContinue |
    Select-Object -First 1 -ExpandProperty FullName
  if ($found) { $summary = $found }
}
if (-not (Test-Path $summary)) {
  throw "missing guest summary.txt under $EvidenceHost (see partial evidence if any)"
}
Get-Content $summary | Write-Host
if (-not (Select-String -Path $summary -Pattern "ACCEPT PASS" -Quiet)) {
  throw "guest did not report ACCEPT PASS"
}
if (-not (Select-String -Path $summary -Pattern "platform_class=true-vm" -Quiet)) {
  throw "true-vm run missing platform_class label"
}
Write-Host "HOST ACCEPT COMPLETE evidence=$EvidenceHost platform_class=true-vm multipass_backend=$MpDriver"
