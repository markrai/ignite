//! `scribe-server` — the Scribe Server product binary (D1).
//!
//! ```text
//! scribe-server [serve] [--config /etc/scribe-server/config.toml]
//! scribe-server init    [--config …]   create identity/state; while UNCLAIMED print a fresh
//!                                       one-time setup code (any earlier code stops working)
//! scribe-server status  [--config …]   identity + claim state (no secrets)
//! scribe-server approve-admin <code> [--config …]
//!                                       host-operator recovery: approve the pending browser
//!                                       request showing this comparison code as an ADMIN
//!                                       (run on the server with sudo; never automated)
//! ```
//!
//! The lower-level developer server (`scribe-sync-server --token-verifier-file …`) is unchanged.

use std::path::PathBuf;
use std::sync::mpsc;

use scribe_server::config::{Config, DEFAULT_CONFIG_PATH};
use scribe_server::{display_name, plane, prepare, serve, system_clock};

fn main() {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    if let Some(i) = args.iter().position(|a| a == "--config") {
        if let Some(p) = args.get(i + 1) {
            config_path = PathBuf::from(p);
            args.drain(i..=i + 1);
        }
    }
    let cmd = args.first().map_or("serve", String::as_str).to_string();
    // No config needed (the installer's preflight runs these before any config exists).
    match cmd.as_str() {
        "-h" | "--help" | "help" => {
            println!("usage: scribe-server [serve|init|status] [--config PATH]   (default {DEFAULT_CONFIG_PATH})");
            std::process::exit(0);
        }
        // D1.1: the shared LAN policy's eligible interfaces (installer firewall scoping).
        "lan-interfaces" => {
            let inv = scribe_server::netpolicy::os_inventory();
            for n in scribe_server::netpolicy::eligible_interface_names(&inv) {
                println!("{n}");
            }
            std::process::exit(0);
        }
        "-V" | "--version" | "version" => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        _ => {}
    }
    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("scribe-server: {e}");
            std::process::exit(2);
        }
    };
    let code = match cmd.as_str() {
        "serve" => run_serve(&config),
        "init" => run_init(&config),
        "status" => run_status(&config),
        "approve-admin" => run_approve_admin(&config, args.get(1).map(String::as_str)),
        other => {
            eprintln!("scribe-server: unknown command {other:?}");
            2
        }
    };
    std::process::exit(code);
}

fn run_init(config: &Config) -> i32 {
    match prepare(config, system_clock()) {
        Ok((id, created, store)) => {
            println!("server_id={}", id.server_id);
            println!("server_name={}", display_name(config));
            println!("identity={}", if created { "created" } else { "existing" });
            match store.issue_setup_code() {
                // Printed exactly here (installer output), never logged by the service.
                Ok(Some(code)) => {
                    println!("claimed=false");
                    println!("setup_code={code}");
                }
                Ok(None) => println!("claimed=true"),
                Err(e) => {
                    eprintln!("scribe-server: {e:?}");
                    return 1;
                }
            }
            0
        }
        Err(e) => {
            eprintln!("scribe-server: {e}");
            1
        }
    }
}

fn run_approve_admin(config: &Config, code: Option<&str>) -> i32 {
    let Some(code) = code else {
        eprintln!("usage: scribe-server approve-admin <6-digit comparison code> [--config PATH]");
        return 2;
    };
    match prepare(config, system_clock()) {
        Ok((_, _, store)) => match store.approve_admin_by_code(code) {
            Ok((request_id, name)) => {
                println!("approved_admin request_id={request_id} name={name:?}");
                println!(
                    "The waiting browser is now an administrator (it signs in automatically)."
                );
                0
            }
            Err(e) => {
                eprintln!("scribe-server: no single pending browser request with that code ({e:?}). Ask the browser to Request Access again and use the code it shows.");
                1
            }
        },
        Err(e) => {
            eprintln!("scribe-server: {e}");
            1
        }
    }
}

fn run_status(config: &Config) -> i32 {
    match prepare(config, system_clock()) {
        Ok((id, _, store)) => {
            println!("server_id={}", id.server_id);
            println!("server_name={}", display_name(config));
            println!("claimed={}", store.is_claimed().unwrap_or(false));
            0
        }
        Err(e) => {
            eprintln!("scribe-server: {e}");
            1
        }
    }
}

fn run_serve(config: &Config) -> i32 {
    let running = match serve(config, system_clock(), plane::stderr_sink()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("scribe-server: {e}");
            return 1;
        }
    };
    let claimed = running.plane.store.is_claimed().unwrap_or(false);
    eprintln!(
        "scribe-server event=started server_id={} addr={} advertised={} claimed={claimed}",
        running.plane.server_id,
        running.handle.addr(),
        running.advertisement.is_some(),
    );
    let (tx, rx) = mpsc::channel();
    let _ = ctrlc::set_handler(move || {
        let _ = tx.send(());
    });
    let _ = rx.recv();
    eprintln!("scribe-server event=stopping");
    if let Some(a) = running.advertisement {
        a.stop();
    }
    running.handle.shutdown();
    0
}
