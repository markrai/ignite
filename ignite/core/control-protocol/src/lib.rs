//! D1 control-plane wire contract shared by Scribe Server, Scribe for Kindle and (as documented
//! JSON) ignite-web.
//!
//! This crate is deliberately separate from `sync-protocol`: server identity, discovery,
//! pairing and credentials are **deployment/auth** concepts, never notebook content. Nothing
//! here appears in `SyncSnapshotV1`, operation envelopes or notebook files.
//!
//! ```text
//! DNS-SD   _scribe._tcp.local.   TXT: v=1  sid=<ServerId>  name=<display name>  cp=/control/v1
//!
//! GET  /control/v1/status                          public: identity + claim state + session
//! POST /control/v1/claim            {setup_code}   first browser → admin session (cookie)
//! POST /control/v1/pairing          {client_type, client_id?, display_name}
//!                                   → {request_id, request_secret, comparison_code, expires_in_s}
//! POST /control/v1/pairing/{id}/poll {request_secret}
//!                                   → {state, device_id?, credential? (Kindle, delivered once)}
//! GET  /control/v1/admin/pairing                   pending requests       (admin)
//! POST /control/v1/admin/pairing/{id}/approve|deny                        (admin)
//! GET  /control/v1/admin/devices                                          (admin)
//! POST /control/v1/admin/devices/{id}/revoke                              (admin)
//! GET  /control/v1/admin/server                    status                 (admin)
//! ```
//!
//! Credentials are `<DeviceId>.<secret>` where `secret` is 32 random bytes (256 bits) in
//! unpadded base64url. The server stores only SHA-256(secret) per device record and compares
//! in constant time; the DeviceId prefix selects the one record the secret may authenticate.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// DNS-SD service type (RFC 6763), fully qualified.
pub const SERVICE_TYPE: &str = "_scribe._tcp.local.";
/// Version of the advertisement / control API.
pub const CONTROL_VERSION: u32 = 1;
/// Control API base path.
pub const CONTROL_BASE: &str = "/control/v1";
/// TXT keys (all non-secret).
pub const TXT_VERSION: &str = "v";
pub const TXT_SERVER_ID: &str = "sid";
pub const TXT_NAME: &str = "name";
pub const TXT_CONTROL_PATH: &str = "cp";
/// Keys a TXT record may carry. Anything else is refused by [`validate_txt`] (defence against
/// ever advertising secrets by accident).
pub const TXT_ALLOWED: [&str; 4] = [TXT_VERSION, TXT_SERVER_ID, TXT_NAME, TXT_CONTROL_PATH];
/// A DNS-SD instance name is one DNS label (≤ 63 octets).
pub const MAX_INSTANCE_NAME_BYTES: usize = 63;
/// Browser session cookie name (value is a credential; HttpOnly, SameSite=Strict).
pub const SESSION_COOKIE: &str = "scribe_session";
/// Anti-CSRF request marker header for cookie-authenticated mutations.
pub const CSRF_HEADER: &str = "X-Scribe-Request";
/// Pairing requests expire after this many seconds.
pub const PAIRING_TTL_S: u64 = 300;

/// Durable identity of one Scribe Server installation (UUIDv7, lowercase hyphenated). Never
/// derived from hostname or IP.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServerId(Uuid);

impl ServerId {
    pub fn from_uuid(u: Uuid) -> Result<Self, String> {
        if u.get_version_num() != 7 || u.get_variant() != uuid::Variant::RFC4122 {
            return Err("ServerId must be an RFC 4122 UUIDv7".into());
        }
        Ok(Self(u))
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let u = Uuid::try_parse(s).map_err(|e| format!("ServerId: {e}"))?;
        if u.hyphenated().to_string() != s {
            return Err("ServerId must be canonical lowercase hyphenated".into());
        }
        Self::from_uuid(u)
    }

    /// UUIDv7 from a millisecond timestamp and 10 CSPRNG bytes (RFC 9562 layout).
    pub fn from_parts(unix_ms: u64, random: &[u8; 10]) -> Self {
        let mut b = [0u8; 16];
        b[..6].copy_from_slice(&unix_ms.to_be_bytes()[2..]);
        b[6] = 0x70 | (random[0] & 0x0f);
        b[7] = random[1];
        b[8] = 0x80 | (random[2] & 0x3f);
        b[9..].copy_from_slice(&random[3..]);
        Self(Uuid::from_bytes(b))
    }
}

impl std::fmt::Display for ServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

impl TryFrom<String> for ServerId {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        Self::parse(&s)
    }
}

impl From<ServerId> for String {
    fn from(v: ServerId) -> String {
        v.to_string()
    }
}

/// `"Ignite Server on Workshop"` from a hostname (`workshop`, `workshop.local`).
/// Trimmed to one DNS label's worth of bytes on a char boundary.
pub fn default_display_name(hostname: &str) -> String {
    let short = hostname.split('.').next().unwrap_or("").trim();
    let mut chars = short.chars();
    let host = match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => "this computer".to_string(),
    };
    clamp_label(&format!("Ignite Server on {host}"))
}

/// Clamp to [`MAX_INSTANCE_NAME_BYTES`] on a UTF-8 boundary, removing control characters.
pub fn clamp_label(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > MAX_INSTANCE_NAME_BYTES {
            break;
        }
        out.push(c);
    }
    out.trim().to_string()
}

/// The TXT properties Scribe Server advertises. Non-secret by construction.
pub fn advertisement_txt(server_id: ServerId, name: &str) -> Vec<(String, String)> {
    vec![
        (TXT_VERSION.into(), CONTROL_VERSION.to_string()),
        (TXT_SERVER_ID.into(), server_id.to_string()),
        (TXT_NAME.into(), clamp_label(name)),
        (TXT_CONTROL_PATH.into(), CONTROL_BASE.into()),
    ]
}

/// Refuse a TXT record carrying any key outside [`TXT_ALLOWED`] or oversize values.
pub fn validate_txt(txt: &[(String, String)]) -> Result<(), String> {
    for (k, v) in txt {
        if !TXT_ALLOWED.contains(&k.as_str()) {
            return Err(format!("TXT key {k:?} is not an allowed advertisement key"));
        }
        if k.len() + v.len() + 1 > 255 {
            return Err(format!("TXT entry {k} too long"));
        }
    }
    Ok(())
}

/// A server seen on the LAN, parsed from an advertisement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advertised {
    pub server_id: ServerId,
    pub name: String,
    pub control_path: String,
}

/// Parse and validate a received TXT record. Unknown versions are refused.
pub fn parse_advertisement(txt: &[(String, String)]) -> Result<Advertised, String> {
    let get = |k: &str| {
        txt.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    match get(TXT_VERSION) {
        Some(v) if v == CONTROL_VERSION.to_string() => {}
        other => return Err(format!("unsupported advertisement version {other:?}")),
    }
    let server_id = ServerId::parse(get(TXT_SERVER_ID).ok_or("missing sid")?)?;
    let name = clamp_label(get(TXT_NAME).unwrap_or(""));
    let control_path = get(TXT_CONTROL_PATH).unwrap_or(CONTROL_BASE).to_string();
    if control_path != CONTROL_BASE {
        return Err(format!("unsupported control path {control_path:?}"));
    }
    Ok(Advertised {
        server_id,
        name: if name.is_empty() {
            "Ignite Server".into()
        } else {
            name
        },
        control_path,
    })
}

// ---------------------------------------------------------------------------- credentials

/// Base64url (RFC 4648 §5) without padding.
pub fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        let take = chunk.len() + 1;
        for i in 0..take {
            out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

pub fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    let val = |c: u8| -> Result<u32, String> {
        Ok(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return Err("invalid base64url".into()),
        } as u32)
    };
    let b = s.as_bytes();
    if b.len() % 4 == 1 {
        return Err("invalid base64url length".into());
    }
    let mut out = Vec::with_capacity(b.len() * 3 / 4);
    for chunk in b.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// Bytes of randomness in every credential secret (256 bits).
pub const CREDENTIAL_SECRET_BYTES: usize = 32;

/// A parsed `<DeviceId>.<secret>` credential. `Debug` never prints the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    pub device_id: String,
    pub secret: Vec<u8>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credential({}.<redacted>)", self.device_id)
    }
}

impl Credential {
    pub fn new(device_id: &str, secret: [u8; CREDENTIAL_SECRET_BYTES]) -> Self {
        Self {
            device_id: device_id.to_string(),
            secret: secret.to_vec(),
        }
    }

    /// The wire form. Handle as a secret: never log it.
    pub fn encode(&self) -> String {
        format!("{}.{}", self.device_id, b64url(&self.secret))
    }

    pub fn parse(s: &str) -> Option<Self> {
        let (id, secret) = s.trim().split_once('.')?;
        let u = Uuid::try_parse(id).ok()?;
        if u.hyphenated().to_string() != id {
            return None;
        }
        let secret = b64url_decode(secret).ok()?;
        (secret.len() == CREDENTIAL_SECRET_BYTES).then(|| Self {
            device_id: id.to_string(),
            secret,
        })
    }
}

// ---------------------------------------------------------------------------- human codes

/// Crockford-style alphabet without I, L, O, U (unambiguous when read aloud or typed).
pub const SETUP_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// `K7M4-P2Q9`: 8 symbols × 5 bits = 40 bits from 5 random bytes. One-time, rate limited.
pub fn setup_code(random: &[u8; 5]) -> String {
    let mut n: u64 = 0;
    for &b in random {
        n = (n << 8) | u64::from(b);
    }
    let mut s = String::with_capacity(9);
    for i in 0..8 {
        if i == 4 {
            s.push('-');
        }
        s.push(SETUP_ALPHABET[((n >> (35 - 5 * i)) & 31) as usize] as char);
    }
    s
}

/// Canonical form of a typed setup code (case/space/dash tolerant, ambiguous letters mapped).
pub fn normalize_setup_code(s: &str) -> String {
    let mut out: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect();
    if out.len() == 8 {
        out.insert(4, '-');
    }
    out
}

/// `482 731`: a 6-digit comparison code from a uniform random value < 10^6 (the caller
/// supplies rejection-sampled randomness via [`comparison_code_from`]).
pub fn comparison_code_from(uniform_below_million: u32) -> String {
    let n = uniform_below_million % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

/// Rejection-sample a value < 10^6 from 32-bit random words (unbiased).
pub fn uniform_below_million(mut next_u32: impl FnMut() -> u32) -> u32 {
    const LIMIT: u32 = u32::MAX - (u32::MAX % 1_000_000);
    loop {
        let v = next_u32();
        if v < LIMIT {
            return v % 1_000_000;
        }
    }
}

// ---------------------------------------------------------------------------- DTOs

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientType {
    Kindle,
    Browser,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    Member,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInfo {
    pub device_id: String,
    pub role: Role,
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusResponse {
    pub v: u32,
    pub mode: String,
    pub server_id: ServerId,
    pub server_name: String,
    pub version: String,
    pub claimed: bool,
    pub session: Option<SessionInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingCreate {
    pub client_type: ClientType,
    /// Client-generated stable id (optional; descriptive only, never trusted as identity).
    #[serde(default)]
    pub client_id: Option<String>,
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingCreated {
    pub request_id: String,
    /// Proof of being the requester when polling (random; only this client holds it).
    pub request_secret: String,
    pub comparison_code: String,
    pub expires_in_s: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingPoll {
    pub request_secret: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    Pending,
    Approved,
    Denied,
    Expired,
    /// Approved and the credential was already delivered (a second poll gets nothing).
    Delivered,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingStatus {
    pub state: PairingState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// Kindle/native clients only, exactly once. Browsers receive a cookie instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    pub server_id: ServerId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRequest {
    pub request_id: String,
    pub client_type: ClientType,
    pub display_name: String,
    pub comparison_code: String,
    pub expires_in_s: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceRecord {
    pub device_id: String,
    pub client_type: ClientType,
    pub display_name: String,
    pub role: Role,
    pub created_ms: u64,
    pub last_seen_ms: Option<u64>,
    pub revoked: bool,
    /// True for the session making this request.
    pub current: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerStatus {
    pub server_id: ServerId,
    pub server_name: String,
    pub version: String,
    pub started_ms: u64,
    pub library_count: u64,
    pub device_count: u64,
    pub pending_requests: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid() -> ServerId {
        ServerId::from_parts(1_790_000_000_000, &[7; 10])
    }

    #[test]
    fn server_id_is_v7_canonical_and_round_trips() {
        let s = sid();
        let text = s.to_string();
        assert_eq!(ServerId::parse(&text).unwrap(), s);
        assert!(ServerId::parse(&text.to_uppercase()).is_err());
        assert!(ServerId::parse("00000000-0000-4000-8000-000000000000").is_err()); // v4
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, format!("\"{text}\""));
    }

    #[test]
    fn display_names() {
        assert_eq!(
            default_display_name("workshop"),
            "Ignite Server on Workshop"
        );
        assert_eq!(
            default_display_name("WORKSHOP.local"),
            "Ignite Server on Workshop"
        );
        assert_eq!(default_display_name(""), "Ignite Server on this computer");
        let long = "x".repeat(200);
        assert!(default_display_name(&long).len() <= MAX_INSTANCE_NAME_BYTES);
        let emoji = format!("Ignite Server on {}", "é".repeat(40));
        let c = clamp_label(&emoji);
        assert!(c.len() <= MAX_INSTANCE_NAME_BYTES && c.is_char_boundary(c.len()));
        assert_eq!(clamp_label("a\u{7}b\n"), "ab");
    }

    #[test]
    fn advertisement_is_nonsecret_and_parses() {
        let txt = advertisement_txt(sid(), "Ignite Server on Workshop");
        validate_txt(&txt).unwrap();
        let keys: Vec<&str> = txt.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["v", "sid", "name", "cp"]);
        let a = parse_advertisement(&txt).unwrap();
        assert_eq!(a.server_id, sid());
        assert_eq!(a.name, "Ignite Server on Workshop");
        let mut bad = txt.clone();
        bad.push(("token".into(), "x".into()));
        assert!(validate_txt(&bad).is_err());
        let mut v2 = txt.clone();
        v2[0].1 = "2".into();
        assert!(parse_advertisement(&v2).is_err());
    }

    #[test]
    fn credentials_round_trip_and_redact() {
        let c = Credential::new("01a0de1f-f9d6-7036-a2b7-478dc58007ff", [0xAB; 32]);
        let wire = c.encode();
        assert_eq!(Credential::parse(&wire).unwrap(), c);
        assert!(!format!("{c:?}").contains(&b64url(&[0xAB; 32])));
        assert!(Credential::parse("nope").is_none());
        assert!(Credential::parse("01a0de1f-f9d6-7036-a2b7-478dc58007ff.AAAA").is_none()); // short
        for n in 0..40 {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b64url_decode(&b64url(&bytes)).unwrap(), bytes);
        }
    }

    #[test]
    fn human_codes() {
        let c = setup_code(&[0xde, 0xad, 0xbe, 0xef, 0x42]);
        assert_eq!(c.len(), 9);
        assert_eq!(&c[4..5], "-");
        assert!(c
            .bytes()
            .filter(|&b| b != b'-')
            .all(|b| SETUP_ALPHABET.contains(&b)));
        assert_eq!(normalize_setup_code(&c.to_lowercase().replace('-', " ")), c);
        assert_eq!(normalize_setup_code("k7m4 p2q9"), "K7M4-P2Q9");
        assert_eq!(comparison_code_from(482_731), "482 731");
        assert_eq!(comparison_code_from(7), "000 007");
        let mut seq = [u32::MAX, u32::MAX - 1, 123_456_789].into_iter();
        assert_eq!(uniform_below_million(|| seq.next().unwrap()), 456_789);
    }
}
