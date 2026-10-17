//! D1 Scribe Server discovery for Scribe for Kindle (DNS-SD `_scribe._tcp.local.`).
//!
//! * [`ServerTable`] — pure reducer over resolve/remove events. Servers are keyed by
//!   **ServerId** (never IP): duplicate advertisements are idempotent, a changed endpoint
//!   replaces the old one, same-name servers with different ids stay separate.
//! * [`MdnsBrowser`] — `mdns-sd` browse on a background thread (never the pen/render path).
//! * [`Aperture`] — the Kindle firewall drops inbound UDP except ESTABLISHED flows, so while a
//!   search runs Scribe inserts ONE tagged rule admitting exactly multicast-DNS answers:
//!   `-i wlan0 -p udp -d 224.0.0.251 --sport 5353 --dport 5353` (the only traffic shape
//!   `mdns-sd` uses: QM queries/answers on the mDNS group). It lives in a dedicated
//!   `SCRIBE_MDNS` chain, jumped to from a commented INPUT rule, and is removed when the
//!   search stops, on shutdown, and (stale-rule cleanup) at startup. No other rule is touched.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use control_protocol::{parse_advertisement, ServerId, SERVICE_TYPE};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredServer {
    pub server_id: ServerId,
    pub name: String,
    /// `http://<ip>[:port]` — the currently advertised endpoint (transient, not identity).
    pub endpoint: String,
    pub instance: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryEvent {
    Resolved {
        instance: String,
        txt: Vec<(String, String)>,
        addrs: Vec<IpAddr>,
        port: u16,
    },
    Removed {
        instance: String,
    },
}

/// Best address for a LAN HTTP endpoint: IPv4 (private first), then IPv6 non-link-local,
/// then IPv4 loopback as a last resort.
pub fn endpoint_for(addrs: &[IpAddr], port: u16) -> Option<String> {
    let mut v4: Vec<_> = addrs
        .iter()
        .filter_map(|a| match a {
            IpAddr::V4(v) if !v.is_loopback() && !v.is_link_local() && !v.is_unspecified() => {
                Some(*v)
            }
            _ => None,
        })
        .collect();
    v4.sort_by_key(|v| (!v.is_private(), v.octets()));
    let loopback = addrs.iter().find_map(|a| match a {
        IpAddr::V4(v) if v.is_loopback() => Some(*v),
        _ => None,
    });
    let host = if let Some(v) = v4.first() {
        v.to_string()
    } else if let Some(v6) = addrs.iter().find_map(|a| match a {
        IpAddr::V6(v) if !v.is_loopback() && (v.segments()[0] & 0xffc0) != 0xfe80 => Some(*v),
        _ => None,
    }) {
        format!("[{v6}]")
    } else if let Some(lo) = loopback {
        // Last resort (single-host development); never preferred over a LAN address.
        lo.to_string()
    } else {
        return None;
    };
    Some(if port == 80 {
        format!("http://{host}")
    } else {
        format!("http://{host}:{port}")
    })
}

#[derive(Clone, Debug, Default)]
pub struct ServerTable {
    by_instance: BTreeMap<String, DiscoveredServer>,
    version: u64,
}

impl ServerTable {
    /// Apply one event; returns true if the visible list changed.
    pub fn apply(&mut self, ev: DiscoveryEvent) -> bool {
        let before = self.servers();
        match ev {
            DiscoveryEvent::Resolved {
                instance,
                txt,
                addrs,
                port,
            } => {
                let (Ok(ad), Some(endpoint)) =
                    (parse_advertisement(&txt), endpoint_for(&addrs, port))
                else {
                    return false; // not a (supported) Scribe Server, or no usable address
                };
                // A ServerId re-announced under another instance name replaces the old row.
                self.by_instance
                    .retain(|k, s| k == &instance || s.server_id != ad.server_id);
                self.by_instance.insert(
                    instance.clone(),
                    DiscoveredServer {
                        server_id: ad.server_id,
                        name: ad.name,
                        endpoint,
                        instance,
                    },
                );
            }
            DiscoveryEvent::Removed { instance } => {
                self.by_instance.remove(&instance);
            }
        }
        let changed = self.servers() != before;
        if changed {
            self.version += 1;
        }
        changed
    }

    /// One entry per ServerId, ordered by display name then id (stable for the UI).
    pub fn servers(&self) -> Vec<DiscoveredServer> {
        let mut v: Vec<DiscoveredServer> = self.by_instance.values().cloned().collect();
        v.sort_by(|a, b| (a.name.as_str(), a.server_id).cmp(&(b.name.as_str(), b.server_id)));
        v.dedup_by_key(|s| s.server_id);
        v
    }

    /// The paired server, matched by ServerId only.
    pub fn find(&self, id: ServerId) -> Option<DiscoveredServer> {
        self.servers().into_iter().find(|s| s.server_id == id)
    }

    pub fn version(&self) -> u64 {
        self.version
    }
}

// ------------------------------------------------------------------------------ firewall

/// Runs one firewall command (argv without the program); `true` on exit status 0.
pub trait FirewallRunner: Send + Sync {
    fn run(&self, args: &[&str]) -> bool;
    /// `iptables -S` output, for verification.
    fn list(&self) -> String;
}

pub struct Iptables {
    pub program: String,
}

impl FirewallRunner for Iptables {
    fn run(&self, args: &[&str]) -> bool {
        std::process::Command::new(&self.program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    fn list(&self) -> String {
        std::process::Command::new(&self.program)
            .arg("-S")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }
}

/// No firewall management (hosts/tests, or no iptables available).
pub struct NoFirewall;

impl FirewallRunner for NoFirewall {
    fn run(&self, _: &[&str]) -> bool {
        true
    }
    fn list(&self) -> String {
        String::new()
    }
}

pub const APERTURE_CHAIN: &str = "SCRIBE_MDNS";
pub const APERTURE_TAG: &str = "scribe-mdns-discovery";

/// Exact rule arguments (shared by open/close so removal matches what was inserted).
pub fn jump_rule() -> Vec<&'static str> {
    vec![
        "INPUT",
        "-i",
        "wlan0",
        "-p",
        "udp",
        "-d",
        "224.0.0.251",
        "--dport",
        "5353",
        "-m",
        "comment",
        "--comment",
        APERTURE_TAG,
        "-j",
        APERTURE_CHAIN,
    ]
}

pub fn accept_rule() -> Vec<&'static str> {
    vec![
        APERTURE_CHAIN,
        "-i",
        "wlan0",
        "-p",
        "udp",
        "-d",
        "224.0.0.251",
        "--sport",
        "5353",
        "--dport",
        "5353",
        "-m",
        "comment",
        "--comment",
        APERTURE_TAG,
        "-j",
        "ACCEPT",
    ]
}

/// Scribe's temporary mDNS aperture on the Kindle firewall.
pub struct Aperture {
    runner: Arc<dyn FirewallRunner>,
    open: Mutex<bool>,
}

impl Aperture {
    pub fn new(runner: Arc<dyn FirewallRunner>) -> Self {
        Self {
            runner,
            open: Mutex::new(false),
        }
    }

    /// The Kindle's iptables when running as root with it present; otherwise a no-op.
    pub fn for_this_device() -> Self {
        let prog = "/usr/sbin/iptables";
        let root = std::fs::read_to_string("/proc/self/status")
            .map(|s| {
                s.lines()
                    .any(|l| l.starts_with("Uid:") && l.split_whitespace().nth(1) == Some("0"))
            })
            .unwrap_or(false);
        if root && std::path::Path::new(prog).exists() {
            Self::new(Arc::new(Iptables {
                program: prog.into(),
            }))
        } else {
            Self::new(Arc::new(NoFirewall))
        }
    }

    /// Remove every trace of Scribe's aperture (idempotent; also run at startup to clean up
    /// after a crash). Touches only the tagged jump rule and the `SCRIBE_MDNS` chain.
    pub fn close(&self) {
        let jump = jump_rule();
        for _ in 0..16 {
            let mut args = vec!["-D"];
            args.extend(&jump);
            if !self.runner.run(&args) {
                break;
            }
        }
        self.runner.run(&["-F", APERTURE_CHAIN]);
        self.runner.run(&["-X", APERTURE_CHAIN]);
        *self.open.lock().unwrap() = false;
    }

    /// Admit mDNS answers (only) for the duration of a search.
    pub fn open(&self) -> bool {
        let mut g = self.open.lock().unwrap();
        if *g {
            return true;
        }
        drop(g);
        self.close(); // never stack rules
        let ok = self.runner.run(&["-N", APERTURE_CHAIN])
            && self.runner.run(&[&["-A"][..], &accept_rule()].concat())
            && self.runner.run(
                &[&["-I"][..], &{
                    let mut j = jump_rule();
                    j.insert(1, "1");
                    j
                }]
                .concat(),
            );
        if !ok {
            self.close();
            return false;
        }
        g = self.open.lock().unwrap();
        *g = true;
        true
    }

    pub fn is_open(&self) -> bool {
        *self.open.lock().unwrap()
    }

    /// Number of Scribe aperture lines currently in the firewall (0 when closed).
    pub fn residue(&self) -> usize {
        self.runner
            .list()
            .lines()
            .filter(|l| l.contains(APERTURE_CHAIN) || l.contains(APERTURE_TAG))
            .count()
    }
}

// ------------------------------------------------------------------------------ browser

/// A live DNS-SD browse. Dropping/stopping it ends the search.
pub struct MdnsBrowser {
    daemon: mdns_sd::ServiceDaemon,
    table: Arc<Mutex<ServerTable>>,
}

impl MdnsBrowser {
    pub fn start() -> Result<Self, String> {
        let daemon = mdns_sd::ServiceDaemon::new().map_err(|e| format!("mdns: {e}"))?;
        let rx = daemon
            .browse(SERVICE_TYPE)
            .map_err(|e| format!("mdns browse: {e}"))?;
        let table = Arc::new(Mutex::new(ServerTable::default()));
        let t = Arc::clone(&table);
        std::thread::Builder::new()
            .name("scribe-discovery".into())
            .spawn(move || {
                while let Ok(ev) = rx.recv() {
                    let ev = match ev {
                        mdns_sd::ServiceEvent::ServiceResolved(info) => DiscoveryEvent::Resolved {
                            instance: info.fullname.clone(),
                            txt: info
                                .txt_properties
                                .iter()
                                .map(|p| (p.key().to_string(), p.val_str().to_string()))
                                .collect(),
                            addrs: info.addresses.iter().map(|a| a.to_ip_addr()).collect(),
                            port: info.port,
                        },
                        mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => {
                            DiscoveryEvent::Removed { instance: fullname }
                        }
                        mdns_sd::ServiceEvent::SearchStopped(_) => break,
                        _ => continue,
                    };
                    t.lock().unwrap().apply(ev);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { daemon, table })
    }

    pub fn table(&self) -> ServerTable {
        self.table.lock().unwrap().clone()
    }

    pub fn stop(self) {
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        if let Ok(rx) = self.daemon.shutdown() {
            let _ = rx.recv_timeout(Duration::from_millis(500));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use control_protocol::advertisement_txt;

    fn sid(n: u8) -> ServerId {
        ServerId::from_parts(1_790_000_000_000 + u64::from(n), &[n; 10])
    }

    fn resolved(inst: &str, id: ServerId, name: &str, ip: &str, port: u16) -> DiscoveryEvent {
        DiscoveryEvent::Resolved {
            instance: inst.into(),
            txt: advertisement_txt(id, name),
            addrs: vec![ip.parse().unwrap()],
            port,
        }
    }

    #[test]
    fn zero_one_many_duplicate_and_same_name_servers() {
        let mut t = ServerTable::default();
        assert!(t.servers().is_empty());
        assert!(t.apply(resolved(
            "server-b",
            sid(1),
            "Test Server B",
            "192.0.2.10",
            80
        )));
        assert_eq!(t.servers().len(), 1);
        assert_eq!(t.servers()[0].endpoint, "http://192.0.2.10");
        // Duplicate advertisement: idempotent.
        assert!(!t.apply(resolved(
            "server-b",
            sid(1),
            "Test Server B",
            "192.0.2.10",
            80
        )));
        // A second server, and another with the SAME name but a different ServerId.
        // "Test Server A" sorts before "Test Server B" (same relative order as the
        // original two distinct display names).
        t.apply(resolved(
            "server-a",
            sid(2),
            "Test Server A",
            "192.0.2.20",
            8787,
        ));
        t.apply(resolved(
            "server-b-2",
            sid(3),
            "Test Server B",
            "192.0.2.30",
            80,
        ));
        let s = t.servers();
        assert_eq!(s.len(), 3);
        assert_eq!(t.find(sid(2)).unwrap().endpoint, "http://192.0.2.20:8787");
        assert_eq!(s[0].name, "Test Server A", "sorted by display name");
        assert_eq!(s.iter().filter(|x| x.name == "Test Server B").count(), 2);
        assert_eq!(t.find(sid(3)).unwrap().endpoint, "http://192.0.2.30");
    }

    #[test]
    fn same_server_id_new_address_is_the_same_server() {
        let mut t = ServerTable::default();
        t.apply(resolved(
            "server-b",
            sid(1),
            "Test Server B",
            "192.0.2.10",
            80,
        ));
        // DHCP moved the server: same ServerId, new endpoint → accepted automatically.
        assert!(t.apply(resolved(
            "server-b",
            sid(1),
            "Test Server B",
            "192.0.2.20",
            80
        )));
        assert_eq!(t.servers().len(), 1);
        assert_eq!(t.find(sid(1)).unwrap().endpoint, "http://192.0.2.20");
        // Re-announced under a new instance name: still one row for that ServerId.
        t.apply(resolved(
            "server-b-renamed",
            sid(1),
            "Test Server B",
            "192.0.2.40",
            80,
        ));
        assert_eq!(t.servers().len(), 1);
        assert_eq!(t.find(sid(1)).unwrap().endpoint, "http://192.0.2.40");
        // An IP previously used by ServerId 1 now advertised by ANOTHER server is not it.
        t.apply(resolved(
            "imposter",
            sid(9),
            "Test Server B",
            "192.0.2.10",
            80,
        ));
        assert_eq!(t.find(sid(1)).unwrap().endpoint, "http://192.0.2.40");
    }

    #[test]
    fn disappear_and_return_with_new_endpoint() {
        let mut t = ServerTable::default();
        t.apply(resolved(
            "server-b",
            sid(1),
            "Test Server B",
            "192.0.2.10",
            80,
        ));
        assert!(t.apply(DiscoveryEvent::Removed {
            instance: "server-b".into()
        }));
        assert!(t.find(sid(1)).is_none());
        t.apply(resolved(
            "server-b",
            sid(1),
            "Test Server B",
            "198.51.100.10",
            8080,
        ));
        assert_eq!(
            t.find(sid(1)).unwrap().endpoint,
            "http://198.51.100.10:8080"
        );
    }

    #[test]
    fn non_scribe_or_unsupported_advertisements_are_ignored() {
        let mut t = ServerTable::default();
        let mut txt = advertisement_txt(sid(1), "x");
        txt[0].1 = "9".into();
        assert!(!t.apply(DiscoveryEvent::Resolved {
            instance: "a".into(),
            txt,
            addrs: vec!["192.0.2.2".parse().unwrap()],
            port: 80
        }));
        assert!(!t.apply(DiscoveryEvent::Resolved {
            instance: "b".into(),
            txt: vec![],
            addrs: vec!["192.0.2.2".parse().unwrap()],
            port: 80
        }));
        // Link-local-only v6 is unusable; loopback only as a last resort.
        assert_eq!(
            endpoint_for(
                &["127.0.0.1".parse().unwrap(), "192.0.2.9".parse().unwrap()],
                80
            )
            .unwrap(),
            "http://192.0.2.9"
        );
        assert!(endpoint_for(&["fe80::1".parse().unwrap()], 80).is_none());
        assert_eq!(
            endpoint_for(
                &["fd00::5".parse().unwrap(), "169.254.1.1".parse().unwrap()],
                81
            )
            .unwrap(),
            "http://[fd00::5]:81"
        );
    }

    struct Recorder {
        log: Mutex<Vec<String>>,
        rules: Mutex<Vec<String>>,
    }

    impl FirewallRunner for Recorder {
        fn run(&self, args: &[&str]) -> bool {
            let line = args.join(" ");
            self.log.lock().unwrap().push(line.clone());
            let mut r = self.rules.lock().unwrap();
            match args[0] {
                "-N" => {
                    if r.iter().any(|l| l == &format!("-N {APERTURE_CHAIN}")) {
                        return false;
                    }
                    r.push(line);
                    true
                }
                "-A" | "-I" => {
                    r.push(format!(
                        "-A {}",
                        args[1..].join(" ").replacen(" 1 ", " ", 1)
                    ));
                    true
                }
                "-D" => {
                    let want = format!("-A {}", args[1..].join(" "));
                    match r.iter().position(|l| *l == want) {
                        Some(i) => {
                            r.remove(i);
                            true
                        }
                        None => false,
                    }
                }
                "-F" => {
                    r.retain(|l| !l.starts_with(&format!("-A {APERTURE_CHAIN}")));
                    true
                }
                "-X" => {
                    let n = r.len();
                    r.retain(|l| l != &format!("-N {APERTURE_CHAIN}"));
                    r.len() != n
                }
                _ => true,
            }
        }
        fn list(&self) -> String {
            let mut out = String::from(
                "-P INPUT DROP\n-A INPUT -i wlan0 -p tcp -m tcp --dport 22 -j ACCEPT\n",
            );
            for l in self.rules.lock().unwrap().iter() {
                out.push_str(l);
                out.push('\n');
            }
            out
        }
    }

    #[test]
    fn aperture_is_exact_tagged_and_leaves_zero_residue_over_many_cycles() {
        let rec = Arc::new(Recorder {
            log: Mutex::new(vec![]),
            rules: Mutex::new(vec![]),
        });
        let ap = Aperture::new(rec.clone());
        ap.close(); // startup stale cleanup on a clean firewall is harmless
        assert_eq!(ap.residue(), 0);
        for _ in 0..25 {
            assert!(ap.open());
            assert!(ap.open(), "re-open must not stack rules");
            assert_eq!(ap.residue(), 3, "chain + accept + one jump");
            ap.close();
            assert_eq!(ap.residue(), 0);
        }
        let log = rec.log.lock().unwrap().join("\n");
        assert!(log.contains("-A SCRIBE_MDNS -i wlan0 -p udp -d 224.0.0.251 --sport 5353 --dport 5353 -m comment --comment scribe-mdns-discovery -j ACCEPT"));
        assert!(log.contains("-I INPUT 1 -i wlan0 -p udp -d 224.0.0.251 --dport 5353"));
        // Never flushes or deletes anything but Scribe's own chain/rule.
        for l in log.lines() {
            assert!(
                !l.starts_with("-F INPUT")
                    && l != "-F"
                    && !l.starts_with("-P")
                    && !l.starts_with("-X INPUT"),
                "{l}"
            );
        }
        // The unrelated SSH rule survived every cycle.
        assert!(rec.list().contains("--dport 22 -j ACCEPT"));
        // A stale rule left by a crash is removed by the startup cleanup.
        assert!(ap.open());
        let fresh = Aperture::new(rec.clone());
        fresh.close();
        assert_eq!(fresh.residue(), 0);
    }
}
