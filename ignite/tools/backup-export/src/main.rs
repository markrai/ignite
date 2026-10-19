//! `ignite-backup-export --server http://HOST:PORT --out FILE.zip`
//!
//! Credentials come only from the environment, so they never appear in the process list or
//! shell history: `IGNITE_BACKUP_BEARER` (a device credential) or `IGNITE_BACKUP_SESSION`
//! (the value of a browser `scribe_session` cookie). They are never printed or saved.

use std::path::PathBuf;
use std::process::ExitCode;

use backup_export::{export_backup, Credential, ExportErrorKind, HttpSource, Secret};
use portable_backup::Limits;

const USAGE: &str = "usage: ignite-backup-export --server http://HOST:PORT --out FILE.zip
credential (environment only): IGNITE_BACKUP_BEARER=<device credential>
                           or  IGNITE_BACKUP_SESSION=<scribe_session cookie value>";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("ignite-backup-export: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut server = None;
    let mut out = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--server" => server = args.next(),
            "--out" => out = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    let (Some(server), Some(out)) = (server, out) else {
        return Err(USAGE.to_string());
    };
    let bearer = std::env::var("IGNITE_BACKUP_BEARER")
        .ok()
        .filter(|s| !s.is_empty());
    let session = std::env::var("IGNITE_BACKUP_SESSION")
        .ok()
        .filter(|s| !s.is_empty());
    let credential = match (bearer, session) {
        (Some(_), Some(_)) => {
            return Err("set only one of IGNITE_BACKUP_BEARER / IGNITE_BACKUP_SESSION".into())
        }
        (Some(b), None) => Some(Credential::Bearer(Secret::new(b))),
        (None, Some(s)) => Some(Credential::SessionCookie(Secret::new(s))),
        (None, None) => None,
    };
    let source = HttpSource::new(&server, credential).map_err(|e| e.to_string())?;
    match export_backup(&source, &out, &Limits::DEFAULT) {
        Ok(r) => {
            println!(
                "exported libraries={} blobs={} bytes={} sha256={} path={}",
                r.manifest.libraries.len(),
                r.manifest.blobs.len(),
                r.archive_bytes,
                r.archive_sha256,
                r.path.display()
            );
            for l in &r.manifest.libraries {
                println!(
                    "library={} through_server_seq={} notebooks={} pages={} snapshot_sha256={}",
                    l.source_library_id,
                    l.source_through_server_seq,
                    l.notebook_count,
                    l.page_count,
                    l.source_snapshot_sha256
                );
            }
            Ok(())
        }
        Err(e) if e.kind == ExportErrorKind::Unauthorized => Err(format!(
            "{}\n(set IGNITE_BACKUP_BEARER or IGNITE_BACKUP_SESSION for this server)",
            e.message
        )),
        Err(e) => Err(e.to_string()),
    }
}
