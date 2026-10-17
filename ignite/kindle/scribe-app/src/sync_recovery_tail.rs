//! Read-only authoritative-tail verification (Slice 18E.6A).
//!
//! Three layers, kept separate:
//! 1. **Transport** — obtain GET `/v1/libraries/{id}/operations` response bytes from an
//!    explicitly supplied source. Default tests inject bytes. HTTP exists only behind an
//!    explicit live-read grant and never runs at startup.
//! 2. **Validation** — parse those bytes with the production tail decoder and check
//!    library identity, cursor continuity, page completeness, and this replica's
//!    `client_seq` values. No network.
//! 3. **Planning** — pass the validated items to [`crate::sync_recovery_plan::plan_recovery`].
//!
//! This module never POSTs operations, never opens `OutboundSync`, never opens a write-capable
//! `ConfirmedLog`, and never authorizes recovery upload.

use serde::Deserialize;
use sync_protocol::{check_client_seq, LibraryId, ReplicaId, SyncEnvelopeV1, ENVELOPE_VERSION};

use crate::sync_outbound::{parse_tail, TAIL_PAGE};
use crate::sync_recovery_exec::{recovery_http_exchange, SubmitTarget};
use crate::sync_recovery_plan::{
    plan_recovery, OutboxInput, PlanResult, RecoveryInputs, SequenceAuthority, ServerTailItem,
};

/// Phrase required to construct a live HTTP grant. Not a URL, token, or hostname.
pub const LIVE_READ_AUTHORIZATION_PHRASE: &str = "I AUTHORIZE READ-ONLY LIVE TAIL ACQUISITION";

/// Default page size: the production inbound `TAIL_PAGE` (server max is 500).
pub const VERIFIER_PAGE_LIMIT: u64 = TAIL_PAGE;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TailVerifyError {
    Malformed { detail: String },
    ForeignLibrary { detail: String },
    CursorDiscontinuity { detail: String },
    IncompleteOrGapped { detail: String },
    Protocol { detail: String },
    ReplicaSequence { detail: String },
    Transport { detail: String },
    NotAuthorized { detail: String },
}

impl std::fmt::Display for TailVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed { detail }
            | Self::ForeignLibrary { detail }
            | Self::CursorDiscontinuity { detail }
            | Self::IncompleteOrGapped { detail }
            | Self::Protocol { detail }
            | Self::ReplicaSequence { detail }
            | Self::Transport { detail }
            | Self::NotAuthorized { detail } => write!(f, "{detail}"),
        }
    }
}

/// Trusted inputs for assembling a tail. Checkpoint cursor is a caller-supplied value,
/// never a hardcoded Review 92 constant.
#[derive(Clone, Debug)]
pub struct TailVerifySpec {
    pub library_id: LibraryId,
    pub replica_id: ReplicaId,
    pub checkpoint_cursor: u64,
    pub acked_client_seq: u64,
    pub page_limit: u64,
}

/// Independently validated tail. `claimed_head` is a server assertion checked against the
/// returned range; `complete` means paging reached a short consistent page.
#[derive(Clone, Debug)]
pub struct VerifiedAuthoritativeTail {
    pub items: Vec<ServerTailItem>,
    pub claimed_head: u64,
    pub observed_through: u64,
    pub complete: bool,
    pub replica_ops_in_tail: usize,
    pub live_fetch_used: bool,
    pub upload_authorized: bool,
}

/// Obtains one operations-page body. Implementations must not POST, PUT, or write sync state.
pub trait TailPageSource {
    fn get_operations_page(&self, after: u64, limit: u64) -> Result<Vec<u8>, TailVerifyError>;
}

/// In-memory pages keyed by `after`. Test/default transport.
#[derive(Clone, Debug, Default)]
pub struct InjectedTailSource {
    pub pages: Vec<(u64, Vec<u8>)>,
}

impl TailPageSource for InjectedTailSource {
    fn get_operations_page(&self, after: u64, limit: u64) -> Result<Vec<u8>, TailVerifyError> {
        let _ = limit;
        self.pages
            .iter()
            .find(|(a, _)| *a == after)
            .map(|(_, b)| b.clone())
            .ok_or_else(|| TailVerifyError::Transport {
                detail: format!("no injected page for after={after}"),
            })
    }
}

/// Explicit live-read grant. Construction does not connect. Empty or default endpoints refuse.
#[derive(Clone, Debug)]
pub struct LiveReadGrant {
    base_url: String,
    bearer: String,
    library_id: LibraryId,
}

impl LiveReadGrant {
    pub fn explicit(
        phrase: &str,
        base_url: String,
        bearer: String,
        library_id: LibraryId,
    ) -> Result<Self, TailVerifyError> {
        if phrase != LIVE_READ_AUTHORIZATION_PHRASE {
            return Err(TailVerifyError::NotAuthorized {
                detail: "live read not authorized".into(),
            });
        }
        let base = base_url.trim().trim_end_matches('/').to_string();
        if base.is_empty() || bearer.is_empty() {
            return Err(TailVerifyError::NotAuthorized {
                detail: "live read requires an explicit URL and bearer".into(),
            });
        }
        Ok(Self {
            base_url: base,
            bearer,
            library_id,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

/// HTTP GET-only source. Never POSTs. Not constructed at startup.
pub struct HttpGetSource {
    grant: LiveReadGrant,
}

impl HttpGetSource {
    pub fn new(grant: LiveReadGrant) -> Self {
        Self { grant }
    }
}

impl TailPageSource for HttpGetSource {
    fn get_operations_page(&self, after: u64, limit: u64) -> Result<Vec<u8>, TailVerifyError> {
        let path = format!(
            "/v1/libraries/{}/operations?after={after}&limit={limit}",
            self.grant.library_id
        );
        let target = SubmitTarget {
            base_url: self.grant.base_url.clone(),
            token: self.grant.bearer.clone(),
        };
        let (status, body) = recovery_http_exchange(&target, "GET", &path, None).map_err(|e| {
            TailVerifyError::Transport {
                detail: format!("GET operations: {e}"),
            }
        })?;
        if status != 200 {
            return Err(TailVerifyError::Transport {
                detail: format!("GET operations HTTP {status}"),
            });
        }
        Ok(body)
    }
}

#[derive(Deserialize)]
struct PageMeta {
    library_id: String,
    head_server_seq: u64,
    next_after: u64,
}

/// Validate one GET-operations body. Reuses production [`parse_tail`].
pub fn validate_operations_page(
    body: &[u8],
    library_id: LibraryId,
    after: u64,
    limit: u64,
) -> Result<ValidatedPage, TailVerifyError> {
    let meta: PageMeta = serde_json::from_slice(body).map_err(|e| TailVerifyError::Malformed {
        detail: format!("malformed tail page: {e}"),
    })?;
    if meta.library_id != library_id.to_string() {
        return Err(TailVerifyError::ForeignLibrary {
            detail: format!("page library {} != {}", meta.library_id, library_id),
        });
    }
    let (claimed_head, items) =
        parse_tail(body, library_id, after).map_err(|e| classify_parse_tail(&e))?;
    let last = items.last().map(|i| i.server_seq).unwrap_or(after);
    if meta.next_after != last {
        return Err(TailVerifyError::CursorDiscontinuity {
            detail: format!("next_after {} != last seq {last}", meta.next_after),
        });
    }
    if claimed_head != meta.head_server_seq {
        return Err(TailVerifyError::Malformed {
            detail: "head_server_seq fields disagree".into(),
        });
    }
    for it in &items {
        if it.envelope.v != ENVELOPE_VERSION {
            return Err(TailVerifyError::Protocol {
                detail: format!(
                    "server_seq {}: envelope v={} want {ENVELOPE_VERSION}",
                    it.server_seq, it.envelope.v
                ),
            });
        }
        if it.envelope.library_id != library_id {
            return Err(TailVerifyError::ForeignLibrary {
                detail: format!("envelope library {}", it.envelope.library_id),
            });
        }
    }
    let full = items.len() as u64 == limit;
    let more_pages = full && last < claimed_head;
    if items.is_empty() {
        if after > claimed_head {
            return Err(TailVerifyError::IncompleteOrGapped {
                detail: format!("after {after} is past claimed head {claimed_head}"),
            });
        }
        if after < claimed_head {
            return Err(TailVerifyError::IncompleteOrGapped {
                detail: format!("empty page after {after} but claimed head {claimed_head}"),
            });
        }
    } else if !more_pages && claimed_head > last {
        // Finished paging (short page, or full page that already reached head) but the
        // claimed head is still beyond the last row: a hole in accepted history.
        return Err(TailVerifyError::IncompleteOrGapped {
            detail: format!("page ends at {last} but claimed head {claimed_head}"),
        });
    }
    Ok(ValidatedPage {
        claimed_head,
        items: items
            .into_iter()
            .map(|i| ServerTailItem {
                server_seq: i.server_seq,
                envelope: i.envelope,
            })
            .collect(),
        complete_page: !more_pages,
    })
}

#[derive(Clone, Debug)]
pub struct ValidatedPage {
    pub claimed_head: u64,
    pub items: Vec<ServerTailItem>,
    pub complete_page: bool,
}

fn classify_parse_tail(e: &str) -> TailVerifyError {
    let d = e.to_string();
    if d.contains("foreign library") {
        TailVerifyError::ForeignLibrary { detail: d }
    } else if d.contains("canonical") || d.contains("invalid envelope") {
        TailVerifyError::Protocol { detail: d }
    } else if d.contains("gap") || d.contains("duplicate") || d.contains("expected server_seq") {
        TailVerifyError::CursorDiscontinuity { detail: d }
    } else {
        TailVerifyError::Malformed { detail: d }
    }
}

/// Page until a short consistent page. Does not mutate the source.
pub fn acquire_verified_tail(
    source: &dyn TailPageSource,
    spec: &TailVerifySpec,
) -> Result<VerifiedAuthoritativeTail, TailVerifyError> {
    if spec.page_limit == 0 {
        return Err(TailVerifyError::Malformed {
            detail: "page_limit must be >= 1".into(),
        });
    }
    let mut after = spec.checkpoint_cursor;
    let mut items = Vec::new();
    let mut claimed_head = spec.checkpoint_cursor;
    let mut pages = 0usize;
    loop {
        pages += 1;
        if pages > 10_000 {
            return Err(TailVerifyError::IncompleteOrGapped {
                detail: "pagination exceeded safety bound".into(),
            });
        }
        let body = source.get_operations_page(after, spec.page_limit)?;
        let page = validate_operations_page(&body, spec.library_id, after, spec.page_limit)?;
        claimed_head = claimed_head.max(page.claimed_head);
        if let Some(first) = page.items.first() {
            if first.server_seq != after + 1 {
                return Err(TailVerifyError::CursorDiscontinuity {
                    detail: format!("assembled gap: after {after} next {}", first.server_seq),
                });
            }
        }
        after = page.items.last().map(|i| i.server_seq).unwrap_or(after);
        items.extend(page.items);
        if page.complete_page {
            break;
        }
    }
    verify_replica_client_seq(&items, spec.replica_id, spec.acked_client_seq)?;
    let observed_through = items
        .last()
        .map(|i| i.server_seq)
        .unwrap_or(spec.checkpoint_cursor);
    let replica_ops_in_tail = items
        .iter()
        .filter(|i| i.envelope.replica_id == spec.replica_id)
        .count();
    Ok(VerifiedAuthoritativeTail {
        items,
        claimed_head,
        observed_through,
        complete: true,
        replica_ops_in_tail,
        live_fetch_used: false,
        upload_authorized: false,
    })
}

/// HTTP GET paging behind an explicit grant. Marks `live_fetch_used`. Still never POSTs.
pub fn acquire_verified_tail_http(
    grant: LiveReadGrant,
    spec: &TailVerifySpec,
) -> Result<VerifiedAuthoritativeTail, TailVerifyError> {
    if grant.library_id != spec.library_id {
        return Err(TailVerifyError::ForeignLibrary {
            detail: "grant library_id does not match verify spec".into(),
        });
    }
    let src = HttpGetSource::new(grant);
    let mut tail = acquire_verified_tail(&src, spec)?;
    tail.live_fetch_used = true;
    Ok(tail)
}

fn verify_replica_client_seq(
    items: &[ServerTailItem],
    replica_id: ReplicaId,
    acked_client_seq: u64,
) -> Result<(), TailVerifyError> {
    let mut prev = if acked_client_seq == 0 {
        None
    } else {
        Some(acked_client_seq)
    };
    for it in items {
        if it.envelope.replica_id != replica_id {
            continue;
        }
        check_client_seq(prev, it.envelope.client_seq).map_err(|e| {
            TailVerifyError::ReplicaSequence {
                detail: format!("server_seq {}: {e}", it.server_seq),
            }
        })?;
        prev = Some(it.envelope.client_seq);
    }
    Ok(())
}

/// Invoke the production planner with a validated tail. Never sets upload authorization.
pub fn plan_with_verified_tail(
    inputs: RecoveryInputs<'_>,
    tail: &VerifiedAuthoritativeTail,
) -> PlanResult {
    let _ = tail;
    let result = plan_recovery(inputs);
    debug_assert!(!result.upload_authorized());
    result
}

/// Convenience: plan using `tail.items` as `server_tail`.
pub fn plan_from_verified(
    library_id: LibraryId,
    replica_id: ReplicaId,
    checkpoint: &sync_engine::LibraryState,
    checkpoint_cursor: u64,
    acked_client_seq: u64,
    accepted: &[SyncEnvelopeV1],
    outbox: OutboxInput<'_>,
    local: &sync_protocol::SyncSnapshotV1,
    tail: &VerifiedAuthoritativeTail,
    sequence_authority: SequenceAuthority,
) -> PlanResult {
    plan_with_verified_tail(
        RecoveryInputs {
            library_id,
            replica_id,
            checkpoint,
            checkpoint_cursor,
            acked_client_seq,
            accepted,
            outbox,
            local,
            server_tail: &tail.items,
            sequence_authority,
        },
        tail,
    )
}

#[cfg(test)]
#[path = "sync_recovery_tail_tests.rs"]
mod tests;
