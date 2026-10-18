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
a distinct software-version migration path is **not** claimed by that suite. Notebook/blob
API round-trips are a documented follow-up (filesystem ledger markers only are tested).

## Security notes

- Plain HTTP, trusted LAN only.
- No Docker socket mount, no privileged mode.
- Setup codes / session cookies never belong in Compose, `.env`, or CI evidence artifacts.

## Checks

```bash
bash deployment/linux/docker/test-compose-static.sh
bash deployment/linux/docker/test-accept-safety.sh
bash deployment/linux/docker/assert-evidence-clean.sh deployment/linux/docker/evidence-<id>

# Full disposable accept (explicit opt-in):
IGNITE_DOCKER_ACCEPT=1 bash deployment/linux/docker/accept-disposable.sh
# IGNITE_DOCKER_ACCEPT=multipass bash deployment/linux/docker/accept-disposable.sh
```
