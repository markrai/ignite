//! Private NBK parity against an externally configured fixture root.
//!
//! These tests are feature-gated (`parity-samples`) and `#[ignore]` so ordinary
//! `cargo test` / `--all-features` never touch private notebook data.
//!
//! Manifest schema regression tests in this file are synthetic and always run
//! when the `parity-samples` feature is enabled (they do not need private data).
//!
//! ## Fixture layout (`IGNITE_NBK_PARITY_ROOT`)
//!
//! Preferred paired layout:
//!
//! ```text
//! $IGNITE_NBK_PARITY_ROOT/
//!   <fixture_id>/
//!     source/           # notebook dir containing `nbk` (+ optional actions.log, …)
//!     expected.json     # decode golden (same schema as historical parity goldens)
//! ```
//!
//! Alternate layout (separate trees):
//!
//! ```text
//! $IGNITE_NBK_PARITY_ROOT/
//!   samples/<fixture_id>/   # notebook dir with `nbk`
//!   expected/<fixture_id>.json
//! ```
//!
//! Discovery is deterministic (sorted fixture ids). At least one valid pair is required.
//!
//! ## Import expectations (`IGNITE_NBK_PARITY_EXPECTATIONS`)
//!
//! Absolute path to a private JSON manifest **outside** this repository. Schema
//! (synthetic placeholders only — never commit real fixture ids here):
//!
//! ```json
//! {
//!   "version": 1,
//!   "fixtures": {
//!     "example-importable-fixture": "importable",
//!     "example-pending-fixture": "pending_actions"
//!   }
//! }
//! ```
//!
//! Outcomes: `importable` | `pending_actions`. Every discovered fixture must
//! appear exactly once; every manifest entry must match a discovered fixture;
//! at least one of each outcome is required. Duplicate keys (fixture ids or
//! top-level fields) are rejected during JSON deserialization.
//!
//! ## Run (private machines only)
//!
//! ```text
//! set IGNITE_NBK_PARITY_ROOT=...
//! set IGNITE_NBK_PARITY_EXPECTATIONS=...
//! cargo test -p nbk-reader --features parity-samples --test parity -- --ignored
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use nbk_reader::{container, decode, decode_snapshot, read_source, NbkError};
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::Deserialize;

const ENV_ROOT: &str = "IGNITE_NBK_PARITY_ROOT";
const ENV_EXPECTATIONS: &str = "IGNITE_NBK_PARITY_EXPECTATIONS";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImportOutcome {
    Importable,
    PendingActions,
}

impl<'de> Deserialize<'de> for ImportOutcome {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "importable" => Ok(ImportOutcome::Importable),
            "pending_actions" => Ok(ImportOutcome::PendingActions),
            other => Err(de::Error::unknown_variant(
                other,
                &["importable", "pending_actions"],
            )),
        }
    }
}

/// Fixture map that rejects duplicate keys while streaming JSON (before collapse).
#[derive(Clone, Debug)]
struct StrictFixtures(BTreeMap<String, ImportOutcome>);

impl<'de> Deserialize<'de> for StrictFixtures {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StrictFixturesVisitor;

        impl<'de> Visitor<'de> for StrictFixturesVisitor {
            type Value = StrictFixtures;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a non-empty fixtures object with unique keys")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    let value: ImportOutcome = map.next_value()?;
                    if out.insert(key.clone(), value).is_some() {
                        return Err(de::Error::custom(format!(
                            "duplicate fixture entry {key:?}"
                        )));
                    }
                }
                if out.is_empty() {
                    return Err(de::Error::custom("\"fixtures\" must not be empty"));
                }
                Ok(StrictFixtures(out))
            }
        }

        deserializer.deserialize_map(StrictFixturesVisitor)
    }
}

/// Outer expectation document. Serde's struct visitor rejects duplicate fields;
/// `deny_unknown_fields` rejects unexpected top-level keys.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectationManifest {
    version: u64,
    fixtures: StrictFixtures,
}

#[derive(Clone, Debug)]
struct FixturePair {
    id: String,
    source_dir: PathBuf,
    expected_json: PathBuf,
}

fn abs_env_path(var: &str) -> PathBuf {
    let raw = std::env::var(var).unwrap_or_default();
    let raw = raw.trim();
    assert!(
        !raw.is_empty(),
        "{var} is unset or empty. Point it at an absolute path outside the Ignite tree."
    );
    let path = PathBuf::from(raw);
    assert!(
        path.is_absolute(),
        "{var} must be an absolute path, got {}",
        path.display()
    );
    path
}

fn parity_root() -> PathBuf {
    let root = abs_env_path(ENV_ROOT);
    assert!(
        root.is_dir(),
        "{ENV_ROOT} is not a directory: {}",
        root.display()
    );
    root
}

fn is_notebook_dir(dir: &Path) -> bool {
    dir.join("nbk").is_file()
}

fn discover_fixtures(root: &Path) -> Vec<FixturePair> {
    let mut pairs = Vec::new();

    // Preferred: <root>/<id>/{source/, expected.json}
    if let Ok(entries) = fs::read_dir(root) {
        for ent in entries.flatten() {
            let path = ent.path();
            if !path.is_dir() {
                continue;
            }
            let id = ent.file_name().to_string_lossy().into_owned();
            if id == "samples" || id == "expected" {
                continue;
            }
            let source = path.join("source");
            let expected = path.join("expected.json");
            if is_notebook_dir(&source) && expected.is_file() {
                pairs.push(FixturePair {
                    id,
                    source_dir: source,
                    expected_json: expected,
                });
            }
        }
    }

    // Alternate: <root>/samples/<id>/ + <root>/expected/<id>.json
    let samples = root.join("samples");
    let expected_dir = root.join("expected");
    if samples.is_dir() && expected_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&samples) {
            for ent in entries.flatten() {
                let source = ent.path();
                if !source.is_dir() || !is_notebook_dir(&source) {
                    continue;
                }
                let id = ent.file_name().to_string_lossy().into_owned();
                let expected = expected_dir.join(format!("{id}.json"));
                if expected.is_file() {
                    // Avoid duplicates if preferred layout also matched.
                    if !pairs.iter().any(|p| p.id == id) {
                        pairs.push(FixturePair {
                            id,
                            source_dir: source,
                            expected_json: expected,
                        });
                    }
                }
            }
        }
    }

    pairs.sort_by(|a, b| a.id.cmp(&b.id));
    pairs
}

fn load_fixtures() -> Vec<FixturePair> {
    let root = parity_root();
    let pairs = discover_fixtures(&root);
    assert!(
        !pairs.is_empty(),
        "{ENV_ROOT}={}: no fixture pairs found. Need either \
         <id>/source/nbk + <id>/expected.json, or samples/<id>/nbk + expected/<id>.json",
        root.display()
    );
    pairs
}

/// Deserialize the expectation manifest with duplicate-key detection.
fn parse_expectation_manifest(text: &str) -> Result<ExpectationManifest, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("expectation manifest is empty".into());
    }
    serde_json::from_str(trimmed).map_err(|e| e.to_string())
}

fn validate_expectation_coverage(
    expectations: &BTreeMap<String, ImportOutcome>,
    fixtures: &[FixturePair],
) -> Result<(), String> {
    let discovered: BTreeSet<&str> = fixtures.iter().map(|f| f.id.as_str()).collect();
    let declared: BTreeSet<&str> = expectations.keys().map(|s| s.as_str()).collect();

    let missing: Vec<_> = discovered.difference(&declared).copied().collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing expectations for discovered fixtures: {missing:?}"
        ));
    }
    let stale: Vec<_> = declared.difference(&discovered).copied().collect();
    if !stale.is_empty() {
        return Err(format!(
            "unknown/stale fixture entries (not discovered): {stale:?}"
        ));
    }

    let importable = expectations
        .values()
        .filter(|o| **o == ImportOutcome::Importable)
        .count();
    let pending = expectations
        .values()
        .filter(|o| **o == ImportOutcome::PendingActions)
        .count();
    if importable < 1 {
        return Err(format!(
            "need at least one \"importable\" fixture (got {importable})"
        ));
    }
    if pending < 1 {
        return Err(format!(
            "need at least one \"pending_actions\" fixture (got {pending})"
        ));
    }
    Ok(())
}

/// Load and validate the private expectation manifest against discovered fixtures.
fn load_import_expectations(fixtures: &[FixturePair]) -> BTreeMap<String, ImportOutcome> {
    let path = abs_env_path(ENV_EXPECTATIONS);
    assert!(
        path.is_file(),
        "{ENV_EXPECTATIONS} is not a file: {}",
        path.display()
    );
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed reading {ENV_EXPECTATIONS} {}: {e}", path.display()));
    let manifest = parse_expectation_manifest(&text)
        .unwrap_or_else(|e| panic!("invalid expectation manifest in {}: {e}", path.display()));
    assert_eq!(
        manifest.version, 1,
        "{ENV_EXPECTATIONS}: unsupported version {} (expected 1)",
        manifest.version
    );
    validate_expectation_coverage(&manifest.fixtures.0, fixtures)
        .unwrap_or_else(|e| panic!("{ENV_EXPECTATIONS}: {e}"));
    manifest.fixtures.0
}

fn load_expected(path: &Path) -> serde_json::Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed reading expected golden {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("invalid JSON in {}: {e}", path.display()))
}

/// Fingerprint every file under each fixture source dir (read-only integrity check).
fn source_fingerprint(fixtures: &[FixturePair]) -> Vec<(String, u64, std::time::SystemTime)> {
    let mut out = Vec::new();
    for f in fixtures {
        for e in fs::read_dir(&f.source_dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", f.source_dir.display()))
        {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            if !m.is_file() {
                continue;
            }
            out.push((
                format!("{}/{}", f.id, e.file_name().to_string_lossy()),
                m.len(),
                m.modified().unwrap(),
            ));
        }
    }
    out.sort();
    out
}

fn assert_matches_expected(id: &str, nb: &nbk_reader::DecodedNotebook, golden: &serde_json::Value) {
    let ids: Vec<&str> = nb.pages.iter().map(|p| p.page_id.as_str()).collect();
    let g_ids: Vec<&str> = golden["live_page_ids"]
        .as_array()
        .unwrap_or_else(|| panic!("{id}: live_page_ids missing"))
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(ids, g_ids, "{id}: page order");
    assert_eq!(
        nb.template_ids,
        golden["template_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect::<Vec<_>>(),
        "{id}: templates"
    );
    assert_eq!(
        nb.diagnostics.orphan_page_count as u64,
        golden["orphan_page_count"].as_u64().unwrap(),
        "{id}: orphans"
    );
    assert_eq!(
        nb.stroke_count() as u64,
        golden["decoded_stroke_count"].as_u64().unwrap(),
        "{id}: strokes"
    );
    assert_eq!(
        nb.diagnostics.non_ion_payloads, 0,
        "{id}: bytescan path unused"
    );

    for (p, g) in nb.pages.iter().zip(golden["pages"].as_array().unwrap()) {
        let ctx = format!("{id}/{}", p.page_id);
        assert_eq!(p.width as i64, g["width"].as_i64().unwrap(), "{ctx} width");
        assert_eq!(
            p.height as i64,
            g["height"].as_i64().unwrap(),
            "{ctx} height"
        );
        assert_eq!(
            p.template_id.as_deref(),
            g["template_id"].as_str(),
            "{ctx} template"
        );
        let counts: Vec<u64> = p.strokes.iter().map(|s| s.xs.len() as u64).collect();
        let g_counts: Vec<u64> = g["point_counts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect();
        assert_eq!(counts, g_counts, "{ctx} per-stroke point counts");
        let pts = || p.strokes.iter().flat_map(|s| s.xs.iter().zip(&s.ys));
        assert_eq!(
            pts().map(|(x, _)| x).sum::<i64>(),
            g["sum_x"].as_i64().unwrap(),
            "{ctx} sum_x"
        );
        assert_eq!(
            pts().map(|(_, y)| y).sum::<i64>(),
            g["sum_y"].as_i64().unwrap(),
            "{ctx} sum_y"
        );
        let pressures: Vec<i64> = p
            .strokes
            .iter()
            .filter_map(|s| s.pressure.as_ref())
            .flatten()
            .copied()
            .collect();
        assert_eq!(
            pressures.len() as u64,
            g["points_with_pressure"].as_u64().unwrap(),
            "{ctx} pressure count"
        );
        assert_eq!(
            pressures.iter().sum::<i64>(),
            g["sum_pressure"].as_i64().unwrap(),
            "{ctx} sum_pressure"
        );
        let first = pts().next().map(|(x, y)| vec![*x, *y]);
        let last = pts().last().map(|(x, y)| vec![*x, *y]);
        let as_vec = |v: &serde_json::Value| {
            v.as_array()
                .map(|a| a.iter().map(|n| n.as_i64().unwrap()).collect::<Vec<_>>())
        };
        assert_eq!(first, as_vec(&g["first_point"]), "{ctx} first point");
        assert_eq!(last, as_vec(&g["last_point"]), "{ctx} last point");
    }
}

#[test]
#[ignore = "private fixtures: set IGNITE_NBK_PARITY_ROOT (+ EXPECTATIONS for selection) and run with --features parity-samples -- --ignored"]
fn materialized_decode_matches_expected_on_every_fixture() {
    let fixtures = load_fixtures();
    let before = source_fingerprint(&fixtures);
    for f in &fixtures {
        let golden = load_expected(&f.expected_json);
        let snap = read_source(&f.source_dir)
            .unwrap_or_else(|e| panic!("{}: read_source {}: {e}", f.id, f.source_dir.display()));
        let store = container::read_fragments(&snap.nbk)
            .unwrap_or_else(|e| panic!("{}: read_fragments: {e}", f.id));
        let nb = decode::decode_store(&store);
        assert_matches_expected(&f.id, &nb, &golden);
    }
    assert_eq!(
        source_fingerprint(&fixtures),
        before,
        "fixture sources must never be modified"
    );
}

#[test]
#[ignore = "private fixtures: set IGNITE_NBK_PARITY_ROOT and IGNITE_NBK_PARITY_EXPECTATIONS; run with --features parity-samples -- --ignored"]
fn selection_imports_materialized_state_and_refuses_pending_action_logs() {
    let fixtures = load_fixtures();
    let expectations = load_import_expectations(&fixtures);

    for f in &fixtures {
        let outcome = expectations
            .get(&f.id)
            .copied()
            .unwrap_or_else(|| panic!("internal: missing expectation for {}", f.id));
        let golden = load_expected(&f.expected_json);
        let snap =
            read_source(&f.source_dir).unwrap_or_else(|e| panic!("{}: read_source: {e}", f.id));

        match outcome {
            ImportOutcome::Importable => {
                let nb = decode_snapshot(&snap).unwrap_or_else(|e| {
                    panic!(
                        "{}: expected importable decode_snapshot success, got error: {e}",
                        f.id
                    )
                });
                assert_eq!(
                    (nb.pages.len() as u64, nb.stroke_count() as u64),
                    (
                        golden["live_page_ids"].as_array().unwrap().len() as u64,
                        golden["decoded_stroke_count"].as_u64().unwrap()
                    ),
                    "{}: snapshot page/stroke counts",
                    f.id
                );
                if nb.stroke_count() > 0 {
                    assert!(
                        nb.book_id.is_some(),
                        "{}: importable notebooks with strokes expose book_id",
                        f.id
                    );
                }
            }
            ImportOutcome::PendingActions => match decode_snapshot(&snap) {
                Ok(_) => panic!(
                    "{}: expected NbkError::PendingActions refusal, but import succeeded",
                    f.id
                ),
                Err(NbkError::PendingActions { .. }) => {}
                Err(e) => panic!(
                    "{}: expected NbkError::PendingActions, got different error: {e}",
                    f.id
                ),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Synthetic manifest validation regressions (no private fixtures / env vars)
// ---------------------------------------------------------------------------

fn synthetic_pairs(ids: &[&str]) -> Vec<FixturePair> {
    ids.iter()
        .map(|id| FixturePair {
            id: (*id).to_string(),
            source_dir: PathBuf::from("synthetic-unused-source"),
            expected_json: PathBuf::from("synthetic-unused-expected"),
        })
        .collect()
}

#[test]
fn expectation_manifest_accepts_unique_fixture_keys() {
    let text = r#"{
        "version": 1,
        "fixtures": {
            "example-importable": "importable",
            "example-pending": "pending_actions"
        }
    }"#;
    let manifest = parse_expectation_manifest(text).expect("valid unique-key manifest");
    assert_eq!(manifest.version, 1);
    assert_eq!(
        manifest.fixtures.0.get("example-importable"),
        Some(&ImportOutcome::Importable)
    );
    assert_eq!(
        manifest.fixtures.0.get("example-pending"),
        Some(&ImportOutcome::PendingActions)
    );
}

#[test]
fn expectation_manifest_rejects_duplicate_fixture_key_conflicting_outcomes() {
    let text = r#"{
        "version": 1,
        "fixtures": {
            "example-fixture": "importable",
            "example-fixture": "pending_actions"
        }
    }"#;
    let err = parse_expectation_manifest(text).expect_err("duplicate key must fail");
    assert!(
        err.contains("duplicate fixture entry"),
        "unexpected error: {err}"
    );
}

#[test]
fn expectation_manifest_rejects_duplicate_fixture_key_identical_outcomes() {
    let text = r#"{
        "version": 1,
        "fixtures": {
            "example-fixture": "importable",
            "example-fixture": "importable"
        }
    }"#;
    let err = parse_expectation_manifest(text).expect_err("identical duplicate must fail");
    assert!(
        err.contains("duplicate fixture entry"),
        "unexpected error: {err}"
    );
}

#[test]
fn expectation_manifest_rejects_duplicate_top_level_fixtures_field() {
    let text = r#"{
        "version": 1,
        "fixtures": {
            "example-importable": "importable",
            "example-pending": "pending_actions"
        },
        "fixtures": {
            "example-other": "importable",
            "example-pending-2": "pending_actions"
        }
    }"#;
    let err = parse_expectation_manifest(text).expect_err("duplicate fixtures field must fail");
    assert!(
        err.to_lowercase().contains("duplicate") || err.contains("fixtures"),
        "unexpected error: {err}"
    );
}

#[test]
fn expectation_manifest_rejects_duplicate_top_level_version_field() {
    let text = r#"{
        "version": 1,
        "version": 1,
        "fixtures": {
            "example-importable": "importable",
            "example-pending": "pending_actions"
        }
    }"#;
    let err = parse_expectation_manifest(text).expect_err("duplicate version field must fail");
    assert!(
        err.to_lowercase().contains("duplicate") || err.contains("version"),
        "unexpected error: {err}"
    );
}

#[test]
fn expectation_manifest_rejects_unknown_outcome() {
    let text = r#"{
        "version": 1,
        "fixtures": {
            "example-importable": "importable",
            "example-pending": "maybe_pending"
        }
    }"#;
    let err = parse_expectation_manifest(text).expect_err("unknown outcome must fail");
    assert!(
        err.contains("maybe_pending")
            || err.contains("unknown variant")
            || err.contains("pending_actions"),
        "unexpected error: {err}"
    );
}

#[test]
fn expectation_manifest_rejects_unknown_top_level_field() {
    let text = r#"{
        "version": 1,
        "fixtures": {
            "example-importable": "importable",
            "example-pending": "pending_actions"
        },
        "extra": true
    }"#;
    let err = parse_expectation_manifest(text).expect_err("unknown field must fail");
    assert!(
        err.contains("unknown field") || err.contains("extra"),
        "unexpected error: {err}"
    );
}

#[test]
fn expectation_manifest_rejects_missing_required_field() {
    let missing_fixtures = r#"{ "version": 1 }"#;
    let err = parse_expectation_manifest(missing_fixtures).expect_err("missing fixtures");
    assert!(
        err.contains("fixtures") || err.contains("missing field"),
        "unexpected error: {err}"
    );

    let missing_version = r#"{
        "fixtures": {
            "example-importable": "importable",
            "example-pending": "pending_actions"
        }
    }"#;
    let err = parse_expectation_manifest(missing_version).expect_err("missing version");
    assert!(
        err.contains("version") || err.contains("missing field"),
        "unexpected error: {err}"
    );
}

#[test]
fn expectation_coverage_checks_remain_effective() {
    let text = r#"{
        "version": 1,
        "fixtures": {
            "example-importable": "importable",
            "example-pending": "pending_actions"
        }
    }"#;
    let manifest = parse_expectation_manifest(text).unwrap();
    assert_eq!(manifest.version, 1);

    // Exact coverage succeeds.
    validate_expectation_coverage(
        &manifest.fixtures.0,
        &synthetic_pairs(&["example-importable", "example-pending"]),
    )
    .expect("exact coverage should pass");

    // Missing discovered fixture.
    let err = validate_expectation_coverage(
        &manifest.fixtures.0,
        &synthetic_pairs(&["example-importable", "example-pending", "example-extra"]),
    )
    .expect_err("missing expectation");
    assert!(err.contains("missing expectations"), "{err}");

    // Stale manifest entry.
    let err = validate_expectation_coverage(
        &manifest.fixtures.0,
        &synthetic_pairs(&["example-importable"]),
    )
    .expect_err("stale expectation");
    assert!(err.contains("stale") || err.contains("unknown"), "{err}");

    // No pending_actions among declared outcomes.
    let import_only = BTreeMap::from([
        ("a".into(), ImportOutcome::Importable),
        ("b".into(), ImportOutcome::Importable),
    ]);
    let err = validate_expectation_coverage(&import_only, &synthetic_pairs(&["a", "b"]))
        .expect_err("need pending");
    assert!(err.contains("pending_actions"), "{err}");
}
