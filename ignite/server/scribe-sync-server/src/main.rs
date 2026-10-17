use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use scribe_sync_server::{
    generate_token, ServerConfig, ServerHandle, Store, TokenVerifier, WebRoot,
};

const USAGE: &str = "\
scribe-sync-server — Scribe sync ledger (S2A/S2B/S3A)

Usage:
  scribe-sync-server --data-dir <dir> [--bind 127.0.0.1:8787] [--token-verifier-file <file>]
                     [--web-root <web/scribe-web/dist>] [--accept-replace-stroke-lineages]
  scribe-sync-server new-token --verifier-out <file>

serve       default bind is loopback. A non-loopback bind (e.g. --bind 0.0.0.0:8787) REFUSES to
            start unless --token-verifier-file is given; with it, every /v1 request needs
            `Authorization: Bearer <token>`. Plain HTTP: trusted LAN only (no TLS in S2B).
--web-root  S3A: also serve the read-only Scribe Web bundle for non-/v1 paths (same origin, no
            CORS). Must contain index.html and must not overlap the ledger or verifier.
--accept-replace-stroke-lineages
            opt-in: persist new ReplaceStrokeLineages envelopes. Default off so installing
            this binary cannot add history older clients cannot decode. Once any such
            operation is accepted, rolling the server or clients back is unsafe. Do not
            enable until every active replica can decode the type.
new-token   prints a fresh 256-bit token ONCE (give it to the Kindle sync config) and writes
            only its SHA-256 verifier to <file> for the server.
";

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.first().map(String::as_str) == Some("new-token") {
        let Some(out) = arg(&args, "--verifier-out").map(PathBuf::from) else {
            eprint!("missing --verifier-out\n\n{USAGE}");
            return ExitCode::FAILURE;
        };
        let token = match generate_token() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        };
        let verifier = TokenVerifier::for_token(&token);
        if let Err(e) = std::fs::write(&out, format!("{}\n", verifier.to_hex())) {
            eprintln!("write {}: {e}", out.display());
            return ExitCode::FAILURE;
        }
        println!("{token}");
        eprintln!(
            "verifier written to {} (the token above is not stored anywhere)",
            out.display()
        );
        return ExitCode::SUCCESS;
    }
    let Some(data_dir) = arg(&args, "--data-dir").map(PathBuf::from) else {
        eprint!("missing --data-dir\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    let bind: SocketAddr = match arg(&args, "--bind")
        .unwrap_or_else(|| "127.0.0.1:8787".into())
        .parse()
    {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bad --bind: {e}");
            return ExitCode::FAILURE;
        }
    };
    let auth = match arg(&args, "--token-verifier-file").map(PathBuf::from) {
        None => None,
        Some(p) => match TokenVerifier::load(&p) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        },
    };
    let authed = auth.is_some();
    let web_root = match arg(&args, "--web-root").map(PathBuf::from) {
        None => None,
        Some(p) => {
            let verifier = arg(&args, "--token-verifier-file").map(PathBuf::from);
            let mut forbidden = vec![data_dir.as_path()];
            if let Some(v) = verifier.as_deref() {
                forbidden.push(v);
            }
            match WebRoot::new(&p, &forbidden) {
                Ok(w) => Some(w),
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    let web_s = web_root
        .as_ref()
        .map_or_else(|| "none".to_string(), |w| w.root().display().to_string());
    let store = match Store::open(&data_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open ledger: {e}");
            return ExitCode::FAILURE;
        }
    };
    if args.iter().any(|a| a == "--accept-replace-stroke-lineages") {
        store
            .accept_replace_stroke_lineages
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    match ServerHandle::start_with(
        bind,
        store,
        ServerConfig {
            auth,
            web_root,
            control: None,
        },
    ) {
        Ok(h) => {
            println!(
                "scribe-sync-server listening on {} data_dir={} web_root={web_s} auth={} accept_replace_stroke_lineages={}",
                h.base_url(),
                data_dir.display(),
                if authed {
                    "bearer"
                } else {
                    "none (loopback only)"
                },
                h.store
                    .accept_replace_stroke_lineages
                    .load(std::sync::atomic::Ordering::SeqCst)
            );
            h.join()
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
