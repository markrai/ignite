//! Bound-page ownership: the pen session plus retained addressable texts.
//!
//! [`PageSession`] stays the latency-sensitive ink/session model. This module only
//! groups it with the page's durable [`SyncText`] values so they are not discarded
//! after [`crate::text_paint::TextOverlay`] is rasterized.

use notebook_core::StrokeId;
use sync_protocol::{LinkId, SyncPageLink, SyncText, TextId};

use crate::session::PageSession;

/// Replica-local identity of the currently bound page.
///
/// Local directory names (`notebook-N` / `page-N`) are only unique inside one
/// library. A selection must carry **both** so `page-1` in notebook A cannot
/// resolve against `page-1` in notebook B. This is not the sync UUID pair;
/// selection is ephemeral and local.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BoundPageKey {
    notebook_id: String,
    page_id: String,
}

impl BoundPageKey {
    pub fn new(notebook_id: impl Into<String>, page_id: impl Into<String>) -> Self {
        Self {
            notebook_id: notebook_id.into(),
            page_id: page_id.into(),
        }
    }

    pub fn notebook_id(&self) -> &str {
        &self.notebook_id
    }

    pub fn page_id(&self) -> &str {
        &self.page_id
    }
}

/// Borrowed view of the currently bound page's addressable durable content.
///
/// Not a scene graph: strokes stay in [`PageSession`], texts stay a typed list.
/// [`crate::text_paint::TextOverlay`] is derived from [`Self::texts`] and is not
/// identity.
#[derive(Clone, Copy, Debug)]
pub struct OpenPage<'a> {
    bound: &'a BoundPageKey,
    session: &'a PageSession,
    texts: &'a [SyncText],
    links: &'a [SyncPageLink],
}

impl<'a> OpenPage<'a> {
    pub fn new(bound: &'a BoundPageKey, session: &'a PageSession, texts: &'a [SyncText]) -> Self {
        Self::with_links(bound, session, texts, &[])
    }

    pub fn with_links(
        bound: &'a BoundPageKey,
        session: &'a PageSession,
        texts: &'a [SyncText],
        links: &'a [SyncPageLink],
    ) -> Self {
        Self {
            bound,
            session,
            texts,
            links,
        }
    }

    pub fn bound(&self) -> &'a BoundPageKey {
        self.bound
    }

    /// Local page directory name (`page-N`). Not unique across notebooks; use [`Self::bound`].
    pub fn page_id(&self) -> &str {
        self.bound.page_id()
    }

    pub fn session(&self) -> &'a PageSession {
        self.session
    }

    pub fn texts(&self) -> &'a [SyncText] {
        self.texts
    }

    pub fn links(&self) -> &'a [SyncPageLink] {
        self.links
    }

    pub fn stroke(&self, id: &StrokeId) -> Option<&notebook_core::Stroke> {
        self.session
            .committed_strokes()
            .iter()
            .find(|s| s.id.as_str() == id.as_str())
    }

    pub fn text(&self, id: TextId) -> Option<&SyncText> {
        self.texts.iter().find(|t| t.text_id == id)
    }

    pub fn link(&self, id: LinkId) -> Option<&SyncPageLink> {
        self.links.iter().find(|l| l.link_id == id)
    }

    pub fn contains_stroke(&self, id: &StrokeId) -> bool {
        self.stroke(id).is_some()
    }

    pub fn contains_text(&self, id: TextId) -> bool {
        self.text(id).is_some()
    }

    pub fn contains_link(&self, id: LinkId) -> bool {
        self.link(id).is_some()
    }
}
