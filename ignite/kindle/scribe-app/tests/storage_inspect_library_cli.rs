use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use kindle_platform::rect_checksum;
use scribe_app::library::{
    PersistedLibraryManifestV1, PersistedNotebookInfoV1, LIBRARY_MANIFEST_FILE, NOTEBOOKS_DIR,
    NOTEBOOK_INFO_FILE,
};
use scribe_app::notebook::{PersistedNotebookManifestV1, MANIFEST_FILE};
use scribe_app::profile::RenderProfile;
use scribe_app::store::{
    BackgroundIdentityV1, PersistedPageHeader, BG_LAYOUT_VERSION, EDIT_JOURNAL_VERSION,
    FORMAT_VERSION,
};
use sha2::{Digest, Sha256};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "scribe-library-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let notebook = root.join(NOTEBOOKS_DIR).join("notebook-1");
        let page = notebook.join("page-1");
        fs::create_dir_all(&page).unwrap();
        write_json(
            &root.join(LIBRARY_MANIFEST_FILE),
            &PersistedLibraryManifestV1 {
                v: 1,
                notebook_ids: vec!["notebook-1".into()],
                active_notebook_id: "notebook-1".into(),
                next_notebook_ordinal: 2,
            },
        );
        write_json(
            &notebook.join(NOTEBOOK_INFO_FILE),
            &PersistedNotebookInfoV1 {
                v: 1,
                title: "CLI fixture".into(),
            },
        );
        write_json(
            &notebook.join(MANIFEST_FILE),
            &PersistedNotebookManifestV1 {
                v: 1,
                notebook_id: "notebook-1".into(),
                page_ids: vec!["page-1".into()],
                active_page_id: "page-1".into(),
                next_page_ordinal: 2,
            },
        );
        let background = vec![0xff; 16];
        let profile = RenderProfile::sealed_v2_pressure(0).unwrap();
        write_json(
            &page.join("page.meta"),
            &PersistedPageHeader {
                v: FORMAT_VERSION,
                page_id: "page-1".into(),
                canonical_width: notebook_core::CANONICAL_WIDTH,
                canonical_height: notebook_core::CANONICAL_HEIGHT,
                panel_width: 4,
                panel_height: 4,
                line_length: 4,
                background_bytes: 16,
                background_checksum: rect_checksum(&background),
                bg_layout_version: BG_LAYOUT_VERSION,
                render: serde_json::to_value(profile.to_persisted_v2().unwrap()).unwrap(),
                edit_journal_version: EDIT_JOURNAL_VERSION,
                background_identity: Some(BackgroundIdentityV1::from_pixels(&background)),
            },
        );
        fs::write(page.join("background.raw"), background).unwrap();
        fs::write(page.join("strokes.jsonl"), b"").unwrap();
        fs::write(page.join("edits.jsonl"), b"").unwrap();
        Self(root)
    }

    fn root(&self) -> &Path {
        &self.0
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_json(path: &Path, value: &impl serde::Serialize) {
    let mut bytes = serde_json::to_vec_pretty(value).unwrap();
    bytes.push(b'\n');
    fs::write(path, bytes).unwrap();
}

fn manifest(root: &Path) -> BTreeMap<String, (u64, String)> {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<String, (u64, String)>) {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let bytes = fs::read(&path).unwrap();
                out.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    (bytes.len() as u64, format!("{:x}", Sha256::digest(&bytes))),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn process_cli_emits_exact_report_before_application_initialization() {
    let fixture = Fixture::new();
    let isolated = fixture.root().join("isolated-runtime");
    let before = manifest(fixture.root());
    let expected = scribe_app::storage_inspect_library::inspect_library(fixture.root())
        .canonical_bytes()
        .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
        .args([
            "--inspect-storage-library",
            "--data-dir",
            fixture.root().to_str().unwrap(),
        ])
        .current_dir(fixture.root())
        .env("HOME", &isolated)
        .env("USERPROFILE", &isolated)
        .env("TMPDIR", &isolated)
        .env("SCRIBE_SINGLETON_PATH", isolated.join("scribe-app.lock"))
        .output()
        .expect("run library inspection command");

    assert!(
        output.status.success(),
        "status={} stderr={} stdout={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout, expected);
    assert!(!output
        .stdout
        .windows(b"launch_ms".len())
        .any(|w| w == b"launch_ms"));
    assert!(!isolated.exists());
    assert_eq!(before, manifest(fixture.root()));
}

#[test]
fn process_cli_uses_distinct_issue_status_and_preserves_torn_bytes() {
    let fixture = Fixture::new();
    let journal = fixture
        .root()
        .join(NOTEBOOKS_DIR)
        .join("notebook-1")
        .join("page-1")
        .join("strokes.jsonl");
    fs::write(&journal, b"partial").unwrap();
    let before = manifest(fixture.root());
    let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
        .args([
            "--inspect-storage-library",
            "--data-dir",
            fixture.root().to_str().unwrap(),
        ])
        .output()
        .expect("run library inspection command");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    assert!(String::from_utf8_lossy(&output.stdout).contains("INCOMPLETE_TRANSACTION"));
    assert_eq!(before, manifest(fixture.root()));
}

#[test]
fn process_cli_rejects_duplicate_mixed_and_missing_path_arguments() {
    for args in [
        vec!["--inspect-storage-library"],
        vec![
            "--inspect-storage-library",
            "--data-dir",
            "one",
            "--data-dir",
            "two",
        ],
        vec![
            "--inspect-storage-library",
            "--data-dir",
            "one",
            "--storage-capabilities",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_scribe-app"))
            .args(args)
            .output()
            .expect("run invalid inspection command");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}
