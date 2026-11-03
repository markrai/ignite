# Authenticated Kindle release manifest v1

**Status:** the verifier, offline signing helper, authenticated promotion path, and host tests are
implemented. Production activation is deliberately blocked: the shared public-key pin contains
`UNPROVISIONED`, so both promotion and the official Kindle verifier fail closed. Current releases
still contain the four assets documented in [`release-packaging.md`](release-packaging.md). Do not
dispatch **Promote release** until the maintainer provisions the genuine public key as described
below.

This format authenticates the exact Kindle installer selected by a future auto-updater. It does
not download, install, stop Ignite, alter rollback policy, or write notebook data.

## Release assets

After key provisioning, authenticated promotion adds these two assets for release `vX.Y.Z`:

- `ignite-kindle-vX.Y.Z.manifest`
- `ignite-kindle-vX.Y.Z.manifest.sig`

The existing checksum sidecar may remain for people and compatibility, but it is not an
authentication input for the Kindle updater.

## Exact manifest bytes

The manifest is ASCII (therefore UTF-8), with fields in the exact order below, one LF byte (`0a`)
after every line including the last, and no BOM, CR, blank line, whitespace padding, or additional
field:

```text
schema=ignite-kindle-release-v1
version=X.Y.Z
tag=vX.Y.Z
artifact=Install Ignite vX.Y.Z.sh
bytes=12345678
sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
```

The exact file bytes shown by this grammar are the Ed25519 message. There is no JSON
re-serialization step and no signature over a parsed object.

Validation is deliberately narrower than general text parsing:

- `schema` is the literal `ignite-kindle-release-v1`.
- `version` is canonical SemVer core (`major.minor.patch`) with no leading zeroes. A SemVer
  prerelease suffix is allowed; build metadata is not.
- `tag` is exactly `v` plus `version`, and must equal the release tag requested by discovery.
- `artifact` is exactly `Install Ignite v<version>.sh`.
- `bytes` is a positive base-10 `u64` with no sign or leading zeroes.
- `sha256` is exactly 64 lowercase hexadecimal characters.
- The manifest is at most 1,024 bytes.

Fixed order and an exact six-line count make missing, repeated, reordered, and unknown keys
invalid. In particular, no generic JSON parser can silently accept a duplicate key or normalize
multiple byte representations to the same signed object.

## Detached signature bytes

The signature asset is also canonical ASCII/LF:

```text
schema=ignite-kindle-release-signature-v1
signature=<128 lowercase hexadecimal characters>
```

The value is the 64 raw Ed25519 signature bytes encoded as lowercase hex. The final LF is
required. The signature is verified against the exact manifest asset bytes with the public key
pinned in the Kindle application. Signature-envelope parsing, public-key decoding, and Ed25519
verification must all succeed before any manifest field is parsed or returned.

After authentication, the installer basename, streamed byte count, and streamed SHA-256 must
match the authenticated fields before the file is eligible for any later install step. HTTPS and
the ordinary `.sha256` sidecar do not replace this check.

## Shared production public-key pin

[`tools/release/kindle-release-public-key.hex`](../tools/release/kindle-release-public-key.hex) is
the single pin consumed by both Rust and release promotion. `UNPROVISIONED` is the only disabled
value. The enabled representation is exactly the 32 raw Ed25519 public-key bytes encoded as 64
lowercase hexadecimal characters followed by one LF. No whitespace, label, PEM armor, or second
line is accepted. The Rust verifier decodes those exact bytes at runtime; promotion passes the same
file to the independent OpenSSL verifier. A public caller-selected key is not supported.

The production private key must be generated and retained by the maintainer in an offline or
equivalently controlled environment. It must never enter this repository, GitHub Actions, a
workflow input, or release assets. Test seeds and signatures must never become the pin. The code
also refuses the published RFC 8032 vector and the deterministic Rust verifier fixture keys if one
is accidentally copied into the official pin.

### One-time provisioning (maintainer only)

These are operator instructions, not a key-generation action performed by CI or this repository:

1. On a trusted, offline machine, outside any Git checkout, generate an encrypted Ed25519 key:

   ```sh
   umask 077
   openssl genpkey -algorithm ED25519 -aes-256-cbc \
     -out ignite-release-signing-private.pem
   openssl pkey -in ignite-release-signing-private.pem -pubout -outform DER \
     -out ignite-release-signing-public.der
   ```

2. Confirm that the DER public key is 44 bytes and begins with the Ed25519 SubjectPublicKeyInfo
   prefix `302a300506032b6570032100`. Extract the remaining 32 raw bytes to lowercase hex:

   ```sh
   test "$(wc -c < ignite-release-signing-public.der | tr -d ' ')" = 44
   test "$(head -c 12 ignite-release-signing-public.der | od -An -v -tx1 | tr -d ' \n')" \
     = 302a300506032b6570032100
   tail -c 32 ignite-release-signing-public.der | od -An -v -tx1 | tr -d ' \n'
   ```

3. Independently repeat public-key derivation from the private key on a second trusted system or
   with a separately installed OpenSSL build. Compare the full 64-character raw-key hex and record
   a SHA-256 fingerprint of the DER public key through a separate maintainer-controlled channel.
4. Make at least one encrypted, durable, offline backup of the private key and its recovery
   material. Test restoration by deriving the same public key. Do not store the backup or
   passphrase in GitHub, the repository, or the build machine.
5. Replace `UNPROVISIONED` in the shared pin file with the independently checked 64-character hex
   plus LF. Review that commit as a trust-root change. Run all checks below before attempting a
   release. The private key stays offline.

Until all five steps are complete, no signed release can be promoted and the Kindle official
verification entry point remains unusable by design.

## Offline signing and authenticated promotion

The repository helper requires Python 3 and OpenSSL with Ed25519 `pkeyutl -rawin` support (OpenSSL
3 is used in development and GitHub's Ubuntu runner). It never downloads an artifact or accepts a
caller-authored manifest.

For a release after key provisioning:

1. Download the `ignite-kindle-release` artifact from the already successful, eligible CI run.
   Verify its existing sidecar and the recorded promotion digest exactly as today.
2. On the offline signing system, avoid putting the private-key passphrase in shell history. For
   an encrypted key, read it into the environment used only by the helper, then clear it:

   ```sh
   read -r -s -p 'Signing-key passphrase: ' IGNITE_SIGNING_KEY_PASSPHRASE
   export IGNITE_SIGNING_KEY_PASSPHRASE
   python3 tools/release/kindle_release_sign.py sign \
     --installer '/offline/artifacts/Install Ignite vX.Y.Z.sh' \
     --tag vX.Y.Z \
     --expected-sha256 '<recorded 64-character Kindle SHA-256>' \
     --private-key /offline/keys/ignite-release-signing-private.pem \
     --public-key tools/release/kindle-release-public-key.hex \
     --output-dir /offline/signed-vX.Y.Z
   unset IGNITE_SIGNING_KEY_PASSPHRASE
   ```

   The helper streams and checks the installer, creates the exact manifest, signs its raw bytes,
   and independently verifies the signature against the intended pin before writing outputs. It
   refuses to replace existing output files. Successful output is shaped like:

   ```text
   manifest=/offline/signed-vX.Y.Z/ignite-kindle-vX.Y.Z.manifest
   signature_file=/offline/signed-vX.Y.Z/ignite-kindle-vX.Y.Z.manifest.sig
   signature_hex=<128 lowercase hexadecimal characters>
   ```

3. Inspect the emitted manifest, re-check the installer digest, and copy only the non-secret
   `signature_hex` value into the required `kindle_signature` input of **Promote release**. Do not
   upload the private key or its passphrase.
4. The workflow fetches the same four CI-produced files and repeats all existing run, commit,
   workflow, branch, version, checksum, artifact-set, and confirmation guards. Promotion then
   reconstructs the manifest from the fetched installer and verifies the supplied signature with
   the repository pin. Direct `publish` invocation repeats verification, so it is not a bypass.
5. Only after authentication succeeds does promotion create the canonical manifest and signature
   assets and stage all six assets. One `gh release create` operation publishes the unchanged four
   CI files plus the two authenticated metadata files. Existing Releases are never replaced, tag
   identity remains tied to the approved commit, and stable-tag GHCR publication remains
   downstream of successful promotion.

The workflow accepts no caller-supplied manifest, byte count, artifact name, version, or public
key. The ordinary `.sha256` sidecar is still checked, but it is not publisher authentication.

The low-level signing primitive used by the helper is equivalent to
`openssl pkeyutl -sign -rawin`; private-key contents and passphrases are never command-line
arguments or output. The private-key file path is necessarily supplied locally to the helper.

For reference, promotion's independent verification can also be exercised locally with the
non-secret signature hex (production always selects the repository pin itself):

```sh
python3 tools/release/kindle_release_sign.py verify \
  --installer 'Install Ignite vX.Y.Z.sh' \
  --tag vX.Y.Z \
  --expected-sha256 '<recorded 64-character Kindle SHA-256>' \
  --signature-hex '<128 lowercase hexadecimal characters>' \
  --public-key tools/release/kindle-release-public-key.hex \
  --output-dir ./verified-vX.Y.Z
```

The manual workflow then:

1. Receives the signature only as public authorization data.
2. Reconstructs the same bytes; it does not trust the offline manifest file.
3. Verifies with the same public-key pin as the Kindle application.
4. Fails before `gh release create` on any disagreement.

This preserves the existing promotion architecture:

1. CI builds the original four files and does not sign or publish them.
2. Offline maintainer custody authorizes the exact installer digest.
3. Promotion authenticates and publishes the six-file Release without rebuilding.
4. A future Kindle updater will consume the manifest, signature, and installer.

## Validation

```sh
python3 tools/release/test_kindle_release_sign.py
bash tools/release/test-promote-release.sh
cargo test --locked -p scribe-app --lib release_manifest
cargo fmt --all -- --check
cargo clippy -p scribe-app --lib --locked
git diff --check
```

## Future Kindle verification sequence

The updater will fetch the small manifest and signature into update-owned staging under
`/mnt/us`, enforce size bounds, authenticate and parse them, then download the exact named
installer and stream-check its size and SHA-256. Only that authenticated artifact may be offered
to the existing installer. Transport, staging durability, stop-before-install, launch health, and
last-known-good policy are separate slices.
