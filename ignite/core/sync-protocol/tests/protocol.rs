//! Protocol regression tests: operation/envelope validation, strict parsing, canonical bytes.

use sync_protocol::{
    check_client_seq, normalize_text_content, snapshot_sha256, BackgroundRef, LibraryId,
    LineageReplacement, NotebookId, OperationId, PageId, PageSpace, PageSpec, RenderProfileRef,
    ReplicaId, SnapshotNotebook, SnapshotPage, StrokeId, StrokeStyle, SyncEnvelopeV1,
    SyncOperationV1, SyncPageLink, SyncPoint, SyncSnapshotV1, SyncStroke, SyncText, TextFace,
    MAX_REPLACE_AFTER_STROKES, MAX_REPLACE_LINEAGE_ROOTS,
};

fn id<T: std::str::FromStr>(n: u8) -> T
where
    T::Err: std::fmt::Debug,
{
    format!("019a8ef2-0000-7000-8000-0000000000{n:02x}")
        .parse()
        .unwrap()
}

fn render() -> RenderProfileRef {
    RenderProfileRef {
        profile_version: 2,
        ink_byte: 0,
        legacy_fixed_radius: 2,
        transform_id: "scribe-portrait-v1".into(),
        rasterizer_id: "stamp-v1".into(),
        pressure: None,
    }
}

fn ok_text(tid: u8) -> SyncText {
    SyncText {
        text_id: id(tid),
        x: 100.0,
        y: 200.0,
        width: 5000.0,
        font_size: 340.0,
        face: TextFace::DefaultSansV1,
        content: "hello".into(),
    }
}

fn stroke(sid: u8, root: u8) -> SyncStroke {
    SyncStroke {
        stroke_id: id(sid),
        root_id: id(root),
        style: StrokeStyle::LegacyFixedV1,
        points: vec![
            SyncPoint::new(1.0, 2.0, Some(0.5)),
            SyncPoint::new(3.25, 4.5, None),
        ],
    }
}

fn snapshot() -> SyncSnapshotV1 {
    SyncSnapshotV1 {
        v: 1,
        library_id: id(1),
        page_space: PageSpace::CANONICAL,
        notebooks: vec![SnapshotNotebook {
            notebook_id: id(2),
            title: "Notebook 1".into(),
            pages: vec![SnapshotPage {
                page_id: id(3),
                background: BackgroundRef::Blank,
                render: render(),
                strokes: vec![stroke(4, 4), stroke(5, 9)],
                texts: Vec::new(),
                links: Vec::new(),
            }],
            folder_id: None,
        }],
        folders: Vec::new(),
    }
}

fn envelope(op: SyncOperationV1) -> SyncEnvelopeV1 {
    SyncEnvelopeV1 {
        v: 1,
        operation_id: id(0x40),
        library_id: id(1),
        replica_id: id(0x41),
        client_seq: 1,
        operation: op,
    }
}

#[test]
fn snapshot_canonical_bytes_are_deterministic_and_pinned() {
    let s = snapshot();
    let bytes = s.to_canonical_bytes().unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    // Pinned fixture: any accidental wire-format change fails here.
    let want = concat!(
        r#"{"v":1,"library_id":"019a8ef2-0000-7000-8000-000000000001","#,
        r#""page_space":{"width":15624.0,"height":20832.0},"#,
        r#""notebooks":[{"notebook_id":"019a8ef2-0000-7000-8000-000000000002","title":"Notebook 1","#,
        r#""pages":[{"page_id":"019a8ef2-0000-7000-8000-000000000003","background":{"kind":"blank"},"#,
        r#""render":{"profile_version":2,"ink_byte":0,"legacy_fixed_radius":2,"transform_id":"scribe-portrait-v1","rasterizer_id":"stamp-v1"},"#,
        r#""strokes":[{"stroke_id":"019a8ef2-0000-7000-8000-000000000004","root_id":"019a8ef2-0000-7000-8000-000000000004","style":"legacy-fixed-v1","points":[{"x":1.0,"y":2.0,"pressure":0.5},{"x":3.25,"y":4.5}]},"#,
        r#"{"stroke_id":"019a8ef2-0000-7000-8000-000000000005","root_id":"019a8ef2-0000-7000-8000-000000000009","style":"legacy-fixed-v1","points":[{"x":1.0,"y":2.0,"pressure":0.5},{"x":3.25,"y":4.5}]}]}]}]}"#
    );
    assert_eq!(text, want);

    // Pretty-printed / reordered input parses to the same semantic value and canonical bytes.
    let reordered = serde_json::json!({
        "notebooks": serde_json::to_value(&s.notebooks).unwrap(),
        "page_space": {"height": 20832.0, "width": 15624.0},
        "library_id": s.library_id,
        "v": 1
    });
    let pretty = serde_json::to_vec_pretty(&reordered).unwrap();
    let back = SyncSnapshotV1::parse(&pretty).unwrap();
    assert_eq!(back, s);
    assert_eq!(back.to_canonical_bytes().unwrap(), bytes);
    assert_eq!(
        snapshot_sha256(&back).unwrap(),
        snapshot_sha256(&s).unwrap()
    );
    // serialize → deserialize → serialize is a fixed point.
    let again = SyncSnapshotV1::parse(&bytes)
        .unwrap()
        .to_canonical_bytes()
        .unwrap();
    assert_eq!(again, bytes);
}

#[test]
fn snapshot_hash_tracks_semantic_changes_only() {
    let base = snapshot_sha256(&snapshot()).unwrap();
    let mut renamed = snapshot();
    renamed.notebooks[0].title = "Other".into();
    assert_ne!(snapshot_sha256(&renamed).unwrap(), base);
    let mut reordered = snapshot();
    reordered.notebooks[0].pages[0].strokes.reverse();
    assert_ne!(
        snapshot_sha256(&reordered).unwrap(),
        base,
        "render order is semantic"
    );
    assert_eq!(snapshot_sha256(&snapshot()).unwrap(), base);
}

#[test]
fn snapshot_structural_errors_and_versions_are_refused() {
    let bytes = snapshot().to_canonical_bytes().unwrap();
    let text = String::from_utf8(bytes).unwrap();
    for bad in [
        text.replacen(r#""v":1"#, r#""v":2"#, 1),
        text.replacen(r#""v":1,"#, r#""v":1,"active_page_id":"x","#, 1),
        text.replacen(r#""width":15624.0"#, r#""width":1860.0"#, 1),
        text.replacen(r#""kind":"blank""#, r#""kind":"framebuffer""#, 1),
        text.replacen(r#""x":1.0"#, r#""x":-1.0"#, 1),
        text.replacen(r#""pressure":0.5"#, r#""pressure":2.0"#, 1),
        text.replacen("000000000005", "000000000004", 1), // duplicate stroke id
        text.replacen(r#""title":"Notebook 1""#, r#""title":"  ""#, 1),
        text.replacen(r#""style":"legacy-fixed-v1""#, r#""style":"brush-xyz""#, 1),
    ] {
        assert!(SyncSnapshotV1::parse(bad.as_bytes()).is_err(), "{bad}");
    }
    let mut no_pages = snapshot();
    no_pages.notebooks[0].pages.clear();
    assert!(no_pages.validate().is_err());
    let mut pressure_without_curve = snapshot();
    pressure_without_curve.notebooks[0].pages[0].strokes[0].style = StrokeStyle::PressureWidthV1;
    assert!(pressure_without_curve.validate().is_err());
}

#[test]
fn every_current_semantic_operation_round_trips() {
    let page = PageSpec {
        page_id: id(0x10),
        background: BackgroundRef::Blank,
        render: render(),
    };
    let nb: NotebookId = id(2);
    let pg: PageId = id(3);
    let ops = vec![
        SyncOperationV1::CreateNotebook {
            notebook_id: nb,
            title: "Work".into(),
            after_notebook_id: None,
            first_page: page.clone(),
        },
        SyncOperationV1::RenameNotebook {
            notebook_id: nb,
            title: "Renamed".into(),
        },
        SyncOperationV1::DeleteNotebook { notebook_id: nb },
        SyncOperationV1::CreatePage {
            notebook_id: nb,
            page: page.clone(),
            after_page_id: Some(pg),
        },
        SyncOperationV1::MovePage {
            notebook_id: nb,
            page_id: pg,
            after_page_id: None,
        },
        SyncOperationV1::DeletePage {
            notebook_id: nb,
            page_id: pg,
        },
        SyncOperationV1::AddStroke {
            notebook_id: nb,
            page_id: pg,
            stroke: stroke(7, 7),
        },
        SyncOperationV1::EraseRegion {
            notebook_id: nb,
            page_id: pg,
            observed_roots: vec![id(4), id(7)],
            path: vec![
                SyncPoint::new(10.0, 10.0, None),
                SyncPoint::new(500.0, 600.0, None),
            ],
            radius: 240.0,
        },
        SyncOperationV1::DeleteStrokeLineage {
            notebook_id: nb,
            page_id: pg,
            root_id: id(7),
        },
        SyncOperationV1::MoveStrokeLineages {
            notebook_id: nb,
            page_id: pg,
            root_ids: vec![id(4), id(7)],
            dx: 12.5,
            dy: -8.25,
        },
        SyncOperationV1::AddText {
            notebook_id: nb,
            page_id: pg,
            text: ok_text(0x20),
        },
        SyncOperationV1::UpdateText {
            notebook_id: nb,
            page_id: pg,
            text: ok_text(0x20),
        },
        SyncOperationV1::DeleteText {
            notebook_id: nb,
            page_id: pg,
            text_id: id(0x20),
        },
        SyncOperationV1::AddPageLink {
            notebook_id: nb,
            page_id: pg,
            link: SyncPageLink {
                link_id: id(0x30),
                source_notebook_id: nb,
                source_page_id: pg,
                source_x: 1000.0,
                source_y: 2000.0,
                target_notebook_id: id(0x12),
                target_page_id: id(0x13),
            },
        },
        SyncOperationV1::DeletePageLink {
            notebook_id: nb,
            page_id: pg,
            link_id: id(0x30),
        },
        SyncOperationV1::ReplaceStrokeLineages {
            notebook_id: nb,
            page_id: pg,
            replacements: vec![LineageReplacement {
                root_id: id(7),
                expected: vec![stroke(7, 7)],
                after: vec![stroke(8, 7)],
            }],
        },
    ];
    for op in ops {
        let e = envelope(op);
        let bytes = e.to_canonical_bytes().unwrap();
        let back = SyncEnvelopeV1::parse(&bytes).unwrap();
        assert_eq!(back, e);
        assert_eq!(back.to_canonical_bytes().unwrap(), bytes);
    }
    let t = String::from_utf8(
        envelope(SyncOperationV1::DeleteNotebook { notebook_id: nb })
            .to_canonical_bytes()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        t,
        concat!(
            r#"{"v":1,"operation_id":"019a8ef2-0000-7000-8000-000000000040","#,
            r#""library_id":"019a8ef2-0000-7000-8000-000000000001","#,
            r#""replica_id":"019a8ef2-0000-7000-8000-000000000041","client_seq":1,"#,
            r#""operation":{"type":"delete_notebook","notebook_id":"019a8ef2-0000-7000-8000-000000000002"}}"#
        )
    );
    // No server_seq is client-authored.
    assert!(!t.contains("server_seq"));
}

#[test]
fn ui_and_undo_are_not_operations_and_bad_payloads_are_refused() {
    let base = String::from_utf8(
        envelope(SyncOperationV1::DeleteNotebook { notebook_id: id(2) })
            .to_canonical_bytes()
            .unwrap(),
    )
    .unwrap();
    for ui in [
        "open_notebook",
        "go_home",
        "select_page",
        "show_chrome",
        "hide_chrome",
        "undo",
        "undo_button_pressed",
        "replace_strokes",
    ] {
        let bad = base.replace("delete_notebook", ui);
        assert!(
            SyncEnvelopeV1::parse(bad.as_bytes()).is_err(),
            "{ui} must not parse"
        );
    }
    for bad in [
        base.replace(r#""client_seq":1"#, r#""client_seq":0"#),
        base.replace(r#""v":1"#, r#""v":2"#),
        base.replace(r#""client_seq":1,"#, r#""client_seq":1,"server_seq":9,"#),
        base.replace(
            r#""replica_id":"019a8ef2-0000-7000-8000-000000000041","#,
            "",
        ),
    ] {
        assert!(SyncEnvelopeV1::parse(bad.as_bytes()).is_err(), "{bad}");
    }

    let erase = |roots: Vec<StrokeId>, radius: f32| SyncOperationV1::EraseRegion {
        notebook_id: id(2),
        page_id: id(3),
        observed_roots: roots,
        path: vec![SyncPoint::new(1.0, 1.0, None)],
        radius,
    };
    assert!(
        erase(vec![], 240.0).validate().is_err(),
        "must observe lineages"
    );
    assert!(
        erase(vec![id(7), id(4)], 240.0).validate().is_err(),
        "sorted"
    );
    assert!(
        erase(vec![id(4), id(4)], 240.0).validate().is_err(),
        "unique"
    );
    assert!(erase(vec![id(4)], 0.0).validate().is_err());
    assert!(erase(vec![id(4)], f32::NAN).validate().is_err());
    erase(vec![id(4), id(7)], 240.0).validate().unwrap();

    let movement = |roots: Vec<StrokeId>, dx: f32, dy: f32| SyncOperationV1::MoveStrokeLineages {
        notebook_id: id(2),
        page_id: id(3),
        root_ids: roots,
        dx,
        dy,
    };
    assert!(movement(vec![], 1.0, 0.0).validate().is_err());
    assert!(movement(vec![id(7), id(4)], 1.0, 0.0).validate().is_err());
    assert!(movement(vec![id(4), id(4)], 1.0, 0.0).validate().is_err());
    assert!(movement(vec![id(4)], 0.0, 0.0).validate().is_err());
    assert!(movement(vec![id(4)], f32::NAN, 1.0).validate().is_err());
    let op = movement(vec![id(4), id(7)], 12.5, -8.25);
    op.validate().unwrap();
    let a = envelope(op);
    let first = a.to_canonical_bytes().unwrap();
    let second = SyncEnvelopeV1::parse(&first)
        .unwrap()
        .to_canonical_bytes()
        .unwrap();
    assert_eq!(first, second, "move encoding is deterministic");

    let add = |root: u8| SyncOperationV1::AddStroke {
        notebook_id: id(2),
        page_id: id(3),
        stroke: stroke(7, root),
    };
    assert!(add(8).validate().is_err(), "a new stroke is its own root");
    add(7).validate().unwrap();
    assert!(SyncOperationV1::MovePage {
        notebook_id: id(2),
        page_id: id(3),
        after_page_id: Some(id(3)),
    }
    .validate()
    .is_err());
    assert!(SyncOperationV1::RenameNotebook {
        notebook_id: id(2),
        title: "a\u{7}".into(),
    }
    .validate()
    .is_err());
}

#[test]
fn text_content_normalizes_line_endings_and_keeps_spaces() {
    assert_eq!(normalize_text_content("a\r\nb\rc"), "a\nb\nc");
    assert_eq!(normalize_text_content("  keep  "), "  keep  ");
    let mut t = ok_text(0x21);
    t.content = "  leading".into();
    t.validate().unwrap();
    t.content.clear();
    assert!(t.validate().is_err());
    t.content = "a\tb".into();
    assert!(t.validate().is_err());
    t.content = "a\rb".into();
    assert!(t.validate().is_err());
    t.x = 15_623.5;
    t.width = 1.0;
    t.content = "ok".into();
    assert!(
        t.validate().is_err(),
        "x must be strictly less than page width"
    );
    let mut edge = ok_text(0x22);
    edge.x = 15_000.0;
    edge.width = 624.0;
    edge.validate().unwrap();
    edge.width = 625.0;
    assert!(edge.validate().is_err());
}

#[test]
fn snapshot_omits_empty_texts_from_canonical_bytes() {
    let with_empty = snapshot();
    assert!(with_empty.notebooks[0].pages[0].texts.is_empty());
    let bytes = with_empty.to_canonical_bytes().unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(!text.contains("texts"), "{text}");
    assert!(!text.contains("links"), "{text}");
}

#[test]
fn page_link_self_target_is_refused() {
    let nb: NotebookId = id(2);
    let pg: PageId = id(3);
    let link = SyncPageLink {
        link_id: id(0x30),
        source_notebook_id: nb,
        source_page_id: pg,
        source_x: 100.0,
        source_y: 100.0,
        target_notebook_id: nb,
        target_page_id: pg,
    };
    assert!(link.validate().is_err());
    assert!(SyncOperationV1::AddPageLink {
        notebook_id: nb,
        page_id: pg,
        link,
    }
    .validate()
    .is_err());
}

#[test]
fn client_seq_is_strictly_monotonic_per_replica() {
    check_client_seq(None, 1).unwrap();
    check_client_seq(Some(1), 2).unwrap();
    assert!(check_client_seq(None, 2).is_err());
    assert!(check_client_seq(Some(5), 5).is_err());
    assert!(check_client_seq(Some(5), 7).is_err());
    // Pinned overflow behavior: exact +1 up to u64::MAX, then exhausted (no saturate/wrap).
    assert!(check_client_seq(Some(1), 1).is_err());
    assert!(check_client_seq(Some(1), 3).is_err());
    check_client_seq(Some(u64::MAX - 1), u64::MAX).unwrap();
    for next in [u64::MAX, 0, 1, u64::MAX - 1] {
        let e = check_client_seq(Some(u64::MAX), next).unwrap_err();
        assert!(e.message().contains("exhausted"), "{e}");
    }
    let _: (LibraryId, ReplicaId, OperationId) = (id(1), id(2), id(3));
}

#[test]
fn replace_stroke_lineages_round_trips_and_names_its_type() {
    let op = SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: id(2),
        page_id: id(3),
        replacements: vec![LineageReplacement {
            root_id: id(7),
            expected: vec![stroke(7, 7)],
            after: vec![stroke(8, 7)],
        }],
    };
    op.validate().unwrap();
    let e = envelope(op);
    let bytes = e.to_canonical_bytes().unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(
        text.contains(r#""type":"replace_stroke_lineages""#),
        "{text}"
    );
    assert!(!text.contains("path"), "{text}");
    assert!(!text.contains("radius"), "{text}");
    let back = SyncEnvelopeV1::parse(&bytes).unwrap();
    assert_eq!(back, e);
    let extra = text.replace(
        r#""page_id":"019a8ef2-0000-7000-8000-000000000003""#,
        r#""page_id":"019a8ef2-0000-7000-8000-000000000003","extra":true"#,
    );
    assert!(SyncEnvelopeV1::parse(extra.as_bytes()).is_err());
}

#[test]
fn replace_stroke_lineages_rejects_malformed_payloads() {
    let nb: NotebookId = id(2);
    let pg: PageId = id(3);
    let ok = LineageReplacement {
        root_id: id(7),
        expected: vec![stroke(7, 7)],
        after: vec![stroke(8, 7)],
    };
    assert!(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb,
        page_id: pg,
        replacements: vec![],
    }
    .validate()
    .is_err());
    let mut unsorted = vec![
        LineageReplacement {
            root_id: id(9),
            expected: vec![stroke(9, 9)],
            after: vec![],
        },
        ok.clone(),
    ];
    assert!(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb,
        page_id: pg,
        replacements: unsorted.clone(),
    }
    .validate()
    .is_err());
    unsorted.reverse();
    assert!(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb,
        page_id: pg,
        replacements: unsorted,
    }
    .validate()
    .is_ok());
    assert!(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb,
        page_id: pg,
        replacements: vec![LineageReplacement {
            root_id: id(7),
            expected: vec![],
            after: vec![],
        }],
    }
    .validate()
    .is_err());
    let mut wrong_root = stroke(8, 9);
    wrong_root.root_id = id(9);
    assert!(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb,
        page_id: pg,
        replacements: vec![LineageReplacement {
            root_id: id(7),
            expected: vec![stroke(7, 7)],
            after: vec![wrong_root],
        }],
    }
    .validate()
    .is_err());
    let mut inf = stroke(8, 7);
    inf.points[0].x = f32::INFINITY;
    assert!(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb,
        page_id: pg,
        replacements: vec![LineageReplacement {
            root_id: id(7),
            expected: vec![stroke(7, 7)],
            after: vec![inf],
        }],
    }
    .validate()
    .is_err());
    let too_many_roots: Vec<_> = (0..=MAX_REPLACE_LINEAGE_ROOTS)
        .map(|i| {
            let u: StrokeId = format!("019a8ef2-0000-7000-8000-00000000{i:04x}")
                .parse()
                .unwrap();
            LineageReplacement {
                root_id: u,
                expected: vec![],
                after: vec![SyncStroke {
                    stroke_id: u,
                    root_id: u,
                    style: StrokeStyle::LegacyFixedV1,
                    points: vec![SyncPoint::new(1.0, 2.0, None)],
                }],
            }
        })
        .collect();
    assert!(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb,
        page_id: pg,
        replacements: too_many_roots,
    }
    .validate()
    .is_err());
    let too_many_after = LineageReplacement {
        root_id: id(7),
        expected: vec![stroke(7, 7)],
        after: (0..=MAX_REPLACE_AFTER_STROKES)
            .map(|i| {
                let u: StrokeId = format!("019a8ef2-0000-7000-8000-00000001{i:04x}")
                    .parse()
                    .unwrap();
                SyncStroke {
                    stroke_id: u,
                    root_id: id(7),
                    style: StrokeStyle::LegacyFixedV1,
                    points: vec![SyncPoint::new(1.0, 2.0, None)],
                }
            })
            .collect(),
    };
    assert!(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb,
        page_id: pg,
        replacements: vec![too_many_after],
    }
    .validate()
    .is_err());
}

#[test]
fn hashing_an_invalid_snapshot_is_refused_like_encoding_it() {
    let mut bad = snapshot();
    let dup = bad.notebooks[0].clone();
    bad.notebooks.push(dup); // duplicate notebook id
    assert!(bad.to_canonical_bytes().is_err());
    assert!(snapshot_sha256(&bad).is_err());
    // A valid snapshot keeps the byte-identical hash.
    let good = snapshot();
    assert_eq!(
        snapshot_sha256(&good).unwrap(),
        sync_protocol::sha256_hex(&good.to_canonical_bytes().unwrap())
    );
}

#[test]
fn streamed_snapshot_hash_equals_the_hash_of_the_canonical_bytes() {
    let mut s = snapshot();
    // Floats, text and an empty page: every encoding path the canonical form takes.
    s.notebooks[0].pages[0].texts = vec![ok_text(7)];
    let bytes = s.to_canonical_bytes().unwrap();
    assert_eq!(
        snapshot_sha256(&s).unwrap(),
        sync_protocol::sha256_hex(&bytes)
    );
    assert_eq!(
        sync_protocol::canonical_sha256(&s).unwrap(),
        sync_protocol::sha256_hex(&bytes)
    );
}

/// Pins the f32 text-width semantics that ignite-web mirrors (`textWidthFits`; its protocol
/// tests use these exact values). 15624 - 6500.0005f32 is exactly 9123.99951171875, halfway
/// between two f32s; the f32 subtraction rounds it to 9124, so width 9124 fits.
#[test]
fn text_width_is_checked_in_f32_arithmetic() {
    let x = 6500.0005_f32;
    assert_eq!(f64::from(x), 6500.00048828125);
    let fits = SyncText {
        x,
        width: 9124.0,
        ..ok_text(1)
    };
    fits.validate().unwrap();

    let json = String::from_utf8(sync_protocol::to_canonical_json(&fits).unwrap()).unwrap();
    assert!(json.contains("\"x\":6500.0005,"), "{json}");
    assert!(json.contains("\"width\":9124"), "{json}");
    let parsed: SyncText = sync_protocol::from_json(json.as_bytes()).unwrap();
    assert_eq!(parsed, fits);
    parsed.validate().unwrap();

    let over = SyncText {
        width: f32::from_bits(9124.0_f32.to_bits() + 1),
        ..fits.clone()
    };
    assert!(over.validate().is_err());
}
