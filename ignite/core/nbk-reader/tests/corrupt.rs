//! Always-on malformed / missing-source coverage (no private notebook fixtures).

use std::fs;

use nbk_reader::{decode_snapshot, read_source, NbkError};

#[test]
fn corrupt_and_missing_sources_fail_cleanly() {
    // Unique dir per invocation to avoid parallel-test collisions.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "nbk-reader-corrupt-{}-{}",
        std::process::id(),
        nanos
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    assert!(matches!(read_source(&dir), Err(NbkError::Unsupported(_))));

    fs::write(dir.join("nbk"), b"SQLite format 3\0 but garbage").unwrap();
    let e = decode_snapshot(&read_source(&dir).unwrap()).unwrap_err();
    assert!(matches!(e, NbkError::Container(_)), "{e}");

    fs::write(dir.join("nbk-journal"), b"hot").unwrap();
    assert!(matches!(read_source(&dir), Err(NbkError::SourceBusy(_))));

    let _ = fs::remove_dir_all(&dir);
}
