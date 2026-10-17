//! `scribe-sync-tool`: inspection of a Scribe library's sync identity and semantic snapshot
//! (Sync Phase S1), plus the S2B `compare` diagnostic (the only command that talks to a server).
//!
//! Writes are limited to `<library>/.scribe-sync/identity.json`, `<replica-state-dir>/replica.json`
//! and the requested `--out` snapshot file. Notebook/page content is only ever read; `compare`
//! writes nothing at all.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use scribe_app::sync_local::{
    build_snapshot, ensure_replica_state, ensure_sync_identity, snapshot_bytes_and_hash,
};
use sync_protocol::{snapshot_sha256, SyncSnapshotV1};

pub const USAGE: &str = "\
scribe-sync-tool — Scribe sync identity + semantic snapshot

Usage:
  scribe-sync-tool identity --data-dir <library> --replica-state-dir <dir>
  scribe-sync-tool snapshot --data-dir <library> --replica-state-dir <dir> --out <snapshot.json>
  scribe-sync-tool validate-snapshot <snapshot.json>
  scribe-sync-tool compare --data-dir <library> --sync-dir <dir>
  scribe-sync-tool remote-op --sync-dir <dir> --replica-state-dir <dir> --confirm-write <op> [args]

identity           validate the library (read-only), ensure global identities
                   (<library>/.scribe-sync/identity.json), ensure replica state
                   (<dir>/replica.json), print all bindings
snapshot           as identity, then write the canonical SyncSnapshotV1 and print its SHA-256
validate-snapshot  strict parse + validation; prints SHA-256 and whether bytes are canonical
compare            S2B: local snapshot hash vs the server's (URL + token from <dir>/config.json,
                   never printed); prints server_seq and MATCH/DIFFER (DIFFER exits nonzero).
                   Strictly read-only: identity is read, never minted.
remote-op          DEVELOPER-ONLY, DANGEROUS: act as a SECOND replica (its own ReplicaId, kept in
                   <replica-state-dir>/dev-replica.json) and submit ONE canonical operation to the
                   server in <dir>/config.json — to prove Kindle inbound sync before any browser
                   editing exists. Requires --confirm-write. Scratch libraries only. Ops:
                     add-stroke     --notebook-index N --page-index P [--y Y]
                     erase          --notebook-index N --page-index P   (cuts the last stroke)
                     delete-lineage --notebook-index N --page-index P   (last stroke's lineage)
                     rename         --notebook-index N --title TITLE
                     create-page    --notebook-index N [--after-page-index P]
                     move-page      --notebook-index N --page-index P [--after-page-index Q|--first]
                     delete-page    --notebook-index N --page-index P

The replica state dir must be outside the library (Kindle: /mnt/us/scribe-app/state).
";

fn flag(args: &[String], name: &str) -> Result<PathBuf, String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("{name} needs a value"));
        }
    }
    Err(format!("missing {name}\n\n{USAGE}"))
}

fn check_flags(args: &[String], allowed: &[&str]) -> Result<(), String> {
    let mut i = 0;
    while i < args.len() {
        if !allowed.contains(&args[i].as_str()) {
            return Err(format!("unexpected argument {:?}\n\n{USAGE}", args[i]));
        }
        i += 2;
    }
    Ok(())
}

fn is_within(child: &Path, parent: &Path) -> bool {
    let c = child
        .parent()
        .and_then(|p| fs::canonicalize(p).ok())
        .map(|p| p.join(child.file_name().unwrap_or_default()));
    match (c, fs::canonicalize(parent)) {
        (Some(c), Ok(p)) => c.starts_with(p),
        _ => false,
    }
}

/// Run one command; returns the text to print on success.
pub fn run(args: &[String]) -> Result<String, String> {
    let Some(cmd) = args.first() else {
        return Err(USAGE.to_string());
    };
    let rest = &args[1..];
    match cmd.as_str() {
        "identity" => {
            check_flags(rest, &["--data-dir", "--replica-state-dir"])?;
            identity(
                &flag(rest, "--data-dir")?,
                &flag(rest, "--replica-state-dir")?,
            )
        }
        "snapshot" => {
            check_flags(rest, &["--data-dir", "--replica-state-dir", "--out"])?;
            snapshot(
                &flag(rest, "--data-dir")?,
                &flag(rest, "--replica-state-dir")?,
                &flag(rest, "--out")?,
            )
        }
        "compare" => {
            check_flags(rest, &["--data-dir", "--sync-dir"])?;
            compare(&flag(rest, "--data-dir")?, &flag(rest, "--sync-dir")?)
        }
        "remote-op" => dev_remote::run(rest),
        "validate-snapshot" => match rest {
            [path] => validate_snapshot(Path::new(path)),
            _ => Err(format!("validate-snapshot takes one file\n\n{USAGE}")),
        },
        "-h" | "--help" | "help" => Ok(USAGE.to_string()),
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    }
}

fn identity(data_dir: &Path, state_dir: &Path) -> Result<String, String> {
    let replica = ensure_replica_state(state_dir, Some(data_dir))?;
    let r = ensure_sync_identity(data_dir)?;
    let id = &r.identity;
    let mut out = String::new();
    let _ = writeln!(out, "library_id={}", id.library_id);
    let _ = writeln!(out, "replica_id={}", replica.replica_id);
    let _ = writeln!(
        out,
        "identity_written={} allocated_library={} allocated_notebooks={} allocated_pages={} retired_notebooks={} retired_pages={}",
        r.changed,
        r.allocated_library,
        r.allocated_notebooks.len(),
        r.allocated_pages.len(),
        r.retired_notebooks.len(),
        r.retired_pages.len()
    );
    for nb in id.notebook_bindings() {
        let _ = writeln!(out, "notebook {} -> {}", nb.local_id, nb.sync_id);
        for (page_local, page_id) in &id.pages[&nb.local_id] {
            let _ = writeln!(out, "  page {}/{} -> {}", nb.local_id, page_local, page_id);
        }
    }
    for (nid, rn) in &id.retired_notebooks {
        let _ = writeln!(out, "retired notebook {} (was {})", nid, rn.local_id);
    }
    for (pid, rp) in &id.retired_pages {
        let _ = writeln!(
            out,
            "retired page {} (was {} in notebook {})",
            pid, rp.local_id, rp.notebook_id
        );
    }
    Ok(out)
}

fn snapshot(data_dir: &Path, state_dir: &Path, out_path: &Path) -> Result<String, String> {
    if is_within(out_path, data_dir) {
        return Err(format!(
            "REFUSE: --out {} is inside the library; exports never write into notebook data",
            out_path.display()
        ));
    }
    let mut out = identity(data_dir, state_dir)?;
    let id = scribe_app::sync_local::read_identity(data_dir)?
        .ok_or("internal: identity missing after ensure")?;
    let snap = build_snapshot(data_dir, &id)?;
    let (bytes, hash) = snapshot_bytes_and_hash(&snap)?;
    let tmp = out_path.with_extension("json.tmp");
    fs::write(&tmp, &bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, out_path).map_err(|e| format!("rename {}: {e}", out_path.display()))?;
    let pages: usize = snap.notebooks.iter().map(|n| n.pages.len()).sum();
    let strokes: usize = snap
        .notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .map(|p| p.strokes.len())
        .sum();
    let _ = writeln!(
        out,
        "snapshot_written={} bytes={} notebooks={} pages={} visible_strokes={}",
        out_path.display(),
        bytes.len(),
        snap.notebooks.len(),
        pages,
        strokes
    );
    let _ = writeln!(out, "snapshot_sha256={hash}");
    Ok(out)
}

fn validate_snapshot(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let snap = SyncSnapshotV1::parse(&bytes).map_err(|e| format!("INVALID SNAPSHOT: {e}"))?;
    let canonical = snap.to_canonical_bytes().map_err(|e| e.to_string())?;
    let hash = snapshot_sha256(&snap).map_err(|e| e.to_string())?;
    Ok(format!(
        "valid=true version={} library_id={} notebooks={} canonical_bytes={}\nsnapshot_sha256={hash}\n",
        snap.v,
        snap.library_id,
        snap.notebooks.len(),
        canonical == bytes
    ))
}

fn compare(data_dir: &Path, sync_dir: &Path) -> Result<String, String> {
    let id = scribe_app::sync_local::read_identity(data_dir)?
        .ok_or("REFUSE COMPARE: library has no sync identity (never synced)")?;
    let local = build_snapshot(data_dir, &id)?;
    let local_hash = snapshot_sha256(&local).map_err(|e| e.to_string())?;
    let cfg = scribe_app::sync_outbound::read_config(sync_dir)?;
    let (server_seq, server_hash) =
        scribe_app::sync_outbound::fetch_server_snapshot_hash(&cfg, &id.library_id)?;
    let verdict = if server_hash == local_hash.as_str() {
        "MATCH"
    } else {
        "DIFFER"
    };
    let out = format!(
        "library_id={}\nlocal_snapshot_sha256={local_hash}\nserver_snapshot_sha256={server_hash}\nserver_seq={server_seq}\nresult={verdict}\n",
        id.library_id
    );
    if verdict == "MATCH" {
        Ok(out)
    } else {
        Err(out)
    }
}

/// Developer-only second replica (S2C physical acceptance). Never an end-user feature.
mod dev_remote {
    use std::path::{Path, PathBuf};

    use scribe_app::sync_outbound::{http_request, read_config, SyncConfigV1};
    use serde_json::Value;
    use sync_protocol::{
        BackgroundRef, LibraryId, NotebookId, OperationId, PageId, PageSpec, ReplicaId, StrokeId,
        StrokeStyle, SyncEnvelopeV1, SyncOperationV1, SyncPoint, SyncSnapshotV1, SyncStroke,
        ENVELOPE_VERSION,
    };
    use uuid::Uuid;

    const STATE: &str = "dev-replica.json";
    const ERASER_RADIUS: f32 = 160.0;

    fn opt(args: &[String], name: &str) -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    }

    fn req(args: &[String], name: &str) -> Result<String, String> {
        opt(args, name).ok_or_else(|| format!("missing {name}"))
    }

    fn idx(args: &[String], name: &str) -> Result<usize, String> {
        req(args, name)?
            .parse()
            .map_err(|_| format!("{name} must be a 0-based index"))
    }

    fn json(cfg: &SyncConfigV1, method: &str, path: &str, body: &[u8]) -> Result<Value, String> {
        let (status, b) = http_request(cfg, method, path, body)?;
        if !(200..300).contains(&status) {
            return Err(format!(
                "server {status} for {method} {path}: {}",
                String::from_utf8_lossy(&b)
            ));
        }
        serde_json::from_slice(&b).map_err(|e| e.to_string())
    }

    struct DevReplica {
        path: PathBuf,
        replica: ReplicaId,
        next: u64,
    }

    fn load_replica(dir: &Path, library: LibraryId) -> Result<DevReplica, String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let path = dir.join(STATE);
        if let Ok(b) = std::fs::read(&path) {
            let v: Value = serde_json::from_slice(&b).map_err(|e| e.to_string())?;
            if v["library_id"].as_str() != Some(&library.to_string()) {
                return Err("dev replica state belongs to another library".into());
            }
            return Ok(DevReplica {
                path,
                replica: ReplicaId::parse(v["replica_id"].as_str().unwrap_or_default())
                    .map_err(|e| e.to_string())?,
                next: v["next_client_seq"]
                    .as_u64()
                    .ok_or("bad dev replica state")?,
            });
        }
        Ok(DevReplica {
            path,
            replica: ReplicaId::from_uuid(Uuid::now_v7()).map_err(|e| e.to_string())?,
            next: 1,
        })
    }

    fn save_replica(r: &DevReplica, library: LibraryId) -> Result<(), String> {
        let v = serde_json::json!({
            "v": 1,
            "library_id": library.to_string(),
            "replica_id": r.replica.to_string(),
            "next_client_seq": r.next,
        });
        std::fs::write(&r.path, serde_json::to_vec_pretty(&v).unwrap()).map_err(|e| e.to_string())
    }

    fn big_stroke(y: f32) -> SyncStroke {
        let id = StrokeId::from_uuid(Uuid::now_v7()).unwrap();
        // A wide, obvious wave from light to firm pressure.
        let points = (0..60)
            .map(|i| {
                let t = i as f32 / 59.0;
                SyncPoint::new(
                    1200.0 + 13_000.0 * t,
                    (y + 900.0 * (t * std::f32::consts::TAU * 1.5).sin()).clamp(0.0, 20_832.0),
                    Some(0.15 + 0.8 * t),
                )
            })
            .collect();
        SyncStroke {
            stroke_id: id,
            root_id: id,
            style: StrokeStyle::PressureWidthV1,
            points,
        }
    }

    pub fn run(args: &[String]) -> Result<String, String> {
        let op_name = args.first().cloned().unwrap_or_default();
        if !args.iter().any(|a| a == "--confirm-write") {
            return Err("REFUSE: remote-op writes to the server ledger as a second replica; pass --confirm-write (scratch libraries only)".into());
        }
        let cfg = read_config(Path::new(&req(args, "--sync-dir")?))?;
        let libs = json(&cfg, "GET", "/v1/libraries", b"")?;
        let library = match opt(args, "--library") {
            Some(l) => LibraryId::parse(&l).map_err(|e| e.to_string())?,
            None => {
                let arr = libs["libraries"].as_array().cloned().unwrap_or_default();
                if arr.len() != 1 {
                    return Err(format!(
                        "server has {} libraries: pass --library",
                        arr.len()
                    ));
                }
                LibraryId::parse(arr[0]["library_id"].as_str().unwrap_or_default())
                    .map_err(|e| e.to_string())?
            }
        };
        let snap_v = json(
            &cfg,
            "GET",
            &format!("/v1/libraries/{library}/snapshot"),
            b"",
        )?;
        let snap: SyncSnapshotV1 =
            serde_json::from_value(snap_v["snapshot"].clone()).map_err(|e| e.to_string())?;
        let nbi = idx(args, "--notebook-index")?;
        let nb = snap.notebooks.get(nbi).ok_or("no such notebook index")?;
        let page = |key: &str| -> Result<&sync_protocol::SnapshotPage, String> {
            let i = idx(args, key)?;
            nb.pages
                .get(i)
                .ok_or_else(|| format!("no page at {key} {i}"))
        };
        let nid: NotebookId = nb.notebook_id;
        let operation = match op_name.as_str() {
            "add-stroke" => {
                let p = page("--page-index")?;
                let y: f32 = opt(args, "--y")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10_000.0);
                SyncOperationV1::AddStroke {
                    notebook_id: nid,
                    page_id: p.page_id,
                    stroke: big_stroke(y),
                }
            }
            "erase" | "delete-lineage" => {
                let p = page("--page-index")?;
                let last = p.strokes.last().ok_or("page has no strokes")?;
                if op_name == "delete-lineage" {
                    SyncOperationV1::DeleteStrokeLineage {
                        notebook_id: nid,
                        page_id: p.page_id,
                        root_id: last.root_id,
                    }
                } else {
                    let mid = &last.points[last.points.len() / 2];
                    let mut roots: Vec<StrokeId> = p.strokes.iter().map(|s| s.root_id).collect();
                    roots.sort();
                    roots.dedup();
                    let (y0, y1) = ((mid.y - 1500.0).max(0.0), (mid.y + 1500.0).min(20_832.0));
                    SyncOperationV1::EraseRegion {
                        notebook_id: nid,
                        page_id: p.page_id,
                        observed_roots: roots,
                        path: vec![
                            SyncPoint::new(mid.x, y0, None),
                            SyncPoint::new(mid.x, y0, None),
                            SyncPoint::new(mid.x, y1, None),
                        ],
                        radius: ERASER_RADIUS,
                    }
                }
            }
            "rename" => SyncOperationV1::RenameNotebook {
                notebook_id: nid,
                title: req(args, "--title")?,
            },
            "create-page" => {
                let after = match opt(args, "--after-page-index") {
                    Some(_) => page("--after-page-index")?,
                    None => nb.pages.last().unwrap(),
                };
                SyncOperationV1::CreatePage {
                    notebook_id: nid,
                    page: PageSpec {
                        page_id: PageId::from_uuid(Uuid::now_v7()).map_err(|e| e.to_string())?,
                        background: BackgroundRef::Blank,
                        render: after.render.clone(),
                    },
                    after_page_id: Some(after.page_id),
                }
            }
            "move-page" => SyncOperationV1::MovePage {
                notebook_id: nid,
                page_id: page("--page-index")?.page_id,
                after_page_id: if args.iter().any(|a| a == "--first") {
                    None
                } else {
                    Some(page("--after-page-index")?.page_id)
                },
            },
            "delete-page" => SyncOperationV1::DeletePage {
                notebook_id: nid,
                page_id: page("--page-index")?.page_id,
            },
            other => return Err(format!("unknown remote-op {other:?}")),
        };
        let mut r = load_replica(Path::new(&req(args, "--replica-state-dir")?), library)?;
        let env = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: OperationId::from_uuid(Uuid::now_v7()).map_err(|e| e.to_string())?,
            library_id: library,
            replica_id: r.replica,
            client_seq: r.next,
            operation,
        };
        let bytes = env.to_canonical_bytes().map_err(|e| e.to_string())?;
        let mut body = b"{\"envelopes\":[".to_vec();
        body.extend(&bytes);
        body.extend(b"]}");
        let ack = json(
            &cfg,
            "POST",
            &format!("/v1/libraries/{library}/operations"),
            &body,
        )?;
        r.next += 1;
        save_replica(&r, library)?;
        Ok(format!(
            "remote_op={op_name} replica={} client_seq={} operation_id={} server_seq={} head={}\n",
            r.replica,
            env.client_seq,
            env.operation_id,
            ack["results"][0]["server_seq"],
            ack["head_server_seq"]
        ))
    }
}
