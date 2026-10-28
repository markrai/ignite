//! Compiled, side-effect-free inventory of this executable's audited durable-storage behavior.
//!
//! The document produced here is a declaration, not compatibility evidence. In particular,
//! `coverage=incomplete` and `data_compatibility=unknown` are part of the canonical bytes and no
//! caller may infer a positive A/B decision from successful emission.

use std::fmt::Write as _;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_ENCODED_BYTES: usize = 64 * 1024;
pub const COMMAND: &str = "--storage-capabilities";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadCapability {
    Supported,
    Unsupported,
    Unverified,
}

impl ReadCapability {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unverified => "unverified",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteCapability {
    Emitted,
    NotEmitted,
    Unverified,
}

impl WriteCapability {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Emitted => "emitted",
            Self::NotEmitted => "not-emitted",
            Self::Unverified => "unverified",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnknownFieldBehavior {
    Reject,
    Ignore,
    NotApplicable,
    Unverified,
}

impl UnknownFieldBehavior {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Ignore => "ignore",
            Self::NotApplicable => "not-applicable",
            Self::Unverified => "unverified",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preservation {
    Lossless,
    LossyOnRewrite,
    Opaque,
    NotEstablished,
    NotApplicable,
}

impl Preservation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Lossless => "lossless",
            Self::LossyOnRewrite => "lossy-on-rewrite",
            Self::Opaque => "opaque",
            Self::NotEstablished => "not-established",
            Self::NotApplicable => "not-applicable",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    Verified,
    Inferred,
    Unknown,
}

impl Confidence {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Inferred => "inferred",
            Self::Unknown => "unknown",
        }
    }
}

/// One exact format or record variant. Version ranges and wildcard identifiers are forbidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatCapability {
    pub id: &'static str,
    pub read: ReadCapability,
    pub write: WriteCapability,
    pub unknown_fields: UnknownFieldBehavior,
    pub preservation: Preservation,
    pub automatic: &'static [&'static str],
    pub recovery: &'static [&'static str],
    pub owner: &'static str,
    pub confidence: Confidence,
}

/// Fixed V1 envelope. Its negative policy fields are intentional and fail closed.
#[derive(Clone, Copy, Debug)]
pub struct StorageCapabilitiesV1<'a> {
    pub entries: &'a [FormatCapability],
}

// Keeping each row adjacent to its canonical fields makes the audited table reviewable; grouping
// these arguments would only hide the one-to-one encoding contract.
#[allow(clippy::too_many_arguments)]
const fn capability(
    id: &'static str,
    read: ReadCapability,
    write: WriteCapability,
    unknown_fields: UnknownFieldBehavior,
    preservation: Preservation,
    automatic: &'static [&'static str],
    recovery: &'static [&'static str],
    owner: &'static str,
    confidence: Confidence,
) -> FormatCapability {
    FormatCapability {
        id,
        read,
        write,
        unknown_fields,
        preservation,
        automatic,
        recovery,
        owner,
        confidence,
    }
}

use Confidence::{Inferred, Unknown, Verified};
use Preservation::{Lossless, LossyOnRewrite, NotEstablished, Opaque};
use ReadCapability::{Supported, Unsupported, Unverified};
use UnknownFieldBehavior::{
    Ignore, NotApplicable as FieldsNotApplicable, Reject, Unverified as FieldsUnverified,
};
use WriteCapability::{Emitted, NotEmitted, Unverified as WriteUnverified};

// Keep this array in strict identifier order. Lists inside an entry are also canonical and sorted.
// The inventory document supplies source/function references and the limits behind each claim.
const ENTRIES: &[FormatCapability] = &[
    capability(
        "active-notebook-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &[],
        &["default-on-invalid"],
        "scribe-app.nav-state",
        Verified,
    ),
    capability(
        "adoption-plan-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &["startup-recover-adoption"],
        &["idempotent-roll-forward-or-discard"],
        "scribe-app.sync-adopt",
        Verified,
    ),
    capability(
        "adoption-workspace-v1",
        Supported,
        Emitted,
        FieldsNotApplicable,
        NotEstablished,
        &["startup-recover-adoption"],
        &["directory-swap-journal"],
        "scribe-app.sync-adopt",
        Inferred,
    ),
    capability(
        "background-identity-v1",
        Supported,
        Emitted,
        Ignore,
        LossyOnRewrite,
        &["snapshot-seal-missing-identity"],
        &["reconstruct-when-invalid"],
        "scribe-app.store",
        Verified,
    ),
    capability(
        "background-raw-v1",
        Supported,
        Emitted,
        FieldsNotApplicable,
        Lossless,
        &["notebook-open-repair-background"],
        &["atomic-replace-and-checksum"],
        "scribe-app.store",
        Verified,
    ),
    capability(
        "confirmed-checkpoint-manifest-v3",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &[
            "startup-collect-unreferenced-checkpoint-pages",
            "startup-convert-base-state-v2",
        ],
        &[
            "content-addressed-page-validation",
            "manifest-last-promotion",
        ],
        "scribe-app.sync-confirmed",
        Verified,
    ),
    capability(
        "confirmed-checkpoint-page-unversioned",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &[],
        &["content-addressed-page-validation"],
        "scribe-app.sync-confirmed",
        Verified,
    ),
    capability(
        "confirmed-log-frame-v1",
        Supported,
        Emitted,
        FieldsNotApplicable,
        Lossless,
        &["startup-truncate-torn-confirmed-tail"],
        &["crc-framed-prefix-replay"],
        "scribe-app.sync-confirmed",
        Verified,
    ),
    capability(
        "folders-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &[],
        &["atomic-replace", "refuse-overwrite-when-unreadable"],
        "scribe-app.folders",
        Verified,
    ),
    capability(
        "import-origin-v1",
        Unverified,
        Emitted,
        Reject,
        Lossless,
        &["startup-recover-import"],
        &["provenance-bound-quarantine"],
        "scribe-app.nbk-import",
        Inferred,
    ),
    capability(
        "import-receipts-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &["startup-recover-import"],
        &["pending-receipt-idempotent-cleanup"],
        "scribe-app.nbk-import",
        Verified,
    ),
    capability(
        "inbound-accepted-record-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &["startup-recover-inbound-apply"],
        &[
            "ignore-torn-final-line",
            "strict-identity-and-sequence-validation",
        ],
        "scribe-app.sync-inbound",
        Verified,
    ),
    capability(
        "inbound-apply-marker-v1",
        Unverified,
        Emitted,
        Reject,
        Lossless,
        &["startup-recover-inbound-apply"],
        &["idempotent-materialization-replay"],
        "scribe-app.sync-inbound",
        Unknown,
    ),
    capability(
        "inbound-inbox-record-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &["startup-recover-inbound-apply"],
        &[
            "ignore-torn-final-line",
            "strict-contiguous-sequence-validation",
        ],
        "scribe-app.sync-inbound",
        Verified,
    ),
    capability(
        "library-manifest-v1",
        Supported,
        Emitted,
        Ignore,
        LossyOnRewrite,
        &["startup-create-or-migrate-library"],
        &["atomic-replace", "manifest-first-delete"],
        "scribe-app.library",
        Verified,
    ),
    capability(
        "library-migration-record-v1",
        Supported,
        Emitted,
        Ignore,
        LossyOnRewrite,
        &["startup-create-or-migrate-library"],
        &["idempotent-legacy-layout-migration"],
        "scribe-app.library",
        Verified,
    ),
    capability(
        "local-identity-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &["startup-create-or-extend-identity"],
        &["atomic-replace-under-identity-lock"],
        "sync-protocol.binding",
        Verified,
    ),
    capability(
        "local-mutation-generation-unversioned",
        Supported,
        Emitted,
        FieldsNotApplicable,
        Lossless,
        &["ordinary-library-mutation"],
        &["atomic-decimal-replace"],
        "scribe-app.sync-verified",
        Verified,
    ),
    capability(
        "local-verified-baseline-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &["startup-full-reconcile"],
        &["atomic-self-bound-census"],
        "scribe-app.verified-baseline",
        Verified,
    ),
    capability(
        "local-verified-cleared-marker-v1",
        Supported,
        Emitted,
        FieldsNotApplicable,
        Opaque,
        &["clear-verified-stamp"],
        &["diagnostic-marker-only"],
        "scribe-app.sync-verified",
        Inferred,
    ),
    capability(
        "local-verified-stamp-v3",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &["startup-full-reconcile"],
        &["atomic-census-stamp"],
        "scribe-app.sync-verified",
        Verified,
    ),
    capability(
        "navigation-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &[],
        &["default-on-invalid", "nondurable-temp-rename"],
        "scribe-app.nav-state",
        Verified,
    ),
    capability(
        "notebook-info-v1",
        Supported,
        Emitted,
        Ignore,
        LossyOnRewrite,
        &[],
        &["atomic-replace"],
        "scribe-app.library",
        Verified,
    ),
    capability(
        "notebook-manifest-v1",
        Supported,
        Emitted,
        Ignore,
        LossyOnRewrite,
        &["notebook-open-adopt-legacy-page"],
        &["atomic-replace", "manifest-first-structural-commit"],
        "scribe-app.notebook",
        Verified,
    ),
    capability(
        "orphan-layout-unversioned",
        Unsupported,
        WriteUnverified,
        FieldsUnverified,
        Opaque,
        &[],
        &["ignored-and-reported"],
        "scribe-app.library-notebook",
        Unknown,
    ),
    capability(
        "outbound-base-state-v2",
        Supported,
        NotEmitted,
        Reject,
        Lossless,
        &["startup-convert-base-state-v2"],
        &["convert-to-checkpoint-v3"],
        "scribe-app.sync-outbound",
        Verified,
    ),
    capability(
        "outbound-config-v1",
        Supported,
        NotEmitted,
        Reject,
        Lossless,
        &[],
        &["refuse-invalid-config"],
        "scribe-app.sync-outbound",
        Verified,
    ),
    capability(
        "outbound-genesis-snapshot-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &["startup-establish-sync-epoch"],
        &["canonical-snapshot-validation"],
        "sync-protocol.snapshot",
        Verified,
    ),
    capability(
        "outbound-outbox-record-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &["startup-reconcile-outbox"],
        &["ignore-torn-final-line", "prep-commit-abort-recovery"],
        "scribe-app.sync-outbound",
        Verified,
    ),
    capability(
        "outbound-sync-state-v1",
        Supported,
        NotEmitted,
        Reject,
        Lossless,
        &["startup-migrate-sync-state-v1-to-v2"],
        &["atomic-replace"],
        "scribe-app.sync-outbound",
        Verified,
    ),
    capability(
        "outbound-sync-state-v2",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &[
            "startup-establish-sync-epoch",
            "startup-recover-inbound-apply",
        ],
        &["atomic-replace"],
        "scribe-app.sync-outbound",
        Verified,
    ),
    capability(
        "page-edit-delete-v1",
        Supported,
        Emitted,
        Ignore,
        NotEstablished,
        &[],
        &["append-and-fsync", "truncate-torn-final-line-on-open"],
        "scribe-app.store",
        Verified,
    ),
    capability(
        "page-edit-replace-v2",
        Supported,
        Emitted,
        Ignore,
        NotEstablished,
        &[],
        &["append-and-fsync", "truncate-torn-final-line-on-open"],
        "scribe-app.store",
        Verified,
    ),
    capability(
        "page-header-v2",
        Supported,
        NotEmitted,
        Ignore,
        LossyOnRewrite,
        &["notebook-open-migrate-page-v2-to-v3"],
        &["idempotent-meta-migration"],
        "scribe-app.store",
        Verified,
    ),
    capability(
        "page-header-v3",
        Supported,
        NotEmitted,
        Ignore,
        LossyOnRewrite,
        &["notebook-open-migrate-page-v3-to-v4"],
        &["idempotent-meta-migration"],
        "scribe-app.store",
        Verified,
    ),
    capability(
        "page-header-v4",
        Supported,
        NotEmitted,
        Ignore,
        LossyOnRewrite,
        &["notebook-open-migrate-page-v4-to-v5"],
        &["idempotent-meta-migration"],
        "scribe-app.store",
        Verified,
    ),
    capability(
        "page-header-v5",
        Supported,
        Emitted,
        Ignore,
        LossyOnRewrite,
        &[
            "notebook-open-repair-background",
            "snapshot-seal-missing-identity",
        ],
        &["atomic-meta-replace"],
        "scribe-app.store",
        Verified,
    ),
    capability(
        "page-ledger-record-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &["ordinary-library-mutation"],
        &["ignore-torn-final-line", "strict-bounded-read"],
        "scribe-app.page-ledger",
        Verified,
    ),
    capability(
        "page-link-record-unversioned",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &[],
        &["atomic-whole-file-replace"],
        "sync-protocol.content",
        Verified,
    ),
    capability(
        "page-stroke-record-v1",
        Supported,
        Emitted,
        Ignore,
        NotEstablished,
        &[],
        &["append-and-fsync", "truncate-torn-final-line-on-open"],
        "scribe-app.store",
        Verified,
    ),
    capability(
        "page-stroke-record-v2",
        Supported,
        Emitted,
        Ignore,
        NotEstablished,
        &[],
        &["append-and-fsync", "truncate-torn-final-line-on-open"],
        "scribe-app.store",
        Verified,
    ),
    capability(
        "page-text-record-unversioned",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &[],
        &["atomic-whole-file-replace"],
        "sync-protocol.content",
        Verified,
    ),
    capability(
        "preferences-v1",
        Supported,
        Emitted,
        Ignore,
        LossyOnRewrite,
        &[],
        &["atomic-replace", "default-on-invalid"],
        "scribe-app.prefs",
        Verified,
    ),
    capability(
        "recovery-complete-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &[],
        &["atomic-recovery-finalization"],
        "scribe-app.sync-recovery-exec",
        Verified,
    ),
    capability(
        "recovery-intent-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &[],
        &["crash-safe-frozen-operation-identities"],
        "scribe-app.sync-recovery-exec",
        Verified,
    ),
    capability(
        "recovery-maintenance-marker-unversioned",
        Supported,
        Emitted,
        FieldsNotApplicable,
        Lossless,
        &[],
        &["fail-closed-authoring-hold"],
        "scribe-app.sync-verified",
        Verified,
    ),
    capability(
        "render-profile-v1",
        Supported,
        NotEmitted,
        Ignore,
        LossyOnRewrite,
        &["notebook-open-migrate-page-v3-to-v4"],
        &["strict-profile-validation"],
        "scribe-app.profile",
        Verified,
    ),
    capability(
        "render-profile-v2",
        Supported,
        Emitted,
        Ignore,
        LossyOnRewrite,
        &["notebook-open-migrate-page-v4-to-v5"],
        &["strict-profile-validation"],
        "scribe-app.profile",
        Verified,
    ),
    capability(
        "replica-state-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &["startup-create-replica-state"],
        &["atomic-replace-under-replica-lock"],
        "sync-protocol.replica",
        Verified,
    ),
    capability(
        "retired-sync-epoch-layout-unversioned",
        Unsupported,
        Emitted,
        FieldsNotApplicable,
        Opaque,
        &["library-adoption-retire-sync-epoch"],
        &["preserve-unsent-operations"],
        "scribe-app.sync-adopt",
        Inferred,
    ),
    capability(
        "server-device-state-v1",
        Unverified,
        Emitted,
        Reject,
        LossyOnRewrite,
        &["startup-product-sync-client-id"],
        &["atomic-private-replace", "regenerate-on-invalid"],
        "scribe-app.server-link",
        Unknown,
    ),
    capability(
        "server-profile-v1",
        Unverified,
        Emitted,
        Reject,
        NotEstablished,
        &[],
        &["atomic-private-replace"],
        "scribe-app.server-link",
        Unknown,
    ),
    capability(
        "sync-blob-unversioned",
        Supported,
        Emitted,
        FieldsNotApplicable,
        Lossless,
        &["startup-recover-inbound-apply"],
        &["sha256-content-addressed"],
        "scribe-app.sync-outbound",
        Verified,
    ),
    capability(
        "sync-envelope-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &[],
        &["canonical-json-validation"],
        "sync-protocol.ops",
        Verified,
    ),
    capability(
        "sync-history-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &[],
        &["semantic-state-validation"],
        "sync-engine.state",
        Verified,
    ),
    capability(
        "sync-snapshot-v1",
        Supported,
        Emitted,
        Reject,
        Lossless,
        &["startup-establish-sync-epoch"],
        &["canonical-json-validation", "semantic-state-validation"],
        "sync-protocol.snapshot",
        Verified,
    ),
    capability(
        "trash-notebook-layout-unversioned",
        Unsupported,
        Emitted,
        FieldsNotApplicable,
        Opaque,
        &[],
        &["manifest-first-then-quarantine"],
        "scribe-app.library",
        Inferred,
    ),
    capability(
        "trash-page-layout-unversioned",
        Unsupported,
        Emitted,
        FieldsNotApplicable,
        Opaque,
        &[],
        &["manifest-first-then-quarantine"],
        "scribe-app.notebook",
        Inferred,
    ),
];

pub const COMPILED: StorageCapabilitiesV1<'static> = StorageCapabilitiesV1 { entries: ENTRIES };

/// Returns whether this command was requested. Mixed/duplicate invocation is rejected before any
/// normal startup work. Other command lines remain owned by the existing parser.
pub fn command_requested(args: &[String]) -> Result<bool, String> {
    let count = args.iter().filter(|arg| arg.as_str() == COMMAND).count();
    match count {
        0 => Ok(false),
        1 if args.len() == 1 => Ok(true),
        _ => Err(format!("{COMMAND} must be used alone")),
    }
}

/// Canonical LF-terminated ASCII bytes for the compiled declaration.
pub fn canonical_bytes() -> Result<Vec<u8>, String> {
    encode(&COMPILED)
}

fn encode(contract: &StorageCapabilitiesV1<'_>) -> Result<Vec<u8>, String> {
    validate(contract)?;
    let mut out = String::with_capacity(16 * 1024);
    writeln!(out, "IGNITE_STORAGE_CAPABILITIES_V1").map_err(|e| e.to_string())?;
    writeln!(out, "schema={SCHEMA_VERSION}").map_err(|e| e.to_string())?;
    writeln!(out, "coverage=incomplete").map_err(|e| e.to_string())?;
    writeln!(out, "data_compatibility=unknown").map_err(|e| e.to_string())?;
    writeln!(out, "promotion_authorized=false").map_err(|e| e.to_string())?;
    writeln!(out, "entry_count={}", contract.entries.len()).map_err(|e| e.to_string())?;
    for entry in contract.entries {
        writeln!(
            out,
            "format={}|read={}|write={}|unknown={}|preserve={}|automatic={}|recovery={}|owner={}|confidence={}",
            entry.id,
            entry.read.as_str(),
            entry.write.as_str(),
            entry.unknown_fields.as_str(),
            entry.preservation.as_str(),
            list(entry.automatic),
            list(entry.recovery),
            entry.owner,
            entry.confidence.as_str(),
        )
        .map_err(|e| e.to_string())?;
    }
    if out.len() > MAX_ENCODED_BYTES {
        return Err(format!(
            "storage capability document is {} bytes (limit {MAX_ENCODED_BYTES})",
            out.len()
        ));
    }
    Ok(out.into_bytes())
}

fn list(values: &[&str]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(",")
    }
}

fn validate(contract: &StorageCapabilitiesV1<'_>) -> Result<(), String> {
    if contract.entries.is_empty() {
        return Err("storage capability declaration has no entries".into());
    }
    if contract.entries.len() > 256 {
        return Err("storage capability declaration has too many entries".into());
    }
    let mut previous = None;
    for entry in contract.entries {
        validate_exact_format_id(entry.id)?;
        if let Some(prev) = previous {
            if prev >= entry.id {
                return Err(format!(
                    "storage capability identifiers are duplicate or out of order: {prev}, {}",
                    entry.id
                ));
            }
        }
        previous = Some(entry.id);
        validate_token("owner", entry.owner, true)?;
        validate_list("automatic", entry.automatic)
            .map_err(|error| format!("{}: {error}", entry.id))?;
        validate_list("recovery", entry.recovery)
            .map_err(|error| format!("{}: {error}", entry.id))?;
    }
    Ok(())
}

fn validate_exact_format_id(id: &str) -> Result<(), String> {
    validate_token("format", id, false)?;
    if let Some(prefix) = id.strip_suffix("-unversioned") {
        if prefix.is_empty() || has_embedded_version_classification(prefix) {
            return Err(format!(
                "format identifier has contradictory version classifications: {id}"
            ));
        }
        return Ok(());
    }
    let Some((prefix, version)) = id.rsplit_once("-v") else {
        return Err(format!("format identifier has no exact version: {id}"));
    };
    if prefix.is_empty()
        || has_embedded_version_classification(prefix)
        || version.is_empty()
        || !version.bytes().all(|byte| byte.is_ascii_digit())
        || (version.len() > 1 && version.starts_with('0'))
    {
        return Err(format!(
            "format identifier has a noncanonical version: {id}"
        ));
    }
    Ok(())
}

fn has_embedded_version_classification(prefix: &str) -> bool {
    prefix.split('-').any(|component| {
        component == "unversioned"
            || component.strip_prefix('v').is_some_and(|rest| {
                rest.as_bytes()
                    .first()
                    .is_some_and(|byte| byte.is_ascii_digit())
            })
    })
}

fn validate_list(field: &str, values: &[&str]) -> Result<(), String> {
    if values.len() > 16 {
        return Err(format!("{field} has too many values"));
    }
    let mut previous = None;
    for value in values {
        validate_token(field, value, false)?;
        if let Some(prev) = previous {
            if prev >= *value {
                return Err(format!("{field} values are duplicate or out of order"));
            }
        }
        previous = Some(*value);
    }
    Ok(())
}

fn validate_token(field: &str, value: &str, allow_dot: bool) -> Result<(), String> {
    if value.is_empty()
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || byte == b'-'
                || (allow_dot && byte == b'.')
        })
    {
        return Err(format!("invalid {field} token {value:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_output_is_byte_deterministic_and_bounded() {
        let first = canonical_bytes().unwrap();
        let second = canonical_bytes().unwrap();
        assert_eq!(first, second);
        assert!(first.len() <= MAX_ENCODED_BYTES);
        assert!(first.is_ascii());
        assert_eq!(first.last(), Some(&b'\n'));
        assert!(!first.windows(2).any(|window| window == b"\r\n"));
    }

    #[test]
    fn identifiers_are_unique_ordered_and_exact() {
        validate(&COMPILED).unwrap();
        for entry in COMPILED.entries {
            assert!(!entry.id.contains('*'));
            assert!(!entry.id.contains(".."));
            assert!(
                entry.id.ends_with("-unversioned") || entry.id.rsplit_once("-v").is_some(),
                "{}",
                entry.id
            );
        }
    }

    #[test]
    fn exact_format_identifier_grammar_rejects_contradictory_classifications() {
        for valid in ["page-header-v2", "page-text-record-unversioned"] {
            assert!(validate_exact_format_id(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "page-header-v2-unversioned",
            "page-header-v2-v5",
            "page-header-v02",
            "page-header-v2..v5",
            "page-header-unversioned-v2",
        ] {
            assert!(validate_exact_format_id(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn incomplete_declaration_cannot_look_like_compatibility_or_authorization() {
        let text = String::from_utf8(canonical_bytes().unwrap()).unwrap();
        assert!(text.contains("\ncoverage=incomplete\n"));
        assert!(text.contains("\ndata_compatibility=unknown\n"));
        assert!(text.contains("\npromotion_authorized=false\n"));
        assert!(!text.contains("data_compatibility=compatible"));
        assert!(!text.contains("promotion_authorized=true"));
    }

    #[test]
    fn mixed_and_duplicate_command_invocations_are_rejected() {
        assert!(command_requested(&[COMMAND.into()]).unwrap());
        assert!(!command_requested(&["--help".into()]).unwrap());
        assert!(command_requested(&[COMMAND.into(), "--help".into()]).is_err());
        assert!(command_requested(&[COMMAND.into(), COMMAND.into()]).is_err());
    }

    #[test]
    fn invalid_or_ambiguous_declarations_fail_closed() {
        let base = ENTRIES[0];
        let ambiguous = FormatCapability {
            id: "page-header-v2-v5",
            ..base
        };
        let duplicate = [base, base];
        assert!(encode(&StorageCapabilitiesV1 {
            entries: &duplicate
        })
        .is_err());
        let unordered = [ambiguous, base];
        assert!(encode(&StorageCapabilitiesV1 {
            entries: &unordered
        })
        .is_err());
    }
}
