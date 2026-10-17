//! Deterministic fixtures for reducer/server tests (feature `testkit`; never used in products).

use sync_protocol::{
    global_stroke_id, sha256_hex, BackgroundEncoding, BackgroundFormat, BackgroundRef, FolderId,
    LibraryId, LinkId, NotebookId, OperationId, PageId, PageSpace, PageSpec, PressureCurveRef,
    RenderProfileRef, ReplicaId, SnapshotNotebook, SnapshotPage, StrokeId, StrokeStyle,
    SyncEnvelopeV1, SyncOperationV1, SyncPageLink, SyncPoint, SyncSnapshotV1, SyncStroke, SyncText,
    TextFace, TextId,
};
use uuid::Uuid;

/// A v7-shaped deterministic UUID: `kind` distinguishes entity families, `n` the instance.
pub fn uuid7(kind: u8, n: u64) -> Uuid {
    let mut b = [0u8; 16];
    b[0] = 0x01;
    b[1] = kind;
    b[6] = 0x70;
    b[8] = 0x80;
    b[9..16].copy_from_slice(&n.to_be_bytes()[1..]);
    Uuid::from_bytes(b)
}

pub fn library() -> LibraryId {
    LibraryId::from_uuid(uuid7(1, 1)).unwrap()
}
pub fn nb(n: u64) -> NotebookId {
    NotebookId::from_uuid(uuid7(2, n)).unwrap()
}
pub fn fd(n: u64) -> FolderId {
    FolderId::from_uuid(uuid7(6, n)).unwrap()
}
pub fn pg(n: u64) -> PageId {
    PageId::from_uuid(uuid7(3, n)).unwrap()
}
pub fn sk(n: u64) -> StrokeId {
    StrokeId::from_uuid(uuid7(4, n)).unwrap()
}
pub fn tx(n: u64) -> TextId {
    TextId::from_uuid(uuid7(7, n)).unwrap()
}
pub fn lk(n: u64) -> LinkId {
    LinkId::from_uuid(uuid7(8, n)).unwrap()
}

pub fn text_obj(n: u64, content: &str) -> SyncText {
    SyncText {
        text_id: tx(n),
        x: 100.0,
        y: 200.0,
        width: 5000.0,
        font_size: 340.0,
        face: TextFace::DefaultSansV1,
        content: content.into(),
    }
}

/// Link from notebook 1 / page 1 to notebook 2 / page 3 unless overridden.
pub fn page_link(
    n: u64,
    source_notebook: NotebookId,
    source_page: PageId,
    target_notebook: NotebookId,
    target_page: PageId,
) -> SyncPageLink {
    SyncPageLink {
        link_id: lk(n),
        source_notebook_id: source_notebook,
        source_page_id: source_page,
        source_x: 2000.0,
        source_y: 3000.0,
        target_notebook_id: target_notebook,
        target_page_id: target_page,
    }
}
pub fn replica(n: u64) -> ReplicaId {
    ReplicaId::from_uuid(uuid7(5, n)).unwrap()
}

pub fn render(pressure: bool) -> RenderProfileRef {
    RenderProfileRef {
        profile_version: if pressure { 2 } else { 1 },
        ink_byte: 0x00,
        legacy_fixed_radius: 2,
        transform_id: "scribe-portrait-8.4-v1".into(),
        rasterizer_id: "disc-stamp-v1".into(),
        pressure: pressure.then_some(PressureCurveRef {
            q_r1_max: 1023,
            q_r2_max: 2047,
            q_r3_max: 3071,
            min_radius: 1,
            max_radius: 4,
            missing_fallback_radius: 2,
        }),
    }
}

/// Synthetic nonblank background bytes (tiny gray8 image) and its blob reference.
pub fn blob_background() -> (Vec<u8>, BackgroundRef) {
    let (w, h) = (40u32, 30u32);
    let bytes: Vec<u8> = (0..w * h).map(|i| (i % 251) as u8).collect();
    let r = BackgroundRef::Blob {
        sha256: sha256_hex(&bytes),
        byte_len: bytes.len() as u64,
        format: BackgroundFormat {
            encoding: BackgroundEncoding::Gray8,
            width: w,
            height: h,
            stride: w,
        },
    };
    (bytes, r)
}

/// Straight stroke from `a` to `b` with `n` samples; pressure ramps 0.2→0.9 when `pressure`.
pub fn line(
    id: StrokeId,
    root: StrokeId,
    a: (f32, f32),
    b: (f32, f32),
    n: usize,
    pressure: bool,
) -> SyncStroke {
    let points = (0..n)
        .map(|i| {
            let t = i as f32 / (n - 1).max(1) as f32;
            SyncPoint::new(
                a.0 + (b.0 - a.0) * t,
                a.1 + (b.1 - a.1) * t,
                pressure.then_some(0.2 + 0.7 * t),
            )
        })
        .collect();
    SyncStroke {
        stroke_id: id,
        root_id: root,
        style: if pressure {
            StrokeStyle::PressureWidthV1
        } else {
            StrokeStyle::LegacyFixedV1
        },
        points,
    }
}

/// Representative genesis:
///
/// ```text
/// nb(1) "Notebook 1": pg(1) pressure page with a legacy-derived stroke, a UUID-backed pressure
///                     stroke, and two fragments of an erased legacy lineage; pg(2) blank
/// nb(2) "Work":       pg(3) nonblank blob background, one legacy fixed stroke
/// nb(3) "Scratch":    pg(4)
/// ```
pub fn genesis() -> SyncSnapshotV1 {
    let p1 = pg(1);
    let legacy = global_stroke_id(p1, "s-1727000000000-1").unwrap();
    let legacy_root = global_stroke_id(p1, "s-1727000000000-2").unwrap();
    let frag_a = global_stroke_id(p1, "s-1727000000500-7").unwrap();
    let frag_b = global_stroke_id(p1, "s-1727000000500-8").unwrap();
    let (_, blob) = blob_background();
    SyncSnapshotV1 {
        v: 1,
        library_id: library(),
        page_space: PageSpace::CANONICAL,
        notebooks: vec![
            SnapshotNotebook {
                notebook_id: nb(1),
                title: "Notebook 1".into(),
                pages: vec![
                    SnapshotPage {
                        page_id: p1,
                        background: BackgroundRef::Blank,
                        render: render(true),
                        strokes: vec![
                            line(legacy, legacy, (1000.0, 1000.0), (5000.0, 1000.0), 9, false),
                            line(sk(1), sk(1), (1000.0, 3000.0), (9000.0, 3000.0), 17, true),
                            line(
                                frag_a,
                                legacy_root,
                                (1000.0, 5000.0),
                                (2000.0, 5000.0),
                                3,
                                false,
                            ),
                            line(
                                frag_b,
                                legacy_root,
                                (3000.0, 5000.0),
                                (4000.0, 5000.0),
                                3,
                                false,
                            ),
                        ],
                        texts: Vec::new(),
                        links: Vec::new(),
                    },
                    SnapshotPage {
                        page_id: pg(2),
                        background: BackgroundRef::Blank,
                        render: render(true),
                        strokes: vec![],
                        texts: Vec::new(),
                        links: Vec::new(),
                    },
                ],
                folder_id: None,
            },
            SnapshotNotebook {
                notebook_id: nb(2),
                title: "Work".into(),
                pages: vec![SnapshotPage {
                    page_id: pg(3),
                    background: blob,
                    render: render(false),
                    strokes: vec![line(sk(2), sk(2), (500.0, 500.0), (800.0, 900.0), 4, false)],
                    texts: Vec::new(),
                    links: Vec::new(),
                }],
                folder_id: None,
            },
            SnapshotNotebook {
                notebook_id: nb(3),
                title: "Scratch".into(),
                pages: vec![SnapshotPage {
                    page_id: pg(4),
                    background: BackgroundRef::Blank,
                    render: render(true),
                    strokes: vec![],
                    texts: Vec::new(),
                    links: Vec::new(),
                }],
                folder_id: None,
            },
        ],
        folders: Vec::new(),
    }
}

pub fn page_spec(id: PageId) -> PageSpec {
    PageSpec {
        page_id: id,
        background: BackgroundRef::Blank,
        render: render(true),
    }
}

/// Authors envelopes for one replica with a correct, gap-free `client_seq`.
#[derive(Clone, Debug)]
pub struct Author {
    pub library: LibraryId,
    pub replica: ReplicaId,
    pub next_seq: u64,
}

impl Author {
    pub fn new(replica: ReplicaId) -> Self {
        Self {
            library: library(),
            replica,
            next_seq: 1,
        }
    }

    /// Deterministic OperationId: UUIDv5(replica, client_seq).
    pub fn op_id(&self, seq: u64) -> OperationId {
        OperationId::from_uuid(Uuid::new_v5(&self.replica.as_uuid(), &seq.to_be_bytes())).unwrap()
    }

    pub fn env(&mut self, operation: SyncOperationV1) -> SyncEnvelopeV1 {
        let seq = self.next_seq;
        // Test-fixture authoring only (never protocol or ledger logic, which use checked
        // arithmetic and refuse after u64::MAX): stay pinned at u64::MAX once reached; tests set
        // `next_seq` explicitly when probing boundaries.
        self.next_seq = seq.saturating_add(1);
        SyncEnvelopeV1 {
            v: 1,
            operation_id: self.op_id(seq),
            library_id: self.library,
            replica_id: self.replica,
            client_seq: seq,
            operation,
        }
    }
}
