use sha2::{Digest, Sha256};

use super::super::tests::{cbz_fixture, rectangle_pdf, stored_zip, TempDocumentDir};
use super::super::KnownIdentity;
use super::*;

const WORDS: &[&str] = &[
    "amber", "basalt", "cedar", "dune", "ember", "fjord", "glacier", "harbor", "indigo", "juniper",
    "kestrel", "lagoon", "meadow", "nimbus", "obsidian", "prairie", "quartz", "river", "sierra",
    "tundra", "umber", "valley", "willow", "xenon", "yarrow", "zephyr",
];
const LAYOUT_A: Layout = (418.0, 528.0, 12.0);
const LAYOUT_B: Layout = (300.0, 420.0, 19.0);
const LAYOUT_C: Layout = (600.0, 800.0, 9.0);
const NARROW: Layout = (120.0, 400.0, 14.0);
const OLD_MODEL: &str = "ignite-units-0/mupdf-1.27.0";
const PGM: &[u8] = b"P5\n2 2\n255\n\x00\xff\x80\x40";

type Layout = (f32, f32, f32);

/// Paragraph text in which every 6-word run is unique in the book.
fn paragraph(chapter: usize, index: usize) -> String {
    let mut words = vec![format!("Chapter{chapter}"), format!("paragraph{index}:")];
    for k in 0..40 {
        words.push(WORDS[(chapter * 7 + index * 3 + k * 5) % WORDS.len()].to_string());
        if k % 8 == 7 {
            words.push(format!("c{chapter}p{index}w{k}"));
        }
    }
    words.join(" ") + "."
}

fn chapter_body(chapter: usize, paragraphs: usize) -> String {
    let body: String = (0..paragraphs)
        .map(|p| format!("<p>{}</p>\n", paragraph(chapter, p)))
        .collect();
    format!("<h1>Title{chapter}</h1>\n{body}")
}

fn standard_chapters() -> Vec<String> {
    [9, 14, 3, 11]
        .iter()
        .enumerate()
        .map(|(c, &n)| chapter_body(c, n))
        .collect()
}

/// An EPUB with one spine item per XHTML body, plus extra archive entries (images).
fn epub(bodies: &[String], extra: &[(&str, &[u8])]) -> Vec<u8> {
    let container = br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;
    let manifest: String = (0..bodies.len())
        .map(|i| {
            format!(r#"<item id="c{i}" href="c{i}.xhtml" media-type="application/xhtml+xml"/>"#)
        })
        .collect();
    let spine: String = (0..bodies.len())
        .map(|i| format!(r#"<itemref idref="c{i}"/>"#))
        .collect();
    let package = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="bookid">ignite-anchor-test</dc:identifier><dc:title>Anchors</dc:title><dc:language>en</dc:language></metadata>
<manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
    );
    let documents: Vec<String> = bodies
        .iter()
        .map(|body| {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>t</title></head><body>
{body}</body></html>"#
            )
        })
        .collect();
    let names: Vec<String> = (0..bodies.len())
        .map(|i| format!("OEBPS/c{i}.xhtml"))
        .collect();
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", container),
        ("OEBPS/content.opf", package.as_bytes()),
    ];
    for (name, document) in names.iter().zip(&documents) {
        entries.push((name, document.as_bytes()));
    }
    entries.extend_from_slice(extra);
    stored_zip(&entries)
}

fn identity(bytes: &[u8]) -> DocumentIdentity {
    DocumentIdentity {
        sha256: Sha256::digest(bytes).into(),
        len: bytes.len() as u64,
    }
}

/// An open test document and its identity.
struct Fixture {
    _dir: TempDocumentDir,
    path: std::path::PathBuf,
    identity: DocumentIdentity,
}

impl Fixture {
    fn new(label: &str, bytes: Vec<u8>) -> Self {
        let dir = TempDocumentDir::new(label);
        let path = dir.write("book.epub", &bytes);
        Self {
            _dir: dir,
            path,
            identity: identity(&bytes),
        }
    }

    fn open(&self, layout: Layout) -> Book {
        let mut book = Book::open_path_identified(&self.path, None).unwrap();
        assert_eq!(book.identity(), Some(self.identity));
        relayout(&mut book, layout);
        book
    }
}

fn relayout(book: &mut Book, layout: Layout) {
    book.layout(layout.0, layout.1, layout.2).unwrap();
}

fn anchor(location: ContentLocation) -> TextAnchor {
    match location {
        ContentLocation::Text(anchor) => anchor,
        other => panic!("expected a text anchor, got {other:?}"),
    }
}

fn save(book: &Book, page: u32) -> SavedPosition {
    book.save_position(page).unwrap()
}

fn with_model(saved: &SavedPosition, model: &str) -> SavedPosition {
    let mut anchor = anchor(saved.location.clone());
    anchor.text_model = model.into();
    SavedPosition::new(saved.document, ContentLocation::Text(anchor))
}

/// Independent content check: re-extracts the chapter and requires the anchored context at
/// `offset`, with its first unit on `page` itself (not merely a neighbouring page).
fn assert_content_at(book: &Book, page: u32, chapter: u32, offset: u32, context: &str) {
    let (first, pages) = book.chapter_span(chapter).unwrap().unwrap();
    assert!(
        (first..first + pages).contains(&page),
        "page {page} is outside chapter {chapter}"
    );
    let mut units = Vec::new();
    let mut page_range = 0..0;
    for p in first..first + pages {
        let start = units.len();
        units.extend(book.page_units(p).unwrap());
        if p == page {
            page_range = start..units.len();
        }
    }
    let offset = offset as usize;
    let context: Vec<char> = context.chars().collect();
    assert_eq!(
        units.get(offset..offset + context.len()),
        Some(&context[..]),
        "context is not at offset {offset}"
    );
    if !context.is_empty() {
        assert!(
            page_range.contains(&offset),
            "unit {offset} is not on page {page} (units {page_range:?})"
        );
    }
}

fn exact(book: &Book, saved: &SavedPosition) -> u32 {
    match book.restore(saved).unwrap() {
        Restore::Exact { page, position } => {
            assert_eq!(&position, saved);
            if let ContentLocation::Text(a) = &saved.location {
                assert_content_at(book, page, a.chapter, a.offset, &a.context);
            }
            page
        }
        other => panic!("expected exact restoration of {saved:?}, got {other:?}"),
    }
}

#[test]
fn fixture_has_multipage_chapters_that_repaginate() {
    let fx = Fixture::new("anchor-fixture", epub(&standard_chapters(), &[]));
    let mut book = fx.open(LAYOUT_A);
    let a = book.chapter_page_counts().unwrap();
    relayout(&mut book, LAYOUT_B);
    let b = book.chapter_page_counts().unwrap();
    assert_eq!(a, [3, 4, 1, 3]);
    assert_eq!(b, [13, 20, 5, 15]);
}

#[test]
fn restores_the_same_content_after_font_and_page_size_changes() {
    let fx = Fixture::new("anchor-relayout", epub(&standard_chapters(), &[]));
    let mut book = fx.open(LAYOUT_A);
    let original = book.chapter_page_counts().unwrap()[0] + 2; // third page of chapter 1
    let saved = save(&book, original);
    let a = anchor(saved.location.clone());
    assert_eq!((a.chapter, a.text_model.as_str()), (1, TEXT_MODEL));
    assert!(a.offset > 0);
    assert_eq!(a.context.chars().count(), CONTEXT_UNITS);

    relayout(&mut book, LAYOUT_B);
    let page = exact(&book, &saved);
    assert_ne!(
        page, original,
        "the content must move to another page index"
    );
    relayout(&mut book, LAYOUT_C);
    exact(&book, &saved);
    relayout(&mut book, LAYOUT_A);
    assert_eq!(exact(&book, &saved), original);
}

#[test]
fn restores_after_close_and_reopen_with_a_new_layout() {
    let fx = Fixture::new("anchor-reopen", epub(&standard_chapters(), &[]));
    let book = fx.open(LAYOUT_A);
    let page = book.chapter_page_counts().unwrap()[..3].iter().sum::<u32>() + 1;
    let saved = save(&book, page);
    drop(book);
    let reopened = fx.open(LAYOUT_B);
    let page = exact(&reopened, &saved);
    assert_eq!(reopened.location_of_page(page).unwrap().0, 3);
}

#[test]
fn every_page_including_chapter_boundaries_restores_in_reading_order() {
    let fx = Fixture::new("anchor-all", epub(&standard_chapters(), &[]));
    let mut book = fx.open(LAYOUT_A);
    let total = book.page_count().unwrap();
    // Captured while paging backwards from the last page to the first.
    let saved: Vec<_> = (0..total).rev().map(|p| save(&book, p)).collect();
    for layout in [LAYOUT_B, LAYOUT_C, NARROW] {
        relayout(&mut book, layout);
        let mut previous = None;
        for position in saved.iter().rev() {
            let a = anchor(position.location.clone());
            let page = exact(&book, position);
            let (chapter, in_chapter) = book.location_of_page(page).unwrap();
            assert_eq!(chapter, a.chapter);
            if a.offset == 0 {
                assert_eq!(
                    in_chapter, 0,
                    "a chapter start stays on the chapter's first page"
                );
            }
            assert!(previous <= Some(page), "reading order must be preserved");
            previous = Some(page);
        }
    }
}

#[test]
fn backward_step_across_a_chapter_boundary_stays_in_the_earlier_chapter() {
    let fx = Fixture::new("anchor-back", epub(&standard_chapters(), &[]));
    let mut book = fx.open(LAYOUT_A);
    let last_of_chapter1 = book.chapter_page_counts().unwrap()[..2].iter().sum::<u32>() - 1;
    let saved = save(&book, last_of_chapter1);
    assert_eq!(anchor(saved.location.clone()).chapter, 1);
    relayout(&mut book, LAYOUT_B);
    let page = exact(&book, &saved);
    assert_eq!(book.location_of_page(page).unwrap().0, 1);
    let next = book.location_of_page(page + 1).unwrap();
    assert!(next.0 == 1 || next == (2, 0));
}

#[test]
fn an_older_text_model_relocates_only_a_unique_context() {
    let fx = Fixture::new("anchor-model", epub(&standard_chapters(), &[]));
    let mut book = fx.open(LAYOUT_A);
    let saved = save(&book, 5);
    let a = anchor(saved.location.clone());
    relayout(&mut book, LAYOUT_B);
    let page = exact(&book, &saved);
    assert_eq!(
        book.restore(&with_model(&saved, OLD_MODEL)).unwrap(),
        Restore::Relocated {
            page,
            position: saved.clone()
        },
        "an untrusted offset is never reported as exact; the correction restores the record"
    );
    assert_eq!(a.text_model, TEXT_MODEL);
}

#[test]
fn repeated_paragraphs_are_exact_by_offset_and_ambiguous_by_context() {
    let refrain = "<p>The same refrain repeats again and again, word for word, line by line.</p>\n";
    let mut chapters = standard_chapters();
    chapters[1] = format!(
        "{}{}{}",
        refrain.repeat(20),
        "<p>A unique closing line.</p>",
        refrain.repeat(20)
    );
    let fx = Fixture::new("anchor-repeat", epub(&chapters, &[]));
    let mut book = fx.open(LAYOUT_A);
    let (first, pages) = book.chapter_span(1).unwrap().unwrap();
    assert!(pages >= 3);
    let saved: Vec<_> = (first..first + pages).map(|p| save(&book, p)).collect();
    relayout(&mut book, LAYOUT_B);
    let chapter_start = book.chapter_span(1).unwrap().unwrap().0;
    let mut ambiguous = 0;
    for position in &saved {
        exact(&book, position);
        match book.restore(&with_model(position, OLD_MODEL)).unwrap() {
            Restore::Fallback {
                page,
                target: FallbackTarget::ChapterStart(1),
                cause: Unresolved::Ambiguous { candidates },
            } => {
                assert_eq!(page, chapter_start);
                assert!(candidates >= 2);
                ambiguous += 1;
            }
            Restore::Relocated {
                page,
                position: corrected,
            } => {
                assert_eq!(&corrected, position);
                let a = anchor(position.location.clone());
                assert_content_at(&book, page, 1, a.offset, &a.context);
            }
            Restore::Fallback {
                page,
                cause: Unresolved::Stale(StaleReason::ContextTooShort),
                ..
            } if page == chapter_start => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(
        ambiguous >= 2,
        "repeated text must be reported as ambiguous"
    );
}

#[test]
fn identical_chapters_restore_into_the_saved_chapter() {
    let body = chapter_body(7, 6);
    let chapters = vec![chapter_body(0, 3), body.clone(), body];
    let fx = Fixture::new("anchor-twins", epub(&chapters, &[]));
    let mut book = fx.open(LAYOUT_A);
    let start2 = book.chapter_span(2).unwrap().unwrap().0;
    let saved = save(&book, start2 + 1);
    relayout(&mut book, LAYOUT_B);
    let page = exact(&book, &saved);
    assert_eq!(book.location_of_page(page).unwrap().0, 2);
    match book.restore(&with_model(&saved, OLD_MODEL)).unwrap() {
        Restore::Relocated { page: moved, .. } => assert_eq!(moved, page),
        other => panic!("expected relocation within chapter 2, got {other:?}"),
    }
}

#[test]
fn short_and_empty_chapters_restore_to_their_own_start() {
    let chapters = vec![
        chapter_body(0, 4),
        "<p>Hi.</p>".into(),
        String::new(),
        chapter_body(3, 4),
    ];
    let fx = Fixture::new("anchor-short", epub(&chapters, &[]));
    let mut book = fx.open(LAYOUT_A);
    let counts = book.chapter_page_counts().unwrap();
    assert_eq!(&counts[1..3], [1, 1], "an empty chapter still has one page");
    let short = save(&book, counts[0]);
    let empty = save(&book, counts[0] + 1);
    assert_eq!(anchor(short.location.clone()).context, "Hi.");
    let empty_anchor = anchor(empty.location.clone());
    assert_eq!(
        (
            empty_anchor.chapter,
            empty_anchor.offset,
            empty_anchor.context.as_str()
        ),
        (2, 0, "")
    );

    relayout(&mut book, LAYOUT_B);
    let (short_start, _) = book.chapter_span(1).unwrap().unwrap();
    let (empty_start, _) = book.chapter_span(2).unwrap().unwrap();
    assert_eq!(exact(&book, &short), short_start);
    assert_eq!(exact(&book, &empty), empty_start);
    // A chapter start needs no text model; a short context cannot be relocated.
    let old_empty = with_model(&empty, OLD_MODEL);
    assert_eq!(
        book.restore(&old_empty).unwrap(),
        Restore::Exact {
            page: empty_start,
            position: old_empty.clone()
        }
    );
    assert_eq!(
        book.restore(&with_model(&short, OLD_MODEL)).unwrap(),
        Restore::Fallback {
            page: short_start,
            target: FallbackTarget::ChapterStart(1),
            cause: Unresolved::Stale(StaleReason::ContextTooShort),
        }
    );
}

#[test]
fn images_are_units_so_image_pages_restore_to_the_image() {
    let picture = r#"<p><img src="pic.pgm" style="width:380px;height:460px"/></p>"#;
    let chapters = vec![
        chapter_body(0, 3),
        format!("{}\n{picture}\n{}", chapter_body(1, 4), chapter_body(5, 4)),
        picture.into(),
        chapter_body(3, 2),
    ];
    let fx = Fixture::new("anchor-image", epub(&chapters, &[("OEBPS/pic.pgm", PGM)]));
    let mut book = fx.open(LAYOUT_A);
    let image_pages: Vec<u32> = (0..book.page_count().unwrap())
        .filter(|&p| book.page_units(p).unwrap() == [IMAGE_UNIT])
        .collect();
    assert!(
        image_pages.len() >= 2,
        "fixture needs image-only pages: {image_pages:?}"
    );
    let saved: Vec<_> = image_pages.iter().map(|&p| save(&book, p)).collect();
    for position in &saved {
        assert!(anchor(position.location.clone())
            .context
            .starts_with(IMAGE_UNIT));
    }
    for layout in [LAYOUT_B, NARROW, LAYOUT_A] {
        relayout(&mut book, layout);
        for position in &saved {
            let page = exact(&book, position);
            assert_eq!(book.page_units(page).unwrap().first(), Some(&IMAGE_UNIT));
        }
    }
}

#[test]
fn unicode_text_is_counted_as_reported_without_normalisation() {
    let nfd = "e\u{0301}te\u{0301} Cafe\u{0301} na\u{0308}ive";
    let nfc = "\u{00e9}t\u{00e9} Caf\u{00e9} na\u{00ef}ve";
    let mixed = "Привет, мир! Γειά σου κόσμε. 読書の秋、静かな夜。 हिन्दी पाठ। «guillemets» — “quotes” … ¡hola! ¿qué? a\u{00A0}b c\u{200B}d";
    let arabic = "مرحبا بالعالم، هذا نص عربي طويل للاختبار. ".repeat(120);
    let chapters = vec![
        format!("<p>{}</p>", format!("{nfd} {mixed} ").repeat(30)),
        format!("<p>{}</p>", format!("{nfc} {mixed} ").repeat(30)),
        format!("<p dir=\"rtl\">{arabic}</p>"),
    ];
    let fx = Fixture::new("anchor-unicode", epub(&chapters, &[]));
    let mut book = fx.open(LAYOUT_A);
    let chapter_units = |book: &Book, chapter| {
        let (first, pages) = book.chapter_span(chapter).unwrap().unwrap();
        (first..first + pages)
            .flat_map(|p| book.page_units(p).unwrap())
            .collect::<String>()
    };
    let reference: Vec<String> = (0..3).map(|c| chapter_units(&book, c)).collect();
    assert!(
        reference[0].contains("e\u{0301}te\u{0301}"),
        "combining marks are kept as units"
    );
    assert!(reference[1].contains("\u{00e9}t\u{00e9}"));
    assert_ne!(
        reference[0], reference[1],
        "no normalisation between NFD and NFC"
    );
    assert!(reference[0].contains("Привет,мир!Γειάσουκόσμε.読書の秋、静かな夜。"));
    assert!(
        reference[0].contains("ab") && reference[0].contains("c\u{200B}d"),
        "NBSP is whitespace; ZWSP is a unit"
    );
    assert!(
        reference[2].starts_with("مرحبابالعالم،"),
        "RTL text is in logical order"
    );

    assert!(
        book.chapter_page_counts().unwrap().iter().all(|&n| n > 1),
        "every chapter must span pages"
    );
    let saved: Vec<_> = (0..book.page_count().unwrap())
        .map(|p| save(&book, p))
        .collect();
    for layout in [LAYOUT_B, NARROW, LAYOUT_C] {
        relayout(&mut book, layout);
        for chapter in 0..3 {
            assert_eq!(
                chapter_units(&book, chapter),
                reference[chapter as usize],
                "layout {layout:?}"
            );
        }
        for position in &saved {
            exact(&book, position);
        }
    }

    // An NFC context does not match NFD text: equivalent spellings are different content.
    let nfd_anchor = anchor(saved[0].location.clone());
    let as_nfc = TextAnchor {
        text_model: OLD_MODEL.into(),
        context: "\u{00e9}t\u{00e9}Caf\u{00e9}na\u{00ef}veПривет,мир!".into(),
        ..nfd_anchor
    };
    assert_eq!(
        book.resolve_anchor(&as_nfc).unwrap(),
        Resolution::Unresolved(Unresolved::Stale(StaleReason::ContextNotFound))
    );
}

#[test]
fn soft_hyphens_and_line_breaks_do_not_change_units() {
    let mut chapters = standard_chapters();
    chapters[0] = format!(
        "<p>Supercalifragilistic\u{00AD}expialidocious extra\u{00AD}ordinarily {}</p>",
        "incomprehensibilities ".repeat(30)
    );
    let fx = Fixture::new("anchor-shy", epub(&chapters, &[]));
    let mut book = fx.open(LAYOUT_A);
    let mut seen = Vec::new();
    let mut hyphen_breaks = 0;
    for layout in [LAYOUT_A, LAYOUT_B, LAYOUT_C, NARROW] {
        relayout(&mut book, layout);
        let pages = book.chapter_page_counts().unwrap()[0];
        seen.push(
            (0..pages)
                .flat_map(|p| book.page_units(p).unwrap())
                .collect::<String>(),
        );
        for p in 0..pages {
            let text = book
                .doc
                .load_page(p as i32)
                .unwrap()
                .to_text_page(TextPageFlags::empty())
                .unwrap();
            for block in text.blocks() {
                for line in block.lines() {
                    // MuPDF reports a soft-hyphen line break as a trailing U+00AD.
                    hyphen_breaks += usize::from(
                        line.chars().filter_map(|c| c.char()).last() == Some('\u{00AD}'),
                    );
                }
            }
        }
    }
    assert!(
        hyphen_breaks > 0,
        "fixture must break a word at a soft hyphen"
    );
    assert!(
        seen.windows(2).all(|w| w[0] == w[1]),
        "units depend on layout"
    );
}

#[test]
fn a_different_document_is_rejected_even_when_its_content_would_match() {
    let mut chapters = standard_chapters();
    let v1 = Fixture::new("anchor-v1", epub(&chapters, &[]));
    let book = v1.open(LAYOUT_A);
    let saved = save(&book, book.chapter_page_counts().unwrap()[0] + 3);
    let a = anchor(saved.location.clone());
    drop(book);

    let preface = "An inserted preface paragraph that shifts everything.";
    chapters[1] = chapters[1].replacen("<p>", &format!("<p>{preface}</p>\n<p>"), 1);
    let v2 = Fixture::new("anchor-v2", epub(&chapters, &[]));
    let book = v2.open(LAYOUT_B);
    assert_eq!(
        book.restore(&saved).unwrap(),
        Restore::Rejected(Rejection::DocumentMismatch)
    );
    // The engine could relocate it (a future, explicit edition migration), but restore does not.
    let shift = preface.chars().filter(|&c| is_text_char(c)).count() as u32;
    match book
        .resolve_anchor(&TextAnchor {
            text_model: OLD_MODEL.into(),
            ..a.clone()
        })
        .unwrap()
    {
        Resolution::Relocated { page, offset } => {
            assert_eq!(offset, a.offset + shift);
            assert_content_at(&book, page, 1, offset, &a.context);
        }
        other => panic!("expected relocation, got {other:?}"),
    }
    // Even a byte-identical copy at another path restores: identity is content, not path.
    let copy = Fixture::new("anchor-copy", epub(&standard_chapters(), &[]));
    assert_eq!(copy.identity, v1.identity);
    exact(&copy.open(LAYOUT_C), &saved);
}

#[test]
fn invalid_or_oversized_records_are_rejected_or_stale_never_errors() {
    let fx = Fixture::new("anchor-invalid", epub(&standard_chapters(), &[]));
    let book = fx.open(LAYOUT_A);
    let good = anchor(book.content_location(3).unwrap());
    let text = |a: TextAnchor| SavedPosition::new(fx.identity, ContentLocation::Text(a));
    let rejected = |saved: SavedPosition, rejection| {
        assert_eq!(
            book.restore(&saved).unwrap(),
            Restore::Rejected(rejection),
            "{saved:?}"
        );
    };
    let malformed = [
        TextAnchor {
            context: "x".repeat(1 << 20),
            ..good.clone()
        },
        TextAnchor {
            context: "x".repeat(CONTEXT_UNITS + 1),
            ..good.clone()
        },
        TextAnchor {
            context: "\u{10FFFF}".repeat(CONTEXT_UNITS + 1),
            ..good.clone()
        },
        TextAnchor {
            context: "has space".into(),
            ..good.clone()
        },
        TextAnchor {
            context: "soft\u{00AD}hyphen".into(),
            ..good.clone()
        },
        TextAnchor {
            text_model: String::new(),
            ..good.clone()
        },
        TextAnchor {
            text_model: "m".repeat(MAX_TEXT_MODEL_BYTES + 1),
            ..good.clone()
        },
        TextAnchor {
            text_model: "model\nwith newline".into(),
            ..good.clone()
        },
    ];
    for a in malformed {
        rejected(text(a), Rejection::MalformedAnchor);
    }
    let mut future = text(good.clone());
    future.format = SAVED_POSITION_FORMAT + 1;
    rejected(
        future,
        Rejection::UnsupportedFormat(SAVED_POSITION_FORMAT + 1),
    );
    rejected(
        SavedPosition::new(fx.identity, ContentLocation::FixedPage(0)),
        Rejection::LocationKindMismatch,
    );
    let other = DocumentIdentity {
        len: fx.identity.len + 1,
        ..fx.identity
    };
    rejected(
        SavedPosition {
            document: other,
            ..text(good.clone())
        },
        Rejection::DocumentMismatch,
    );

    let restore = |a: TextAnchor| book.restore(&text(a)).unwrap();
    assert_eq!(
        restore(TextAnchor {
            chapter: u32::MAX,
            ..good.clone()
        }),
        Restore::Fallback {
            page: 0,
            target: FallbackTarget::DocumentStart,
            cause: Unresolved::Stale(StaleReason::ChapterMissing)
        }
    );
    let chapter_start = book.chapter_span(good.chapter).unwrap().unwrap().0;
    let stale = |reason| Restore::Fallback {
        page: chapter_start,
        target: FallbackTarget::ChapterStart(good.chapter),
        cause: Unresolved::Stale(reason),
    };
    assert_eq!(
        restore(TextAnchor {
            context: "nonexistentcontextvalue".into(),
            ..good.clone()
        }),
        stale(StaleReason::ContextNotFound)
    );
    assert_eq!(
        restore(TextAnchor {
            offset: u32::MAX,
            context: String::new(),
            ..good.clone()
        }),
        stale(StaleReason::OffsetOutOfRange)
    );
    // A wrong offset with a unique context is corrected, and reported as such.
    assert!(matches!(
        restore(TextAnchor { offset: u32::MAX, ..good.clone() }),
        Restore::Relocated { position, .. } if position == text(good.clone())
    ));
    assert!(book.content_location(10_000).is_err());
}

#[test]
fn fixed_layout_documents_use_stable_page_indices() {
    let dir = TempDocumentDir::new("anchor-fixed");
    for (name, bytes) in [("a.pdf", rectangle_pdf()), ("a.cbz", cbz_fixture())] {
        let id = identity(&bytes);
        let book = Book::open_path_identified(dir.write(name, &bytes), None).unwrap();
        assert_eq!(book.identity(), Some(id));
        assert_eq!(book.chapter_page_counts().unwrap(), [1], "{name}");
        assert_eq!(book.doc.chapter_page_count(1).unwrap(), 0, "{name}");
        assert_eq!(book.doc.chapter_page_count(-1).unwrap(), 0, "{name}");
        let saved = save(&book, 0);
        assert_eq!(saved.location, ContentLocation::FixedPage(0));
        assert_eq!(
            book.restore(&saved).unwrap(),
            Restore::Exact {
                page: 0,
                position: saved.clone()
            }
        );
        assert_eq!(
            book.restore(&SavedPosition::new(id, ContentLocation::FixedPage(7)))
                .unwrap(),
            Restore::Fallback {
                page: 0,
                target: FallbackTarget::DocumentStart,
                cause: Unresolved::Stale(StaleReason::PageOutOfRange)
            }
        );
        let text = TextAnchor {
            text_model: TEXT_MODEL.into(),
            chapter: 0,
            offset: 0,
            context: String::new(),
        };
        assert_eq!(
            book.restore(&SavedPosition::new(id, ContentLocation::Text(text)))
                .unwrap(),
            Restore::Rejected(Rejection::LocationKindMismatch)
        );
        assert!(book.content_location(1).is_err());
    }
}

#[test]
fn only_successful_restores_yield_a_position() {
    let fx = Fixture::new("anchor-outcomes", epub(&standard_chapters(), &[]));
    let mut book = fx.open(LAYOUT_A);
    let saved = save(&book, 6);
    let mut stale = anchor(saved.location.clone());
    stale.context = "nonexistentcontextvalue".into();
    let stale = SavedPosition::new(fx.identity, ContentLocation::Text(stale));
    let foreign = SavedPosition {
        document: DocumentIdentity {
            sha256: [7; 32],
            ..fx.identity
        },
        ..saved.clone()
    };
    relayout(&mut book, LAYOUT_B);

    let outcomes = [
        book.restore(&saved).unwrap(),
        book.restore(&with_model(&saved, OLD_MODEL)).unwrap(),
        book.restore(&stale).unwrap(),
        book.restore(&foreign).unwrap(),
    ];
    assert!(matches!(outcomes[0], Restore::Exact { .. }));
    assert!(matches!(outcomes[1], Restore::Relocated { .. }));
    assert!(matches!(outcomes[2], Restore::Fallback { .. }));
    assert!(matches!(
        outcomes[3],
        Restore::Rejected(Rejection::DocumentMismatch)
    ));
    let restored: Vec<_> = outcomes.iter().map(Restore::restored).collect();
    let page = exact(&book, &saved);
    assert_eq!(restored[0], Some((page, &saved)));
    assert_eq!(
        restored[1],
        Some((page, &saved)),
        "the relocated record is the current one"
    );
    assert_eq!(restored[2], None, "a fallback is not a restored position");
    assert_eq!(restored[3], None);

    // The fallback page is somewhere to open, not the saved content: anchoring it would record
    // a different location, which is why a fallback carries no position.
    let Restore::Fallback { page: fallback, .. } = outcomes[2] else {
        unreachable!()
    };
    assert_ne!(save(&book, fallback).location, stale.location);
    assert_ne!(save(&book, fallback).location, saved.location);
}

#[test]
fn unidentified_books_cannot_save_or_restore() {
    let fx = Fixture::new("anchor-unidentified", epub(&standard_chapters(), &[]));
    let identified = fx.open(LAYOUT_A);
    let saved = save(&identified, 2);
    let book = Book::open_path(&fx.path).unwrap();
    assert_eq!(book.identity(), None);
    assert_eq!(book.known_identity(), None);
    assert!(book.save_position(2).is_err());
    assert_eq!(
        book.restore(&saved).unwrap(),
        Restore::Rejected(Rejection::DocumentUnidentified)
    );
}

#[cfg(unix)]
#[test]
fn identity_is_hashed_from_the_opened_file_and_reused_only_for_the_same_stamp() {
    let fx = Fixture::new("anchor-identity", epub(&standard_chapters(), &[]));
    let book = Book::open_path_identified(&fx.path, None).unwrap();
    assert_eq!(book.identity(), Some(fx.identity));
    let old = save(&book, 0);
    let stamp = book.stamp.expect("Unix files have a stamp");
    // Just written: the stamp is too recent to reuse, so the next open would hash again.
    assert!(!book.stamp_reusable);
    assert_eq!(book.known_identity(), None);
    drop(book);

    // A matching stamp is trusted without hashing; this is the documented trust boundary.
    let planted = KnownIdentity {
        stamp,
        identity: DocumentIdentity {
            sha256: [9; 32],
            ..fx.identity
        },
    };
    let trusted = Book::open_path_identified(&fx.path, Some(&planted)).unwrap();
    assert_eq!(trusted.identity(), Some(planted.identity));
    assert_eq!(trusted.known_identity(), Some(planted));
    drop(trusted);

    // A changed file never matches an old stamp: replacement by rename (new inode) and an
    // in-place rewrite (new size and times) are both hashed again.
    let replacement = epub(&[chapter_body(9, 3)], &[]);
    let staged = fx.path.with_extension("tmp.epub");
    std::fs::write(&staged, &replacement).unwrap();
    std::fs::rename(&staged, &fx.path).unwrap();
    let renamed = Book::open_path_identified(&fx.path, Some(&planted)).unwrap();
    assert_eq!(renamed.identity(), Some(identity(&replacement)));
    assert_ne!(renamed.stamp.unwrap().ino, stamp.ino);
    let renamed_stamp = renamed.stamp.unwrap();
    drop(renamed);

    let rewritten = epub(&standard_chapters(), &[("OEBPS/extra.txt", b"x")]);
    std::fs::write(&fx.path, &rewritten).unwrap();
    let reused = KnownIdentity {
        stamp: renamed_stamp,
        identity: identity(&replacement),
    };
    let book = Book::open_path_identified(&fx.path, Some(&reused)).unwrap();
    assert_eq!(book.identity(), Some(identity(&rewritten)));
    // A position saved for the replaced content is rejected by the new identity.
    assert_eq!(
        book.restore(&old).unwrap(),
        Restore::Rejected(Rejection::DocumentMismatch)
    );
}

/// Manual benchmark; prints one `key=value` line. Set `IGNITE_ANCHOR_BENCH_PARAGRAPHS` and run
/// alone (`--ignored --exact --test-threads=1`) so peak-RSS figures belong to this test only.
#[test]
#[ignore = "manual benchmark"]
#[cfg(target_os = "linux")]
fn bench_anchor_cost_by_chapter_length() {
    use std::time::Instant;
    fn status(key: &str) -> u64 {
        let s = std::fs::read_to_string("/proc/self/status").unwrap();
        s.lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap()
            .trim()
            .trim_end_matches(" kB")
            .parse()
            .unwrap()
    }
    fn reset_peak() {
        std::fs::write("/proc/self/clear_refs", "5").unwrap();
    }
    let paragraphs: usize = std::env::var("IGNITE_ANCHOR_BENCH_PARAGRAPHS")
        .unwrap()
        .parse()
        .unwrap();
    let bytes = epub(&[chapter_body(0, 2), chapter_body(1, paragraphs)], &[]);
    let fx = Fixture::new("anchor-bench", bytes);
    let mut out = format!("paragraphs={paragraphs} epub_bytes={}", fx.identity.len);
    let mut measure = |label: &str, f: &mut dyn FnMut()| {
        let rss = status("VmRSS:");
        reset_peak();
        let start = Instant::now();
        f();
        let ms = start.elapsed().as_secs_f64() * 1e3;
        out += &format!(
            " {label}_ms={ms:.1} {label}_peak_over_rss_kib={}",
            status("VmHWM:").saturating_sub(rss)
        );
    };
    let mut book = None;
    measure("open_layout", &mut || book = Some(fx.open(LAYOUT_A)));
    let mut unhashed = None;
    measure("open_unidentified", &mut || {
        unhashed = Some(Book::open_path(&fx.path).unwrap())
    });
    drop(unhashed);
    let block = vec![0x5au8; 64 << 20];
    measure("sha256_64mib", &mut || {
        std::hint::black_box(Sha256::digest(&block));
    });
    drop(block);
    let book = book.unwrap();
    let mut span = None;
    // MuPDF parses and lays out chapters lazily, on the first page count.
    measure("first_page_counts", &mut || {
        span = book.chapter_span(1).unwrap()
    });
    let (first, pages) = span.unwrap();
    let last = first + pages - 1;
    let mut units = 0;
    measure("extract_chapter", &mut || {
        units = (first..=last)
            .map(|p| book.page_units(p).unwrap().len())
            .sum::<usize>()
    });
    let mut saved = None;
    measure("anchor_first", &mut || saved = Some(save(&book, first)));
    let early = saved.take().unwrap();
    measure("anchor_last", &mut || saved = Some(save(&book, last)));
    let late = saved.unwrap();
    measure("restore_exact_first", &mut || {
        assert!(matches!(
            book.restore(&early).unwrap(),
            Restore::Exact { .. }
        ))
    });
    measure("restore_exact_last", &mut || {
        assert!(matches!(
            book.restore(&late).unwrap(),
            Restore::Exact { .. }
        ))
    });
    let old = with_model(&early, OLD_MODEL);
    measure("restore_relocate_scan", &mut || {
        assert!(matches!(
            book.restore(&old).unwrap(),
            Restore::Relocated { .. }
        ))
    });
    println!(
        "{out} chapter_pages={pages} chapter_units={units} rss_kib={}",
        status("VmRSS:")
    );
}
