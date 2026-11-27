use std::time::Instant;

use super::*;
use crate::reader_engine::tests::{rectangle_pdf, TempDocumentDir};

const LAYOUT: PageLayout = PageLayout {
    width: 260.0,
    height: 360.0,
    em: 18.0,
};

struct Fixture {
    _files: TempDocumentDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let files = TempDocumentDir::new("reader-app");
        let root = files.0.join("reader");
        fs::create_dir_all(root.join("books")).unwrap();
        fs::write(root.join("books/book.pdf"), rectangle_pdf()).unwrap();
        Self {
            _files: files,
            root,
        }
    }

    fn mode(&self) -> ReaderMode {
        ReaderMode::start(
            ReaderPaths::new(&self.root, &[]).unwrap(),
            SavePolicy::default(),
        )
        .unwrap()
    }
}

fn event(mode: &mut ReaderMode) -> Event {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(event) = mode.poll(Duration::ZERO).unwrap() {
            return event;
        }
        assert!(Instant::now() < deadline, "reader mode did not reply");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn enter(mode: &mut ReaderMode) {
    mode.enter(Path::new("book.pdf"), LAYOUT).unwrap();
    assert_eq!(mode.state(), ModeState::Opening);
    assert!(matches!(event(mode), Event::Opened { .. }));
    assert_eq!(mode.state(), ModeState::Active);
}

fn finish(mode: ReaderMode) {
    mode.shutdown().ok().unwrap().unwrap();
}

#[test]
fn storage_roots_are_separate_and_book_names_are_leaves() {
    let fixture = Fixture::new();
    let protected = fixture.root.join("notebooks");
    assert!(ReaderPaths::new(&fixture.root, &[&protected]).is_err());
    assert!(ReaderPaths::new(&fixture.root, &[&fixture._files.0]).is_err());
    assert!(ReaderPaths::new(Path::new("relative"), &[]).is_err());
    let paths = ReaderPaths::new(&fixture.root, &[]).unwrap();
    assert_eq!(
        paths.book(Path::new("book.pdf")).unwrap(),
        fixture.root.join("books/book.pdf")
    );
    for bad in ["", ".", "../book.pdf", "books/book.pdf", "/book.pdf"] {
        assert!(paths.book(Path::new(bad)).is_err());
    }
    assert!(!paths.positions().exists());
}

#[cfg(unix)]
#[test]
fn symlinked_storage_is_rejected() {
    let fixture = Fixture::new();
    let outside = fixture._files.0.join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, fixture.root.join("positions")).unwrap();
    assert!(ReaderPaths::new(&fixture.root, &[]).is_err());
    let alias = fixture._files.0.join("alias");
    std::os::unix::fs::symlink(&fixture.root, &alias).unwrap();
    assert!(ReaderPaths::new(&alias, &[]).is_err());
}

#[test]
fn entering_and_leaving_releases_the_session_without_presenting() {
    let fixture = Fixture::new();
    let mut mode = fixture.mode();
    enter(&mut mode);
    assert!(matches!(
        mode.enter(Path::new("book.pdf"), LAYOUT),
        Err(ModeError::InvalidState)
    ));
    mode.request_page(0, 260, 360).unwrap();
    let token = match event(&mut mode) {
        Event::PageReady { pixels, .. } => pixels.token,
        other => panic!("expected page, got {other:?}"),
    };
    assert!(matches!(
        mode.leave(),
        Err(ModeError::Coordinator(CoordinatorError::Busy))
    ));
    mode.discard(token).unwrap();
    mode.leave().unwrap();
    assert_eq!(mode.state(), ModeState::Leaving);
    assert!(matches!(event(&mut mode), Event::Closed));
    assert_eq!(mode.state(), ModeState::Closed);
    assert!(!fixture.root.join("positions").exists());

    enter(&mut mode);
    assert!(matches!(
        mode.presented(token),
        Err(ModeError::Coordinator(CoordinatorError::StalePresentation))
    ));
    mode.leave().unwrap();
    assert!(matches!(event(&mut mode), Event::Closed));
    finish(mode);
}

#[test]
fn leave_during_open_drains_before_allowing_reentry() {
    let fixture = Fixture::new();
    let mut mode = fixture.mode();
    mode.enter(Path::new("book.pdf"), LAYOUT).unwrap();
    mode.leave().unwrap();
    assert_eq!(mode.state(), ModeState::Leaving);
    assert!(matches!(
        mode.request_page(0, 260, 360),
        Err(ModeError::InvalidState)
    ));
    assert!(matches!(event(&mut mode), Event::Opened { .. }));
    assert_eq!(mode.state(), ModeState::Leaving);
    assert!(matches!(event(&mut mode), Event::Closed));
    enter(&mut mode);
    mode.leave().unwrap();
    assert!(matches!(event(&mut mode), Event::Closed));
    finish(mode);
}

#[test]
fn failed_open_returns_to_closed_and_allows_reentry() {
    let fixture = Fixture::new();
    let mut mode = fixture.mode();
    mode.enter(Path::new("missing.pdf"), LAYOUT).unwrap();
    assert!(matches!(event(&mut mode), Event::OpenFailed(_)));
    assert_eq!(mode.state(), ModeState::Closed);
    enter(&mut mode);
    mode.leave().unwrap();
    assert!(matches!(event(&mut mode), Event::Closed));
    finish(mode);
}

#[test]
fn confirmed_page_is_flushed_before_mode_exit() {
    let fixture = Fixture::new();
    let mut mode = fixture.mode();
    enter(&mut mode);
    mode.request_page(0, 260, 360).unwrap();
    let token = match event(&mut mode) {
        Event::PageReady { pixels, .. } => pixels.token,
        other => panic!("expected page, got {other:?}"),
    };
    mode.presented(token).unwrap();
    mode.leave().unwrap();
    assert!(matches!(event(&mut mode), Event::Presented));
    assert!(matches!(
        event(&mut mode),
        Event::SaveComplete(super::super::session::SaveStatus::Saved(1))
    ));
    assert!(matches!(event(&mut mode), Event::Closed));
    assert!(fixture.root.join("positions").is_dir());
    finish(mode);
}
