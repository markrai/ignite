//! Small portable helpers: time, CSPRNG, restrictive-permission files.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn random_bytes(buf: &mut [u8]) -> Result<(), String> {
    getrandom::fill(buf).map_err(|e| format!("OS randomness unavailable: {e}"))
}

pub fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    random_bytes(&mut b).expect("OS randomness");
    u32::from_le_bytes(b)
}

/// Create `dir` (and parents) owner-only (0700 on Unix).
pub fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("chmod {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// Write `bytes` to `path` atomically (temp + fsync + rename), owner-only (0600 on Unix).
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

/// Restrict an existing file to its owner (0600 on Unix; no-op elsewhere).
pub fn restrict_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}
