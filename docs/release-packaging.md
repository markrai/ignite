# Prospective GitHub release packaging

This document describes the **layout and verification** of a public Ignite
release. It does **not** create GitHub Releases or upload binaries.

## Artifacts (per version `X.Y.Z`)

| Artifact | Source |
|----------|--------|
| Source tag / zip | GitHub source archive of the release tag |
| `LICENSE`, `NOTICE` | Repository root |
| `ignite/assets/THIRD_PARTY_NOTICES.md` | Fonts + Dropbear + dependency notes |
| `Install Ignite vX.Y.Z.sh` | `scripts/kindle/package-release.ps1` |
| `Install Ignite vX.Y.Z.sha256` | Emitted next to the Scriptlet |
| Optional support ZIP | Same script (`Ignite-Kindle-X.Y.Z.zip` + `.sha256`) |
| This compatibility doc | `docs/compatibility.md` |
| Install instructions | `deployment/kindle/README.txt` + root `README.md` |

Version number: `ignite/VERSION`.

## Build workflow (packaging host)

1. Clean checkout of the release tag.
2. Build Dropbear client: `tools/dropbear/build-armv7hf-client.sh` (Docker).
3. Build Kindle binaries + Scriptlet:
   ```powershell
   .\scripts\kindle\package-release.ps1
   ```
4. Confirm outputs under `target/package/kindle/`.
5. Verify SHA-256 files match the Scriptlet bytes.
6. Confirm package **does not** contain:
   - `kindle-supervisor-harness`
   - Synthetic SSH private keys
   - `.env` / credentials
   - `ignite-review` evidence trees

## Provenance

- Rust binaries: built from the tagged source via `tools/docker_build_armv7hf.sh`
  inside `rust:1-bullseye` with path remapping for reproducibility *of paths*,
  not bit-for-bit guarantees across Docker versions.
- Dropbear: pinned tarball digest + upstream signing key fingerprint in
  `tools/dropbear/PINNED.txt`; packaged ELF digest recorded in
  `THIRD_PARTY_NOTICES.md` / package manifest.

**`-SkipBuild` is not provenance-safe.** It reuses whatever ARMv7hf release
ELFs already sit under `target/`; those binaries may predate the current
source (including handler identity strings). Use a full Docker rebuild for any
customer-facing Scriptlet. Do not treat a successful `-SkipBuild` packaging
run as proof that packaged binaries match the tagged tree.

Do not claim bit-for-bit reproducibility across hosts without a dedicated
repro CI job.

## Customer installation

Single-file experience preserved: send only `Install Ignite vX.Y.Z.sh`.
See `deployment/kindle/README.txt`.

## Checksum verification (example)

```bash
sha256sum -c "Install Ignite vX.Y.Z.sha256"
```

On Windows (PowerShell):

```powershell
(Get-FileHash ".\Install Ignite vX.Y.Z.sh" -Algorithm SHA256).Hash
Get-Content ".\Install Ignite vX.Y.Z.sha256"
```
