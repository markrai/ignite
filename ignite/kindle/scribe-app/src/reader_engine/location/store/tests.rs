use std::sync::Arc;
use std::thread;

use super::*;
use crate::reader_engine::location::{ContentLocation, TextAnchor, TEXT_MODEL};

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let number = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ignite-position-store-{}-{number}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn store(&self) -> PositionStore {
        PositionStore::new(self.0.join("positions"))
    }

    fn slot(&self, position: &SavedPosition, index: usize) -> PathBuf {
        self.0
            .join("positions")
            .join(format!("{}.slot{index}", key(&position.document)))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn fixed(page: u32) -> SavedPosition {
    SavedPosition::new(
        DocumentIdentity {
            sha256: [0x7c; 32],
            len: 4096,
        },
        ContentLocation::FixedPage(page),
    )
}

fn text() -> SavedPosition {
    SavedPosition::new(
        DocumentIdentity {
            sha256: [0xb8; 32],
            len: 8267,
        },
        ContentLocation::Text(TextAnchor {
            text_model: TEXT_MODEL.to_owned(),
            chapter: 7,
            offset: 139,
            context: "Café世界🌲".to_owned(),
        }),
    )
}

#[test]
fn missing_store_reads_as_none_without_creating_directories() {
    let dir = TestDirectory::new();
    let store = dir.store();
    assert!(store.load(&fixed(1).document).unwrap().is_none());
    assert!(!store.root.exists());
}

#[test]
fn first_save_is_private_durable_and_survives_new_instance() {
    let dir = TestDirectory::new();
    let store = dir.store();
    let position = text();
    assert_eq!(store.save(&position).unwrap(), 1);
    assert_eq!(
        dir.store().load(&position.document).unwrap(),
        Some(position.clone())
    );
    assert!(dir.slot(&position, 0).is_file());
    assert!(!dir.slot(&position, 1).exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&store.root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(dir.slot(&position, 0))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn alternating_slots_keep_previous_and_newest_generations() {
    let dir = TestDirectory::new();
    let store = dir.store();
    for page in 0..4 {
        assert_eq!(store.save(&fixed(page)).unwrap(), u64::from(page) + 1);
        assert_eq!(
            store.load(&fixed(page).document).unwrap(),
            Some(fixed(page))
        );
    }
    assert!(dir.slot(&fixed(0), 0).exists());
    assert!(dir.slot(&fixed(0), 1).exists());
    let mut pages = Vec::new();
    for index in 0..2 {
        let bytes = fs::read(dir.slot(&fixed(0), index)).unwrap();
        let wire: SlotWire = serde_json::from_slice(&bytes).unwrap();
        let position = SavedPosition::from_json_bytes(wire.position.get().as_bytes()).unwrap();
        if let ContentLocation::FixedPage(page) = position.location {
            pages.push((wire.generation, page));
        }
    }
    pages.sort();
    assert_eq!(pages, vec![(3, 2), (4, 3)]);
}

#[test]
fn corrupt_newest_recovers_previous_and_replaces_only_corrupt_slot() {
    let dir = TestDirectory::new();
    let store = dir.store();
    store.save(&fixed(1)).unwrap();
    store.save(&fixed(2)).unwrap();
    let older = fs::read(dir.slot(&fixed(1), 0)).unwrap();
    let latest = dir.slot(&fixed(1), 1);
    let mut bytes = fs::read(&latest).unwrap();
    let needle = br#""page":2"#;
    let index = bytes
        .windows(needle.len())
        .position(|x| x == needle)
        .unwrap();
    bytes[index + 7] = b'9';
    fs::write(&latest, bytes).unwrap();
    assert_eq!(store.load(&fixed(1).document).unwrap(), Some(fixed(1)));
    assert_eq!(store.save(&fixed(3)).unwrap(), 2);
    assert_eq!(store.load(&fixed(3).document).unwrap(), Some(fixed(3)));
    assert_eq!(fs::read(dir.slot(&fixed(1), 0)).unwrap(), older);
}

#[test]
fn corrupted_both_slots_is_an_error_and_cannot_be_silently_overwritten() {
    let dir = TestDirectory::new();
    let store = dir.store();
    store.save(&fixed(1)).unwrap();
    store.save(&fixed(2)).unwrap();
    let first = dir.slot(&fixed(1), 0);
    let second = dir.slot(&fixed(1), 1);
    fs::write(&first, b"broken").unwrap();
    fs::write(&second, b"{").unwrap();
    assert!(matches!(
        store.load(&fixed(1).document),
        Err(PositionStoreError::Corrupt)
    ));
    assert!(matches!(
        store.save(&fixed(3)),
        Err(PositionStoreError::Corrupt)
    ));
    assert_eq!(fs::read(&first).unwrap(), b"broken");
    assert_eq!(fs::read(&second).unwrap(), b"{");
}

#[test]
fn invalid_single_slot_is_not_mistaken_for_no_saved_position() {
    let dir = TestDirectory::new();
    let store = dir.store();
    store.save(&fixed(1)).unwrap();
    let slot = dir.slot(&fixed(1), 0);
    fs::write(&slot, vec![b'x'; MAX_SLOT_BYTES + 40]).unwrap();
    assert!(matches!(
        store.load(&fixed(1).document),
        Err(PositionStoreError::Corrupt)
    ));
    assert!(matches!(
        store.save(&fixed(2)),
        Err(PositionStoreError::Corrupt)
    ));
}

#[test]
fn future_store_format_blocks_downgrade_even_if_old_slot_is_valid() {
    let dir = TestDirectory::new();
    let store = dir.store();
    store.save(&fixed(1)).unwrap();
    store.save(&fixed(2)).unwrap();
    let slot = dir.slot(&fixed(1), 1);
    let original = fs::read_to_string(&slot).unwrap();
    let changed = original.replace(r#""slot_format":1"#, r#""slot_format":2"#);
    assert_ne!(original, changed);
    fs::write(&slot, &changed).unwrap();
    assert!(matches!(
        store.load(&fixed(1).document),
        Err(PositionStoreError::UnsupportedStoreFormat(2))
    ));
    assert!(matches!(
        store.save(&fixed(3)),
        Err(PositionStoreError::UnsupportedStoreFormat(2))
    ));
    assert_eq!(fs::read_to_string(&slot).unwrap(), changed);
}

#[test]
fn future_position_format_is_not_overwritten_on_downgrade() {
    let dir = TestDirectory::new();
    let store = dir.store();
    store.save(&fixed(1)).unwrap();
    let future = String::from_utf8(fixed(2).to_json_bytes().unwrap())
        .unwrap()
        .replace(r#""format":1"#, r#""format":2"#);
    assert!(future.contains(r#""format":2"#));
    let slot = dir.slot(&fixed(1), 1);
    let bytes = encode_slot(2, future.as_bytes()).unwrap();
    fs::write(&slot, &bytes).unwrap();
    assert!(matches!(
        store.load(&fixed(1).document),
        Err(PositionStoreError::UnsupportedPositionFormat(2))
    ));
    assert!(matches!(
        store.save(&fixed(3)),
        Err(PositionStoreError::UnsupportedPositionFormat(2))
    ));
    assert_eq!(fs::read(slot).unwrap(), bytes);
}

#[test]
fn equal_generations_are_a_conflict_not_a_tie_break() {
    let dir = TestDirectory::new();
    let store = dir.store();
    store.save(&fixed(1)).unwrap();
    store.save(&fixed(2)).unwrap();
    fs::copy(dir.slot(&fixed(1), 0), dir.slot(&fixed(1), 1)).unwrap();
    assert!(matches!(
        store.load(&fixed(1).document),
        Err(PositionStoreError::ConflictingGenerations)
    ));
    assert!(matches!(
        store.save(&fixed(3)),
        Err(PositionStoreError::ConflictingGenerations)
    ));
}

#[test]
fn generation_overflow_refuses_write() {
    let dir = TestDirectory::new();
    let store = dir.store();
    let position = fixed(3);
    store.save(&position).unwrap();
    let slot = dir.slot(&position, 0);
    fs::write(
        &slot,
        encode_slot(u64::MAX, &position.to_json_bytes().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        store.load(&position.document).unwrap(),
        Some(position.clone())
    );
    assert!(matches!(
        store.save(&fixed(4)),
        Err(PositionStoreError::GenerationExhausted)
    ));
}

#[test]
fn garbage_temp_files_do_not_override_valid_records() {
    let dir = TestDirectory::new();
    let store = dir.store();
    store.save(&fixed(9)).unwrap();
    let orphan = store.root.join("unrelated.tmp");
    fs::write(&orphan, b"half-written record").unwrap();
    assert_eq!(store.load(&fixed(9).document).unwrap(), Some(fixed(9)));
    assert_eq!(store.save(&fixed(10)).unwrap(), 2);
    assert_eq!(fs::read(orphan).unwrap(), b"half-written record");
}

#[test]
fn interruption_before_rename_keeps_the_last_valid_position() {
    let dir = TestDirectory::new();
    let store = dir.store();
    store.save(&fixed(1)).unwrap();
    let unchanged = fs::read(dir.slot(&fixed(1), 0)).unwrap();
    for failure in [FailurePoint::AfterWrite, FailurePoint::AfterFileSync] {
        assert!(store.save_with_failure(&fixed(2), failure).is_err());
        assert_eq!(store.load(&fixed(1).document).unwrap(), Some(fixed(1)));
        assert!(!dir.slot(&fixed(1), 1).exists());
    }
    assert_eq!(fs::read(dir.slot(&fixed(1), 0)).unwrap(), unchanged);
}

#[test]
fn interruption_after_rename_cannot_destroy_previous_valid_slot() {
    let dir = TestDirectory::new();
    let store = dir.store();
    store.save(&fixed(1)).unwrap();
    let original = fs::read(dir.slot(&fixed(1), 0)).unwrap();
    assert!(store
        .save_with_failure(&fixed(2), FailurePoint::AfterRename)
        .is_err());
    assert_eq!(store.load(&fixed(1).document).unwrap(), Some(fixed(2)));
    assert_eq!(fs::read(dir.slot(&fixed(1), 0)).unwrap(), original);
}

#[test]
fn separate_documents_cannot_share_slot_files() {
    let dir = TestDirectory::new();
    let store = dir.store();
    let a = fixed(1);
    let b = text();
    store.save(&a).unwrap();
    store.save(&b).unwrap();
    assert_eq!(store.load(&a.document).unwrap(), Some(a));
    assert_eq!(store.load(&b.document).unwrap(), Some(b));
    let mut other = fixed(10);
    other.document.len += 1;
    assert_eq!(store.load(&other.document).unwrap(), None);
}

#[test]
fn wrong_document_in_an_existing_slot_is_invalid() {
    let dir = TestDirectory::new();
    let store = dir.store();
    let original = fixed(1);
    store.save(&original).unwrap();
    let wrong = text();
    fs::write(
        dir.slot(&original, 0),
        encode_slot(3, &wrong.to_json_bytes().unwrap()).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        store.load(&original.document),
        Err(PositionStoreError::Corrupt)
    ));
}

#[test]
fn malicious_or_unsupported_envelope_fields_are_invalid() {
    let dir = TestDirectory::new();
    let store = dir.store();
    store.save(&fixed(1)).unwrap();
    let slot = dir.slot(&fixed(1), 0);
    let original = fs::read_to_string(&slot).unwrap();
    for altered in [
        original.replace(r#""generation":1"#, r#""generation":2"#),
        original.replace(r#""generation":1"#, r#""generation":1,"generation":1"#),
        original.replace(r#""slot_format":1"#, r#""slot_format":1,"unknown":true"#),
        format!("{original}{{}}"),
        original.replace(r#""checksum":"#, r#""checksum":"XX"#),
    ] {
        assert_ne!(altered, original);
        fs::write(&slot, altered).unwrap();
        assert!(matches!(
            store.load(&fixed(1).document),
            Err(PositionStoreError::Corrupt)
        ));
    }
}

#[test]
fn invalid_position_fails_before_creating_storage() {
    let dir = TestDirectory::new();
    let store = dir.store();
    let mut position = text();
    if let ContentLocation::Text(anchor) = &mut position.location {
        anchor.context = "x".repeat(5000);
    }
    assert!(matches!(
        store.save(&position),
        Err(PositionStoreError::Codec(PositionCodecError::InvalidAnchor))
    ));
    assert!(!store.root.exists());
}

#[cfg(unix)]
#[test]
fn symlinked_root_and_slot_are_rejected() {
    use std::os::unix::fs::symlink;

    let dir = TestDirectory::new();
    let root = dir.0.join("positions");
    symlink(&dir.0, &root).unwrap();
    let store = PositionStore::new(root);
    assert!(matches!(
        store.save(&fixed(1)),
        Err(PositionStoreError::UnsafePath)
    ));
    fs::remove_file(&store.root).unwrap();

    store.save(&fixed(1)).unwrap();
    let slot = dir.slot(&fixed(1), 0);
    fs::remove_file(&slot).unwrap();
    symlink(dir.0.join("target"), &slot).unwrap();
    assert!(matches!(
        store.load(&fixed(1).document),
        Err(PositionStoreError::UnsafePath)
    ));
    assert!(matches!(
        store.save(&fixed(2)),
        Err(PositionStoreError::UnsafePath)
    ));
}

#[cfg(unix)]
#[test]
fn concurrent_writers_observe_monotonic_generations() {
    let dir = TestDirectory::new();
    let store = Arc::new(dir.store());
    let mut handles = Vec::new();
    for page in 0..12u32 {
        let shared = Arc::clone(&store);
        handles.push(thread::spawn(move || shared.save(&fixed(page)).unwrap()));
    }
    let mut generations: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    generations.sort_unstable();
    assert_eq!(generations, (1..=12).collect::<Vec<_>>());
    assert!(store.load(&fixed(0).document).unwrap().is_some());
}

#[cfg(unix)]
#[test]
fn file_permission_failure_does_not_overwrite_existing_record() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TestDirectory::new();
    let store = dir.store();
    store.save(&fixed(1)).unwrap();
    let record = fs::read(dir.slot(&fixed(1), 0)).unwrap();
    fs::set_permissions(&store.root, fs::Permissions::from_mode(0o500)).unwrap();
    let result = store.save(&fixed(2));
    fs::set_permissions(&store.root, fs::Permissions::from_mode(0o700)).unwrap();
    if result.is_err() {
        assert_eq!(fs::read(dir.slot(&fixed(1), 0)).unwrap(), record);
    }
}
