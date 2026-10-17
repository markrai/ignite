//! Control-plane state: `<state_dir>/control.sqlite` (separate from the notebook ledger).
//!
//! ```text
//! meta     (key, value)                 claimed_ms, setup_code_verifier, setup_code_created_ms
//! devices  (device_id PK, client_type, display_name, role, verifier, created_ms, last_seen_ms,
//!           revoked_ms, client_id)      one row per paired Kindle / authorized browser
//! pairing  (request_id PK, secret_verifier, client_type, client_id, display_name,
//!           comparison_code, created_ms, expires_ms, state, device_id, peer)
//! ```
//!
//! Only SHA-256 verifiers of secrets are stored (setup code, credential secrets, pairing
//! request secrets); every secret has ≥ 256 bits except the one-time setup code (40 bits,
//! rate limited) and the comparison code (a display aid, not a secret). Verification is
//! constant-time. The ledger (`ledger/`) never sees any of this.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use control_protocol::{
    comparison_code_from, normalize_setup_code, setup_code, uniform_below_million, ClientType,
    Credential, DeviceRecord, PairingState, PendingRequest, Role, CREDENTIAL_SECRET_BYTES,
    PAIRING_TTL_S,
};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::fsutil::{random_bytes, random_u32, restrict_file};

pub const CONTROL_DB: &str = "control.sqlite";
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS devices (
    device_id     TEXT PRIMARY KEY,
    client_type   TEXT NOT NULL CHECK (client_type IN ('kindle','browser')),
    display_name  TEXT NOT NULL,
    role          TEXT NOT NULL CHECK (role IN ('admin','member')),
    verifier      BLOB NOT NULL CHECK (length(verifier) = 32),
    created_ms    INTEGER NOT NULL,
    last_seen_ms  INTEGER,
    revoked_ms    INTEGER,
    client_id     TEXT
);
CREATE TABLE IF NOT EXISTS pairing (
    request_id       TEXT PRIMARY KEY,
    secret_verifier  BLOB NOT NULL CHECK (length(secret_verifier) = 32),
    client_type      TEXT NOT NULL CHECK (client_type IN ('kindle','browser')),
    client_id        TEXT,
    display_name     TEXT NOT NULL,
    comparison_code  TEXT NOT NULL,
    created_ms       INTEGER NOT NULL,
    expires_ms       INTEGER NOT NULL,
    state            TEXT NOT NULL,
    device_id        TEXT,
    peer             TEXT
);
"#;

pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

#[derive(Debug, PartialEq, Eq)]
pub enum ControlError {
    NotFound,
    Conflict(&'static str),
    Invalid(&'static str),
    Storage(String),
}

impl From<rusqlite::Error> for ControlError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Storage(e.to_string())
    }
}

type R<T> = Result<T, ControlError>;
/// `(client_type, role, display_name, verifier, revoked_ms, last_seen_ms)`
type DeviceRow = (String, String, String, Vec<u8>, Option<i64>, Option<i64>);
/// `(secret_verifier, client_type, client_id, display_name, expires_ms, state)`
type PairingRow = (Vec<u8>, String, Option<String>, String, i64, String);

pub fn verifier(secret: &[u8]) -> [u8; 32] {
    Sha256::digest(secret).into()
}

/// Constant-time equality of two 32-byte verifiers.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y;
    }
    d == 0
}

fn client_type_str(t: ClientType) -> &'static str {
    match t {
        ClientType::Kindle => "kindle",
        ClientType::Browser => "browser",
    }
}

fn parse_client_type(s: &str) -> ClientType {
    if s == "kindle" {
        ClientType::Kindle
    } else {
        ClientType::Browser
    }
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::Admin => "admin",
        Role::Member => "member",
    }
}

fn parse_role(s: &str) -> Role {
    if s == "admin" {
        Role::Admin
    } else {
        Role::Member
    }
}

/// A new UUIDv7 text from OS randomness (device and request ids).
pub fn new_id(now: u64) -> String {
    let mut r = [0u8; 10];
    random_bytes(&mut r).expect("OS randomness");
    control_protocol::ServerId::from_parts(now, &r).to_string()
}

/// An authenticated principal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub device_id: String,
    pub client_type: ClientType,
    pub role: Role,
    pub display_name: String,
}

/// A freshly issued credential (the only moment its plaintext exists server-side).
pub struct Issued {
    pub device_id: String,
    pub credential: Credential,
}

pub struct ControlStore {
    conn: Mutex<Connection>,
    clock: Clock,
    path: PathBuf,
}

impl ControlStore {
    pub fn open(state_dir: &Path, clock: Clock) -> Result<Self, String> {
        let path = state_dir.join(CONTROL_DB);
        let conn = Connection::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
        )
        .and_then(|()| conn.execute_batch(SCHEMA))
        .map_err(|e| format!("init {}: {e}", path.display()))?;
        restrict_file(&path);
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn now(&self) -> u64 {
        (self.clock)()
    }

    fn meta(&self, c: &Connection, key: &str) -> R<Option<String>> {
        Ok(
            c.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
                .optional()?,
        )
    }

    pub fn is_claimed(&self) -> R<bool> {
        let c = self.conn.lock().unwrap();
        Ok(self.meta(&c, "claimed_ms")?.is_some())
    }

    /// While unclaimed: issue a fresh one-time setup code (any previous code stops working).
    /// Returns `None` once claimed (the claim window never reopens).
    pub fn issue_setup_code(&self) -> R<Option<String>> {
        let c = self.conn.lock().unwrap();
        if self.meta(&c, "claimed_ms")?.is_some() {
            return Ok(None);
        }
        let mut r = [0u8; 5];
        random_bytes(&mut r).map_err(ControlError::Storage)?;
        let code = setup_code(&r);
        let v = verifier(normalize_setup_code(&code).as_bytes());
        c.execute(
            "INSERT INTO meta(key, value) VALUES ('setup_code_verifier', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [hex(&v)],
        )?;
        c.execute(
            "INSERT INTO meta(key, value) VALUES ('setup_code_created_ms', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [self.now().to_string()],
        )?;
        Ok(Some(code))
    }

    pub fn has_setup_code(&self) -> R<bool> {
        let c = self.conn.lock().unwrap();
        Ok(self.meta(&c, "setup_code_verifier")?.is_some())
    }

    /// First-run claim: the setup code is consumed atomically with creating the admin device.
    pub fn claim(&self, code: &str, display_name: &str) -> R<Issued> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let claimed: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key='claimed_ms'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if claimed.is_some() {
            return Err(ControlError::Conflict("already_claimed"));
        }
        let stored: Option<String> = tx
            .query_row(
                "SELECT value FROM meta WHERE key='setup_code_verifier'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let Some(stored) = stored.and_then(|h| unhex(&h)) else {
            return Err(ControlError::Invalid("no_setup_code"));
        };
        let presented = verifier(normalize_setup_code(code).as_bytes());
        if !ct_eq(&presented, &stored) {
            return Err(ControlError::Invalid("wrong_setup_code"));
        }
        let now = self.now();
        let issued = insert_device(
            &tx,
            now,
            ClientType::Browser,
            display_name,
            Role::Admin,
            None,
        )?;
        tx.execute(
            "DELETE FROM meta WHERE key IN ('setup_code_verifier','setup_code_created_ms')",
            [],
        )?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('claimed_ms', ?1)",
            [now.to_string()],
        )?;
        tx.commit()?;
        Ok(issued)
    }

    /// Authenticate a presented credential; revoked or unknown → `None`. Updates last_seen
    /// at most once a minute per device.
    pub fn authenticate(&self, presented: &str) -> R<Option<Principal>> {
        let Some(cred) = Credential::parse(presented) else {
            return Ok(None);
        };
        let c = self.conn.lock().unwrap();
        let row: Option<DeviceRow> = c
            .query_row(
                "SELECT client_type, role, display_name, verifier, revoked_ms, last_seen_ms
                 FROM devices WHERE device_id = ?1",
                [&cred.device_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()?;
        // Always hash, so unknown ids cost the same as known ones.
        let presented_v = verifier(&cred.secret);
        let Some((ct, role, name, stored_v, revoked, last_seen)) = row else {
            return Ok(None);
        };
        if !ct_eq(&presented_v, &stored_v) || revoked.is_some() {
            return Ok(None);
        }
        let now = self.now();
        if last_seen.is_none_or(|t| now.saturating_sub(t as u64) >= 60_000) {
            c.execute(
                "UPDATE devices SET last_seen_ms = ?1 WHERE device_id = ?2",
                params![now as i64, cred.device_id],
            )?;
        }
        Ok(Some(Principal {
            device_id: cred.device_id,
            client_type: parse_client_type(&ct),
            role: parse_role(&role),
            display_name: name,
        }))
    }

    /// Unauthenticated pairing request (the caller rate-limits). Refused while unclaimed.
    pub fn create_pairing(
        &self,
        client_type: ClientType,
        client_id: Option<&str>,
        display_name: &str,
        peer: Option<&str>,
        max_pending: u64,
    ) -> R<(String, String, String)> {
        let c = self.conn.lock().unwrap();
        if self.meta(&c, "claimed_ms")?.is_none() {
            return Err(ControlError::Conflict("unclaimed"));
        }
        let now = self.now();
        c.execute(
            "UPDATE pairing SET state='expired' WHERE state='pending' AND expires_ms <= ?1",
            [now as i64],
        )?;
        let pending: i64 = c.query_row(
            "SELECT COUNT(*) FROM pairing WHERE state='pending'",
            [],
            |r| r.get(0),
        )?;
        if pending as u64 >= max_pending {
            return Err(ControlError::Conflict("too_many_pending"));
        }
        let request_id = new_id(now);
        let mut secret = [0u8; 32];
        random_bytes(&mut secret).map_err(ControlError::Storage)?;
        let code = comparison_code_from(uniform_below_million(random_u32));
        c.execute(
            "INSERT INTO pairing(request_id, secret_verifier, client_type, client_id, display_name,
                                 comparison_code, created_ms, expires_ms, state, peer)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9)",
            params![
                request_id,
                verifier(&secret).to_vec(),
                client_type_str(client_type),
                client_id,
                display_name,
                code,
                now as i64,
                (now + PAIRING_TTL_S * 1000) as i64,
                peer,
            ],
        )?;
        Ok((request_id, control_protocol::b64url(&secret), code))
    }

    /// The requester polls with its request secret. On the first poll after approval the
    /// device record + credential are created and returned exactly once.
    pub fn poll_pairing(
        &self,
        request_id: &str,
        request_secret: &str,
    ) -> R<(PairingState, ClientType, Option<Issued>)> {
        let secret =
            control_protocol::b64url_decode(request_secret).map_err(|_| ControlError::NotFound)?;
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let row: Option<PairingRow> = tx
            .query_row(
                "SELECT secret_verifier, client_type, client_id, display_name, expires_ms, state
                 FROM pairing WHERE request_id = ?1",
                [request_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((sv, ct, client_id, name, expires, state)) = row else {
            return Err(ControlError::NotFound);
        };
        if !ct_eq(&verifier(&secret), &sv) {
            return Err(ControlError::NotFound); // indistinguishable from unknown
        }
        let ctype = parse_client_type(&ct);
        let now = self.now();
        let result = match state.as_str() {
            "pending" if (expires as u64) <= now => {
                tx.execute(
                    "UPDATE pairing SET state='expired' WHERE request_id=?1",
                    [request_id],
                )?;
                (PairingState::Expired, ctype, None)
            }
            "pending" => (PairingState::Pending, ctype, None),
            st @ ("approved" | "approved_admin") => {
                let role = if st == "approved_admin" && ctype == ClientType::Browser {
                    Role::Admin
                } else {
                    Role::Member
                };
                let issued = insert_device(&tx, now, ctype, &name, role, client_id.as_deref())?;
                tx.execute(
                    "UPDATE pairing SET state='delivered', device_id=?2 WHERE request_id=?1",
                    params![request_id, issued.device_id],
                )?;
                (PairingState::Approved, ctype, Some(issued))
            }
            "denied" => (PairingState::Denied, ctype, None),
            "delivered" => (PairingState::Delivered, ctype, None),
            _ => (PairingState::Expired, ctype, None),
        };
        tx.commit()?;
        Ok(result)
    }

    /// Admin decision on a pending, unexpired request.
    /// Host-operator recovery (a lost admin browser): approve the pending BROWSER request
    /// showing `comparison_code` as an administrator. Exactly one unexpired match is required.
    pub fn approve_admin_by_code(&self, comparison_code: &str) -> R<(String, String)> {
        let digits: String = comparison_code
            .chars()
            .filter(char::is_ascii_digit)
            .collect();
        if digits.len() != 6 {
            return Err(ControlError::Invalid("comparison code must have 6 digits"));
        }
        let code = format!("{} {}", &digits[..3], &digits[3..]);
        let c = self.conn.lock().unwrap();
        let now = self.now() as i64;
        let mut st = c.prepare(
            "SELECT request_id, display_name FROM pairing
             WHERE state='pending' AND client_type='browser' AND comparison_code=?1 AND expires_ms > ?2",
        )?;
        let rows: Vec<(String, String)> = st
            .query_map(params![code, now], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        match rows.as_slice() {
            [(id, name)] => {
                c.execute(
                    "UPDATE pairing SET state='approved_admin' WHERE request_id=?1",
                    [id],
                )?;
                Ok((id.clone(), name.clone()))
            }
            [] => Err(ControlError::NotFound),
            _ => Err(ControlError::Conflict("ambiguous")),
        }
    }

    pub fn decide_pairing(&self, request_id: &str, approve: bool) -> R<(ClientType, String)> {
        let c = self.conn.lock().unwrap();
        let now = self.now();
        let row: Option<(String, i64, String, String)> = c
            .query_row(
                "SELECT state, expires_ms, client_type, display_name FROM pairing WHERE request_id=?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((state, expires, ct, name)) = row else {
            return Err(ControlError::NotFound);
        };
        if state != "pending" {
            return Err(ControlError::Conflict("not_pending"));
        }
        if (expires as u64) <= now {
            c.execute(
                "UPDATE pairing SET state='expired' WHERE request_id=?1",
                [request_id],
            )?;
            return Err(ControlError::Conflict("expired"));
        }
        c.execute(
            "UPDATE pairing SET state=?2 WHERE request_id=?1",
            params![request_id, if approve { "approved" } else { "denied" }],
        )?;
        Ok((parse_client_type(&ct), name))
    }

    pub fn pending(&self) -> R<Vec<PendingRequest>> {
        let c = self.conn.lock().unwrap();
        let now = self.now();
        let mut st = c.prepare(
            "SELECT request_id, client_type, display_name, comparison_code, expires_ms
             FROM pairing WHERE state='pending' AND expires_ms > ?1 ORDER BY created_ms",
        )?;
        let rows = st.query_map([now as i64], |r| {
            let ct: String = r.get(1)?;
            let exp: i64 = r.get(4)?;
            Ok(PendingRequest {
                request_id: r.get(0)?,
                client_type: parse_client_type(&ct),
                display_name: r.get(2)?,
                comparison_code: r.get(3)?,
                expires_in_s: (exp as u64).saturating_sub(now) / 1000,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn devices(&self, current: &str) -> R<Vec<DeviceRecord>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT device_id, client_type, display_name, role, created_ms, last_seen_ms, revoked_ms
             FROM devices ORDER BY created_ms",
        )?;
        let rows = st.query_map([], |r| {
            let id: String = r.get(0)?;
            let ct: String = r.get(1)?;
            let role: String = r.get(3)?;
            let created: i64 = r.get(4)?;
            let seen: Option<i64> = r.get(5)?;
            let revoked: Option<i64> = r.get(6)?;
            Ok(DeviceRecord {
                current: id == current,
                device_id: id,
                client_type: parse_client_type(&ct),
                display_name: r.get(2)?,
                role: parse_role(&role),
                created_ms: created as u64,
                last_seen_ms: seen.map(|v| v as u64),
                revoked: revoked.is_some(),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Revoke one device's credential. Only that record is affected.
    pub fn revoke(&self, device_id: &str) -> R<()> {
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "UPDATE devices SET revoked_ms = ?2 WHERE device_id = ?1 AND revoked_ms IS NULL",
            params![device_id, self.now() as i64],
        )?;
        if n == 0 {
            let exists: Option<i64> = c
                .query_row(
                    "SELECT 1 FROM devices WHERE device_id=?1",
                    [device_id],
                    |r| r.get(0),
                )
                .optional()?;
            return if exists.is_some() {
                Err(ControlError::Conflict("already_revoked"))
            } else {
                Err(ControlError::NotFound)
            };
        }
        Ok(())
    }

    pub fn counts(&self) -> R<(u64, u64)> {
        let c = self.conn.lock().unwrap();
        let devices: i64 = c.query_row(
            "SELECT COUNT(*) FROM devices WHERE revoked_ms IS NULL",
            [],
            |r| r.get(0),
        )?;
        let pending: i64 = c.query_row(
            "SELECT COUNT(*) FROM pairing WHERE state='pending' AND expires_ms > ?1",
            [self.now() as i64],
            |r| r.get(0),
        )?;
        Ok((devices as u64, pending as u64))
    }
}

fn insert_device(
    c: &Connection,
    now: u64,
    ct: ClientType,
    name: &str,
    role: Role,
    client_id: Option<&str>,
) -> R<Issued> {
    let device_id = new_id(now);
    let mut secret = [0u8; CREDENTIAL_SECRET_BYTES];
    random_bytes(&mut secret).map_err(ControlError::Storage)?;
    c.execute(
        "INSERT INTO devices(device_id, client_type, display_name, role, verifier, created_ms, client_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            device_id,
            client_type_str(ct),
            name,
            role_str(role),
            verifier(&secret).to_vec(),
            now as i64,
            client_id,
        ],
    )?;
    Ok(Issued {
        credential: Credential::new(&device_id, secret),
        device_id,
    })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}
