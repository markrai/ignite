//! Experimental reading positions that survive EPUB reflow and restart (milestones C.1–C.1.2).
//!
//! Three concepts are kept apart:
//! - [`DocumentIdentity`]: which bytes a position belongs to. The [`Book`] binds it when opened
//!   with [`Book::open_path_identified`]; callers never supply it to [`Book::restore`];
//! - [`ContentLocation`]: what the reader was looking at, independent of layout;
//! - the current page index, which is valid only for the current layout and is returned by
//!   [`Book::restore`] but never stored.
//!
//! MuPDF's own `fz_make_bookmark` is not used. For EPUB it returns the address of a laid-out text
//! node, which is valid only while that chapter stays in MuPDF's store and is meaningless after
//! the document is closed.
//!
//! A reflowable location is a [`TextAnchor`]: an EPUB spine index, the number of *units* in that
//! chapter before the anchored one, and up to [`CONTEXT_UNITS`] units from the anchor onward.
//! Units come from MuPDF's structured text: every character except whitespace and soft hyphens
//! (layout adds and removes line-break whitespace and reports a word broken at a soft hyphen as
//! ending in U+00AD), plus [`IMAGE_UNIT`] for each image. No Unicode normalisation is applied:
//! units are the characters MuPDF reports, in the order it reports them.
//!
//! Restoration fails closed. A position saved for another document, an unknown record format or
//! a malformed anchor is rejected. Within the same document, the stored offset is trusted only
//! when the text model matches; otherwise the anchor is relocated by its context and only when
//! the context occurs exactly once in the chapter. Anything else is an explicit
//! [`Restore::Fallback`] that names its cause. Only [`Restore::restored`] yields a position that
//! may be stored again or used to anchor an annotation; a fallback page is a place to open, not a
//! location of the user's content.
//!
//! Cost: saving a position extracts the text of the chapter's pages up to the saved page (and up
//! to the chapter's end for the context); an exact restore reads up to the anchor; relocation and
//! every unresolved outcome read the whole chapter. The first page count after a layout change
//! makes MuPDF lay out every chapter. All of it runs on the thread that owns the `Book`.
//!
//! Fixed-layout documents (PDF, CBZ) have stable page indices and use [`ContentLocation::FixedPage`].

use std::collections::VecDeque;

use mupdf::text_page::{TextBlockType, TextPageFlags};

use super::{Book, DocumentIdentity, ReaderError};

mod codec;
pub use codec::{PositionCodecError, MAX_SAVED_POSITION_BYTES};

/// Version of the [`SavedPosition`] record layout.
pub const SAVED_POSITION_FORMAT: u16 = 1;
/// The unit rules of this module and the MuPDF release whose text extraction they count. Change
/// it whenever either changes, so that older offsets are no longer trusted.
pub const TEXT_MODEL: &str = "ignite-units-1/mupdf-1.28.6";
/// Longest text-model identifier accepted from stored data.
pub const MAX_TEXT_MODEL_BYTES: usize = 64;
/// Units stored with an anchor for validation and relocation.
pub const CONTEXT_UNITS: usize = 32;
/// Shortest context that may be used to relocate an anchor whose offset is not trusted.
pub const MIN_RELOCATION_UNITS: usize = 12;
/// The unit that stands for an image. The same character in the book's text is not a unit.
pub const IMAGE_UNIT: char = '\u{FFFC}';

/// A layout-independent position in a reflowable document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextAnchor {
    /// [`TEXT_MODEL`] when the anchor was made.
    pub text_model: String,
    /// EPUB spine index.
    pub chapter: u32,
    /// Units in the chapter before the anchored unit.
    pub offset: u32,
    /// Up to [`CONTEXT_UNITS`] units starting at the anchor; shorter only at the chapter's end.
    pub context: String,
}

/// What the reader was showing, independent of the current layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentLocation {
    /// A page of a fixed-layout document (PDF, CBZ), 0-based.
    FixedPage(u32),
    /// A position in a reflowable document (EPUB).
    Text(TextAnchor),
}

/// A reading position as it would be stored. The page index is deliberately absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedPosition {
    pub format: u16,
    pub document: DocumentIdentity,
    pub location: ContentLocation,
}

impl SavedPosition {
    pub fn new(document: DocumentIdentity, location: ContentLocation) -> Self {
        Self {
            format: SAVED_POSITION_FORMAT,
            document,
            location,
        }
    }
}

/// The outcome of restoring a [`SavedPosition`]. Every page index refers to the current layout.
///
/// `Exact` and `Relocated` are successful restorations and carry the position that now holds.
/// `Fallback` only offers somewhere to open: it carries no position, so it cannot be stored as
/// the reading position or used as an annotation anchor by mistake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restore {
    /// The stored location holds: the anchored content starts on `page`. `position` is the
    /// saved record, unchanged.
    Exact { page: u32, position: SavedPosition },
    /// The stored offset was not trusted or no longer held, and the context occurs exactly once
    /// in the chapter, starting on `page`. `position` is the corrected record (current text
    /// model and offset), which restores exactly.
    Relocated { page: u32, position: SavedPosition },
    /// The content could not be placed. `page` is the start of `target`, offered explicitly.
    /// The saved record should be kept as it is.
    Fallback {
        page: u32,
        target: FallbackTarget,
        cause: Unresolved,
    },
    /// The record does not apply to this document. No page is offered.
    Rejected(Rejection),
}

impl Restore {
    /// The restored page and the position that holds there, only for `Exact` and `Relocated`.
    pub fn restored(&self) -> Option<(u32, &SavedPosition)> {
        match self {
            Self::Exact { page, position } | Self::Relocated { page, position } => {
                Some((*page, position))
            }
            Self::Fallback { .. } | Self::Rejected(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackTarget {
    ChapterStart(u32),
    DocumentStart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unresolved {
    /// The context occurs `candidates` times in the chapter and the offset cannot choose.
    Ambiguous {
        candidates: u32,
    },
    Stale(StaleReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleReason {
    ChapterMissing,
    PageOutOfRange,
    OffsetOutOfRange,
    ContextTooShort,
    ContextNotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    UnsupportedFormat(u16),
    /// The book was opened without an identity ([`Book::open_path`] or [`Book::open_bytes`]).
    DocumentUnidentified,
    /// The position was saved for a different document (or a different edition of it).
    DocumentMismatch,
    /// A fixed-page location for a reflowable document, or the reverse.
    LocationKindMismatch,
    /// Oversized or invalid anchor fields.
    MalformedAnchor,
}

/// How an anchor resolved in the current layout, before fallback policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resolution {
    Exact { page: u32 },
    Relocated { page: u32, offset: u32 },
    Unresolved(Unresolved),
}

fn is_text_char(c: char) -> bool {
    !c.is_whitespace() && c != '\u{00AD}' && c != IMAGE_UNIT
}

fn is_unit(c: char) -> bool {
    is_text_char(c) || c == IMAGE_UNIT
}

/// Checks the fields of an externally supplied anchor before any document work. Bounds the
/// byte length before decoding so oversized input costs nothing beyond its own storage.
fn validate_anchor(anchor: &TextAnchor) -> Result<(), Rejection> {
    let model = &anchor.text_model;
    let context = &anchor.context;
    if model.is_empty() || model.len() > MAX_TEXT_MODEL_BYTES || model.chars().any(char::is_control)
    {
        return Err(Rejection::MalformedAnchor);
    }
    if context.len() > CONTEXT_UNITS * char::MAX.len_utf8()
        || context.chars().count() > CONTEXT_UNITS
        || !context.chars().all(is_unit)
    {
        return Err(Rejection::MalformedAnchor);
    }
    Ok(())
}

impl Book {
    /// Pages per chapter under the current layout. Fixed-layout documents have one chapter.
    pub fn chapter_page_counts(&self) -> Result<Vec<u32>, ReaderError> {
        let chapters = self.doc.chapter_count()?;
        (0..chapters)
            .map(|chapter| {
                u32::try_from(self.doc.chapter_page_count(chapter)?)
                    .map_err(|_| ReaderError("negative chapter page count".into()))
            })
            .collect()
    }

    /// `(chapter, page within chapter)` for a 0-based page index of the whole document.
    pub fn location_of_page(&self, page: u32) -> Result<(u32, u32), ReaderError> {
        let mut first = 0u32;
        for (chapter, pages) in self.chapter_page_counts()?.into_iter().enumerate() {
            if page - first < pages {
                return Ok((chapter as u32, page - first));
            }
            first += pages;
        }
        Err(ReaderError(format!("page {page} is outside {first} pages")))
    }

    /// The position of the start of `page`, for the identity this book was opened with.
    pub fn save_position(&self, page: u32) -> Result<SavedPosition, ReaderError> {
        let identity = self.identity.ok_or_else(|| {
            ReaderError("a position needs a book opened with its identity".into())
        })?;
        Ok(SavedPosition::new(identity, self.content_location(page)?))
    }

    /// The layout-independent location of the start of `page`.
    fn content_location(&self, page: u32) -> Result<ContentLocation, ReaderError> {
        if self.is_reflowable()? {
            Ok(ContentLocation::Text(self.anchor_at_page_start(page)?))
        } else if page < self.page_count()? {
            Ok(ContentLocation::FixedPage(page))
        } else {
            Err(ReaderError(format!("page {page} is out of range")))
        }
    }

    /// Restores `saved` in the current layout. A record for any identity other than the one this
    /// book was opened with is rejected.
    pub fn restore(&self, saved: &SavedPosition) -> Result<Restore, ReaderError> {
        if saved.format != SAVED_POSITION_FORMAT {
            return Ok(Restore::Rejected(Rejection::UnsupportedFormat(
                saved.format,
            )));
        }
        let Some(identity) = self.identity else {
            return Ok(Restore::Rejected(Rejection::DocumentUnidentified));
        };
        if saved.document != identity {
            return Ok(Restore::Rejected(Rejection::DocumentMismatch));
        }
        let anchor = match (&saved.location, self.is_reflowable()?) {
            (ContentLocation::Text(anchor), true) => anchor,
            (ContentLocation::FixedPage(page), false) => {
                return Ok(if *page < self.page_count()? {
                    Restore::Exact {
                        page: *page,
                        position: saved.clone(),
                    }
                } else {
                    Restore::Fallback {
                        page: 0,
                        target: FallbackTarget::DocumentStart,
                        cause: Unresolved::Stale(StaleReason::PageOutOfRange),
                    }
                });
            }
            _ => return Ok(Restore::Rejected(Rejection::LocationKindMismatch)),
        };
        if let Err(rejection) = validate_anchor(anchor) {
            return Ok(Restore::Rejected(rejection));
        }
        Ok(match self.resolve_anchor(anchor)? {
            Resolution::Exact { page } => Restore::Exact {
                page,
                position: saved.clone(),
            },
            Resolution::Relocated { page, offset } => Restore::Relocated {
                page,
                position: SavedPosition::new(
                    identity,
                    ContentLocation::Text(TextAnchor {
                        text_model: TEXT_MODEL.to_owned(),
                        offset,
                        ..anchor.clone()
                    }),
                ),
            },
            Resolution::Unresolved(cause @ Unresolved::Stale(StaleReason::ChapterMissing)) => {
                Restore::Fallback {
                    page: 0,
                    target: FallbackTarget::DocumentStart,
                    cause,
                }
            }
            Resolution::Unresolved(cause) => Restore::Fallback {
                page: self
                    .chapter_span(anchor.chapter)?
                    .map_or(0, |(first, _)| first),
                target: FallbackTarget::ChapterStart(anchor.chapter),
                cause,
            },
        })
    }

    /// `(first page, page count)` of a chapter under the current layout.
    fn chapter_span(&self, chapter: u32) -> Result<Option<(u32, u32)>, ReaderError> {
        let counts = self.chapter_page_counts()?;
        Ok(counts
            .get(chapter as usize)
            .map(|&pages| (counts[..chapter as usize].iter().sum(), pages)))
    }

    /// The units of one page, in MuPDF's reading order.
    fn page_units(&self, page: u32) -> Result<Vec<char>, ReaderError> {
        let index = i32::try_from(page).map_err(|_| ReaderError("page index too large".into()))?;
        let text = self
            .doc
            .load_page(index)?
            .to_text_page(TextPageFlags::PRESERVE_IMAGES)?;
        let mut units = Vec::new();
        for block in text.blocks() {
            match block.r#type() {
                TextBlockType::Image => units.push(IMAGE_UNIT),
                TextBlockType::Text => {
                    for line in block.lines() {
                        units.extend(
                            line.chars()
                                .filter_map(|c| c.char())
                                .filter(|&c| is_text_char(c)),
                        );
                    }
                }
                _ => {}
            }
        }
        Ok(units)
    }

    /// Anchors the first unit on `page`, or the next unit in the same chapter if the page has
    /// none. Reads the chapter's pages up to `page` plus as many as the context needs.
    fn anchor_at_page_start(&self, page: u32) -> Result<TextAnchor, ReaderError> {
        let (chapter, page_in_chapter) = self.location_of_page(page)?;
        let first = page - page_in_chapter;
        let pages = u32::try_from(self.doc.chapter_page_count(chapter as i32)?)
            .map_err(|_| ReaderError("negative chapter page count".into()))?;
        let mut offset = 0usize;
        for before in first..page {
            offset += self.page_units(before)?.len();
        }
        let mut context = String::new();
        let mut taken = 0;
        for next in page..first + pages {
            for unit in self
                .page_units(next)?
                .into_iter()
                .take(CONTEXT_UNITS - taken)
            {
                context.push(unit);
                taken += 1;
            }
            if taken == CONTEXT_UNITS {
                break;
            }
        }
        Ok(TextAnchor {
            text_model: TEXT_MODEL.to_owned(),
            chapter,
            offset: u32::try_from(offset).map_err(|_| ReaderError("chapter too long".into()))?,
            context,
        })
    }

    /// Finds an anchor in the current layout without identity checks. Streams the chapter one
    /// page at a time: memory is one page's units plus the page start table, whatever the anchor.
    fn resolve_anchor(&self, anchor: &TextAnchor) -> Result<Resolution, ReaderError> {
        let stale = |reason| Ok(Resolution::Unresolved(Unresolved::Stale(reason)));
        let Some((first, pages)) = self.chapter_span(anchor.chapter)? else {
            return stale(StaleReason::ChapterMissing);
        };
        let trust_offset = anchor.text_model == TEXT_MODEL;
        let context: Vec<char> = anchor.context.chars().collect();
        let len = context.len() as u64;
        let offset = u64::from(anchor.offset);
        // The start of a chapter is the same in every text model.
        if offset == 0 && context.is_empty() {
            return Ok(Resolution::Exact { page: first });
        }

        let mut page_starts: Vec<u64> = Vec::with_capacity(pages as usize);
        let page_of = |starts: &[u64], unit: u64| {
            first
                + starts
                    .partition_point(|&start| start <= unit)
                    .saturating_sub(1) as u32
        };
        let mut window = VecDeque::with_capacity(context.len());
        let mut seen = 0u64;
        let mut candidates = 0u32;
        let mut found = 0u64;
        for page in first..first + pages {
            page_starts.push(seen);
            for unit in self.page_units(page)? {
                if window.len() == context.len() {
                    window.pop_front();
                }
                window.push_back(unit);
                seen += 1;
                if !context.is_empty()
                    && window.len() == context.len()
                    && window.iter().eq(&context)
                {
                    let start = seen - len;
                    if trust_offset && start == offset {
                        return Ok(Resolution::Exact {
                            page: page_of(&page_starts, start),
                        });
                    }
                    candidates = candidates.saturating_add(1);
                    if candidates == 1 {
                        found = start;
                    }
                }
            }
        }

        if context.is_empty() {
            // Only an anchor at the very end of a chapter has no context.
            return if trust_offset && offset == seen {
                Ok(Resolution::Exact {
                    page: first + pages.saturating_sub(1),
                })
            } else {
                stale(StaleReason::OffsetOutOfRange)
            };
        }
        match candidates {
            0 => stale(StaleReason::ContextNotFound),
            _ if context.len() < MIN_RELOCATION_UNITS => stale(StaleReason::ContextTooShort),
            1 => Ok(Resolution::Relocated {
                page: page_of(&page_starts, found),
                offset: u32::try_from(found).map_err(|_| ReaderError("chapter too long".into()))?,
            }),
            candidates => Ok(Resolution::Unresolved(Unresolved::Ambiguous { candidates })),
        }
    }
}

#[cfg(test)]
mod tests;
