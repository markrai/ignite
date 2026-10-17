//! Scribe sync server (Phase S2A): durable replication ledger, deduplication and canonical
//! sequencing. **The server assigns order; `sync-engine` defines meaning.** It never edits
//! notebook content on its own; every state change is the reducer applying an accepted
//! operation in server order.
//!
//! Default: loopback-only development server without auth. S2B adds bearer-token device auth;
//! a non-loopback (LAN) bind refuses to start unless auth is configured. Plain HTTP: trusted
//! LAN only — TLS belongs in front of it (reverse proxy) in a later phase.
//!
//! S3A: `GET /v1/libraries` (read-only discovery) and optional same-origin static hosting of
//! the Scribe Web bundle (`--web-root`, see [`web`]).

pub mod auth;
pub mod http;
pub mod store;
pub mod web;

pub use auth::{generate_token, TokenVerifier};
pub use http::{ControlPlane, ControlReply, RequestInfo, ServerConfig, ServerHandle};
pub use store::{Store, StoreError};
pub use web::WebRoot;
