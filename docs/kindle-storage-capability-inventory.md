# Kindle storage capability inventory (Slices 3D-B2a.4.1–4.2b)

**Status:** source inventory, compiled declaration, a partial library-only inspection probe, and a
separate partial external sync/device inspection probe.
This document does not establish pairwise compatibility, authenticate an executable, inspect all
device data, admit a candidate, authorize promotion, or make rollback safe.
`data_compatibility=unknown` remains a hard blocker.

Slice 3D-B2a.4.3's chain-of-custody audit is documented in
[`kindle-installed-release-provenance-feasibility.md`](kindle-installed-release-provenance-feasibility.md).
Authenticated installed provenance remains unavailable: the production key is unprovisioned, the
signed release manifest binds only the transient whole installer, and installation preserves no
signed component/capability closure. The declaration below is therefore still source/test evidence,
not publisher-authenticated evidence for installed A or B.
The new read-only provenance diagnostic can verify a future signed binding to these exact bytes in a
caller-supplied component set, but reports `installation_binding=unverified` because it does not
establish actual A/B slot ownership or canonical paths. Its official entry point reports
`AUTHORITY_UNAVAILABLE` until that release/persistence chain and the production key exist.

The exact `scribe-app` executable now exposes:

```text
scribe-app --storage-capabilities
```

The command emits the compiled `IGNITE_STORAGE_CAPABILITIES_V1` document and exits before launch
timing, the application singleton, runtime configuration, display/input setup, library open, sync,
pairing, migrations, recovery, threads, or networking. The production lifecycle wrapper is a
different program and has pre-`go` writes described below; callers that require a no-write
declaration must execute the already-identified `scribe-app` binary directly.

## Canonical contract

`StorageCapabilitiesV1` is a small typed Rust declaration in
`scribe-app/src/storage_capabilities.rs`. Its encoding contract is:

```text
IGNITE_STORAGE_CAPABILITIES_V1\n
schema=1\n
coverage=incomplete\n
data_compatibility=unknown\n
promotion_authorized=false\n
entry_count=<canonical decimal>\n
format=<id>|read=<value>|write=<value>|unknown=<value>|preserve=<value>|automatic=<list>|recovery=<list>|owner=<id>|confidence=<value>\n
```

- Bytes are ASCII and every line, including the final line, ends in LF. CRLF is not canonical.
- Fields and field order are fixed. This is not JSON and has no duplicate-key interpretation.
- Format IDs are strictly ordered, unique lowercase tokens ending in one exact `-vN`, or in
  `-unversioned` when the durable representation has no version discriminator. Ranges, wildcards,
  leading-zero versions, and multiple version suffixes are rejected.
- Automatic/recovery lists are ordered unique lowercase tokens joined by comma; an empty list is
  exactly `none`.
- Read values are `supported`, `unsupported`, or `unverified`; write values are `emitted`,
  `not-emitted`, or `unverified`.
- Unknown-field behavior is `reject`, `ignore`, `not-applicable`, or `unverified`.
- Preservation is `lossless`, `lossy-on-rewrite`, `opaque`, `not-established`, or
  `not-applicable`. `opaque` means bytes/directories are retained but not semantically read.
- Confidence is `verified`, `inferred`, or `unknown`. Here `verified` means the current source and
  same-release tests establish the declared behavior. It is not cross-release compatibility,
  release authentication, or Kindle power-loss qualification.
- The encoder validates the compiled declaration before output and refuses output over 65,536
  bytes. The current declaration has 58 entries and remains below that bound.

The negative policy lines are mandatory canonical bytes. An incomplete declaration cannot be
converted into positive compatibility evidence by a consumer.

## Native library and user-visible state

The table abbreviates the compiled fields but does not widen them. `R/W` is read/write;
`unknown→rewrite` records unknown-field handling and rewrite preservation.

| Exact capability ID / durable location | R/W; unknown→rewrite | Automatic mutation and crash behavior | Source owner; confidence |
|---|---|---|---|
| `library-manifest-v1` — `library.meta` | supported/emitted; ignore→lossy | Startup can create the library or drive legacy-layout migration. Writes are temp+fsync+rename; deletes commit the manifest before quarantine. | `library.rs`: `validate_library_manifest`, `open_or_create_library_shell_with_hooks`, `write_library_manifest_atomic`; verified |
| `notebook-info-v1` — `notebook.info` | supported/emitted; ignore→lossy | Rename atomically replaces the whole file. | `library.rs`: `read_notebook_info`, `write_notebook_info`; verified |
| `notebook-manifest-v1` — `notebook.meta` | supported/emitted; ignore→lossy | Notebook open may adopt a lone legacy page. Structural operations publish the manifest before page quarantine. | `notebook.rs`: `validate_manifest`, `open_or_create_notebook`, `write_manifest_atomic_with`; verified |
| `library-migration-record-v1` — `.scribe-library-migration-v1` | supported/emitted; ignore→lossy | Startup resumes/discards the staged flat-layout migration idempotently and removes a completed marker. | `library.rs`: `migrate_legacy_layout`; verified |
| `page-header-v2` — `page.meta` | supported/not emitted; ignore→lossy | Opening any notebook migrates every referenced v2 page to v3, creating an empty durable edit journal before publishing v3 metadata. | `store.rs`: `migrate_page_v2_to_v3_if_needed`; verified |
| `page-header-v3` — `page.meta` | supported/not emitted; ignore→lossy | Notebook open rewrites metadata to v4 and converts render profile v1 to v2. | `store.rs`: `migrate_page_v3_to_v4_if_needed`; verified |
| `page-header-v4` — `page.meta` | supported/not emitted; ignore→lossy | Notebook open validates and rewrites metadata to v5; historical missing fallback radius is normalized only for this edge. | `store.rs`: `migrate_page_v4_to_v5_if_needed`; verified |
| `page-header-v5` — `page.meta` | supported/emitted; ignore→lossy | Notebook load can repair background chrome; snapshot construction can seal a missing background identity. Both rewrite the whole header. | `store.rs`: `load_existing_page`, `install_background`, `resolve_background_ref`; verified |
| `render-profile-v1` — embedded in v2/v3 `page.meta` | supported/not emitted; ignore→lossy | v3→v4 migration converts it to profile v2. | `profile.rs`, `store.rs`: `validate_persisted_render_profile_v1`; verified |
| `render-profile-v2` — embedded in v4/v5 `page.meta` | supported/emitted; ignore→lossy | v4→v5 migration may normalize the historical absent fallback field. | `profile.rs`, `store.rs`: `validate_persisted_render_profile_v2`; verified |
| `background-identity-v1` — embedded in `page.meta` | supported/emitted; ignore→lossy | Invalid/missing identity is reconstructed from background bytes; publication rewrites metadata. | `store.rs`: `BackgroundIdentityV1::validate`, `resolve_background_ref`; verified |
| `background-raw-v1` — `background.raw`, layout version 1 | supported/emitted; fields n/a→lossless | Background repair writes bytes durably before the matching checksum/identity metadata. | `store.rs`: `write_background_atomic`, `install_background`; verified |
| `page-stroke-record-v1` — `strokes.jsonl` | supported/emitted; ignore→not established | Appends and fsyncs complete LF records. Open truncates a torn suffix to the last verified LF. Unknown fields can be ignored during parse, but no cross-version semantic preservation claim exists. | `store.rs`: `parse_stroke_record`, `append_committed_stroke`; verified behavior, no compatibility claim |
| `page-stroke-record-v2` — `strokes.jsonl` | supported/emitted; ignore→not established | Same journal framing; v2 is emitted for pressure-width strokes. | `store.rs`: `PersistedStrokeV2`, `parse_stroke_record`; verified behavior, no compatibility claim |
| `page-edit-delete-v1` — `edits.jsonl` | supported/emitted; ignore→not established | Append+fsync; open truncates a torn suffix and replays delete tombstones. | `store.rs`: `parse_edit_record_line`, `append_delete_stroke`; verified behavior, no compatibility claim |
| `page-edit-replace-v2` — `edits.jsonl` | supported/emitted; ignore→not established | Append+fsync; replay validates lineage replacement shape and identity collisions. | `store.rs`: `PersistedReplaceStrokesV2`, `append_replace_strokes`; verified behavior, no compatibility claim |
| `page-text-record-unversioned` — `texts.jsonl` | supported/emitted; reject→lossless | Every strict `SyncText` is validated; the whole file is atomically replaced or removed. There is no record-level version field. | `store.rs`, `sync-protocol/content.rs`: `load_page_texts`, `write_page_texts`; verified |
| `page-link-record-unversioned` — `links.jsonl` | supported/emitted; reject→lossless | Every strict `SyncPageLink` is validated; the whole file is atomically replaced or removed. There is no record-level version field. | `store.rs`, `sync-protocol/content.rs`: `load_page_links`, `write_page_links`; verified |
| `folders-v1` — `.scribe-folders/folders.json` | supported/emitted; reject→lossless | Atomic replacement; an unreadable existing file makes the store read-only and is not overwritten. | `folders.rs`: `read_folders`, `write_folders`; verified |
| `import-receipts-v1` — `.scribe-import/receipts.json` | supported/emitted; reject→lossless | A pending receipt precedes native mutation. Startup removes a provenance-bound partial copy and makes the receipt retryable. | `nbk_import.rs`: `Ledger::open`, `Ledger::put`, `recover_interrupted`; verified |
| `import-origin-v1` — notebook `import-origin.json` | unverified/emitted; reject→lossless | Used to bind cleanup/quarantine to an Amazon origin. The parser does not currently enforce `v == 1`, so version discrimination remains unresolved. | `nbk_import.rs`: `marker_origin`, `build`; inferred |
| `navigation-v1` — `.scribe-sync/navigation-v1.json` | supported/emitted; reject→lossless | Malformed/oversized state defaults safely. Convenience publication is temp+rename without fsync. | `nav_state.rs`: `selected_page`, `remember`; verified |
| `active-notebook-v1` — `.scribe-sync/active-notebook-v1.json` | supported/emitted; reject→lossless | Malformed state falls back to canonical `library.meta`; temp+rename without fsync. | `nav_state.rs`: `selected_notebook`, `remember_notebook`; verified |
| `preferences-v1` — state `preferences.json` | supported/emitted; ignore→lossy | Malformed/unknown versions default. Save atomically rewrites the known fields and drops ignored fields. | `prefs.rs`: `load`, `save_side_button`; verified |
| `trash-notebook-layout-unversioned` — `.notebook-trash` | unsupported/emitted; n/a→opaque | Notebook delete commits `library.meta` first and then renames the unreferenced directory. There is no semantic reader or schema. | `library.rs`: `delete_notebook_durable`; inferred |
| `trash-page-layout-unversioned` — `.page-trash` | unsupported/emitted; n/a→opaque | Page delete commits `notebook.meta` first and then renames the unreferenced directory. | `notebook.rs`: `delete_page_durable`; inferred |
| `orphan-layout-unversioned` — unreferenced notebook/page dirs | unsupported/unverified; unverified→opaque | Open reports and ignores orphans. Creation/quarantine failures can leave them; no complete recovery grammar exists. | `library.rs`, `notebook.rs`: `list_orphan_*`; unknown |

## Sync, identity, diagnostics, and recovery state

| Exact capability ID / durable location | R/W; unknown→rewrite | Automatic mutation and crash behavior | Source owner; confidence |
|---|---|---|---|
| `local-identity-v1` — library `.scribe-sync/identity.json` | supported/emitted; reject→lossless | Sync startup creates or extends bindings under the identity lock using atomic replacement. | `sync-protocol/binding.rs`, `sync_local.rs`: `IdentityFileV1::parse`, `ensure_sync_identity`; verified |
| `replica-state-v1` — sync `replica.json` | supported/emitted; reject→lossless | Created on sync startup and atomically published under `replica.lock`. | `sync-protocol/replica.rs`, `sync_local.rs`: `ensure_replica_state`; verified |
| `outbound-config-v1` — sync `config.json` | supported/not emitted; reject→lossless | Developer-provisioned input; an invalid file refuses sync. The application does not write it. | `sync_outbound.rs`: `read_config`, `OutboundSync::open_with`; verified |
| `outbound-sync-state-v1` — `sync-state.json` | supported/not emitted; reject→lossless | Startup accepts v1 and rewrites it as v2. | `sync_outbound.rs`: `establish_epoch`; verified |
| `outbound-sync-state-v2` — `sync-state.json` | supported/emitted; reject→lossless | Startup establishes/reconciles the epoch and inbound recovery advances the cursor only after durable materialization. Atomic replacement. | `sync_outbound.rs`, `sync_inbound.rs`; verified |
| `outbound-genesis-snapshot-v1` — `genesis.json` | supported/emitted; reject→lossless | New epoch startup captures, validates, and atomically publishes a canonical semantic snapshot. | `sync-protocol/snapshot.rs`, `sync_outbound.rs`: `establish_epoch`; verified |
| `sync-snapshot-v1` | supported/emitted; reject→lossless | Semantic state validates exact nested notebook/page/folder/content structures. | `sync-protocol/snapshot.rs`: `SyncSnapshotV1`; verified |
| `sync-history-v1` | supported/emitted; reject→lossless | Reducer history validates deleted/known identities against the live snapshot. | `sync-engine/state.rs`: `HistoryV1`; verified |
| `sync-envelope-v1` | supported/emitted; reject→lossless | Canonical JSON and semantic operation validation are required. | `sync-protocol/ops.rs`: `SyncEnvelopeV1`; verified |
| `outbound-outbox-record-v1` — `outbox.jsonl` | supported/emitted; reject→lossless | Startup ignores only an unterminated final record, validates canonical embedded envelopes, and resolves prep/commit/abort state. Rewrites are atomic. | `sync_outbound.rs`: `parse_journal_from`, `rewrite_journal`; verified |
| `inbound-inbox-record-v1` — `inbox.jsonl` | supported/emitted; reject→lossless | Complete records must be contiguous and canonical; an unterminated suffix is ignored. | `sync_inbound.rs`: `load_inbox`; verified |
| `inbound-accepted-record-v1` — `accepted.jsonl` | supported/emitted; reject→lossless | Strict identity/sequence/duplicate validation; torn suffix ignored; bounded compaction atomically rewrites live records. | `sync_inbound.rs`: `load_accepted`, `finish_apply`; verified |
| `inbound-apply-marker-v1` — `apply.json` | unverified/emitted; reject→lossless | Startup treats presence as an in-flight idempotent materialization. `read_marker` does not currently enforce `v == 1`, so exact-version reading is unverified. | `sync_inbound.rs`: `read_marker`, `apply_inbound`; unknown |
| `outbound-base-state-v2` — legacy `base-state.json` | supported/not emitted | Startup validates and converts it to checkpoint v3, then removes it only after checkpoint promotion. The writer helper is not used by current production paths. | `sync_outbound.rs`: `load_base_store`; verified |
| `confirmed-log-frame-v1` — `confirmed.log`, magic `SCL1` | supported/emitted; binary fields n/a→lossless | CRC-framed prefix replay; startup can discard a torn suffix and opens at the verified length. | `sync_confirmed.rs`: `scan_log`, `ConfirmedLog::open`; verified |
| `confirmed-checkpoint-manifest-v3` — `checkpoint/manifest.json` | supported/emitted; reject→lossless | Content-addressed pages are written first; the fsynced manifest rename promotes the checkpoint. Startup converts legacy base v2 and collects unreferenced pages. | `sync_confirmed.rs`: `write_checkpoint`, `read_checkpoint_head`; verified |
| `confirmed-checkpoint-page-unversioned` — `checkpoint/pages/<sha256>` | supported/emitted; reject→lossless | A strict `SnapshotPage` has no separate in-band page-file version; SHA-256 and semantic state are validated before use. | `sync_confirmed.rs`: `page_bytes`, `materialize_checkpoint`; verified |
| `sync-blob-unversioned` — `blobs/<sha256>` | supported/emitted; bytes n/a→lossless | Raw content is accepted only through its SHA-256 address; inbound recovery may consume it. | `sync_inbound.rs`, `sync_outbound.rs`; verified |
| `local-mutation-generation-unversioned` — library `.scribe-sync/local-mutation-gen` | supported/emitted; scalar n/a→lossless | The first successful cooperating admission for a library's in-process session cell atomically publishes one saturating generation increment before content becomes durable. Later admissions using that cell do not increment it; registry teardown removes the cell, so a later session can increment again. | `sync_verified.rs`: `note_library_mutation_scoped`, `invalidate_durable`, `clear_registry_for`; verified |
| `page-ledger-record-v1` — library `.scribe-sync/page-ledger-v1.jsonl` | supported/emitted; reject→lossless | Mutation admission appends/fsyncs scope evidence first. Reads are byte/record bounded and ignore only a torn suffix. | `page_ledger.rs`: `SessionLedger::record`, `read_provenance`; verified |
| `local-verified-stamp-v3` — sync `local-verified-v1.json` | supported/emitted; reject→lossless | Full reconciliation atomically publishes a content-aware census stamp; mutation invalidation removes it. | `sync_verified.rs`: `evaluate_fast_path_inner`, `write_stamp_after_full_reconcile`; verified |
| `local-verified-cleared-marker-v1` — `local-verified-v1.json.cleared` | supported/emitted; marker n/a→opaque | Empty tombstone distinguishes a dirtied prior stamp; it is diagnostic, not content authority. | `sync_verified.rs`: `clear_stamp_file`, `evaluate_fast_path_inner`; inferred |
| `local-verified-baseline-v1` — sync `local-verified-baseline-v1.json` | supported/emitted; reject→lossless | Full reconcile atomically publishes a bounded, self-bound census used only for shadow diagnostics. | `verified_baseline.rs`: `read_baseline`, `write_baseline`; verified |
| `recovery-maintenance-marker-unversioned` — library `.scribe-sync/recovery-maintenance` | supported/emitted; marker n/a→lossless | Presence fails ordinary authoring closed during explicit recovery maintenance. | `sync_verified.rs`: `publish_recovery_maintenance`, `hold_authoring_for_maintenance`; verified |
| `recovery-intent-v1` — sync `recovery-intent-v1.json` | supported/emitted; reject→lossless | Explicit recovery freezes operation identities/sequences before submission; not invoked by normal startup. | `sync_recovery_exec.rs`: `RecoveryIntentV1`; verified |
| `recovery-complete-v1` — sync `recovery-complete-v1.json` | supported/emitted; reject→lossless | Atomic explicit-recovery finalization proof; not a normal startup path. | `sync_recovery_exec.rs`: `RecoveryCompleteV1`; verified |
| `adoption-plan-v1` — sibling `.adopt/plan.json` | supported/emitted; reject→lossless | Startup rolls a committed directory-swap plan forward or discards an uncommitted preparation. | `sync_adopt.rs`: `recover_interrupted_adoption`; verified |
| `adoption-workspace-v1` — sibling library/sync `.adopt` trees | supported/emitted; structure n/a→not established | A v1 plan owns staged directory swaps, but the workspace contains a closure of other formats rather than one standalone schema. | `sync_adopt.rs`: `Layout`, `publish`; inferred |
| `retired-sync-epoch-layout-unversioned` — sibling `sync.retired-<adoption>` | unsupported/emitted; structure n/a→opaque | Adoption preserves the old epoch by directory rename. It is not automatically replayed or deleted. | `sync_adopt.rs`; inferred |

The corresponding retired library directory is likewise opaque recovery-significant data. Its
contents are represented by the native-library entries above; the current application does not
provide a semantic reader for the retired tree as a unit.

The compiled `automatic=ordinary-library-mutation` token on the generation entry identifies the
class of event that can trigger invalidation; it is not a claim that every mutation receives a
distinct generation. Cooperating library, notebook, page, folder, local-identity, import, migration,
sync-materialization, and torn-journal recovery paths call the admission helpers. The first such
call for a library session cell bumps the counter and clears the verified stamp; later calls still
take admission guards and may append page-ledger scopes but skip another generation write. Sync
shutdown and adoption call `clear_registry_for`, which removes that in-memory cell.

Coverage is not complete. Active sync journals/checkpoints, device/pairing state, navigation, and
preferences live outside this library counter. Adoption publication swaps whole directories rather
than incrementing the former live library's counter. Some import-receipt recovery/carry writes are
not independently admitted, path helpers intentionally return an untracked guard for paths that do
not look like a library tree, and older, manual, or external writers need not call the helpers at
all. The staged adopted library merely carries the counter produced by its cooperating construction
paths. Consequently this generation is a fast-path invalidation diagnostic, not an authoritative
sequence of writes, adoption transaction identity, or proof that a complete device-data root stayed
unchanged.

## Pairing and device state

| Exact capability ID / durable location | R/W; unknown→rewrite | Automatic mutation and crash behavior | Source owner; confidence |
|---|---|---|---|
| `server-profile-v1` — device `server-profile.json` | unverified/emitted; reject→not established | Pairing/adoption atomically writes a private file. `load_profile` parses the `v` field but does not enforce `v == 1`; an unknown version can therefore enter current logic. | `server_link.rs`: `load_profile`, `save_profile`; unknown |
| `server-device-state-v1` — device `device.json` | unverified/emitted; reject→lossy | Product-sync startup reads a client ID or regenerates/replaces it when parsing fails. It does not enforce `v == 1`. | `server_link.rs`: `client_id`; unknown |

## Startup, notebook-open, and pre-`go` mutations

The compiled `automatic=` tokens are intentionally format-specific. The complete source order is:

1. `deployment/kindle/scribe-lifecycle-host.sh` runs before lifecycle `go`. It creates
   `/mnt/us/scribe-app/logs`, `state/sync`, and `state/device`, and appends
   `logs/lifecycle-host.log`. These are release-owned writes before `scribe-app` is selected.
2. `scribe-lifecycle` creates/appends its host log and owns `/tmp` coordination sockets/locks. On
   an admitted `go`, it creates/appends the child log before spawning. It does not open the
   notebook library, but these writes mean invoking the normal appmgrd handler is not a no-write
   capability query.
3. Normal `scribe-app` takes `/tmp/scribe-app.lock`, initializes devices, then
   `runtime::run_page_session` runs adoption recovery, opens/creates/migrates the library,
   establishes/reconciles sync, and recovers interrupted imports before Home readiness.
4. Opening a notebook calls `migrate_all_pages` for every referenced page before loading the
   selected page. Loading can truncate torn stroke/edit tails and repair background chrome.

The new command is dispatched immediately after collecting process arguments and before
`launch_clock::mark("process_entry")`. Its call graph is only command validation, compiled-contract
validation/encoding, and stdout write/flush. It does not consult environment paths. The subprocess
test redirects common path variables into an empty temporary root and confirms no files are
created there; source review, not that filesystem observation alone, establishes the isolation.

## Unresolved compatibility behavior

These facts keep coverage deliberately incomplete:

- Permissive library, notebook, page-header, render-profile, stroke, edit, preference, and
  background-identity structs can ignore unknown JSON fields. Whole-record rewrites drop them;
  journal appends preserve old bytes but do not prove semantic losslessness.
- `import-origin-v1`, `inbound-apply-marker-v1`, `server-profile-v1`, and
  `server-device-state-v1` do not consistently enforce their embedded version value. The compiled
  declaration marks their exact read behavior inferred, unverified, or unknown.
- Text/link line records, checkpoint page files, raw sync blobs, mutation generation, recovery
  maintenance, trash, retired epochs, and orphan structures lack an independent in-band version.
  Their `-unversioned` identifiers are explicit where appropriate; no version was invented.
- Retired epochs, trash, and orphans have preservation value but no complete semantic scanner or
  automatic safe replay contract.
- No A-writer→B-reader or B-writer→A-reader fixture matrix is bound to this declaration. Current
  same-release tests cannot establish backward compatibility.
- No full device-data probe, stable data root, data-writer boundary, USB exclusion, authenticated
  installed provenance, or pre-first-launch admission exists.
- `/mnt/us` directory-fsync and power-cut behavior, lifecycle reboot/replay behavior, and realistic
  full-scan duration still require Kindle hardware qualification. No hardware test was performed
  for this slice.

## Trust and future consumption

The output authenticates nothing by itself. A future release process must bind canonical capability
bytes (or their SHA-256) to the exact `scribe-app` hash covered by the authenticated release
manifest, and also bind the corresponding lifecycle, host-wrapper, and retained-A identities.
Release-time cross-version fixture results need their own real, reproducible attestation identity;
this slice includes no placeholder digest or signing material.

Future pairwise assessment must compare exact format/action sets, then run read-only probes and a
trusted scanner against stable current device data. It must not infer compatibility from matching
IDs alone. The three predecessor classes are:

- **Native-contract A:** authenticated executable provenance, this compiled contract, and—once
  implemented—an exact-A read-only probe.
- **Cataloged legacy A:** a signed catalog entry matching exact official executable identities,
  audited capability facts backed by cross-version tests, and a separate trusted
  version-neutral scanner. The result must state that the original A executable did not run a
  future probe.
- **Unsupported A:** missing/untrusted provenance, a modified or unknown executable, or missing
  supported evidence. Automatic candidate activation remains blocked.

## Library-only inspection implemented in Slice 3D-B2a.4.2a

`scribe-app --inspect-storage-library --data-dir PATH` is dispatched beside the capability command,
before normal application initialization. Its production call graph is confined to argument
validation, `storage_inspect_library::inspect_library`, read-only file descriptors, pure
validators/parsers, canonical report encoding, and stdout. It does not take the application
singleton, create the root, start runtime/sync threads, open hardware, run recovery, or access the
network.

It validates the native live-library entries in this inventory: library/notebook manifests and
titles; page headers v2–v5 and render profiles v1/v2; backgrounds; stroke v1/v2 and edit v1/v2
journals; optional text/link records; folders; import receipts/origins; and library-local identity.
It also reports migration/recovery markers, torn journals, temporary files, trash, and orphans
without modifying them. Permissive historical records with unknown fields are `UNVERIFIED`, not
silently treated as losslessly preserved. Formats for which no safe complete semantic reader exists
(including trash/orphan layout, navigation, and page-ledger semantics in this probe) remain
explicitly unverified.

The deterministic report is `IGNITE_STORAGE_LIBRARY_INSPECTION_V1`, with sorted findings and fixed
classification tokens: `VALID`, `UNSUPPORTED_FORMAT`, `CORRUPT`, `INCOMPLETE_TRANSACTION`,
`MISSING_REQUIRED`, `UNVERIFIED`, and `INSPECTION_ERROR`. It always states
`scope=library-only`, `coverage=partial`, `atomic_snapshot=false`,
`data_compatibility=unknown`, and `promotion_authorized=false`. Tree, file, record, JSON, finding,
and output limits are normative in the implementation and documented in
`kindle-data-compatibility-contract.md` Section 5.2.1.

The probe surrounds semantic inspection with equal path/type/size/content-hash censuses and uses
Unix `O_NOFOLLOW` plus file-identity/hard-link checks. This demonstrates no application writes and
detects ordinary concurrent changes, but it is not writer exclusion or an atomic snapshot. Native
Linux tests must exercise the Unix-only path; Kindle `/mnt/us` no-follow, hard-link reporting,
performance, and concurrent USB behavior remain hardware qualifications.

## External-state inspection implemented in Slice 3D-B2a.4.2b

`scribe-app --inspect-storage-external --data-dir LIBRARY --sync-dir SYNC --device-dir DEVICE
--state-dir STATE` is dispatched before application initialization. Its callable API is
`storage_inspect_external::inspect_external(&ExternalInspectionConfig)`. The report contract and
limits are specified in `kindle-data-compatibility-contract.md` Section 5.2.2.

The source audit that determines its exact scope is below. “Side effects” describes the normal
reader/open path deliberately not invoked by the diagnostic. “Essential” means the bytes can be
the only durable evidence needed to preserve or resume user work; it does not mean other rows are
safe to delete.

| Record family / namespace | Production reader and writer | Versions and pure validation | Normal-open/recovery side effects avoided | Read-only strategy and status | Unsynced-work essential |
|---|---|---|---|---|---|
| Library `.scribe-sync/identity.json` | `sync_local::read_identity`; `ensure_sync_identity` | identity v1; strict `IdentityFileV1::validate` | `ensure_*` creates/extends identities and lock files | strict no-follow read; bind `library_id` to active/staged/retired epoch; complete for represented v1 | Yes: identity continuity |
| Sync `replica.json`, `replica.lock` | `ensure_replica_state` | replica v1; `ReplicaStateV1::validate` | creates identity and lock; malformed identity is refused | strict no-follow read; lock bytes non-authoritative; bind `replica_id`; complete for v1 | Yes: authored sequence identity |
| Sync `config.json` | `read_config`, `OutboundSync::open_with`; developer provisioned | config v1 strict DTO | open can start network workers and update link state | parse without reporting URL/token; server credential validity `UNVERIFIED` | No, but needed to resume upload |
| `sync-state.json` | `establish_epoch`, inbound/ACK persistence | state v1/v2 strict DTO plus identity/cursor checks | v1 may be rewritten v2; cursors and ACKs may be recovered and persisted | inspect v1 transition constraints and v2 relationships; future versions unsupported | Yes |
| `genesis.json` | `establish_epoch`, genesis upload | canonical snapshot v1; `SyncSnapshotV1::validate`; SHA-256 | epoch creation captures/writes snapshot and blobs; upload mutates server/state | require exact canonical bytes, library identity, and state digest | Yes |
| `outbox.jsonl` | `parse_journal_from`, `reconcile`, uploader; append/rewrite writers | record v1 op/prep/commit/abort; canonical envelope v1 | startup ignores torn suffix and empty records, accepts CRLF as JSON whitespace, resolves prep, re-derives operations, compacts after ACK, uploads | one bounded streamed pass into the source transaction-state core; 16 MiB/record and 64 MiB retained-input caps; noncanonical accepted bytes are `UNVERIFIED`; committed pending is `VALID_PENDING`; prep/torn is preserved incomplete | **Yes** |
| `blobs/<sha256>` | outbound store/upload; inbound materializer | raw content addressed by lowercase SHA-256 plus referenced byte count/format | normal sync may download/write/upload; checkpoint GC is separate | merge constraints across every record family without overwrite; conflicting lengths are corrupt; stream hash every discovered reference; missing/mismatched and unreferenced cases visible; 128 MiB/file | **Yes** when referenced by queued operation/genesis |
| `inbox.jsonl` | `load_inbox`, `finish_apply`; tail fetch/append | record v1; canonical envelope v1; contiguous server sequence | fetch appends; finish rewrites/removes consumed items | strict streamed duplicate/conflict/gap validation; pending is `VALID_PENDING`; torn suffix preserved incomplete | Yes |
| `accepted.jsonl` | `load_accepted`, `finish_apply`; ACK append | record v1; canonical own envelope and identity uniqueness | ACK recovery can advance persisted `acked_client_seq`; finish compacts | strict streamed identity, duplicate, and monotonic server-sequence validation | Yes |
| `apply.json` | `read_marker`, `apply_inbound`, `finish_apply` | marker v1; scanner enforces version and target cursor | normal recovery rematerializes, persists confirmed state/cursor, removes marker | strict parse; presence is an interrupted transaction, never completed by inspection | **Yes** |
| Legacy `base-state.json` | `load_base_store`; historical writer | base v2, snapshot v1, history v1, `LibraryState::validate` | converts to checkpoint v3 and removes legacy file after promotion | bounded strict read (128 MiB), reducer validation; reported pending conversion; shadowing visible | Yes |
| `confirmed.log` | `scan_log`, `replay_log`, `ConfirmedLog::open` | binary SCL1, CRC, contiguous sequence, canonical envelope v1 | append-open truncates torn tail; startup replay can persist recovered cursor/compact | factored source scan core over diagnostic-opened descriptor; one payload at a time, 64 MiB cumulative diagnostic replay and 200,000-record limits, no retained diagnostic compaction index; torn tail preserved | Yes |
| `checkpoint/manifest.json`, `pages/<sha256>` | `read_checkpoint_head`, `materialize_checkpoint`, checkpoint writer/GC | manifest v3; strict `SnapshotPage`; snapshot/history reducer validation | startup can collect unreferenced pages and promote/compact checkpoints | strict manifest, per-page hash/identity, full assembled state validation; streaming parse/canonical comparison, 128 MiB aggregate reconstructed source; unreferenced pages preserved pending | Yes |
| `local-verified-v1.json`, `.cleared`, library mutation generation/page ledger | `evaluate_fast_path`, `read_provenance`; reconcile/invalidation writers | stamp v3, generation scalar, ledger record v1 | reconcile may rewrite stamp/baseline; invalidation removes stamp/appends ledger | structural validation only; always state that generation/stamp/ledger are cache/cooperating diagnostics, not compatibility proof | No authority; ledger can aid diagnosis |
| Recovery intent, completion, claim, and library maintenance marker | explicit `sync_recovery_exec` and `sync_verified` functions | intent v1, completion v1, canonical frozen envelopes, marker `1\n` | prepare/revalidate can bind identities, submit, reconcile, finalize, remove hold | strict invariant/candidate binding; every active marker is incomplete or historical/unverified and preserved | **Yes** |
| Device `device.json`, `server-profile.json`, replacement temps | `client_id`, `load_profile`; private atomic writers/pairing/adoption | scanner requires v1 and strict fields/UUIDs | `client_id` regenerates malformed identity; pairing/profile publication writes credentials | strict read and server binding; secrets never emitted; authentication/server truth remains `UNVERIFIED` | Needed to resume authenticated sync |
| External `preferences.json` in sync/device/state | `prefs::load`; `save_side_button` | preference v1 and closed side-button enum | malformed/unknown defaults; save drops unknown fields | strict parse each present location; temps reported incomplete; no fallback/defaulting | No |
| `<sync>.adopt/{plan.json,sync,server-profile.json}`, `<library>.adopt`, `.adopt.lock` | `recover_interrupted_adoption`, `adopt_library` | plan v1 UUID and staged epoch/profile schemas | startup discards uncommitted staging or rolls committed directory swaps forward | census every workspace; strict plan; scan staged epoch and cross-bind identities; never recover/discard/swap | **Yes** |
| `<sync>.retired-<adoption>`, `<library>.retired-<adoption>` | created by adoption publication; no semantic live reader/cleanup | closure of formats above; retired layout unversioned | no supported automatic replay or deletion | discover exact sibling grammar, full census, scan paired retired sync epoch, preserve opaque library semantics as `UNVERIFIED`; missing pair incomplete | **Yes** |

The scanner also inventories unknown and atomic-temp objects rather than normalizing them. It calls
none of `OutboundSync::open_with`, `establish_epoch`, `load_base_store`, `ConfirmedLog::open`,
`finish_apply`, identity/replica `ensure_*`, server `client_id`, adoption recovery/publication,
recovery submit/reconcile/release, preference fallback, or network/background-worker entry points.

Directory discovery is fail-closed before allocation can grow past its configured cap. Every
iterator result and safe component is checked across checkpoint pages/artifacts, blobs, device and
state, adoption, retired siblings, and the active sync root. A read error, unsafe name, or overflow
is `INSPECTION_ERROR`/exit 3; partial discovery is never reported as a clean scan. The retained
sibling maps are built once per semantic pass rather than repeatedly rescanning them while pairing.

Large genesis, legacy-base, and checkpoint inputs are strict-validated, deserialized, and compared
with canonical bytes in separate streaming file passes, avoiding simultaneous raw/generic/typed/
canonical full-size copies. JSONL keeps one at-most-16-MiB line and its typed record while retained
semantic input is capped at 64 MiB. Reconstructed checkpoint source is capped at 128 MiB and
diagnostic confirmed replay at 64 MiB; strict JSON duplicate tracking refuses more than 4,096 keys
in one object. Typed reducer state and collection overhead remain resident, so these are defensible
source bounds rather than an exact RSS guarantee. A Linux-only 24 MiB structured-input stress
fixture is provided for `/usr/bin/time -v`; target-device memory margin is still a hardware
qualification.

Same-release fixture tests cover required versus optional roots, fresh/unpaired state, v1/v2
epochs, bounded oversized and aggregate JSONL input, canonical and historical journal encodings,
outbound/inbound pending and torn states, accepted/apply state, legacy confirmed state,
version/identity/digest contradictions, conflicting blob declarations, pairing unknown fields,
preferences, adoption staging, correctly namespaced retired mismatches, recovery markers,
filesystem/resource attacks, injected directory-enumeration errors, reduced aggregate limits,
collision-safe owned cleanup, concurrent census changes, deterministic reports, and complete
before/after byte preservation. Linux-only tests cover symlink, hard-link, special-file refusal, and
the large structured-input stress fixture. These fixtures test diagnostic behavior; they are not A/B
compatibility evidence.

The existing capability command remains byte-for-byte unchanged and retains
`coverage=incomplete`, `data_compatibility=unknown`, and `promotion_authorized=false`. Remaining
work is exact historical release-pair fixtures, authenticated installed probe provenance, a stable
writer-excluded data boundary, a composed state root, pairwise evaluation, and pre-first-launch
admission. A clean external report is structural diagnostic information only.
