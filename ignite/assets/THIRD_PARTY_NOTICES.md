# Third-party notices

Ignite first-party software is licensed under **AGPL-3.0-only** (see root `LICENSE`
and `NOTICE`).

This file lists third-party material that is redistributed with Ignite source or
fetched/built as part of packaging. Each remains under its own license.

---

## Fonts (bundled in source; some embedded in the Kindle binary)

### Source Sans 3 Regular (`default-sans-v1`)

| | |
|---|---|
| Distributed file | `assets/typography/default-sans-v1.ttf` |
| Family / style | Source Sans 3 Regular |
| Protocol face token | `default-sans-v1` (Ignite/Scribe text contract; not a CSS family name) |
| Copyright | Copyright 2010-2024 Adobe (http://www.adobe.com/), with Reserved Font Name 'Source' |
| License | SIL Open Font License 1.1 |
| Full license text | `assets/typography/OFL.txt` |
| Upstream | Adobe Source Sans 3 / Source Sans Pro family |
| Unchanged | Yes (SHA-256 `4644C81B86EC9CAAA76B634889968ED3C4F4F52F054855933ACC7C2B21E53B0F`) |

Reserved Font Name **Source** applies. Do not release modified versions under that name.

Wrap/metrics contract for this face: `assets/typography/README.md`.

### Noto Emoji (monochrome variable)

| | |
|---|---|
| Distributed file | `assets/typography/noto-emoji/NotoEmoji[wght].ttf` |
| Family / style | Noto Emoji (monochrome / black-and-white, variable `wght`) |
| Role | Kindle cmap fallback only when `default-sans-v1` has no glyph (or base+U+FE0F) |
| Copyright | Copyright 2013 Google LLC |
| License | SIL Open Font License 1.1 |
| Full license text | `licenses/noto-emoji/OFL.txt` |
| Additional notices | `licenses/noto-emoji/README.md`, `DESCRIPTION.en_us.html`, `upstream_info.md` |
| Upstream | Google Fonts Noto Emoji / [googlefonts/emoji-bw](https://github.com/googlefonts/emoji-bw) (documented as 3.002 in source provenance) |
| Unchanged | Yes (SHA-256 `DE6C18832938AFC99CAF132B39D6A30A19BAC7F2E812E28DB2535B4608D27551`) |

The Noto Emoji OFL text is also compile-time embedded via `include_str!` from
`licenses/noto-emoji/OFL.txt`. Device packaging installs that notice under
`/mnt/us/scribe-app/assets/licenses/noto-emoji/` at install time; the font binary
itself is embedded in the executable and is not required as a separate runtime file
for painting.

---

## Dropbear SSH client (built at package time; not AGPL)

Ignite’s Kindle recovery transport optionally packages a **client-only**
`dropbearmulti` (dbclient) binary for ARMv7hf. Source is **not vendored** in this
repository. Packaging scripts download a pinned upstream tarball, verify
signatures/digests, and cross-compile.

| | |
|---|---|
| Upstream | [Dropbear SSH](https://matt.ucc.asn.au/dropbear/dropbear.html) |
| Pinned version | 2024.85 (see `tools/dropbear/PINNED.txt`) |
| Build scripts | `tools/dropbear/build-armv7hf-client.sh`, `verify-source.sh` |
| Upstream license | Dropbear’s LICENSE (MIT-style / permissive; see tarball `LICENSE`) |
| Copyright | Matt Johnston and contributors (see upstream) |
| Redistribution | Allowed under Dropbear’s upstream terms; **do not claim Dropbear is AGPL** |
| Packaged ELF digest (Kindle) | SHA-256 `d20b510a58b59ff5bf482bb5fcc66768d4c4c51705855b9f5c9e47ffaaf3d1e9` |

The customer Scriptlet may include the verified client under a recovery path.
Ordinary notebook use does not activate recovery networking. Live recovery POST
remains disabled in product code (`LIVE_NON_LOOPBACK_POST_ENABLED = false`).

---

## Rust / Cargo dependencies

Workspace crates pull crates.io dependencies declared in `Cargo.toml` /
`Cargo.lock`. Typical licenses include MIT, Apache-2.0, BSD, and Unicode license
texts. Regenerating a full SPDX bill of materials:

```bash
cargo install cargo-license   # optional
cargo license -d
```

CI does not currently fail on dependency license changes; review `Cargo.lock`
updates before release.

---

## Python packaging / installer tests

Installer validation uses the host Python standard library plus pytest (dev
dependency for contributors). No Python runtime is shipped to the Kindle.

---

## Amazon Kindle platform

Ignite runs on a prepared (jailbroken / community-tooling) Kindle Scribe and
registers with Amazon’s app manager using the historical handler id
`com.markrai.ignite` (legacy installs may still show `com.markrai.scribe` until
upgraded). Amazon trademarks and Kindle firmware remain Amazon’s.
Ignite does not redistribute Kindle firmware or proprietary Amazon libraries.
