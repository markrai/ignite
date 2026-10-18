# Ignite Server — Docker / Portainer (Linux host networking)

Production-oriented packaging for **Linux Docker Engine** (including Portainer on a
Linux server or compatible NAS). Separate from the native systemd installer.

| Item | Value |
|------|--------|
| Image (local tag) | `ignite-server:local` |
| Runtime PID 1 UID/GID | `19187:19187` (after entrypoint drop) |
| Image `Config.User` | root (entrypoint chown then `setpriv`) |
| State | volume → `/var/lib/scribe-server` |
| Config | embedded in image; optional host override |
| Web assets | `/usr/share/scribe-server/web` (image layers) |
| Network | `network_mode: host` |
| Default listen | `0.0.0.0:8787` |
| Architecture | **linux/amd64** validated; ARM not claimed |

**Not supported as the default:** ordinary bridge networking + published ports.
See [`../docker-network-feasibility.md`](../docker-network-feasibility.md).

## Build the image

Runtime Compose is **image-only** (no `build:` / `context/`). Build separately:

```bash
bash deployment/linux/build-bundle.sh
bash deployment/linux/docker/build-image.sh
# → ignite-server:local
```

Or: `IGNITE_DOCKER_BUILD_BUNDLE=1 bash deployment/linux/docker/build-image.sh`

Images are **not** published to Docker Hub/GHCR by these scripts. For Portainer on
another host, `docker save` / `docker load` (or your private distribution) the local tag.

## Local Docker Compose

```bash
cd deployment/linux/docker
bash ./init-once.sh          # first run only; prints setup_code=… while unclaimed
docker compose up -d
curl -fsS http://127.0.0.1:8787/control/v1/status
```

The stack uses the **image-embedded** `/etc/scribe-server/config.toml`. To override
locally, uncomment the bind in `docker-compose.yml` pointing at `./config.toml`.

## Portainer (Linux Engine)

1. Build/load `ignite-server:local` (or your chosen tag) on that engine.
2. Create a stack from `docker-compose.yml` (paste or repo). Do **not** expect a
   generated `context/` directory — there is no Compose `build:`.
3. Config:
   - **Default:** omit any config bind; the image embeds `config.toml`.
   - **Custom:** add a bind with an **absolute host path**, e.g.
     `/opt/ignite/config.toml:/etc/scribe-server/config.toml:ro`
     (copy from this directory’s `config.toml` example). Relative `./config.toml`
     often does not resolve usefully in Portainer’s web editor.
4. Ensure **host networking** is available. Do not add `ports:` alongside host mode.
5. First-run init against the **same** named volume (`ignite-server-state`):

```bash
docker compose run --rm ignite-server init --config /etc/scribe-server/config.toml
# or Portainer one-off container with the same image + volume + command
```

Capture `setup_code=` from that command’s stdout only, then start the stack and claim
in ignite-web. Never put setup codes in stack env vars or Compose YAML.

### Host firewall (operator responsibility)

Compose **does not** apply the native installer firewall. Allow only your trusted LAN
to TCP `8787` and mDNS as appropriate. Application `netpolicy` is not a firewall substitute.

### Port conflicts

If `8787` is taken, override config `listen` and the healthcheck URL together.

## Volume ownership and privileges

- **Image USER** is root so the entrypoint can `chown` a fresh named volume.
- Entrypoint then `setpriv` → **PID 1 = 19187:19187** with inheritable/ambient caps cleared
  and Compose `no-new-privileges`.
- Do not claim that *no* process in the container ever runs as root; the entrypoint’s
  brief root phase is intentional for volume init.

## Backup / restore / upgrades

Back up the Docker volume for `/var/lib/scribe-server` (and any host config override).
Restore onto the same volume and start a current image. Do **not** use
`docker compose down -v` unless you intend to destroy data.

**Image upgrade:** replace the image tag and recreate the container with the **same**
volume. Disposable acceptance proves restart/recreate with the **same** image digest;
a distinct software-version migration path is **not** claimed by that suite. The suite does
pair a disposable client and round-trip a fixture blob, library, and operations through the
product API, then re-verifies them after restart and after container recreate.

## Security notes

- Plain HTTP, trusted LAN only.
- No Docker socket mount, no privileged mode.
- Setup codes / session cookies never belong in Compose, `.env`, or CI evidence artifacts.

## Checks

```bash
bash deployment/linux/docker/test-compose-static.sh
bash deployment/linux/docker/test-accept-safety.sh
bash deployment/linux/docker/assert-evidence-clean.sh deployment/linux/docker/evidence-<id>

# Full disposable accept in a fresh Multipass VM (the default; no flags needed):
bash deployment/linux/docker/accept-disposable.sh

# Explicit: this host's Linux Docker Engine instead (CI runners, Linux dev boxes):
IGNITE_DOCKER_ACCEPT=local bash deployment/linux/docker/accept-disposable.sh
```

### Disposable acceptance

`accept-disposable.sh` defaults to a new Ubuntu 24.04 Multipass VM with Docker installed
inside the guest only. From Windows Command Prompt, run it from the repository root:

```bat
bash deployment/linux/docker/accept-disposable.sh
```

`bash` here is WSL. The script uses the Windows `multipass.exe` through WSL interop (found on
`PATH` or at `C:\Program Files\Multipass\bin\multipass.exe`), so Multipass does not need
to be installed inside WSL. Git Bash, Linux, and macOS work the same way. For an unusual
install location set `IGNITE_MULTIPASS=/path/to/multipass[.exe]`. Windows CMD `set VAR=...`
values are **not** visible inside WSL unless listed in `WSLENV`; the default needs none.

`multipass.exe` can only read files on a Windows drive, and it does not pass piped stdin through
to `multipass exec`. So when the Windows executable is in use, the payload is staged in a private
directory under the Windows user's `%TEMP%`, sent with a relative `multipass transfer`, and
checked by sha256 inside the guest. The stage is removed afterwards. If no drive-backed temp
directory can be found, set `IGNITE_DOCKER_STAGE_DIR` to a writable directory on a Windows drive.
Evidence comes back the same way: the guest archive is downloaded with `multipass transfer`
into the stage under a relative name and checked against the guest's sha256 before it is
extracted. `multipass.exe` can exit nonzero with `cannot set permissions for local file` after
writing the whole file. That case is accepted, with a warning, only when the hash matches. A
missing, truncated or corrupt download fails the run. Neither direction streams base64 through
`multipass exec`. A transfer that takes longer than `IGNITE_MULTIPASS_TRANSFER_TIMEOUT` seconds
(default 900) fails the run, and the VM is still cleaned up. This bound needs GNU `timeout`
(or `gtimeout` on macOS via `brew install coreutils`). Without either, the run stops before
any transfer.

Safety comes from isolation and ownership, not confirmation flags:

- The VM, compose project, volume, and image carry this run's unique id. A preexisting
  instance or resource with that name is refused, never adopted.
- Only what this run created is deleted, on success, failure, or Ctrl-C. The VM deletion is
  retried and verified; `MULTIPASS_CLEANUP=PASS|FAIL` is recorded, and a failed purge makes the
  run exit nonzero with the `multipass delete --purge <vm>` command to finish it.
- If Multipass is missing, the run stops with one error. It never falls back to the host's
  Docker Engine and never installs anything on the host.
- Setup codes, session cookies, pairing secrets, and bearer credentials stay in a private
  scratch directory (inside the guest in Multipass mode). They are never printed, passed on
  a command line, or written to evidence. Retrieved evidence is secret-scanned again on the
  host.

Optional browser claim: `IGNITE_DOCKER_BROWSER_SMOKE=1` claims the unclaimed server through
Playwright Chromium (installed inside the disposable guest in Multipass mode; never installed
on the host in local mode). `BROWSER_CLAIM` is reported separately from `API_CLAIM`. When the
browser claim is requested, missing dependencies report `BLOCKED` and a failure reports `FAIL`;
either one makes the overall `RESULT` non-PASS while the API results stay visible.

`summary.txt` reports each check on its own line: `NOTEBOOK_BLOB_ROUNDTRIP`,
`RESTART_PERSISTENCE`, `RECREATE_PERSISTENCE`, `API_CLAIM`, `BROWSER_CLAIM`, and `EVIDENCE_SCAN`.
Virtual networking is not physical-LAN proof: `MILESTONE_B_PHYSICAL_LAN` and
`MILESTONE_C_KINDLE_E2E` stay `NOT_TESTED`.
