use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::{ModeState, ReaderMode, ReaderPaths, DEFAULT_READER_ROOT};
use crate::reader_engine::coordinator::{Event, SavePolicy};
use crate::reader_engine::session::PageLayout;

#[derive(Debug)]
pub struct ReaderCheck {
    root: PathBuf,
    book: PathBuf,
}

impl ReaderCheck {
    pub fn parse(args: &[String]) -> Result<Option<Self>, String> {
        if !args.iter().any(|arg| {
            arg.starts_with("--reader-lifecycle-check") || arg.starts_with("--reader-root")
        }) {
            return Ok(None);
        }
        if args.len() % 2 != 0 {
            return Err("reader lifecycle check requires option/value pairs".into());
        }
        let mut root = None;
        let mut book = None;
        for pair in args.chunks_exact(2) {
            if pair[1].is_empty() || pair[1].starts_with("--") {
                return Err("reader lifecycle check requires nonempty values".into());
            }
            match pair[0].as_str() {
                "--reader-lifecycle-check" if book.is_none() => {
                    book = Some(PathBuf::from(&pair[1]));
                }
                "--reader-root" if root.is_none() => {
                    root = Some(PathBuf::from(&pair[1]));
                }
                _ => return Err("reader lifecycle check must be used alone".into()),
            }
        }
        let book = book.ok_or("reader root requires --reader-lifecycle-check")?;
        Ok(Some(Self {
            root: root.unwrap_or_else(|| PathBuf::from(DEFAULT_READER_ROOT)),
            book,
        }))
    }

    pub fn run(&self, stop: &AtomicBool) -> Result<(), String> {
        let protected = [
            Path::new(crate::store::DEFAULT_DATA_DIR),
            Path::new("/mnt/us/scribe-app"),
            Path::new("/mnt/us/scribe-sync"),
            Path::new("/mnt/us/scribe-device"),
        ];
        let paths = ReaderPaths::new(&self.root, &protected)?;
        self.run_with_paths(paths, stop)
    }

    fn run_with_paths(&self, paths: ReaderPaths, stop: &AtomicBool) -> Result<(), String> {
        let layout = PageLayout {
            width: 550.0,
            height: 720.0,
            em: 12.0,
        };
        if stop.load(Ordering::SeqCst) {
            return Err("reader lifecycle check interrupted".into());
        }
        let mut reader = ReaderMode::start(paths, SavePolicy::default())
            .map_err(|error| error.to_string())?;
        reader
            .enter(&self.book, layout)
            .map_err(|error| format!("reader entry: {error:?}"))?;
        let start = Instant::now();
        let mut outcome = Err("reader lifecycle check produced no page".to_owned());
        loop {
            if stop.load(Ordering::SeqCst) {
                outcome = Err("reader lifecycle check interrupted".into());
                reader
                    .leave()
                    .map_err(|error| format!("reader leave: {error:?}"))?;
            }
            match reader
                .poll(start.elapsed())
                .map_err(|error| format!("reader poll: {error:?}"))?
            {
                Some(Event::Opened {
                    selected_page,
                    page_count,
                    ..
                }) if reader.state() == ModeState::Active => {
                    println!("reader_lifecycle_opened page_count={page_count}");
                    if let Some(page) = selected_page {
                        reader
                            .request_page(page, 550, 720)
                            .map_err(|error| format!("reader request: {error:?}"))?;
                    } else {
                        reader
                            .leave()
                            .map_err(|error| format!("reader leave: {error:?}"))?;
                    }
                }
                Some(Event::PageReady { pixels, .. }) => {
                    println!(
                        "reader_lifecycle_rendered page={} width={} height={}",
                        pixels.page, pixels.width, pixels.height
                    );
                    reader
                        .discard(pixels.token)
                        .map_err(|error| format!("reader discard: {error:?}"))?;
                    reader
                        .leave()
                        .map_err(|error| format!("reader leave: {error:?}"))?;
                    outcome = Ok(());
                }
                Some(Event::OpenFailed(error)) => {
                    outcome = Err(format!("reader open: {error:?}"));
                }
                Some(Event::RenderFailed { error, .. }) => {
                    outcome = Err(format!("reader render: {error:?}"));
                    reader
                        .leave()
                        .map_err(|error| format!("reader leave: {error:?}"))?;
                }
                Some(Event::Opened { .. } | Event::Cancelled(_) | Event::Closed) | None => {}
                Some(event) => return Err(format!("unexpected reader diagnostic event: {event:?}")),
            }
            if reader.state() == ModeState::Closed {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        reader
            .shutdown()
            .map_err(|_| "reader did not close".to_owned())?
            .map_err(|_| "reader thread panicked".to_owned())?;
        println!("reader_lifecycle_closed presented=false");
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader_engine::tests::{rectangle_pdf, TempDocumentDir};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn diagnostic_is_explicit_and_rejects_mixed_options() {
        assert!(ReaderCheck::parse(&args(&["--confirm-write"]))
            .unwrap()
            .is_none());
        for invalid in [
            vec!["--reader-root", "/tmp"],
            vec!["--reader-lifecycle-check"],
            vec!["--reader-lifecycle-check", "book.pdf", "--confirm-write", "yes"],
            vec!["--reader-lifecycle-check", "book.pdf", "--reader-root", "--bad"],
        ] {
            assert!(ReaderCheck::parse(&args(&invalid)).is_err());
        }
        let check = ReaderCheck::parse(&args(&["--reader-lifecycle-check", "book.pdf"]))
            .unwrap()
            .unwrap();
        assert_eq!(check.root, Path::new(DEFAULT_READER_ROOT));
    }

    #[test]
    fn diagnostic_opens_renders_discards_and_closes_without_writes() {
        let files = TempDocumentDir::new("reader-app-diagnostic");
        let root = files.0.join("reader");
        std::fs::create_dir_all(root.join("books")).unwrap();
        std::fs::write(root.join("books/book.pdf"), rectangle_pdf()).unwrap();
        let check = ReaderCheck {
            root: root.clone(),
            book: PathBuf::from("book.pdf"),
        };
        let paths = ReaderPaths::new(&root, &[]).unwrap();
        check
            .run_with_paths(paths, &AtomicBool::new(false))
            .unwrap();
        assert!(!root.join("positions").exists());
    }
}
