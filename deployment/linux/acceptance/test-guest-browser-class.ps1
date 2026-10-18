#Requires -Version 5.1
<#
.SYNOPSIS
  Structure regression: guest-acceptance.sh only labels playwright-chromium after smoke success,
  and REQUIRE_BROWSER=1 has no HTTP-only claim fallback.
#>
$ErrorActionPreference = "Stop"
$Guest = Join-Path $PSScriptRoot "guest-acceptance.sh"
if (-not (Test-Path -LiteralPath $Guest)) { throw "missing $Guest" }
$src = Get-Content -LiteralPath $Guest -Raw

if ($src -notmatch 'IGNITE_ACCEPT_REQUIRE_BROWSER') {
  throw "guest-acceptance.sh must honor IGNITE_ACCEPT_REQUIRE_BROWSER"
}
if ($src -notmatch 'browser_class=playwright-chromium') {
  throw "guest-acceptance.sh must record browser_class=playwright-chromium"
}
if ($src -notmatch 'run_browser_smoke') {
  throw "guest-acceptance.sh must define/run browser smoke"
}

# Success path must require smoke JSON ok + class before labeling.
if ($src -notmatch 'grep -q ''"browser_class": "playwright-chromium"''') {
  throw "must grep browser-smoke.json for playwright-chromium before labeling"
}
if ($src -notmatch 'grep -q ''"ok": true''') {
  throw "must require browser-smoke.json ok=true before labeling"
}

# REQUIRE_BROWSER=1 must refuse HTTP-only fallback on smoke failure.
if ($src -notmatch 'no HTTP-only fallback') {
  throw "REQUIRE_BROWSER failure path must state no HTTP-only fallback"
}

# Curl claim must live only in the non-require / fallback branch (after optional smoke).
$curlClaimIdx = $src.IndexOf('control/v1/claim')
$requireIdx = $src.IndexOf('REQUIRE_BROWSER="${IGNITE_ACCEPT_REQUIRE_BROWSER:-0}"')
if ($curlClaimIdx -lt 0) { throw "curl claim path missing (needed for non-require fallback)" }
if ($requireIdx -lt 0) { throw "REQUIRE_BROWSER binding missing" }
if ($curlClaimIdx -lt $requireIdx) {
  throw "curl claim must not run before REQUIRE_BROWSER branching (would claim before Playwright)"
}

# Optional path: curl only when smoke not attempted (rc 2/3/4); attempted failure must fail.
if ($src -notmatch 'browser_smoke_skipped=not_attempted') {
  throw "optional path must note curl fallback only when smoke not attempted"
}
if ($src -notmatch 'refusing curl re-claim') {
  throw "optional path must refuse curl re-claim after attempted Playwright failure"
}
if ($src -notmatch 'smoke_rc" -eq 2') {
  throw "optional path must gate curl fallback on smoke_rc 2/3/4"
}

Write-Host "PASS: guest-acceptance.sh browser_class gating structure ok"
exit 0
