//! `scribe_app` library: the Kindle notebook runtime's modules, shared by the `scribe-app`
//! binary and host tools (e.g. `scribe-sync-tool`, which reuses the durable-format validators
//! read-only). The physical runtime (`runtime`) is unix-only.

// Physical Kindle runtime is unix-only; host builds still compile session/orch for unit tests.
#![cfg_attr(not(unix), allow(dead_code))]

pub mod app_ui;
mod background_identity_tests;
pub mod cadence;
pub mod chrome;
pub mod content;
pub mod discovery;
pub mod display_events;
pub mod eraser;
mod f10_tests;
mod f111_tests;
mod f112_tests;
mod f113_tests;
mod f11_tests;
mod f2_nav_safety_tests;
mod f2_tests;
mod f3_tests;
mod f4_tests;
mod f5_tests;
mod f6_contract_tests;
mod f6_tests;
mod f7_tests;
mod f7_v4_migrate_compat_tests;
mod f8_tests;
mod f9_tests;
mod folder_ui_tests;
pub mod folders;
pub mod fullscreen;
pub mod home;
pub mod ink;
pub mod input_lag;
pub mod keyboard;
#[cfg(feature = "kindle_supervisor_harness")]
mod kindle_supervisor_harness;
pub mod lasso;
pub mod launch_clock;
pub mod launch_diag;
pub mod launch_health;
pub mod library;
pub mod link_paint;
pub mod link_picker;
mod link_tests;
pub mod modal;
pub mod nav;
pub mod nav_state;
pub mod nbk_import;
pub mod notebook;
pub mod open_page;
pub mod orch;
pub mod page_label;
pub mod page_ledger;
pub mod page_thumb;
pub mod prefs;
pub mod pressure;
pub mod pressure_curve;
pub mod profile;
pub mod release_manifest;
pub mod render;
#[cfg(unix)]
pub mod runtime;
mod s1_sync_tests;
mod s2_erase_parity_tests;
pub mod server_link;
pub mod session;
pub mod settings_ui;
pub mod singleton;
pub mod sleep_ui;
pub mod storage_capabilities;
pub mod storage_compatibility_preflight;
pub mod storage_inspect_external;
pub mod storage_inspect_library;
pub mod storage_installed_slot_binding;
pub mod storage_release_provenance;
pub mod storage_writer_exclusion;
pub mod store;
pub mod sync_adopt;
pub mod sync_local;
pub mod sync_materialize;
pub mod sync_outbound;
mod sync_recovery_attest;
mod sync_recovery_bind;
mod sync_recovery_compare;
mod sync_recovery_exec;
mod sync_recovery_extract;
mod sync_recovery_observe;
mod sync_recovery_plan;
mod sync_recovery_policy;
mod sync_recovery_rehearse;
mod sync_recovery_stream;
mod sync_recovery_supervise;
mod sync_recovery_tail;
mod sync_recovery_tunnel;
mod sync_startup_diag;
#[cfg(feature = "kindle_supervisor_harness")]
pub use kindle_supervisor_harness::run as run_kindle_supervisor_harness;
#[cfg(test)]
mod sync_recovery_observe_tests;
#[cfg(test)]
mod sync_recovery_stream_tests;
#[cfg(test)]
mod sync_recovery_supervise_tests;
pub mod sync_ui;
pub mod sync_verified;
pub mod text_paint;
pub mod tools_ui;
pub mod touch;
pub mod ui_gfx;
pub mod verified_baseline;
