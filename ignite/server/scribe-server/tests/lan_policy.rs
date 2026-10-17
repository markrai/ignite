//! D1.1: the product accept path enforces the shared LAN policy on REAL sockets. The server binds
//! the wildcard address; this host's own private IPv4 is labelled by an injected inventory as a
//! VPN interface (refused) or as the physical LAN (served). No firewall is involved.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use scribe_server::config::{Config, DiscoverySection, ServerSection};
use scribe_server::netpolicy::{os_inventory, IfaceAddr, LanPolicy};

/// A private (RFC 1918) IPv4 of this host, so the "LAN" half of the test is meaningful.
fn host_private_ipv4() -> Option<IpAddr> {
    os_inventory()
        .into_iter()
        .map(|a| a.ip)
        .find(|ip| matches!(ip, IpAddr::V4(v) if v.is_private()))
}

fn get_status(addr: SocketAddr) -> Result<String, String> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .map_err(|e| format!("connect: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    s.write_all(b"GET /control/v1/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .map_err(|e| format!("write: {e}"))?;
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    if out.is_empty() {
        Err("closed without a response".into())
    } else {
        Ok(out)
    }
}

/// Serve with an inventory in which `ip` belongs to interface `label`.
fn serve_labelled(
    ip: IpAddr,
    label: &'static str,
    tag: &str,
) -> (
    scribe_server::Running,
    Arc<Mutex<Vec<String>>>,
    std::path::PathBuf,
) {
    let dir = std::env::temp_dir().join(format!("scribe-lan-policy-{tag}-{}", std::process::id()));
    let config = Config {
        server: ServerSection {
            listen: "0.0.0.0:0".parse().unwrap(),
            state_dir: dir.clone(),
            web_root: None,
            name: "Scribe Server on Test".into(),
        },
        discovery: DiscoverySection { advertise: false },
    };
    let policy = Arc::new(LanPolicy::new(move || {
        vec![
            IfaceAddr {
                name: "lo".into(),
                ip: "127.0.0.1".parse().unwrap(),
                prefix_len: 8,
            },
            IfaceAddr {
                name: label.into(),
                ip,
                prefix_len: 24,
            },
        ]
    }));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let ev = events.clone();
    let running = scribe_server::serve_with_policy(
        &config,
        scribe_server::system_clock(),
        Arc::new(move |line: &str| ev.lock().unwrap().push(line.into())),
        policy,
    )
    .unwrap();
    (running, events, dir)
}

#[test]
fn wildcard_listener_serves_lan_and_loopback_but_refuses_vpn_interfaces() {
    let Some(ip) = host_private_ipv4() else {
        eprintln!("skipped: no private IPv4 on this host");
        return;
    };

    // 1. The host address is (per the inventory) a VPN interface: refused before any HTTP;
    //    loopback diagnostics still work.
    let (vpn, events, d1) = serve_labelled(ip, "tailscale0", "vpn");
    let port = vpn.handle.addr().port();
    let lo = get_status(SocketAddr::new("127.0.0.1".parse().unwrap(), port)).unwrap();
    assert!(
        lo.contains("\"mode\":\"product\""),
        "loopback diagnostics must be served"
    );
    let refused = get_status(SocketAddr::new(ip, port));
    assert!(
        refused.is_err(),
        "VPN interface must be refused, got {refused:?}"
    );
    assert!(events
        .lock()
        .unwrap()
        .iter()
        .any(|e| e.contains("event=connection_refused_non_lan")));
    vpn.handle.shutdown();

    // 2. The same address as the physical LAN: served.
    let (lan, _, d2) = serve_labelled(ip, "eno1", "lan");
    let port = lan.handle.addr().port();
    let served = get_status(SocketAddr::new(ip, port)).expect("LAN interface must be served");
    assert!(served.contains("\"mode\":\"product\""));
    lan.handle.shutdown();

    let _ = std::fs::remove_dir_all(d1);
    let _ = std::fs::remove_dir_all(d2);
}
