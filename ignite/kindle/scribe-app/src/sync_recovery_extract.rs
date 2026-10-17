//! Read-only extraction of checkpoint, outbox, and local snapshot for recovery rehearsal.
//!
//! Never calls `OutboundSync::open`, `ConfirmedLog::open`, `ensure_sync_identity`,
//! `persist_planned_identity`, or `build_snapshot` (the last may write `page.meta`).

use std::fs;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;
use sync_engine::LibraryState;
use sync_protocol::{
    from_json, sha256_hex, snapshot_sha256, LibraryId, ReplicaId, SyncEnvelopeV1, SyncSnapshotV1,
};

use crate::sync_local::{build_snapshot_read_only, plan_sync_identity};
use crate::sync_outbound::confirmed::{self, CONFIRMED_LOG_FILE};
use crate::sync_outbound::{parse_journal, SyncStateV1, CONFIG_FILE, OUTBOX_FILE, STATE_FILE};
use crate::sync_recovery_plan::{sim_env, OutboxInput, SequenceAuthority};

#[derive(Clone, Debug, Serialize)]
pub struct ExtractTimingsMs {
    pub checkpoint: u128,
    pub snapshot: u128,
    pub outbox: u128,
}

#[derive(Clone, Debug)]
pub struct ExtractedHistory {
    pub library_id: LibraryId,
    pub replica_id: ReplicaId,
    pub checkpoint: LibraryState,
    pub checkpoint_cursor: u64,
    pub acked_client_seq: u64,
    pub next_client_seq: u64,
    pub local: SyncSnapshotV1,
    pub pending: Vec<SyncEnvelopeV1>,
    pub sequence_authority: SequenceAuthority,
    pub outbox_invalid: Option<String>,
    pub identity_changed: bool,
    pub identity_allocated_pages: usize,
    pub identity_allocated_notebooks: usize,
    pub pairing_present: bool,
    pub confirmed_log_bytes: u64,
    pub outbox_bytes: u64,
    pub timings: ExtractTimingsMs,
    pub checkpoint_snapshot_sha256: String,
    pub local_snapshot_sha256: String,
}

impl ExtractedHistory {
    pub fn outbox_input(&self) -> OutboxInput<'_> {
        match &self.outbox_invalid {
            Some(detail) => OutboxInput::Invalid { detail },
            None => OutboxInput::Valid {
                envelopes: &self.pending,
            },
        }
    }
}

/// Load historical sync + library state using only read-only production parsers.
pub fn extract_read_only(library_root: &Path, sync_dir: &Path) -> Result<ExtractedHistory, String> {
    let pairing_present = sync_dir.join(CONFIG_FILE).is_file();
    let t0 = Instant::now();
    let (mut checkpoint, checkpoint_cursor) = load_checkpoint_read_only(sync_dir)?;
    let confirmed_path = sync_dir.join(CONFIRMED_LOG_FILE);
    let confirmed_log_bytes = if confirmed_path.is_file() {
        fs::metadata(&confirmed_path).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    if confirmed_log_bytes > 0 {
        confirmed::replay_log(sync_dir, &mut checkpoint, checkpoint_cursor, &|bytes| {
            from_json(bytes).map_err(|e| e.to_string())
        })?;
    }
    let checkpoint_ms = t0.elapsed().as_millis();

    let state_bytes =
        fs::read(sync_dir.join(STATE_FILE)).map_err(|e| format!("read sync-state.json: {e}"))?;
    let st: SyncStateV1 =
        serde_json::from_slice(&state_bytes).map_err(|e| format!("sync-state.json: {e}"))?;
    if st.library_id != checkpoint.snapshot.library_id {
        return Err("sync-state library_id does not match checkpoint".into());
    }

    let t1 = Instant::now();
    let outbox_path = sync_dir.join(OUTBOX_FILE);
    let outbox_bytes = if outbox_path.is_file() {
        fs::metadata(&outbox_path).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    let raw = if outbox_path.is_file() {
        fs::read(&outbox_path).map_err(|e| format!("read outbox: {e}"))?
    } else {
        Vec::new()
    };
    let (pending, outbox_invalid) = match parse_journal(&raw) {
        Ok(j) if j.trailing_prep.is_some() => {
            (Vec::new(), Some("unresolved trailing prep".to_string()))
        }
        Ok(j) if j.torn_tail => (Vec::new(), Some("torn outbox tail".to_string())),
        Ok(j) => (j.committed.into_iter().map(|p| p.envelope).collect(), None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let outbox_ms = t1.elapsed().as_millis();

    let sequence_authority = if outbox_invalid.is_some() {
        SequenceAuthority::Unknown
    } else {
        SequenceAuthority::Known {
            next_client_seq: st.next_client_seq,
        }
    };

    let t2 = Instant::now();
    let (_existing, plan) = plan_sync_identity(library_root)?;
    let local = build_snapshot_read_only(library_root, &plan.identity)?;
    let snapshot_ms = t2.elapsed().as_millis();

    if local.library_id != checkpoint.snapshot.library_id {
        return Err("local snapshot library_id does not match checkpoint".into());
    }

    let checkpoint_snapshot_sha256 = snapshot_sha256(&checkpoint.snapshot)
        .map(|h| h.to_string())
        .unwrap_or_default();
    let local_snapshot_sha256 = snapshot_sha256(&local)
        .map(|h| h.to_string())
        .unwrap_or_default();

    Ok(ExtractedHistory {
        library_id: st.library_id,
        replica_id: st.replica_id,
        checkpoint,
        checkpoint_cursor,
        acked_client_seq: st.acked_client_seq,
        next_client_seq: st.next_client_seq,
        local,
        pending,
        sequence_authority,
        outbox_invalid,
        identity_changed: plan.changed,
        identity_allocated_pages: plan.allocated_pages.len(),
        identity_allocated_notebooks: plan.allocated_notebooks.len(),
        pairing_present,
        confirmed_log_bytes,
        outbox_bytes,
        timings: ExtractTimingsMs {
            checkpoint: checkpoint_ms,
            snapshot: snapshot_ms,
            outbox: outbox_ms,
        },
        checkpoint_snapshot_sha256,
        local_snapshot_sha256,
    })
}

fn load_checkpoint_read_only(sync_dir: &Path) -> Result<(LibraryState, u64), String> {
    match confirmed::load_checkpoint(sync_dir)? {
        Some((state, through, _)) => Ok((state, through)),
        None => Err("missing required checkpoint".into()),
    }
}

/// SHA-256 of a file if it exists; `None` if missing. Read-only.
pub fn file_sha256(path: &Path) -> Result<Option<String>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(sha256_hex(&bytes).to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("hash {}: {e}", path.display())),
    }
}

pub fn apply_candidates(
    start: &LibraryState,
    library: LibraryId,
    replica: ReplicaId,
    ops: &[crate::sync_recovery_plan::CandidateOp],
) -> Result<LibraryState, String> {
    let mut state = start.clone();
    for (i, c) in ops.iter().enumerate() {
        let env = sim_env(library, replica, i as u64 + 1, c.operation.clone());
        match state.apply(&env) {
            Ok((next, _)) => state = next,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    use kindle_platform::FbGeometry;
    use sync_protocol::sha256_hex;

    use crate::ink::InkRequest;
    use crate::library::open_or_create_library;
    use crate::store::{page_dir, page_meta_path_for_test};
    use crate::sync_local::{build_snapshot_read_only, ensure_sync_identity, read_identity};
    use crate::ui_gfx::paper_background;

    fn tmp(label: &str) -> std::path::PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scribe-extract-{label}-{n}"));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn geo() -> FbGeometry {
        FbGeometry {
            xres: 200,
            yres: 160,
            xres_virtual: 208,
            yres_virtual: 160,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: 8,
            line_length: 208,
            smem_len: 208 * 160,
        }
    }

    #[test]
    fn missing_checkpoint_is_refused() {
        let root = tmp("no-cp");
        let lib = root.join("lib");
        let sync = root.join("sync");
        std::fs::create_dir_all(&sync).unwrap();
        let err = extract_read_only(&lib, &sync).unwrap_err();
        assert!(err.contains("missing required checkpoint"), "{err}");
    }

    #[test]
    fn empty_outbox_parses_as_valid() {
        let parsed = crate::sync_outbound::parse_journal(b"").unwrap();
        assert!(parsed.committed.is_empty());
        assert!(parsed.trailing_prep.is_none());
        assert!(!parsed.torn_tail);
    }

    #[test]
    fn read_only_snapshot_does_not_write_page_meta() {
        let lib = tmp("ro-snap").join("library");
        let g = geo();
        open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
        ensure_sync_identity(&lib).unwrap();
        let identity = read_identity(&lib).unwrap().unwrap();
        let meta = page_meta_path_for_test(&page_dir(
            &lib.join("notebooks").join("notebook-1"),
            "page-1",
        ));
        let before = std::fs::read(&meta).unwrap();
        let snap = build_snapshot_read_only(&lib, &identity).unwrap();
        assert_eq!(snap.notebooks.len(), 1);
        assert_eq!(std::fs::read(&meta).unwrap(), before);
        assert_eq!(
            sha256_hex(&before).to_string(),
            file_sha256(&meta).unwrap().unwrap()
        );
    }
}
