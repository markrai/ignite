//! Product configuration (`/etc/scribe-server/config.toml` on the Linux reference deployment).
//!
//! ```toml
//! [server]
//! listen    = "0.0.0.0:80"                       # installer picks 80, or a fallback port
//! state_dir = "/var/lib/scribe-server"            # ServerId, control.sqlite, ledger/
//! web_root  = "/usr/share/scribe-server/web"      # built Scribe Web bundle
//! name      = ""                                  # "" → "Scribe Server on <Hostname>"
//!
//! [discovery]
//! advertise = true                                # DNS-SD _scribe._tcp on the LAN
//! ```
//!
//! No secret ever lives in this file.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerSection,
    #[serde(default)]
    pub discovery: DiscoverySection,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    pub listen: SocketAddr,
    pub state_dir: PathBuf,
    #[serde(default)]
    pub web_root: Option<PathBuf>,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoverySection {
    #[serde(default = "yes")]
    pub advertise: bool,
}

impl Default for DiscoverySection {
    fn default() -> Self {
        Self { advertise: true }
    }
}

fn yes() -> bool {
    true
}

pub const DEFAULT_CONFIG_PATH: &str = "/etc/scribe-server/config.toml";

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| format!("config: {e}"))
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read config {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn ledger_dir(&self) -> PathBuf {
        self.server.state_dir.join("ledger")
    }
}

/// This machine's short hostname (portable; used only for the default display name and the
/// `.local` host advertised over DNS-SD).
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        if let Ok(h) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
            let h = h.trim();
            if !h.is_empty() {
                return h.to_string();
            }
        }
        if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
            let h = h.trim();
            if !h.is_empty() {
                return h.to_string();
            }
        }
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "scribe".into())
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reference_config_and_refuses_unknown_keys() {
        let c = Config::parse(
            "[server]\nlisten = \"0.0.0.0:80\"\nstate_dir = \"/var/lib/scribe-server\"\nweb_root = \"/usr/share/scribe-server/web\"\n",
        )
        .unwrap();
        assert_eq!(c.server.listen.port(), 80);
        assert!(c.discovery.advertise);
        assert_eq!(
            c.ledger_dir(),
            PathBuf::from("/var/lib/scribe-server/ledger")
        );
        assert!(
            Config::parse("[server]\nlisten=\"0.0.0.0:80\"\nstate_dir=\"/x\"\ntoken=\"s\"\n")
                .is_err()
        );
    }
}
