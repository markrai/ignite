#Requires -Version 5.1
<#
.SYNOPSIS
  Regression: Windows PowerShell 5.1 must forward a literal "--" to Multipass.

.DESCRIPTION
  ValueFromRemainingArguments drops "--" (end-of-parameters), which turned
  `apt-get update -qq` into options seen as "q, q". run-host.ps1 must use an
  explicit [string[]] -Arguments splat instead.

  This test drives a fake Multipass executable through the same Invoke-Multipass
  calling convention as run-host.ps1 and asserts argv retains "--" and "-qq".
#>
$ErrorActionPreference = "Stop"

$RunHost = Join-Path $PSScriptRoot "run-host.ps1"
if (-not (Test-Path -LiteralPath $RunHost)) {
  throw "missing run-host.ps1 at $RunHost"
}

$src = Get-Content -LiteralPath $RunHost -Raw
if ($src -match '(?m)param\s*\(\s*\[Parameter\([^\)]*ValueFromRemainingArguments') {
  throw "run-host.ps1 still binds Multipass args via ValueFromRemainingArguments"
}
if ($src -notmatch '(?s)function\s+Invoke-Multipass\s*\{[^}]*\[string\[\]\]\s*\$Arguments') {
  throw "run-host.ps1 Invoke-Multipass must take explicit [string[]] `$Arguments"
}
if ($src -notmatch 'Invoke-Multipass\s+-Arguments\s+@\(\s*''exec'',\s*\$Name,\s*''--''') {
  throw "run-host.ps1 exec call sites must pass literal '--' inside -Arguments @(...)"
}
if ($src -notmatch "'apt-get',\s*'update',\s*'-qq'") {
  throw "run-host.ps1 must pass apt-get update -qq as separate argv elements after '--'"
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("ignite-mp-argv-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$log = Join-Path $tmp "argv.txt"
$fake = Join-Path $tmp "fake-multipass.ps1"

@'
# Fake Multipass: record each argv element on its own line (no joining).
$ErrorActionPreference = "Stop"
$logPath = $env:IGNITE_FAKE_MP_ARGV_LOG
if (-not $logPath) { throw "IGNITE_FAKE_MP_ARGV_LOG unset" }
$lines = @("---")
foreach ($a in $args) { $lines += [string]$a }
Add-Content -LiteralPath $logPath -Value $lines -Encoding utf8
exit 0
'@ | Set-Content -LiteralPath $fake -Encoding utf8

$env:IGNITE_FAKE_MP_ARGV_LOG = $log
"" | Set-Content -LiteralPath $log -Encoding utf8

# Same binding as run-host.ps1 (explicit string array — not ValueFromRemainingArguments).
function Invoke-Multipass {
  param(
    [Parameter(Mandatory = $true)]
    [string[]]$Arguments
  )
  & $fake @Arguments
  if ($LASTEXITCODE -ne 0) {
    throw "fake multipass failed (exit $LASTEXITCODE)"
  }
}

# Contrast: the broken binding drops "--" under Windows PowerShell 5.1.
function Invoke-MultipassBroken {
  param(
    [Parameter(ValueFromRemainingArguments = $true)]
    $RemainingArguments
  )
  & $fake @RemainingArguments
}

Remove-Item -LiteralPath $log -ErrorAction SilentlyContinue
"" | Set-Content -LiteralPath $log -Encoding utf8

Invoke-MultipassBroken exec vm-name -- apt-get update -qq
$broken = Get-Content -LiteralPath $log
$brokenFlat = ($broken | Where-Object { $_ -ne "---" }) -join "`n"
if ($brokenFlat -match '(?m)^--$') {
  Write-Warning "Broken VFR binding unexpectedly kept '--' on this host; still requiring fixed path to keep it."
} else {
  Write-Host "CONTROL: ValueFromRemainingArguments dropped literal '--' as expected on this PowerShell."
}

Remove-Item -LiteralPath $log -ErrorAction SilentlyContinue
"" | Set-Content -LiteralPath $log -Encoding utf8

# Exact argv shape used by run-host.ps1 for apt-get update.
Invoke-Multipass -Arguments @('exec', 'ignite-linux-accept-deadbeef', '--', 'sudo', 'apt-get', 'update', '-qq')

$recorded = Get-Content -LiteralPath $log | Where-Object { $_ -ne "" -and $_ -ne "---" }
$expected = @(
  "exec",
  "ignite-linux-accept-deadbeef",
  "--",
  "sudo",
  "apt-get",
  "update",
  "-qq"
)
if ($recorded.Count -lt $expected.Count) {
  throw "fake multipass recorded too few args: $($recorded -join ' | ')"
}
# Last invocation only (after --- separator from this call).
$chunk = Get-Content -LiteralPath $log
$lastSep = ($chunk.Count - 1)
for ($i = $chunk.Count - 1; $i -ge 0; $i--) {
  if ($chunk[$i] -eq "---") { $lastSep = $i; break }
}
$got = @($chunk[($lastSep + 1)..($chunk.Count - 1)] | Where-Object { $_ -ne "" })
for ($i = 0; $i -lt $expected.Count; $i++) {
  if ($got[$i] -ne $expected[$i]) {
    throw ("argv[$i] expected '{0}' got '{1}'. Full: {2}" -f $expected[$i], $got[$i], ($got -join ' | '))
  }
}
if ($got -notcontains "--") {
  throw "literal '--' separator missing from forwarded argv"
}
if ($got[-1] -ne "-qq") {
  throw "-qq was not preserved as a single argv element (got last='$($got[-1])')"
}

# Also exercise the install-shaped call with -qq after '--'.
Invoke-Multipass -Arguments @(
  'exec', 'ignite-linux-accept-deadbeef', '--',
  'sudo', 'env', 'DEBIAN_FRONTEND=noninteractive',
  'apt-get', 'install', '-y', '-qq', 'curl', 'ca-certificates', 'avahi-utils'
)
$chunk2 = Get-Content -LiteralPath $log
$lastSep2 = 0
for ($i = $chunk2.Count - 1; $i -ge 0; $i--) {
  if ($chunk2[$i] -eq "---") { $lastSep2 = $i; break }
}
$got2 = @($chunk2[($lastSep2 + 1)..($chunk2.Count - 1)] | Where-Object { $_ -ne "" })
if ($got2[2] -ne "--") {
  throw "install-shaped exec lost '--' (argv: $($got2 -join ' | '))"
}
if ($got2 -notcontains "-qq") {
  throw "install-shaped exec lost -qq (argv: $($got2 -join ' | '))"
}

Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
Write-Host "PASS: run-host.ps1 argv convention retains literal '--' and '-qq' under Windows PowerShell $($PSVersionTable.PSVersion)"
exit 0
