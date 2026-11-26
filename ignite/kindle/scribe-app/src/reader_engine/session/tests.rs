use std::fs;

use super::*;
use crate::reader_engine::location::{
    ContentLocation, Restore, StaleReason, TextAnchor, Unresolved,
};
use crate::reader_engine::tests::{cbz_fixture, rectangle_pdf, stored_zip, TempDocumentDir};

const NARROW: PageLayout = PageLayout {
    width: 260.0,
    height: 360.0,
    em: 18.0,
};
const WIDE: PageLayout = PageLayout {
    width: 550.0,
    height: 720.0,
    em: 10.0,
};

fn epub_fixture(extra: &str) -> Vec<u8> {
    let container = br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;
    let package = br#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="bookid">ignite-session</dc:identifier><dc:title>Reader Session</dc:title><dc:language>en</dc:language></metadata>
<manifest><item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/></manifest>
<spine><itemref idref="chapter"/></spine>
</package>"#;
    let mut paragraphs = String::new();
    for index in 0..90 {
        paragraphs.push_str(&format!(
            "<p>Unique passage {index:03}: quiet forests overlook a narrow valley with streams and open skies, number {index}.</p>"
        ));
    }
    let html = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Session</title></head><body>{paragraphs}<p>{extra}</p></body></html>"#
    );
    stored_zip(&[
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", container),
        ("OEBPS/content.opf", package),
        ("OEBPS/chapter.xhtml", html.as_bytes()),
    ])
}

struct Fixture {
    _files: TempDocumentDir,
    path: std::path::PathBuf,
    store_root: std::path::PathBuf,
}

impl Fixture {
    fn new(name: &str, bytes: &[u8]) -> Self {
        let files = TempDocumentDir::new("session");
        let path = files.write(name, bytes);
        let store_root = files.0.join("reader-positions");
        Self {
            _files: files,
            path,
            store_root,
        }
    }

    fn open(&self, layout: PageLayout) -> ReaderSession {
        ReaderSession::open(&self.path, PositionStore::new(&self.store_root), layout).unwrap()
    }

    fn store(&self) -> PositionStore {
        PositionStore::new(&self.store_root)
    }

    fn confirm(&self, session: &mut ReaderSession) {
        let rendered = session.render_selected(260, 360).unwrap();
        assert!(!rendered.image.pixels().is_empty());
        session.acknowledge_displayed(rendered.token).unwrap();
    }
}

#[test]
fn new_pdf_stays_unwritten_until_page_is_rendered_and_acknowledged() {
    let fixture = Fixture::new("book.pdf", &rectangle_pdf());
    let mut session = fixture.open(NARROW);
    assert_eq!(session.status(), &OpenStatus::New);
    assert_eq!(session.selected_page(), Some(0));
    assert_eq!(session.displayed_page(), None);
    assert_eq!(
        session.save_displayed().unwrap(),
        SaveStatus::NoDisplayedPage
    );
    assert!(!fixture.store_root.exists());
    assert!(matches!(
        session.navigate_to(1),
        Err(SessionError::PageOutOfRange { page: 1, total: 1 })
    ));
    assert_eq!(session.selected_page(), Some(0));
    assert!(session.render_selected(0, 200).is_err());
    assert_eq!(
        session.save_displayed().unwrap(),
        SaveStatus::NoDisplayedPage
    );
    let rendered = session.render_selected(260, 360).unwrap();
    assert!(matches!(
        session.acknowledge_displayed(rendered.token + 1),
        Err(SessionError::StaleRender)
    ));
    assert_eq!(
        session.save_displayed().unwrap(),
        SaveStatus::NoDisplayedPage
    );
    session.acknowledge_displayed(rendered.token).unwrap();
    assert_eq!(session.displayed_page(), Some(0));
    assert_eq!(session.save_displayed().unwrap(), SaveStatus::Saved(1));
    assert_eq!(session.save_displayed().unwrap(), SaveStatus::Unchanged);
    drop(session);

    let mut reopened = fixture.open(WIDE);
    assert!(matches!(
        reopened.status(),
        OpenStatus::Saved(Restore::Exact { page: 0, .. })
    ));
    fixture.confirm(&mut reopened);
    assert_eq!(reopened.save_displayed().unwrap(), SaveStatus::Unchanged);
}

#[test]
fn fixed_layout_pdf_and_cbz_reopen_at_saved_position() {
    for (name, bytes) in [("file.pdf", rectangle_pdf()), ("file.cbz", cbz_fixture())] {
        let fixture = Fixture::new(name, &bytes);
        let mut session = fixture.open(NARROW);
        fixture.confirm(&mut session);
        assert_eq!(session.save_displayed().unwrap(), SaveStatus::Saved(1));
        drop(session);
        let mut reopened = fixture.open(WIDE);
        assert!(matches!(
            reopened.status(),
            OpenStatus::Saved(Restore::Exact { page: 0, .. })
        ));
        fixture.confirm(&mut reopened);
        assert_eq!(reopened.save_displayed().unwrap(), SaveStatus::Unchanged);
    }
}

#[test]
fn epub_reflows_and_preserves_content_anchor_until_user_navigates() {
    let fixture = Fixture::new("book.epub", &epub_fixture("First edition"));
    let mut original = fixture.open(NARROW);
    assert!(original.page_count() > 4);
    let requested = original.page_count() / 2;
    original.navigate_to(requested).unwrap();
    fixture.confirm(&mut original);
    assert_eq!(original.save_displayed().unwrap(), SaveStatus::Saved(1));
    let identity = original.book.identity().unwrap();
    let saved = fixture.store().load(&identity).unwrap().unwrap();
    let old_anchor = match &saved.location {
        ContentLocation::Text(value) => value,
        _ => panic!("expected EPUB text anchor"),
    };
    assert!(old_anchor.offset > 0);
    drop(original);

    let mut reopened = fixture.open(WIDE);
    let current_page = match reopened.status() {
        OpenStatus::Saved(Restore::Exact { page, position }) => {
            assert_eq!(position, &saved);
            *page
        }
        other => panic!("expected exact EPUB restoration: {other:?}"),
    };
    assert_ne!(current_page, requested);
    assert_eq!(reopened.selected_page(), Some(current_page));
    fixture.confirm(&mut reopened);
    assert_eq!(reopened.save_displayed().unwrap(), SaveStatus::Unchanged);
    assert_eq!(
        fixture.store().load(&identity).unwrap(),
        Some(saved.clone())
    );

    let next = if current_page + 1 < reopened.page_count() {
        current_page + 1
    } else {
        current_page - 1
    };
    reopened.navigate_to(next).unwrap();
    assert_eq!(reopened.save_displayed().unwrap(), SaveStatus::Unchanged);
    fixture.confirm(&mut reopened);
    assert_eq!(reopened.save_displayed().unwrap(), SaveStatus::Saved(2));
    let updated = fixture.store().load(&identity).unwrap().unwrap();
    assert_ne!(updated, saved);
    assert_eq!(updated.document, saved.document);
    drop(reopened);

    let last = fixture.open(WIDE);
    assert!(
        matches!(last.status(), OpenStatus::Saved(Restore::Exact { page, .. }) if *page == next)
    );
}

#[test]
fn navigation_and_failed_render_do_not_persist_an_unseen_page() {
    let fixture = Fixture::new("book.epub", &epub_fixture("Navigation"));
    let mut session = fixture.open(NARROW);
    fixture.confirm(&mut session);
    assert_eq!(session.save_displayed().unwrap(), SaveStatus::Saved(1));
    let identity = session.book.identity().unwrap();
    let prior = fixture.store().load(&identity).unwrap().unwrap();

    session.navigate_to(1).unwrap();
    assert_eq!(session.displayed_page(), Some(0));
    assert_eq!(session.save_displayed().unwrap(), SaveStatus::Unchanged);
    assert!(session.render_selected(0, 100).is_err());
    assert_eq!(session.save_displayed().unwrap(), SaveStatus::Unchanged);
    assert_eq!(fixture.store().load(&identity).unwrap(), Some(prior));

    fixture.confirm(&mut session);
    assert_eq!(session.displayed_page(), Some(1));
    assert_eq!(session.save_displayed().unwrap(), SaveStatus::Saved(2));
}

#[test]
fn old_render_tokens_cannot_acknowledge_newer_or_other_page_renders() {
    let fixture = Fixture::new("book.epub", &epub_fixture("Tokens"));
    let mut session = fixture.open(NARROW);
    let first = session.render_selected(260, 360).unwrap();
    let second = session.render_selected(260, 360).unwrap();
    assert!(matches!(
        session.acknowledge_displayed(first.token),
        Err(SessionError::StaleRender)
    ));
    session.acknowledge_displayed(second.token).unwrap();
    assert!(matches!(
        session.acknowledge_displayed(second.token),
        Err(SessionError::StaleRender)
    ));
    session.navigate_to(1).unwrap();
    let stale = session.render_selected(260, 360).unwrap();
    session.navigate_to(2).unwrap();
    assert!(matches!(
        session.acknowledge_displayed(stale.token),
        Err(SessionError::StaleRender)
    ));
    assert_eq!(session.displayed_page(), Some(0));
    assert_eq!(session.save_displayed().unwrap(), SaveStatus::Saved(1));
}

#[test]
fn invalid_navigation_does_not_invalidate_a_pending_valid_render() {
    let fixture = Fixture::new("book.pdf", &rectangle_pdf());
    let mut session = fixture.open(NARROW);
    let rendered = session.render_selected(200, 200).unwrap();
    assert!(matches!(
        session.navigate_to(u32::MAX),
        Err(SessionError::PageOutOfRange { .. })
    ));
    session.acknowledge_displayed(rendered.token).unwrap();
    assert_eq!(session.save_displayed().unwrap(), SaveStatus::Saved(1));
}

#[test]
fn unresolvable_saved_anchor_never_gets_overwritten_by_a_fallback() {
    let fixture = Fixture::new("book.epub", &epub_fixture("Fallback"));
    let session = fixture.open(NARROW);
    let mut position = session.book.save_position(2).unwrap();
    let identity = position.document;
    if let ContentLocation::Text(anchor) = &mut position.location {
        anchor.offset = 987;
        anchor.text_model = "older-text-model".to_owned();
        anchor.context = "ThisDoesNotAppearInTheBook123".to_owned();
    }
    assert_eq!(fixture.store().save(&position).unwrap(), 1);
    drop(session);

    let mut reopened = fixture.open(NARROW);
    assert!(matches!(
        reopened.status(),
        OpenStatus::Saved(Restore::Fallback {
            page: 0,
            cause: Unresolved::Stale(StaleReason::ContextNotFound),
            ..
        })
    ));
    fixture.confirm(&mut reopened);
    assert_eq!(
        reopened.save_displayed().unwrap(),
        SaveStatus::ProtectedPosition
    );
    assert_eq!(
        fixture.store().load(&identity).unwrap(),
        Some(position.clone())
    );

    reopened.navigate_to(1).unwrap();
    assert_eq!(
        reopened.save_displayed().unwrap(),
        SaveStatus::ProtectedPosition
    );
    fixture.confirm(&mut reopened);
    assert_eq!(reopened.save_displayed().unwrap(), SaveStatus::Saved(2));
    assert_ne!(fixture.store().load(&identity).unwrap(), Some(position));
}

#[test]
fn rejected_saved_position_requires_an_explicit_navigation_choice() {
    let fixture = Fixture::new("book.epub", &epub_fixture("Rejected"));
    let session = fixture.open(NARROW);
    let mut position = session.book.save_position(1).unwrap();
    position.location = ContentLocation::FixedPage(1);
    fixture.store().save(&position).unwrap();
    drop(session);

    let mut reopened = fixture.open(NARROW);
    assert!(matches!(
        reopened.status(),
        OpenStatus::Saved(Restore::Rejected(_))
    ));
    assert_eq!(reopened.status().suggested_page(), None);
    assert_eq!(reopened.selected_page(), None);
    assert!(matches!(
        reopened.render_selected(200, 200),
        Err(SessionError::NoSelectedPage)
    ));
    assert_eq!(
        reopened.save_displayed().unwrap(),
        SaveStatus::NoDisplayedPage
    );
    reopened.navigate_to(1).unwrap();
    fixture.confirm(&mut reopened);
    assert_eq!(reopened.save_displayed().unwrap(), SaveStatus::Saved(2));
}

#[test]
fn corrupt_store_stops_open_without_creating_an_empty_session() {
    let fixture = Fixture::new("book.pdf", &rectangle_pdf());
    let mut original = fixture.open(NARROW);
    fixture.confirm(&mut original);
    original.save_displayed().unwrap();
    let identity = original.book.identity().unwrap();
    let path = &fixture.store_root;
    drop(original);

    let slot = std::fs::read_dir(path)
        .unwrap()
        .map(|x| x.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "slot0"))
        .unwrap();
    fs::write(&slot, b"bad").unwrap();
    let result = ReaderSession::open(&fixture.path, fixture.store(), NARROW);
    assert!(matches!(
        result,
        Err(SessionError::Store(PositionStoreError::Corrupt))
    ));
    assert_eq!(fs::read(&slot).unwrap(), b"bad");
    assert!(fixture.store().load(&identity).is_err());
}

#[test]
fn different_edition_starts_fresh_and_old_position_survives() {
    let fixture = Fixture::new("book.epub", &epub_fixture("Version one"));
    let mut old = fixture.open(NARROW);
    fixture.confirm(&mut old);
    old.save_displayed().unwrap();
    let previous_identity = old.book.identity().unwrap();
    let previous_position = fixture.store().load(&previous_identity).unwrap().unwrap();
    drop(old);

    fs::write(&fixture.path, epub_fixture("Version two")).unwrap();
    let new = fixture.open(NARROW);
    assert_eq!(new.status(), &OpenStatus::New);
    assert_ne!(new.book.identity().unwrap(), previous_identity);
    assert_eq!(
        fixture.store().load(&previous_identity).unwrap(),
        Some(previous_position)
    );
}

#[cfg(unix)]
#[test]
fn failed_store_write_preserves_old_position_and_can_retry() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new("book.epub", &epub_fixture("Retry"));
    let mut session = fixture.open(NARROW);
    fixture.confirm(&mut session);
    assert_eq!(session.save_displayed().unwrap(), SaveStatus::Saved(1));
    let identity = session.book.identity().unwrap();
    let previous = fixture.store().load(&identity).unwrap().unwrap();

    session.navigate_to(1).unwrap();
    fixture.confirm(&mut session);
    let lock = fixture.store_root.join(".writer-lock");
    fs::remove_file(&lock).unwrap();
    symlink(fixture.path.clone(), &lock).unwrap();
    assert!(matches!(
        session.save_displayed(),
        Err(SessionError::Store(PositionStoreError::Io(_)))
    ));
    assert_eq!(fixture.store().load(&identity).unwrap(), Some(previous));
    fs::remove_file(lock).unwrap();
    assert_eq!(session.save_displayed().unwrap(), SaveStatus::Saved(2));
}

#[test]
fn dropping_session_does_not_autosave() {
    let fixture = Fixture::new("book.epub", &epub_fixture("Close"));
    let mut session = fixture.open(NARROW);
    session.navigate_to(1).unwrap();
    fixture.confirm(&mut session);
    drop(session);
    assert!(!fixture.store_root.exists());
    let reopened = fixture.open(NARROW);
    assert_eq!(reopened.status(), &OpenStatus::New);
}
