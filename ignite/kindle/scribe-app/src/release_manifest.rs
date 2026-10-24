//! Authentication and strict parsing for Kindle release metadata.
//!
//! This module deliberately contains no networking, filesystem policy, installer invocation, or
//! UI. A future updater may pass downloaded bytes here, but must not use any manifest field until
//! [`authenticate_official_release`] returns an [`AuthenticatedReleaseManifest`].

use std::io::Read;

use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

pub const MANIFEST_SCHEMA: &str = "ignite-kindle-release-v1";
pub const SIGNATURE_SCHEMA: &str = "ignite-kindle-release-signature-v1";
pub const MAX_MANIFEST_BYTES: usize = 1024;

// This is the same repository pin consumed by the promotion verifier. Provisioning replaces the
// sentinel with exactly 64 lowercase hex characters plus LF; the private key never enters Git or
// GitHub Actions. Keeping the only public entry point closed until then prevents a test key,
// caller-selected key, or unauthenticated compatibility fallback from becoming a trust root.
const OFFICIAL_RELEASE_PUBLIC_KEY_HEX: &str =
    include_str!("../../../../tools/release/kindle-release-public-key.hex");
const KNOWN_TEST_PUBLIC_KEY_HEX: [&str; 3] = [
    "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
    "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c",
    "1398f62c6d1a457c51ba6a4b5f3dbd2f69fca93216218dc8997e416bd17d93ca",
];

#[derive(Debug, PartialEq, Eq)]
pub enum ReleaseVerifyError {
    OfficialKeyNotProvisioned,
    ManifestTooLarge,
    MissingSignature,
    MalformedSignature,
    InvalidPinnedPublicKey,
    InvalidSignature,
    UnsupportedSchema,
    MalformedManifest(&'static str),
    UnexpectedReleaseTag,
    InconsistentVersionTag,
    UnexpectedArtifactName,
    ArtifactRead(std::io::ErrorKind),
    ArtifactByteCountMismatch,
    ArtifactDigestMismatch,
}

impl std::fmt::Display for ReleaseVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::OfficialKeyNotProvisioned => "official release signing key is not provisioned",
            Self::ManifestTooLarge => "release manifest is too large",
            Self::MissingSignature => "release manifest signature is missing",
            Self::MalformedSignature => "release manifest signature is malformed",
            Self::InvalidPinnedPublicKey => "pinned release public key is invalid",
            Self::InvalidSignature => "release manifest signature is invalid",
            Self::UnsupportedSchema => "release manifest schema is unsupported",
            Self::MalformedManifest(reason) => reason,
            Self::UnexpectedReleaseTag => {
                "release manifest tag does not match the requested release"
            }
            Self::InconsistentVersionTag => "release manifest version and tag are inconsistent",
            Self::UnexpectedArtifactName => "release manifest installer filename is unexpected",
            Self::ArtifactRead(_) => "could not read the installer while verifying it",
            Self::ArtifactByteCountMismatch => {
                "installer byte count does not match the authenticated manifest"
            }
            Self::ArtifactDigestMismatch => {
                "installer digest does not match the authenticated manifest"
            }
        };
        f.write_str(message)
    }
}

impl std::error::Error for ReleaseVerifyError {}

/// Manifest fields made available only after the exact input bytes pass Ed25519 verification and
/// the canonical record passes all structural and cross-field checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedReleaseManifest {
    version: String,
    tag: String,
    installer_filename: String,
    installer_bytes: u64,
    installer_sha256: [u8; 32],
}

impl AuthenticatedReleaseManifest {
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn installer_filename(&self) -> &str {
        &self.installer_filename
    }

    pub fn installer_bytes(&self) -> u64 {
        self.installer_bytes
    }

    pub fn installer_sha256_hex(&self) -> String {
        encode_hex(&self.installer_sha256)
    }

    /// Stream and authenticate an installer against this already-authenticated manifest.
    ///
    /// The caller supplies the basename separately so a correctly hashed file under an attacker-
    /// chosen name is refused. No bytes are written and the reader need not fit in memory.
    pub fn verify_installer(
        &self,
        artifact_name: &str,
        mut reader: impl Read,
    ) -> Result<(), ReleaseVerifyError> {
        if artifact_name != self.installer_filename {
            return Err(ReleaseVerifyError::UnexpectedArtifactName);
        }

        let mut hasher = Sha256::new();
        let mut byte_count = 0_u64;
        let mut buffer = [0_u8; 8192];
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|error| ReleaseVerifyError::ArtifactRead(error.kind()))?;
            if read == 0 {
                break;
            }
            byte_count = byte_count
                .checked_add(read as u64)
                .ok_or(ReleaseVerifyError::ArtifactByteCountMismatch)?;
            hasher.update(&buffer[..read]);
        }

        if byte_count != self.installer_bytes {
            return Err(ReleaseVerifyError::ArtifactByteCountMismatch);
        }
        let digest: [u8; 32] = hasher.finalize().into();
        if digest != self.installer_sha256 {
            return Err(ReleaseVerifyError::ArtifactDigestMismatch);
        }
        Ok(())
    }
}

/// Authenticate an official manifest with the single public key pinned into the application.
///
/// Until production key provisioning replaces the repository sentinel, this entry point returns
/// [`ReleaseVerifyError::OfficialKeyNotProvisioned`] for every input. There is intentionally no
/// public caller-selected-key alternative.
pub fn authenticate_official_release(
    manifest_bytes: &[u8],
    signature_bytes: &[u8],
    expected_tag: &str,
) -> Result<AuthenticatedReleaseManifest, ReleaseVerifyError> {
    let public_key = official_release_public_key()?;
    authenticate_with_key(manifest_bytes, signature_bytes, expected_tag, &public_key)
}

fn official_release_public_key() -> Result<[u8; 32], ReleaseVerifyError> {
    parse_official_public_key(OFFICIAL_RELEASE_PUBLIC_KEY_HEX)
}

fn parse_official_public_key(encoded: &str) -> Result<[u8; 32], ReleaseVerifyError> {
    if encoded == "UNPROVISIONED\n" {
        return Err(ReleaseVerifyError::OfficialKeyNotProvisioned);
    }
    let hex = encoded
        .strip_suffix('\n')
        .filter(|hex| hex.len() == 64 && !hex.contains('\n'))
        .ok_or(ReleaseVerifyError::InvalidPinnedPublicKey)?;
    if KNOWN_TEST_PUBLIC_KEY_HEX.contains(&hex) {
        return Err(ReleaseVerifyError::InvalidPinnedPublicKey);
    }
    decode_hex::<32>(hex.as_bytes()).ok_or(ReleaseVerifyError::InvalidPinnedPublicKey)
}

fn authenticate_with_key(
    manifest_bytes: &[u8],
    signature_bytes: &[u8],
    expected_tag: &str,
    public_key: &[u8; 32],
) -> Result<AuthenticatedReleaseManifest, ReleaseVerifyError> {
    // A fixed bound is safe to enforce before authentication; no field is inspected here.
    if manifest_bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ReleaseVerifyError::ManifestTooLarge);
    }

    let signature = parse_signature(signature_bytes)?;
    let verifying_key = VerifyingKey::from_bytes(public_key)
        .map_err(|_| ReleaseVerifyError::InvalidPinnedPublicKey)?;
    verifying_key
        .verify_strict(manifest_bytes, &signature)
        .map_err(|_| ReleaseVerifyError::InvalidSignature)?;

    // Parsing and every semantic decision happen only after verification of the exact bytes.
    parse_authenticated_manifest(manifest_bytes, expected_tag)
}

fn parse_signature(bytes: &[u8]) -> Result<Signature, ReleaseVerifyError> {
    if bytes.is_empty() {
        return Err(ReleaseVerifyError::MissingSignature);
    }
    let prefix = format!("schema={SIGNATURE_SCHEMA}\nsignature=");
    let expected_len = prefix.len() + 128 + 1;
    if bytes.len() != expected_len
        || !bytes.starts_with(prefix.as_bytes())
        || bytes.last() != Some(&b'\n')
    {
        return Err(ReleaseVerifyError::MalformedSignature);
    }
    let raw = decode_hex::<64>(&bytes[prefix.len()..bytes.len() - 1])
        .ok_or(ReleaseVerifyError::MalformedSignature)?;
    Ok(Signature::from_bytes(&raw))
}

fn parse_authenticated_manifest(
    bytes: &[u8],
    expected_tag: &str,
) -> Result<AuthenticatedReleaseManifest, ReleaseVerifyError> {
    if bytes.last() != Some(&b'\n') || !bytes.is_ascii() || bytes.contains(&b'\r') {
        return Err(ReleaseVerifyError::MalformedManifest(
            "release manifest is not canonical ASCII with LF line endings",
        ));
    }
    let lines: Vec<&[u8]> = bytes[..bytes.len() - 1]
        .split(|byte| *byte == b'\n')
        .collect();
    if lines.len() != 6 {
        return Err(ReleaseVerifyError::MalformedManifest(
            "release manifest must contain exactly six lines",
        ));
    }
    let schema = field(lines[0], b"schema=")?;
    if schema != MANIFEST_SCHEMA.as_bytes() {
        return Err(ReleaseVerifyError::UnsupportedSchema);
    }

    let version = ascii_field(lines[1], b"version=")?;
    if !valid_release_version(version) {
        return Err(ReleaseVerifyError::MalformedManifest(
            "release manifest version is malformed",
        ));
    }
    let tag = ascii_field(lines[2], b"tag=")?;
    let installer_filename = ascii_field(lines[3], b"artifact=")?;
    let byte_count = ascii_field(lines[4], b"bytes=")?;
    let digest_hex = field(lines[5], b"sha256=")?;

    let canonical_tag = format!("v{version}");
    if tag != canonical_tag {
        return Err(ReleaseVerifyError::InconsistentVersionTag);
    }
    if tag != expected_tag {
        return Err(ReleaseVerifyError::UnexpectedReleaseTag);
    }

    let canonical_filename = format!("Install Ignite v{version}.sh");
    if installer_filename != canonical_filename {
        return Err(ReleaseVerifyError::UnexpectedArtifactName);
    }

    if byte_count.is_empty()
        || !byte_count.bytes().all(|byte| byte.is_ascii_digit())
        || (byte_count.len() > 1 && byte_count.starts_with('0'))
    {
        return Err(ReleaseVerifyError::MalformedManifest(
            "release manifest byte count is not canonical",
        ));
    }
    let installer_bytes = byte_count
        .parse::<u64>()
        .ok()
        .filter(|count| *count > 0)
        .ok_or(ReleaseVerifyError::MalformedManifest(
            "release manifest byte count is invalid",
        ))?;
    let installer_sha256 =
        decode_hex::<32>(digest_hex).ok_or(ReleaseVerifyError::MalformedManifest(
            "release manifest digest must be 64 lowercase hexadecimal characters",
        ))?;

    Ok(AuthenticatedReleaseManifest {
        version: version.to_owned(),
        tag: tag.to_owned(),
        installer_filename: installer_filename.to_owned(),
        installer_bytes,
        installer_sha256,
    })
}

fn field<'a>(line: &'a [u8], prefix: &[u8]) -> Result<&'a [u8], ReleaseVerifyError> {
    line.strip_prefix(prefix)
        .ok_or(ReleaseVerifyError::MalformedManifest(
            "release manifest fields are missing, duplicated, or out of order",
        ))
}

fn ascii_field<'a>(line: &'a [u8], prefix: &[u8]) -> Result<&'a str, ReleaseVerifyError> {
    let value = field(line, prefix)?;
    std::str::from_utf8(value).map_err(|_| {
        ReleaseVerifyError::MalformedManifest("release manifest contains non-ASCII field data")
    })
}

fn valid_release_version(version: &str) -> bool {
    if version.is_empty() || version.len() > 64 || !version.is_ascii() {
        return false;
    }
    let (core, prerelease) = match version.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (version, None),
    };
    let components: Vec<&str> = core.split('.').collect();
    if components.len() != 3 || !components.into_iter().all(valid_numeric_identifier) {
        return false;
    }
    prerelease.is_none_or(|suffix| {
        !suffix.is_empty()
            && suffix.split('.').all(|identifier| {
                !identifier.is_empty()
                    && identifier
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    && (!identifier.bytes().all(|byte| byte.is_ascii_digit())
                        || valid_numeric_identifier(identifier))
            })
    })
}

fn valid_numeric_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.bytes().all(|byte| byte.is_ascii_digit())
        && (identifier == "0" || !identifier.starts_with('0'))
}

fn decode_hex<const N: usize>(hex: &[u8]) -> Option<[u8; N]> {
    if hex.len() != N * 2 {
        return None;
    }
    let mut out = [0_u8; N];
    for (index, pair) in hex.chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        out[index] = (high << 4) | low;
    }
    Some(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor};

    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};

    use super::*;

    const VERSION: &str = "1.2.3";
    const TAG: &str = "v1.2.3";
    const INSTALLER: &[u8] = b"exact CI-produced installer bytes\n";

    fn manifest_with(
        version: &str,
        tag: &str,
        artifact: &str,
        byte_count: &str,
        sha256: &str,
    ) -> Vec<u8> {
        format!(
            "schema={MANIFEST_SCHEMA}\nversion={version}\ntag={tag}\nartifact={artifact}\nbytes={byte_count}\nsha256={sha256}\n"
        )
        .into_bytes()
    }

    fn valid_manifest() -> Vec<u8> {
        manifest_with(
            VERSION,
            TAG,
            "Install Ignite v1.2.3.sh",
            &INSTALLER.len().to_string(),
            &encode_hex(&Sha256::digest(INSTALLER)),
        )
    }

    fn signature(manifest: &[u8], seed: u8) -> (Vec<u8>, [u8; 32]) {
        let signing_key = SigningKey::from_bytes(&[seed; 32]);
        let signature = signing_key.sign(manifest);
        let envelope = format!(
            "schema={SIGNATURE_SCHEMA}\nsignature={}\n",
            encode_hex(&signature.to_bytes())
        )
        .into_bytes();
        (envelope, signing_key.verifying_key().to_bytes())
    }

    fn authenticate(manifest: &[u8]) -> Result<AuthenticatedReleaseManifest, ReleaseVerifyError> {
        let (sig, key) = signature(manifest, 7);
        authenticate_with_key(manifest, &sig, TAG, &key)
    }

    #[test]
    fn valid_signature_manifest_and_installer() {
        let manifest = valid_manifest();
        assert_eq!(
            String::from_utf8(manifest.clone()).unwrap(),
            concat!(
                "schema=ignite-kindle-release-v1\n",
                "version=1.2.3\n",
                "tag=v1.2.3\n",
                "artifact=Install Ignite v1.2.3.sh\n",
                "bytes=34\n",
                "sha256=be3cf18e4bdc10a6132eb0df5e3879ba1e5d7b4b62ab81221c56edd6260ab96f\n"
            )
        );
        let authenticated = authenticate(&manifest).unwrap();
        assert_eq!(authenticated.version(), VERSION);
        assert_eq!(authenticated.tag(), TAG);
        assert_eq!(
            authenticated.installer_filename(),
            "Install Ignite v1.2.3.sh"
        );
        assert_eq!(authenticated.installer_bytes(), INSTALLER.len() as u64);
        assert_eq!(
            authenticated.installer_sha256_hex(),
            encode_hex(&Sha256::digest(INSTALLER))
        );
        authenticated
            .verify_installer("Install Ignite v1.2.3.sh", Cursor::new(INSTALLER))
            .unwrap();
    }

    #[test]
    fn modified_signed_bytes_are_rejected_before_parsing() {
        let original = valid_manifest();
        let (sig, key) = signature(&original, 7);
        let mut modified = original;
        let modified_index = modified.len() - 2;
        modified[modified_index] ^= 1;
        assert_eq!(
            authenticate_with_key(&modified, &sig, TAG, &key),
            Err(ReleaseVerifyError::InvalidSignature)
        );

        let malformed = b"duplicate=field\nduplicate=field\n";
        assert_eq!(
            authenticate_with_key(malformed, &sig, TAG, &key),
            Err(ReleaseVerifyError::InvalidSignature),
            "malformed unsigned fields must not influence the result"
        );
    }

    #[test]
    fn wrong_public_key_is_rejected() {
        let manifest = valid_manifest();
        let (sig, _) = signature(&manifest, 7);
        let (_, wrong_key) = signature(&manifest, 8);
        assert_eq!(
            authenticate_with_key(&manifest, &sig, TAG, &wrong_key),
            Err(ReleaseVerifyError::InvalidSignature)
        );
    }

    #[test]
    fn missing_and_malformed_signatures_are_rejected() {
        let manifest = valid_manifest();
        let (_, key) = signature(&manifest, 7);
        assert_eq!(
            authenticate_with_key(&manifest, b"", TAG, &key),
            Err(ReleaseVerifyError::MissingSignature)
        );
        for malformed in [
            b"not-a-signature".as_slice(),
            b"schema=ignite-kindle-release-signature-v2\nsignature=00\n".as_slice(),
            b"schema=ignite-kindle-release-signature-v1\nsignature=GG\n".as_slice(),
        ] {
            assert_eq!(
                authenticate_with_key(&manifest, malformed, TAG, &key),
                Err(ReleaseVerifyError::MalformedSignature)
            );
        }
    }

    #[test]
    fn signed_unsupported_schema_is_rejected() {
        let mut manifest = valid_manifest();
        manifest.splice(
            .."schema=ignite-kindle-release-v1".len(),
            b"schema=ignite-kindle-release-v2".iter().copied(),
        );
        assert_eq!(
            authenticate(&manifest),
            Err(ReleaseVerifyError::UnsupportedSchema)
        );
    }

    #[test]
    fn wrong_version_tag_and_artifact_name_are_rejected() {
        let digest = encode_hex(&Sha256::digest(INSTALLER));
        let size = INSTALLER.len().to_string();

        let wrong_version = manifest_with(
            "1.2.4",
            "v1.2.4",
            "Install Ignite v1.2.4.sh",
            &size,
            &digest,
        );
        assert_eq!(
            authenticate(&wrong_version),
            Err(ReleaseVerifyError::UnexpectedReleaseTag)
        );

        let wrong_tag = manifest_with(
            VERSION,
            "v1.2.4",
            "Install Ignite v1.2.3.sh",
            &size,
            &digest,
        );
        assert_eq!(
            authenticate(&wrong_tag),
            Err(ReleaseVerifyError::InconsistentVersionTag)
        );

        let wrong_artifact =
            manifest_with(VERSION, TAG, "Install Ignite v1.2.3.sha256", &size, &digest);
        assert_eq!(
            authenticate(&wrong_artifact),
            Err(ReleaseVerifyError::UnexpectedArtifactName)
        );
    }

    #[test]
    fn wrong_artifact_digest_and_byte_count_are_rejected() {
        let authenticated = authenticate(&valid_manifest()).unwrap();
        let mut wrong_digest = INSTALLER.to_vec();
        wrong_digest[0] ^= 1;
        assert_eq!(
            authenticated.verify_installer("Install Ignite v1.2.3.sh", Cursor::new(wrong_digest)),
            Err(ReleaseVerifyError::ArtifactDigestMismatch)
        );
        assert_eq!(
            authenticated.verify_installer(
                "Install Ignite v1.2.3.sh",
                Cursor::new(&INSTALLER[..INSTALLER.len() - 1])
            ),
            Err(ReleaseVerifyError::ArtifactByteCountMismatch)
        );
        assert_eq!(
            authenticated.verify_installer("attacker.sh", Cursor::new(INSTALLER)),
            Err(ReleaseVerifyError::UnexpectedArtifactName)
        );
    }

    #[test]
    fn malformed_duplicate_and_noncanonical_metadata_is_rejected() {
        let digest = encode_hex(&Sha256::digest(INSTALLER));
        let size = INSTALLER.len().to_string();
        let duplicate = format!(
            "schema={MANIFEST_SCHEMA}\nversion={VERSION}\nversion={VERSION}\ntag={TAG}\nartifact=Install Ignite v1.2.3.sh\nbytes={size}\nsha256={digest}\n"
        );
        let crlf = String::from_utf8(valid_manifest())
            .unwrap()
            .replace('\n', "\r\n")
            .into_bytes();
        let missing_digest = format!(
            "schema={MANIFEST_SCHEMA}\nversion={VERSION}\ntag={TAG}\nartifact=Install Ignite v1.2.3.sh\nbytes={size}\n"
        );

        let mut cases: Vec<Vec<u8>> = vec![
            duplicate.into_bytes(),
            missing_digest.into_bytes(),
            valid_manifest()[..valid_manifest().len() - 1].to_vec(),
            crlf,
            manifest_with(
                VERSION,
                TAG,
                "Install Ignite v1.2.3.sh",
                &format!("0{size}"),
                &digest,
            ),
            manifest_with(
                VERSION,
                TAG,
                "Install Ignite v1.2.3.sh",
                &size,
                &digest.to_uppercase(),
            ),
        ];
        let mut invalid_utf8 = valid_manifest();
        invalid_utf8[0] = 0xff;
        cases.push(invalid_utf8);

        for manifest in cases {
            assert!(matches!(
                authenticate(&manifest),
                Err(ReleaseVerifyError::MalformedManifest(_))
            ));
        }
    }

    #[test]
    fn oversized_manifest_and_reader_errors_fail_closed() {
        let manifest = vec![b'x'; MAX_MANIFEST_BYTES + 1];
        let (sig, key) = signature(&manifest, 7);
        assert_eq!(
            authenticate_with_key(&manifest, &sig, TAG, &key),
            Err(ReleaseVerifyError::ManifestTooLarge)
        );

        struct BrokenReader;
        impl Read for BrokenReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::Other, "injected"))
            }
        }
        let authenticated = authenticate(&valid_manifest()).unwrap();
        assert_eq!(
            authenticated.verify_installer("Install Ignite v1.2.3.sh", BrokenReader),
            Err(ReleaseVerifyError::ArtifactRead(io::ErrorKind::Other))
        );
    }

    #[test]
    fn official_pin_configuration_is_valid_or_explicitly_unprovisioned() {
        match official_release_public_key() {
            Err(ReleaseVerifyError::OfficialKeyNotProvisioned) => {
                assert_eq!(OFFICIAL_RELEASE_PUBLIC_KEY_HEX, "UNPROVISIONED\n");
                assert_eq!(
                    parse_official_public_key(OFFICIAL_RELEASE_PUBLIC_KEY_HEX),
                    Err(ReleaseVerifyError::OfficialKeyNotProvisioned)
                );
            }
            Ok(public_key) => {
                VerifyingKey::from_bytes(&public_key)
                    .expect("configured official release pin must be a valid Ed25519 public key");
            }
            Err(error) => panic!("official release public-key configuration is invalid: {error}"),
        }
    }

    #[test]
    fn official_entry_point_rejects_an_untrusted_signature_in_every_pin_state() {
        let manifest = valid_manifest();
        let (untrusted_signature, _) = signature(&manifest, 7);
        let actual = authenticate_official_release(&manifest, &untrusted_signature, TAG);

        match official_release_public_key() {
            Err(ReleaseVerifyError::OfficialKeyNotProvisioned) => {
                assert_eq!(actual, Err(ReleaseVerifyError::OfficialKeyNotProvisioned))
            }
            Ok(_) => assert_eq!(actual, Err(ReleaseVerifyError::InvalidSignature)),
            Err(error) => panic!("official release public-key configuration is invalid: {error}"),
        }
    }

    #[test]
    fn official_pin_parser_rejects_malformed_and_known_test_keys() {
        for invalid in [
            "",
            "UNPROVISIONED",
            "00\n",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
            concat!(
                "d75a980182b10ab7d54bfed3c964073a0",
                "ee172f3daa62325af021a68f707511a\n"
            ),
            concat!(
                "00000000000000000000000000000000",
                "00000000000000000000000000000000\n\n"
            ),
        ] {
            assert_eq!(
                parse_official_public_key(invalid),
                Err(ReleaseVerifyError::InvalidPinnedPublicKey)
            );
        }
        assert_eq!(
            parse_official_public_key("UNPROVISIONED\n"),
            Err(ReleaseVerifyError::OfficialKeyNotProvisioned)
        );
        for test_key in KNOWN_TEST_PUBLIC_KEY_HEX {
            assert_eq!(
                parse_official_public_key(&format!("{test_key}\n")),
                Err(ReleaseVerifyError::InvalidPinnedPublicKey)
            );
        }
        assert_eq!(
            parse_official_public_key(concat!(
                "01010101010101010101010101010101",
                "01010101010101010101010101010101\n"
            )),
            Ok([1_u8; 32])
        );
    }

    #[test]
    fn release_version_grammar_boundaries_are_canonical() {
        for valid in ["0.0.0", "1.2.3", "10.20.30-rc.1", "1.2.3-alpha-beta.0"] {
            assert!(valid_release_version(valid), "expected valid: {valid}");
        }
        for invalid in [
            "",
            "v1.2.3",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "1.2.3-",
            "1.2.3-.rc",
            "1.2.3-rc.",
            "1.2.3-rc..1",
            "1.2.3-01",
            "1.2.3+build",
            "1.2.3-rc+build",
            "1.2.3_rc",
            "1.2.3-rc/1",
        ] {
            assert!(
                !valid_release_version(invalid),
                "expected invalid: {invalid}"
            );
        }
        assert!(!valid_release_version(&format!("1.2.3-{}", "a".repeat(59))));
    }
}
