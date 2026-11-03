# Kindle release-component profiles (Slice 3D-B2a.4.3c)

**Status:** build-time, deterministic package closure evidence. Not an admission
token, not production-authenticated installation authority, and not a change to
installer or launch behavior.

Related: [`kindle-installed-slot-binding.md`](kindle-installed-slot-binding.md),
[`kindle-installed-release-provenance-feasibility.md`](kindle-installed-release-provenance-feasibility.md),
[`kindle-release-manifest.md`](kindle-release-manifest.md).

## 1. Exact component profile schema

ASCII / UTF-8, LF after every line including the last, no BOM/CR/blank lines.
Header and fixed preface, then exactly 22 `component=` lines in
`COMPONENT_ROLES` order (same identifiers as
`storage_release_provenance.rs`).

```text
IGNITE_RELEASE_COMPONENT_PROFILE_V1
schema=1
profile=package|candidate-b|retained-a-future-layout
release_version=X.Y.Z
authentication=build-evidence-only
installation_binding=unverified
execution_binding=unverified
first_launch_admission=unavailable
data_compatibility=unknown
promotion_authorized=false
layout_subject=this-release-if-retained          # retained-a-future-layout only
device_retained_a_identity=unauthenticated       # retained-a-future-layout only
component_count=22
capabilities_sha256=<optional 64 lowercase hex>
component=<role>|<file|absent>|<bytes>|<sha256|->|<classification>|<mapping>
```

`capabilities_sha256` is optional. When present it records the storage-capabilities
declaration digest for cross-checking against a future signed provenance record; it
is not a package file and is never treated as install authority.

Every generated profile repeats the non-authorizing flags above. No profile may set
`promotion_authorized=true` or claim verified installation/execution binding.

## 2. Package, candidate B, and retained-A future layout

Generator: `deployment/kindle/release_component_profiles.py`.
Input: the staged `Ignite-Kindle-<version>/` payload (or an equivalent extracted
tree that matches that inventory). Digests are always computed from package bytes.

| Profile | Meaning |
|---------|---------|
| `package` | Exact release-owned files in the staged payload (package-relative mappings) |
| `candidate-b` | Expected post-install destinations for those bytes using symbolic roots `APP` / `DOCS` / `REC` (same digests for direct copies; transformed/generated gaps forced absent) |
| `retained-a-future-layout` | How **this** release would look if later retained under `APP/previous-release/`. Digests for direct-copy roles come from this package. **Does not authenticate** any currently retained previous release already on a device. |

Symbolic roots (never hardcoded production absolutes in profile bytes):

| Root | Production default (documentation only) |
|------|-----------------------------------------|
| `APP` | `/mnt/us/scribe-app` |
| `DOCS` | `/mnt/us/documents` |
| `REC` | `/var/local/ignite-recovery` |

Candidate-B mappings are source-verified against
`storage_installed_slot_binding.rs::expected_component_path(CandidateB)` and
`install-runtime.sh` install destinations. Tests parse the Rust match arms and
require equality with `CANDIDATE_B_DESTINATION`.

## 3. Direct-copy versus generated/transformed

| Role | Package | Candidate B | Retained-A future layout |
|------|---------|-------------|--------------------------|
| Executables, host, launcher, helpers, assets, customer scripts, recovery dropbear + elf.sha256 + provenance | direct-copy | direct-copy under `APP`/`DOCS`/`REC` | direct-copy under `APP/previous-release/<basename>` except recovery |
| `registration-source` | direct-copy (`registration/ignite.json`) | **transformed** (mapping `-`, digest absent) | **transformed** → `APP/previous-release/appreg-handler.sql` (digest absent) |
| `recovery-version` | **generated** (absent in package) | **generated** (`REC/manifest/version`, digest absent) | **shared** / not retained with A |
| `license-noto-emoji-description` / `upstream-info` | optional | optional | optional |
| Recovery dropbear / manifests | direct-copy | direct-copy under `REC/...` | **shared**, not in `previous-release` |

Do not invent byte-for-byte equality across transformed or generated roles.

### Retained-A identity rule

A profile generated from an incoming release **cannot** authenticate the previous
release already retained as A on a device. The `retained-a-future-layout` profile
is solely that release's potential future retention layout. Claims about a
device's current retained-A require authenticated evidence from that prior
release's own originating artifacts. Schema fields
`layout_subject=this-release-if-retained` and
`device_retained_a_identity=unauthenticated` make this explicit.

## 4. Authentication and signature coverage

| Artifact | Authenticates | Relationship to profiles |
|----------|---------------|---------------------------|
| Ed25519 release manifest + `.sig` | Exact customer `Install Ignite vX.Y.Z.sh` bytes | Outer installer only; does not enumerate members |
| Installed provenance record (v1) | Caller-supplied component closure + capabilities + installer identity | Same 22 roles; production pin still `UNPROVISIONED` |
| Component profiles (this slice) | **Nothing by themselves** | Build evidence that the staged payload matches the role inventory; may later be compared to a signed provenance component list |

Profiles must not be described as publisher-authenticated installation authority.
Embedding them inside the customer Scriptlet would create a circular dependency with
the installer digest; they are written beside packaging output under
`component-profiles/` and are **not** GitHub Release assets.

## 5. Payload inspection hardening

`inspect_package_components` / `_enumerate_payload_files`:

- Refuse symlink directories, unexpected directories (closed `ALLOWED_PAYLOAD_DIRS`),
  special files, and hard links (`nlink != 1`).
- Bound both directory and file enumeration (`MAX_PAYLOAD_DIRS`, `MAX_PAYLOAD_FILES`).
- Hash via file-descriptor open (`O_NOFOLLOW` when available) with pre/post
  identity checks (`dev`/`ino`/`size`/`mtime_ns`/`ctime_ns`/`nlink`/`mode`);
  refuse growth or truncation during read. No unrestricted `Path.read_bytes()`.

## 6. CI reproducibility and Scriptlet equivalence gate

- `package_release.py` generates all three profiles twice (`verify_deterministic`) after
  payload validation and writes them under `<out-root>/component-profiles/`.
- `validate_stage.validate_payload` calls `inspect_package_components` (unknown paths,
  mandatory presence, symlink/special/unexpected-dir refusal).
- `.github/workflows/ci.yml` `kindle-package` regenerates profiles from the staged
  payload with `--check-deterministic` after Scriptlet validation.

### Customer Scriptlet payload equivalence (Slice 3D-B2a.4.3d)

Verifier: `deployment/kindle/verify_sfx_payload_equivalence.py`.

**SFX format (source-verified):** the customer `Install Ignite vX.Y.Z.sh` is an immutable
concatenation of (1) an LF-only shell prefix stamped with `IGNITE_ARCHIVE_OFFSET`,
`IGNITE_ARCHIVE_BYTES`, `IGNITE_ARCHIVE_SHA256`, `IGNITE_ARCHIVE_MD5`, and
`IGNITE_PACKAGE_VERSION`, then (2) a gzip-compressed ustar archive. Members are
regular files only named `Ignite-Kindle-<version>/<payload-rel>` with modes `0755`
for `*.sh` / `scribe-app` / `scribe-lifecycle` else `0644`, `mtime=0`, empty owner
names. The production builder does not emit directory, symlink, hardlink, or special
members. The Scriptlet becomes immutable once `package_release.py` writes the SFX
bytes; CI copies that exact file to `dist/kindle-release/` before upload.

**Inspection:** the verifier reads the final Scriptlet via `O_NOFOLLOW` (symlinked SFX
paths refused; no `Path.resolve()` before the check), splits via the stamped OFFSET
contract (`build_sfx.split_sfx`), then decompresses with `zlib.decompressobj(wbits=31)`
so that **exactly one** gzip member is accepted (header flags/mtime, deflate body,
CRC/trailer, `eof`, empty `unused_data` — refusing trailing zeros and empty or
nonempty concatenated members). A raw ustar framing pass then requires
`ustar\\0`/`00` magic/version, valid header checksums, regular-file typeflags only,
member boundaries, and two end-of-archive zero blocks (zero padding after EOA is
allowed; alternate GNU/PAX formats and missing EOA are refused). Member payloads
are hashed incrementally against a single decompressed buffer. Exact tar metadata
must match the production builder: mode `0755`/`0644` with no setuid/setgid/sticky
bits, `uid=gid=0`, `mtime=0`, empty owner names. Absolute/`..` paths, duplicates,
oversized members, and unbounded member counts / uncompressed size are refused.
Embedded inventory is compared to the independently staged payload (size +
SHA-256 + exact mode) and to the canonical package component profile (22 roles).
Success cannot rest solely on a staging-only archive checksum.

**CI placement:** after `dist/kindle-release/Install Ignite vX.Y.Z.sh` is copied and
checksum-verified, and after package-profile regeneration; before artifact upload.
`package_release.py` also runs the same check against its local SFX output. Report
bytes are build evidence under `component-profiles/sfx-payload-equivalence.v1` and
are not GitHub Release assets.

**PASS means:** the inspected customer Scriptlet embeds the same release-owned
payload bytes as the independently staged release and agrees with the package
profile. It does **not** mean publisher authentication, installed-slot trust,
execution binding, retained-A authentication, data compatibility, or promotion
authorization. Non-authorizing flags remain forced in the equivalence report.

## 7. Known unclosed component mappings

- `registration-source` ↔ live appreg / retained SQL (transformed).
- `recovery-version` (installer-generated).
- Shared recovery tree vs retained A.
- Optional upstream license info not emitted by current packager.
- Storage-capabilities declaration lives in the compiled app, not as a package file.
- Production Ed25519 pin remains `UNPROVISIONED`.
- Device current retained-A identity remains unauthenticated from these profiles.

## 8. Tests and evidence

- `deployment/kindle/test_release_component_profiles.py` — determinism, mutation,
  missing/unknown/optional/unexpected-dir, topology gaps, symbolic candidate-B
  destinations vs Rust inspector, retained future-layout identity flags, symlink
  file/dir refusal, special-file refusal, enumeration bounds, non-admission flags,
  Rust `COMPONENT_ROLES` sync.
- `deployment/kindle/test_sfx_payload_equivalence.py` — exact equivalence; missing/
  extra/changed/same-size mutation; duplicates; traversal/absolute paths; symlink/
  hardlink/fifo/directory members; truncated archive; wrong version; incorrect
  optional presence; wrong staged profile; SFX/staged drift after profile generation;
  size and member-count bounds; repeated identical reports; non-admission flags.
- Slot-binding B2a.4.3b regressions: post-auth closed-directory / transition-marker
  TOCTOU tests; fixture cleanup refuses substituted roots even with a copied ownership
  marker (Unix device/inode).

## 9. What a future trusted installer publication must prove

1. Authenticated outer installer (provisioned pin + release manifest).
2. Membership of these exact component digests in that installer (or a signed component
   record covering them).
3. Durable publication of those digests into live-B / retained-A / recovery paths for a
   specific pending transaction.
4. Kernel-backed execution binding to the same objects.
5. Bidirectional data compatibility before first mutating launch.
6. Independent authentication of any currently retained previous release (cannot be
   inferred from an incoming release's future-layout profile).

Until then: `installation_binding=unverified`, `execution_binding=unverified`,
`first_launch_admission=unavailable`, `data_compatibility=unknown`,
`promotion_authorized=false`, `device_retained_a_identity=unauthenticated`.
