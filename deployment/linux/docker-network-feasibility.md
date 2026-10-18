# Ignite Server — Docker networking feasibility

**Date:** 2026-10-18  
**Scope:** Evaluate packaging Ignite Server for Docker/Portainer **without** weakening
[`netpolicy.rs`](../../ignite/server/scribe-server/src/netpolicy.rs) / DNS-SD eligibility.  
**No Compose implementation in this phase.**

## Policy constraints (current code)

From `ignite/server/scribe-server/src/netpolicy.rs` and `discovery.rs`:

| Rule | Behavior |
|------|----------|
| Eligible interface | Name **not** in `NON_LAN_PREFIXES` (`docker*`, `br-*`, `veth*`, `lo`, VPN/VM prefixes, …) **and** address is private RFC1918 / IPv6 ULA |
| Accept | Local addr is eligible **and** peer is on that interface’s subnet, **or** both ends loopback |
| Refuse | Traffic arriving via docker/bridge/VPN/public/CGNAT paths to a LAN bind |
| Advertise | `_scribe._tcp` only on policy-eligible interfaces; loopback never advertised |

Implication: a process that only sees `docker0` / `veth*` / container `eth0@ifN` on `172.17.0.0/16` has **no eligible LAN address** for product accept/advertise (except loopback diagnostics).

## Modes evaluated

### 1. Docker host networking (Linux) — preferred

| Question | Assessment |
|----------|------------|
| LAN clients pass policy? | **Yes, when the host’s physical/private NIC is eligible** — container shares host netns, so `os_inventory()` sees `eth0`/`enp*`/etc. Peer must still be on-link to that subnet. |
| Loopback health checks? | **Yes** — `127.0.0.1` ↔ `127.0.0.1` → `AllowLoopback`. |
| NAT / address translation? | **None inside the container** for host network; observed local/peer addrs are host addrs. |
| mDNS to real Kindles? | **Feasible on a Linux host on the real LAN** (multicast on eligible iface). **Not proven** on Multipass HCS (virtual `10.97.0.0/24`, no physical Kindle path). |
| Portainer stacks? | Supported on Linux engines via `"network_mode": "host"` (Compose `network_mode: host`). **Not available the same way on Docker Desktop Windows/macOS.** |
| Privileges / conflicts | Needs Docker permission to use host net; **port 8787 (or chosen port) must be free on the host**; no extra NET_ADMIN required for Ignite itself; still run as non-root UID inside the container. |
| Accidental WAN exposure? | Host firewall / installer ufw scoping still apply to eligible ifaces only. Host networking does **not** open public interfaces that fail eligibility. Risk is **same as bare-metal install** on that host. |

**Probe evidence (disposable Multipass Ubuntu 24.04 + Docker 29.1.3, 2026-10-18):**

- Host ifaces: `lo`, `eth0` (`10.97.0.86/24`), `docker0` (`172.17.0.1/16`).
- `--network host` container saw the **same** `lo` + `eth0` + `docker0` inventory.
- Host-network loopback HTTP on `127.0.0.1:18787` → `ok-hostnet`.
- Name eligibility simulation: `eth0` eligible; `docker0`/`br-*`/`veth*`/`lo` not.

Script: [`docker-network-probe.sh`](docker-network-probe.sh) (disposable; not a Kindle proof).

### 2. Ordinary bridge networking + published ports — unsuitable without app changes

| Question | Assessment |
|----------|------------|
| LAN clients pass policy? | **No (as-is).** Server inside bridge netns typically binds/sees `172.17.0.x` on `eth0@if*` (veth). That name/address pair is **not eligible**. Even if listening on `0.0.0.0`, accept filter uses **local destination addr + peer**; Docker DNAT often presents peer as gateway/docker bridge — fails on-link subnet check. |
| Loopback health checks? | Container-local loopback works **inside** the netns; host `curl` via `-p` published ports hits DNAT path, **not** the documented loopback allow pair unless healthcheck runs in-container. |
| NAT / translation? | **Yes** — SNAT/DNAT rewrites peers; breaks “peer on eligible subnet” intent. |
| mDNS | Advertisement from container netns goes out docker/veth interfaces → **filtered off** by policy predicate. Host-side proxy hacks would be a product change. |
| Portainer | Common default; **conflicts with current security model**. |
| Exposure | Publishing `0.0.0.0:8787` on the host can widen reach at the Docker/host firewall layer even while the app refuses non-LAN peers — confusing ops story; still must not relax `netpolicy`. |

**Probe evidence:** bridge container had only `lo` + `eth0@if*` `172.17.0.2/16`. Published `127.0.0.1:18788→8000` returned `ok-bridge` (connectivity ≠ policy-eligible LAN).

### 3. macvlan / ipvlan — only if host networking unsuitable

| Question | Assessment |
|----------|------------|
| LAN clients / mDNS | **Can** place a container on a real LAN L2 segment with its own MAC/IP so inventory shows an eligible `eth*`-like name + RFC1918 addr. |
| Ops cost | Requires parent NIC, IPAM discipline, Portainer/macvlan network setup; often **breaks host↔container** connectivity without an auxiliary interface; elevated networking privileges. |
| When to use | Linux hosts where host networking is forbidden by policy **and** bare-metal install is unacceptable. |
| This phase | **Not tested** — host networking already preserves policy without app changes. |

## Environment limitations (honesty)

| Environment | What it can prove | What it cannot |
|-------------|-------------------|----------------|
| This Windows 11 + Multipass HCS guest | Host vs bridge netns shape; loopback HTTP; NON_LAN prefix behavior | Physical-LAN Kindle discovery; real Wi‑Fi multicast; Portainer on true LAN |
| Docker Desktop on Windows (engine was **not running** here) | N/A this run | Linux `network_mode: host` is not a first-class Desktop feature |
| Production / Vega | **Out of scope — not touched** | — |

**Unproven (must not be marked PASS):** real Kindle `_scribe._tcp` browse/resolve on a home LAN from a container; Portainer UX on customer NAS hardware.

## Decision

### GO — next phase may implement Linux Compose/Portainer packaging **using host networking**

Rationale: host networking is the **smallest safe path** that preserves existing accept + advertise policy with **no** `netpolicy.rs` changes.

### NO-GO — bridge-published-port packaging as the default product path

Rationale: incompatible with current eligibility/on-link rules without weakening security.

### NO-GO — claiming physical-LAN validation from Multipass HCS alone

Rationale: virtual NAT subnet ≠ customer LAN.

## Recommended smallest safe Compose design (next phase only)

Outline only — **do not implement in this phase**:

```yaml
# Illustrative — Linux Docker Engine / Portainer
services:
  ignite-server:
    image: <local-or-private>/ignite-server-linux:<ver>
    network_mode: host
    user: "nnn:nnn"          # non-root; matches scribe UID or mapped package user
    restart: unless-stopped
    volumes:
      - ignite-state:/var/lib/scribe-server
      - ignite-config:/etc/scribe-server:ro
    environment:
      # explicit paths only; no secrets in compose if avoidable
    healthcheck:
      test: ["CMD", "curl", "-fsS", "http://127.0.0.1:8787/control/v1/status"]
      interval: 30s
      timeout: 5s
      retries: 3
```

**Image construction:** reuse `deployment/linux` portable bundle (`scribe-server` + `web/` + unit semantics) in a distroless/debian-slim image; run the same binary as systemd packaging; **do not** embed Docker bridge assumptions.

**Host requirements (document for operators):**

- Linux Docker Engine (not Desktop-for-Windows host networking).
- Free host TCP port (default 8787).
- Persistent volumes for state + config.
- Firewall still scoped via existing installer/`lan-interfaces` story when used on bare metal; for Compose-only hosts, document that host networking inherits host firewall.

**Explicit non-goals for next Compose phase:** relaxing `NON_LAN_PREFIXES`, accepting docker0 peers, or advertising on bridge interfaces.
