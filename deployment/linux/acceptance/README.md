# Ignite Server — disposable Linux host acceptance

Live-systemd acceptance for the portable Linux bundle. This is **not** the
staging `--root=` suite and is **not** a Docker Compose / Portainer package.

Evidence classes are recorded explicitly and must not be conflated:

| Class | Meaning |
|-------|---------|
| `true-vm` | Hypervisor VM (Multipass / similar) with systemd as PID 1 |
| `systemd-container` | Privileged disposable container with systemd as PID 1 — **not** a Multipass VM |
| `playwright-chromium` | Real Chromium via Playwright: UI claim, HttpOnly session, authed API, reload, WASM init + canvas project |
| `http-client` | curl HTML/manifest/claim/session only — **not** JS/WASM proof |
| `headless-chromium` | Weak `--dump-dom` only — **not** interactive claim / WASM proof |
| `mdns-observed` / `mdns-unavailable` / … | LAN discovery evidence |

Staging seams (`--test-seam=…`) are intentionally **not** used here.

## What it proves (when the chosen platform succeeds)

1. Documented `./install.sh` on a live systemd guest
2. `scribe-server.service` active as user `scribe`
3. ignite-web HTML + PWA id `/ignite-web`
4. **Windows Multipass path:** Playwright Chromium smoke (claim UI, auth, WASM-backed viewer)
5. State permissions, listen binding, discovery best-effort
6. Upgrade preserves Server ID / claim / ledger
7. Failed upgrade rolls back via real `systemctl` recovery
8. Uninstall keeps data; reinstall resumes identity

## Platforms

### True VM (preferred): Multipass on Windows

Use **native Windows Multipass** from **PowerShell** (not WSL). Record whatever
`multipass get local.driver` returns (`hcs`, `virtualbox`, etc.) — do not assume
or force the deprecated `hyperv` driver name, and do not change the driver inside
automation. If the configured driver is `virtualbox`, Oracle VirtualBox /
`VBoxManage.exe` must be installed; otherwise Multipass cannot launch guests.

```powershell
multipass version
multipass get local.driver
# From the repository root in PowerShell:
.\deployment\linux\acceptance\run-host.ps1 -BundleDir dist\ignite-server-linux
# On failure, keep the VM for inspection:
.\deployment\linux\acceptance\run-host.ps1 -BundleDir dist\ignite-server-linux -KeepVmOnFailure
```

`run-host.ps1` requires Multipass (resolves `Program Files\Multipass\bin` if
needed), labels evidence `platform_class=true-vm`, records
`multipass_backend=<driver>`, and sets `IGNITE_ACCEPT_REQUIRE_BROWSER=1`.
It installs Node/npm in the guest, runs `npm ci` + Playwright Chromium from
[`browser-smoke/`](browser-smoke/), and **fails** if the smoke does not report
`browser_class=playwright-chromium`. It does **not** fall back to Docker or to
HTTP-only claim.

On guest failure it attempts to pull sanitized evidence before deleting the VM
(use `-KeepVmOnFailure` to leave the instance for inspection).

Guest resources: 2 CPUs / 3G RAM / 15G disk (Chromium + npm).

### Systemd container (explicit fallback — not a VM)

`run-host.sh` may use Docker `jrei/systemd-ubuntu:24.04` when Multipass is absent.
That path labels evidence `platform_class=systemd-container` and must **never** be
reported as a Multipass / true-VM result. Browser smoke is **optional** there
unless `IGNITE_ACCEPT_REQUIRE_BROWSER=1` is set.

```bash
# Linux or WSL with Docker:
bash deployment/linux/acceptance/run-host.sh dist/ignite-server-linux
# Force container path even if multipass exists:
IGNITE_ACCEPT_FORCE_DOCKER=1 bash deployment/linux/acceptance/run-host.sh dist/ignite-server-linux
```

Neither path runs `install.sh` on the Windows development host.

## Browser smoke (`browser-smoke/`)

Pinned Playwright (`package-lock.json` + `npm ci`). The smoke:

1. Loads the claim UI and claims via `setup-form` / `setup-code` / `claim`
2. Asserts HttpOnly `scribe_session`, authenticated `/control/v1/status`, reload survival
3. Seeds a fixture library (Kindle pairing API; credentials never persisted)
4. Opens the library and waits for `notebook-item` + canvas `data-rendered=true` (WASM path)
5. Writes sanitized `browser-smoke.json` (+ post-claim screenshot) even on failure

Local regressions (no Multipass / no browser launch):

```powershell
cd deployment\linux\acceptance\browser-smoke
npm ci --omit=dev
npm test
cd ..\..\..\..
powershell -NoProfile -File deployment\linux\acceptance\test-guest-browser-class.ps1
powershell -NoProfile -File deployment\linux\acceptance\test-run-host-ps1-argv.ps1
```

## Evidence hygiene

- Setup codes and session cookies are redacted / kept in process memory only.
- Temporary bad-upgrade bundles and binary copies live under `/tmp` scratch, not the evidence archive.
- Browser build trees are not copied into evidence.
- Smoke evidence strips cookie values, setup codes, Bearer tokens, and URL query strings.
- Generated dirs `deployment/linux/acceptance/evidence-*/` are gitignored.

## Limitations

- Guest uses `--firewall=none --port=8787` so the developer host firewall is untouched.
- mDNS browse may fail on bridge/NAT networks without multicast.
- Playwright smoke is claim + empty-then-seeded library viewer readiness — not full ink/e2e product suite.
- Docker/`http-client` acceptance must not be reported as `playwright-chromium`.
