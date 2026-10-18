# Ignite Server — Docker networking feasibility

**Date:** 2026-10-18 (analysis corrected same day)

**Scope:** Evaluate packaging Ignite Server for Docker/Portainer **without** weakening
[`netpolicy.rs`](../../ignite/server/scribe-server/src/netpolicy.rs) / DNS-SD eligibility.
**No Compose implementation in this phase.**

## Policy constraints (current code)

From `ignite/server/scribe-server/src/netpolicy.rs` and `discovery.rs`:

| Rule | Behavior |
|------|----------|
| Eligible interface | Name **not** in `NON_LAN_PREFIXES` (`docker*`, `br-*`, `veth*`, `lo`, VPN/VM prefixes, …) **and** address is private RFC1918 / IPv6 ULA |
| Accept | Local (destination) addr is eligible **and** peer is on that interface’s subnet, **or** both ends loopback |
| Refuse | Connections that fail the above (including many VPN/container/public paths) |
| Advertise | `_scribe._tcp` only on policy-eligible interfaces; loopback never advertised |

**Interface naming note:** `ip addr` often displays a veth endpoint as `eth0@ifN` (peer ifindex). Applications using `if_addrs` / the kernel ifname commonly see simply `eth0`. That name is **not** matched by the `veth*` prefix, so a bridge container’s `eth0` can be **name-eligible**, and a `172.17.0.0/16` address is still **RFC1918-private**. Eligibility of the *interface address* is therefore possible under bridge networking; **connection acceptance still depends on destination address, peer address, and on-link subnet membership after Docker’s NAT/routing**. Do **not** claim bridge mode necessarily refuses every connection.

## Modes evaluated

### 1. Docker host networking (Linux) — preferred

| Question | Assessment |
|----------|------------|
| LAN clients pass policy? | **Yes, when the host’s physical/private NIC is eligible** — container shares the host network namespace, so inventory matches the host (`eth0`/`enp*`/…). Peer must still be on-link to that subnet. |
| Loopback health checks? | **Yes** — `127.0.0.1` ↔ `127.0.0.1` → `AllowLoopback`. |
| NAT / address translation? | **None inside the container** for host network; observed local/peer addrs are host addrs. |
| mDNS to real Kindles? | **Plausible on a Linux host on the real LAN** (multicast on eligible iface). **Not proven** on Multipass HCS (virtual `10.97.0.0/24`, no physical Kindle path). |
| Portainer stacks? | Supported on Linux engines via Compose `network_mode: host`. **Not available the same way on Docker Desktop Windows/macOS.** |
| Privileges / conflicts | Needs permission to use host net; **chosen TCP port must be free on the host**; run as non-root UID inside the container. |
| Exposure / firewall | Host-network containers inherit the **host network namespace**, **not** the native Ignite installer firewall unit/rules. Compose-only deployments require the **operator** to configure and verify host firewall restrictions. Host networking does **not** inherently prevent exposure: Ignite’s application policy adds filtering but is **not** a substitute for host firewall configuration. |

**Probe evidence (disposable Multipass Ubuntu 24.04 + Docker 29.1.3, 2026-10-18) — tested:**

- Host ifaces: `lo`, `eth0` (`10.97.0.86/24`), `docker0` (`172.17.0.1/16`).
- `--network host` container saw the **same** `lo` + `eth0` + `docker0` inventory.
- Host-network loopback HTTP → `ok-hostnet`.
- Name-prefix simulation: `eth0` name-eligible; `docker0` / `br-*` / `veth*` / `lo` not.

Script: [`docker-network-probe.sh`](docker-network-probe.sh) (opt-in disposable; not a Kindle proof).

**Inference (not re-tested here):** on a real LAN host, eligible NIC + on-link Kindle clients should satisfy accept/advertise the same as bare metal.

### 2. Ordinary bridge networking + published ports — not the default product path

| Question | Assessment |
|----------|------------|
| LAN clients / accept policy? | **Do not assume universal refuse.** Bridge containers often present `eth0` (displayed as `eth0@ifN`) with an RFC1918 `172.17.0.0/16` address — that can be **name- and address-eligible**. Whether a given connection is **AllowLan** still hinges on the **local destination address** the kernel reports and whether the **peer** is on that `/16` (or other published path) after Docker DNAT/SNAT. Host clients published via `-p` frequently appear as the bridge gateway or other non-LAN peers → many real-world paths still **Refuse**, but this is topology-dependent, not a blanket invariant. |
| Loopback health checks? | In-container loopback allow still applies. Host `curl` to a published port is **not** the loopback allow pair unless the healthcheck runs inside the container netns. |
| NAT / translation? | **Yes** — DNAT/SNAT change observed peers/destinations relative to a physical LAN client. |
| mDNS / physical LAN | **Still problematic** for product goals: advertisement from the bridge netns targets docker/bridge interfaces and addresses, not the customer Wi‑Fi/Ethernet L2 domain Kindles browse. Ordinary bridge mode is a poor fit for `_scribe._tcp` discovery integration. |
| Portainer | Common default stack networking — **not recommended as Ignite’s default** given discovery and on-link intent. |
| Exposure | Publishing `0.0.0.0:8787` on the host widens reach at the Docker/host firewall layer independently of app policy. |

**Probe evidence (tested):** bridge container showed `lo` + `eth0@if*` with `172.17.0.2/16`; published `127.0.0.1→8000` returned `ok-bridge` (**connectivity ≠ product LAN/discovery success**).

### 3. macvlan / ipvlan — only if host networking unsuitable

| Question | Assessment |
|----------|------------|
| LAN clients / mDNS | **Can** place a container on a real LAN L2 segment with its own MAC/IP so inventory shows an eligible NIC-like name + RFC1918 addr. |
| Ops cost | Parent NIC, IPAM, Portainer macvlan setup; often awkward host↔container paths; elevated networking privileges. |
| This phase | **Not tested** — host networking already preserves policy shape without app changes. |

## Environment limitations (honesty)

| Environment | What it can prove | What it cannot |
|-------------|-------------------|----------------|
| Windows 11 + Multipass HCS guest (2026-10-18) | Host vs bridge netns shape; loopback HTTP; name-prefix simulation | Physical-LAN Kindle discovery; real Wi‑Fi multicast; Portainer on true LAN |
| Docker Desktop on Windows (engine was **not running** during the probe) | N/A that run | Linux `network_mode: host` is not a first-class Desktop feature |
| Production / Vega | **Out of scope — not touched** | — |

**Unproven (must not be marked PASS):** real Kindle `_scribe._tcp` browse/resolve on a home LAN from a container; Portainer UX on customer NAS hardware.

## Decision

### GO — next phase may implement Linux Compose/Portainer packaging **using host networking**

Rationale: smallest path that keeps accept/advertise aligned with a normal Linux host inventory, **subject to real LAN/Kindle validation** before calling discovery done. Operators must still configure host firewall appropriately (installer firewall is not inherited automatically).

### NO-GO — bridge-published-port packaging as Ignite’s **default** deployment mode

Rationale: even though some bridge connections might pass the app filter depending on NAT/peers, **mDNS/physical-LAN product integration remains a poor fit**, and the operational/security story is worse than host networking. No `netpolicy` weakening to “make bridge work.”

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
    healthcheck:
      test: ["CMD", "curl", "-fsS", "http://127.0.0.1:8787/control/v1/status"]
      interval: 30s
      timeout: 5s
      retries: 3
```

**Host requirements (operators):**

- Linux Docker Engine (not Desktop-for-Windows host networking).
- Free host TCP port (default 8787).
- Persistent volumes for state + config.
- **Explicit host firewall** allowing only the intended LAN (Compose/host-network does not apply the native installer firewall automatically).

**Explicit non-goals for next Compose phase:** relaxing `NON_LAN_PREFIXES`, treating docker0 as a product LAN, or advertising on bridge-only topologies as the supported default.
