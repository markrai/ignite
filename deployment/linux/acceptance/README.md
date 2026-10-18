# Ignite Server — disposable Linux host acceptance

Live-systemd acceptance for the portable Linux bundle. This is **not** the
staging `--root=` suite and is **not** a Docker Compose / Portainer package.

Evidence classes are recorded explicitly and must not be conflated:

| Class | Meaning |
|-------|---------|
| `true-vm` | Hypervisor VM (Multipass / similar) with systemd as PID 1 |
| `systemd-container` | Privileged disposable container with systemd as PID 1 — **not** a Multipass VM |
| `http-client` / `headless-chromium` | Browser-path evidence (curl vs headless Chromium) |
| `mdns-observed` / `mdns-unavailable` / … | LAN discovery evidence |

Staging seams (`--test-seam=…`) are intentionally **not** used here.

## What it proves (when the chosen platform succeeds)

1. Documented `./install.sh` on a live systemd guest
2. `scribe-server.service` active as user `scribe`
3. ignite-web HTML + PWA id `/ignite-web`, claim, session cookie (cookie kept in memory only)
4. State permissions, listen binding, discovery best-effort
5. Upgrade preserves Server ID / claim / ledger
6. Failed upgrade rolls back via real `systemctl` recovery
7. Uninstall keeps data; reinstall resumes identity

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
```

`run-host.ps1` requires Multipass (resolves `Program Files\Multipass\bin` if
needed), labels evidence `platform_class=true-vm`, and records
`multipass_backend=<driver>`. It does **not** fall back to Docker.
On guest failure it attempts to pull sanitized evidence before deleting the VM
(use `-KeepVmOnFailure` to leave the instance for inspection).

Manual Multipass (any host with `multipass`):

```bash
multipass launch 24.04 --name ignite-accept --cpus 2 --memory 2G --disk 10G
multipass transfer -r dist/ignite-server-linux ignite-accept:/home/ubuntu/bundle
multipass transfer deployment/linux/acceptance/guest-acceptance.sh ignite-accept:/home/ubuntu/
multipass exec ignite-accept -- sudo apt-get update
multipass exec ignite-accept -- sudo apt-get install -y curl
multipass exec ignite-accept -- sudo env IGNITE_ACCEPT_PLATFORM_CLASS=true-vm \
  IGNITE_ACCEPT_PLATFORM_DRIVER=multipass \
  bash guest-acceptance.sh /home/ubuntu/bundle
multipass delete --purge ignite-accept
```

### Systemd container (explicit fallback — not a VM)

`run-host.sh` may use Docker `jrei/systemd-ubuntu:24.04` when Multipass is absent.
That path labels evidence `platform_class=systemd-container` and must **never** be
reported as a Multipass / true-VM result.

```bash
# Linux or WSL with Docker:
bash deployment/linux/acceptance/run-host.sh dist/ignite-server-linux
# Force container path even if multipass exists:
IGNITE_ACCEPT_FORCE_DOCKER=1 bash deployment/linux/acceptance/run-host.sh dist/ignite-server-linux
```

Neither path runs `install.sh` on the Windows development host.

## Evidence hygiene

- Setup codes and session cookies are redacted / kept in process memory only.
- Temporary bad-upgrade bundles and binary copies live under `/tmp` scratch, not the evidence archive.
- Browser build trees are not copied into evidence.
- Generated dirs `deployment/linux/acceptance/evidence-*/` are gitignored.

## Limitations

- Guest uses `--firewall=none --port=8787` so the developer host firewall is untouched.
- mDNS browse may fail on bridge/NAT networks without multicast.
- Curl claim/session is not a GUI browser walkthrough unless headless Chromium is present.
