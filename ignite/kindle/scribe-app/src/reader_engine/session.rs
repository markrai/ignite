use std::path::Path;

use super::location::{PositionStore, PositionStoreError, Restore, SavedPosition};
use super::{Book, GrayPage, ReaderError};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageLayout {
    pub width: f32,
    pub height: f32,
    pub em: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenStatus {
    New,
    Saved(Restore),
}

impl OpenStatus {
    pub fn suggested_page(&self) -> Option<u32> {
        match self {
            Self::New => Some(0),
            Self::Saved(Restore::Exact { page, .. })
            | Self::Saved(Restore::Relocated { page, .. })
            | Self::Saved(Restore::Fallback { page, .. }) => Some(*page),
            Self::Saved(Restore::Rejected(_)) => None,
        }
    }
}

#[derive(Debug)]
pub enum SessionError {
    Reader(ReaderError),
    Store(PositionStoreError),
    EmptyDocument,
    PageOutOfRange { page: u32, total: u32 },
    NoSelectedPage,
    StaleRender,
    RenderTokenExhausted,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reader(error) => write!(f, "{error}"),
            Self::Store(error) => write!(f, "{error}"),
            Self::EmptyDocument => write!(f, "document has no pages"),
            Self::PageOutOfRange { page, total } => {
                write!(f, "page {page} is outside a {total}-page document")
            }
            Self::NoSelectedPage => write!(f, "no page selected for rendering"),
            Self::StaleRender => write!(f, "render acknowledgment is missing or stale"),
            Self::RenderTokenExhausted => write!(f, "render-token sequence exhausted"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<ReaderError> for SessionError {
    fn from(error: ReaderError) -> Self {
        Self::Reader(error)
    }
}

impl From<PositionStoreError> for SessionError {
    fn from(error: PositionStoreError) -> Self {
        Self::Store(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveStatus {
    NoDisplayedPage,
    ProtectedPosition,
    Unchanged,
    Saved(u64),
}

pub struct RenderedPage {
    pub page: u32,
    pub token: u64,
    pub image: GrayPage,
}

#[derive(Clone)]
enum Source {
    Saved(SavedPosition),
    Page,
    Protected,
}

#[derive(Clone)]
struct Selection {
    page: u32,
    source: Source,
}

pub struct ReaderSession {
    book: Book,
    store: PositionStore,
    status: OpenStatus,
    page_count: u32,
    selected: Option<Selection>,
    rendered: Option<(u64, u32)>,
    displayed: Option<Selection>,
    persisted: Option<SavedPosition>,
    next_token: u64,
}

impl ReaderSession {
    pub fn open(
        path: impl AsRef<Path>,
        store: PositionStore,
        layout: PageLayout,
    ) -> Result<Self, SessionError> {
        let mut book = Book::open_path_identified(path, None)?;
        book.layout(layout.width, layout.height, layout.em)?;
        let page_count = book.page_count()?;
        if page_count == 0 {
            return Err(SessionError::EmptyDocument);
        }
        let identity = book.identity().ok_or_else(|| {
            SessionError::Reader(ReaderError("identified book has no identity".into()))
        })?;
        let persisted = store.load(&identity)?;
        let status = match &persisted {
            None => OpenStatus::New,
            Some(saved) => OpenStatus::Saved(book.restore(saved)?),
        };
        let selected = match &status {
            OpenStatus::New => Some(Selection {
                page: 0,
                source: Source::Page,
            }),
            OpenStatus::Saved(Restore::Exact { page, position })
            | OpenStatus::Saved(Restore::Relocated { page, position }) => Some(Selection {
                page: *page,
                source: Source::Saved(position.clone()),
            }),
            OpenStatus::Saved(Restore::Fallback { page, .. }) => Some(Selection {
                page: *page,
                source: Source::Protected,
            }),
            OpenStatus::Saved(Restore::Rejected(_)) => None,
        };
        Ok(Self {
            book,
            store,
            status,
            page_count,
            selected,
            rendered: None,
            displayed: None,
            persisted,
            next_token: 0,
        })
    }

    pub fn status(&self) -> &OpenStatus {
        &self.status
    }

    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    pub fn selected_page(&self) -> Option<u32> {
        self.selected.as_ref().map(|selection| selection.page)
    }

    pub fn displayed_page(&self) -> Option<u32> {
        self.displayed.as_ref().map(|displayed| displayed.page)
    }

    pub fn navigate_to(&mut self, page: u32) -> Result<(), SessionError> {
        if page >= self.page_count {
            return Err(SessionError::PageOutOfRange {
                page,
                total: self.page_count,
            });
        }
        if self.selected_page() != Some(page) {
            self.selected = Some(Selection {
                page,
                source: Source::Page,
            });
            self.rendered = None;
        }
        Ok(())
    }

    pub fn render_selected(
        &mut self,
        box_width: u32,
        box_height: u32,
    ) -> Result<RenderedPage, SessionError> {
        self.rendered = None;
        let page = self.selected_page().ok_or(SessionError::NoSelectedPage)?;
        let token = self
            .next_token
            .checked_add(1)
            .ok_or(SessionError::RenderTokenExhausted)?;
        let image = self.book.render_page_gray(page, box_width, box_height)?;
        self.next_token = token;
        self.rendered = Some((token, page));
        Ok(RenderedPage { page, token, image })
    }

    pub fn acknowledge_displayed(&mut self, token: u64) -> Result<(), SessionError> {
        let (expected, page) = self.rendered.ok_or(SessionError::StaleRender)?;
        if expected != token || self.selected_page() != Some(page) {
            return Err(SessionError::StaleRender);
        }
        self.displayed = self.selected.clone();
        self.rendered = None;
        Ok(())
    }

    pub fn save_displayed(&mut self) -> Result<SaveStatus, SessionError> {
        let Some(displayed) = &self.displayed else {
            return Ok(SaveStatus::NoDisplayedPage);
        };
        let candidate = match &displayed.source {
            Source::Protected => return Ok(SaveStatus::ProtectedPosition),
            Source::Saved(position) => position.clone(),
            Source::Page => self.book.save_position(displayed.page)?,
        };
        if self.persisted.as_ref() == Some(&candidate) {
            return Ok(SaveStatus::Unchanged);
        }
        let generation = self.store.save(&candidate)?;
        self.persisted = Some(candidate);
        Ok(SaveStatus::Saved(generation))
    }
}

#[cfg(test)]
mod tests;
