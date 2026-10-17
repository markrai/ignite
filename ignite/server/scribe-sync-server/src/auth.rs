//! S2B device authentication: one bearer token per development deployment.
//!
//! * Token: 32 bytes from the OS CSPRNG (256 bits), hex-encoded (64 chars). Printed once when
//!   generated; the server stores only its SHA-256 **verifier**.
//! * Check: `Authorization: Bearer <token>` → SHA-256 → constant-time comparison with the
//!   verifier (no early exit on the first differing byte).
//! * Scope: when auth is configured, every `/v1` endpoint (including blobs) requires it.
//!   Loopback binds may run without auth for local development; any non-loopback bind refuses
//!   to start without auth.
//!
//! Not Internet security: tokens travel over plain HTTP in S2B, so this is for a trusted LAN
//! only. TLS belongs in front of the server (reverse proxy) in a later phase.

use std::fmt;
use std::path::Path;

use sha2::{Digest, Sha256};

/// SHA-256 of a bearer token. Debug output never reveals it.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenVerifier([u8; 32]);

impl fmt::Debug for TokenVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenVerifier(<redacted>)")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex32(s: &str) -> Result<[u8; 32], String> {
    let s = s.trim();
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("expected 64 hex characters".into());
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16)
            .map_err(|e| e.to_string())?;
    }
    Ok(out)
}

impl TokenVerifier {
    pub fn for_token(token: &str) -> Self {
        Self(Sha256::digest(token.as_bytes()).into())
    }

    /// Verifier file: 64 lowercase hex chars (SHA-256 of the token).
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self, String> {
        unhex32(s).map(Self)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| format!("read verifier {}: {e}", path.display()))?;
        Self::from_hex(&s).map_err(|e| format!("verifier {}: {e}", path.display()))
    }

    /// Constant-time check of a presented token.
    pub fn verify(&self, presented: &str) -> bool {
        let got: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        let mut diff = 0u8;
        for (a, b) in got.iter().zip(self.0.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }

    /// Check an `Authorization` header value (`Bearer <token>`).
    pub fn verify_header(&self, header: Option<&str>) -> bool {
        match header.and_then(|h| h.strip_prefix("Bearer ")) {
            Some(token) => self.verify(token.trim()),
            None => false,
        }
    }
}

/// A fresh 256-bit token from the OS CSPRNG, hex-encoded.
pub fn generate_token() -> Result<String, String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| format!("OS randomness unavailable: {e}"))?;
    Ok(hex(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_256_bit_and_verify_exactly() {
        let t = generate_token().unwrap();
        assert_eq!(t.len(), 64);
        assert_ne!(t, generate_token().unwrap());
        let v = TokenVerifier::for_token(&t);
        assert!(v.verify(&t));
        assert!(!v.verify(&t[..63]));
        assert!(!v.verify(""));
        assert!(v.verify_header(Some(&format!("Bearer {t}"))));
        assert!(!v.verify_header(Some(&format!("bearer {t}"))));
        assert!(!v.verify_header(Some(&t)));
        assert!(!v.verify_header(None));
        assert_eq!(TokenVerifier::from_hex(&v.to_hex()).unwrap(), v);
        assert!(!format!("{v:?}").contains(&v.to_hex()));
    }
}
