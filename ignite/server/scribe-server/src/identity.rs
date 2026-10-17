//! Durable server identity: `<state_dir>/server.json` = `{"v":1,"server_id":…,"created_ms":…}`.
//!
//! Generated once from OS randomness on first product start (or `scribe-server init`) and never
//! regenerated: it survives restarts, reboots, IP changes and upgrades (the installer never
//! touches the state directory's contents). Hostname and IP are *not* identity.

use std::path::{Path, PathBuf};

use control_protocol::ServerId;
use serde::{Deserialize, Serialize};

use crate::fsutil::{now_ms, random_bytes, write_private_atomic};

pub const IDENTITY_FILE: &str = "server.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerIdentity {
    pub v: u32,
    pub server_id: ServerId,
    pub created_ms: u64,
}

pub fn identity_path(state_dir: &Path) -> PathBuf {
    state_dir.join(IDENTITY_FILE)
}

/// Load the identity, creating it (once) if absent. A present-but-invalid file is an error:
/// never silently replace an identity.
pub fn load_or_create(state_dir: &Path) -> Result<(ServerIdentity, bool), String> {
    let path = identity_path(state_dir);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let id: ServerIdentity = serde_json::from_slice(&bytes).map_err(|e| {
                format!(
                    "{} is invalid (refusing to replace it): {e}",
                    path.display()
                )
            })?;
            if id.v != 1 {
                return Err(format!("{}: unsupported version {}", path.display(), id.v));
            }
            Ok((id, false))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut r = [0u8; 10];
            random_bytes(&mut r)?;
            let now = now_ms();
            let id = ServerIdentity {
                v: 1,
                server_id: ServerId::from_parts(now, &r),
                created_ms: now,
            };
            let mut text = serde_json::to_vec_pretty(&id).map_err(|e| e.to_string())?;
            text.push(b'\n');
            write_private_atomic(&path, &text)?;
            Ok((id, true))
        }
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}
