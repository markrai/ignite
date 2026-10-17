//! DNS-SD / mDNS advertisement of Scribe Server (`_scribe._tcp.local.`).
//!
//! Crate choice (D1 audit): `mdns-sd` — pure Rust, no async runtime, one daemon thread,
//! both responder and browser, Linux/macOS/Windows; no Avahi CLI at runtime. It sends and
//! answers **multicast** only (QM to 224.0.0.251:5353), which is the exact traffic shape the
//! Kindle's temporary discovery aperture admits.
//!
//! Advertised (non-secret only): instance name = display name, host `<hostname>.local.`, the
//! listener port, TXT `v`, `sid` (ServerId), `name`, `cp` (control path). Never tokens, codes,
//! credentials or notebook metadata ([`control_protocol::validate_txt`] enforces the key set).

use std::collections::HashMap;

use control_protocol::{advertisement_txt, clamp_label, validate_txt, ServerId, SERVICE_TYPE};
use mdns_sd::{IfKind, IfPredicate, ServiceDaemon, ServiceInfo};

pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

/// Interface eligibility comes from the ONE shared D1 policy ([`crate::netpolicy`]), the same
/// the accept path and the installer's firewall scoping use.
pub use crate::netpolicy::is_lan_interface_name as is_lan_interface;

fn policy_predicate() -> IfPredicate {
    IfPredicate::new(|intf| {
        let (ip, prefix_len) = match &intf.addr {
            if_addrs::IfAddr::V4(a) => (std::net::IpAddr::V4(a.ip), a.prefixlen),
            if_addrs::IfAddr::V6(a) => (std::net::IpAddr::V6(a.ip), a.prefixlen),
        };
        crate::netpolicy::is_eligible(&crate::netpolicy::IfaceAddr {
            name: intf.name.clone(),
            ip,
            prefix_len,
        })
    })
}

/// DNS-safe `.local.` host label derived from the hostname.
pub fn host_label(hostname: &str) -> String {
    let short = hostname.split('.').next().unwrap_or("scribe");
    let label: String = short
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let label = label.trim_matches('-');
    if label.is_empty() {
        "scribe".into()
    } else {
        label.chars().take(63).collect()
    }
}

/// `(instance name, host, port, TXT)`
pub type ServiceParts = (String, String, u16, Vec<(String, String)>);

/// The DNS-SD records Scribe Server publishes (pure; unit-tested).
pub fn service_parts(
    server_id: ServerId,
    name: &str,
    hostname: &str,
    port: u16,
) -> Result<ServiceParts, String> {
    let instance = clamp_label(name);
    let txt = advertisement_txt(server_id, &instance);
    validate_txt(&txt)?;
    Ok((
        instance,
        format!("{}.local.", host_label(hostname)),
        port,
        txt,
    ))
}

impl Advertisement {
    pub fn start(
        server_id: ServerId,
        name: &str,
        hostname: &str,
        port: u16,
    ) -> Result<Self, String> {
        let (instance, host, port, txt) = service_parts(server_id, name, hostname, port)?;
        let daemon = ServiceDaemon::new().map_err(|e| format!("mdns daemon: {e}"))?;
        // Advertise ONLY on addresses the shared LAN policy accepts (evaluated live, so DHCP
        // changes are followed): everything off, then the policy predicate on (last match wins).
        let _ = daemon.disable_interface(IfKind::All);
        let _ = daemon.enable_interface(IfKind::Predicate(policy_predicate()));
        let props: HashMap<String, String> = txt.into_iter().collect();
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", port, props)
            .map_err(|e| format!("mdns service: {e}"))?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon
            .register(info)
            .map_err(|e| format!("mdns register: {e}"))?;
        Ok(Self { daemon, fullname })
    }

    /// Send goodbye packets and stop the daemon.
    pub fn stop(self) {
        if let Ok(rx) = self.daemon.unregister(&self.fullname) {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
        }
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_records_are_nonsecret_and_well_formed() {
        let sid = ServerId::from_parts(1_790_000_000_000, &[3; 10]);
        let (instance, host, port, txt) =
            service_parts(sid, "Test Server B", "test-server-b", 80).unwrap();
        assert_eq!(SERVICE_TYPE, "_scribe._tcp.local.");
        assert_eq!(instance, "Test Server B");
        assert_eq!(host, "test-server-b.local.");
        assert_eq!(port, 80);
        assert_eq!(
            txt.iter().find(|(k, _)| k == "sid").unwrap().1,
            sid.to_string()
        );
        let joined = format!("{txt:?}").to_lowercase();
        for secret_word in [
            "token",
            "secret",
            "credential",
            "code",
            "cookie",
            "password",
        ] {
            assert!(
                !joined.contains(secret_word),
                "{secret_word} in advertisement"
            );
        }
        assert_eq!(host_label("My Host_1.lan"), "my-host-1");
        for lan in ["eno1", "eth0", "enp3s0", "wlan0", "wlp2s0"] {
            assert!(is_lan_interface(lan), "{lan}");
        }
        for virt in [
            "lo",
            "docker0",
            "br-synth01",
            "veth12ab",
            "tailscale0",
            "wg0",
            "virbr0",
            "lxdbr0",
        ] {
            assert!(!is_lan_interface(virt), "{virt}");
        }
        assert_eq!(host_label(""), "scribe");
    }
}
