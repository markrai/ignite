//! Gate F11 application UI state machine and pen routing.
//!
//! Pure (host-testable) logic. A pen tap is resolved here into a [`UiCommand`]; the runtime
//! executes durable library/page operations through the existing F8–F10 primitives and feeds
//! failures back as error modals. This module never touches storage or the framebuffer.
//!
//! Input priority: modal UI → notebook chrome → Home UI → page gesture/ink engine.
//!
//! F11.3: notebook chrome is a transient overlay ([`NotebookChromeState`]). While hidden the
//! whole panel is page content; chrome hit targets exist only while it is visible.

use kindle_platform::{FbGeometry, StylusEventKind, StylusTool};

use crate::chrome::{ChromeHit, ChromeLayout, PageChrome};
use crate::display_events::DisplayEvents;
use crate::folders::FolderInfo;
use crate::fullscreen::NotebookChromeState;
use crate::home::{home_rows, AppView, HomeCard, HomeHit, HomeLayout, HomeRow, HomeState};
use crate::keyboard::{EditorKey, EditorOutcome, KeyboardLayout, TitleEditor};
use crate::link_picker::LinkPickerState;
use crate::modal::{dialog_spec, ButtonId, DialogLayout, Modal, TitlePurpose, PICKER_PAGE};
use crate::prefs::SideButtonAction;
use crate::server_link::LinkView;
use crate::settings_ui::{PreferencesHit, PreferencesLayout, SettingsChromeHit};
use crate::sync_ui::{SyncHit, SyncLayout};
use crate::tools_ui::{ToolsHit, ToolsLayout, ToolsState};
use crate::touch::FingerAction;
use crate::ui_gfx::{Rect, UiPoint};

/// What the display must do after a UI transition that needs no storage operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Redraw {
    /// Repaint the whole current view (+ modal) and submit GC16 FULL.
    Full,
    /// Repaint the current view but refresh only this rect (keyboard typing).
    Region(Rect),
}

#[derive(Clone, Debug, PartialEq)]
pub enum UiCommand {
    /// Tap consumed with no visible change.
    None,
    Redraw(Redraw),
    OpenNotebook(String),
    CreateNotebook(String),
    RenameNotebook {
        notebook_id: String,
        title: String,
    },
    DeleteNotebook(String),
    GoHome,
    /// Leave Scribe cleanly (Home exit icon).
    ExitApp,
    Undo,
    PreviousPage,
    NextPage,
    InsertPageAfter,
    MovePageLeft,
    MovePageRight,
    DeletePage,
    /// D1: Home → Sync screen.
    OpenSync,
    /// D1: request pairing with this server (by ServerId).
    SyncPair(control_protocol::ServerId),
    SyncCancelPairing,
    /// D1: manual server address typed on the keyboard (fallback when discovery is blocked).
    SyncManualUrl(String),
    /// 1H-D2 Restore from Server (handled by the server link; see `server_link`).
    SyncRestoreBegin,
    SyncRestoreServer(control_protocol::ServerId),
    SyncRestoreLibrary(sync_protocol::LibraryId),
    SyncRestoreConfirm,
    SyncRestoreCancel,
    SyncRestoreDismiss,
    /// Home → Tools screen.
    OpenTools,
    /// Settings tab: Tools (keep scan state).
    ShowToolsTab,
    /// Settings tab: Preferences.
    OpenPreferences,
    /// Persist and apply a side-button preference.
    SetSideButton(crate::prefs::SideButtonAction),
    /// Tools: scan the Kindle's notebooks (read-only).
    ToolsScan,
    /// Tools: import every eligible Kindle notebook (one-way copy).
    ToolsImportAll,
    /// Kindle-local folders (never synced).
    CreateFolder(String),
    RenameFolder {
        folder_id: String,
        title: String,
    },
    /// Delete the folder; its notebooks return to the main list.
    DeleteFolder(String),
    /// Put the notebook in `folder`, or back on the main list (`None`).
    SetNotebookFolder {
        notebook_id: String,
        folder: Option<String>,
    },
    /// + NEW inside a folder: create the notebook, then file it in `folder_id`.
    CreateNotebookInFolder {
        title: String,
        folder_id: String,
    },
    /// Cancel the link-target picker (no durable write).
    LinkPickerCancel,
    /// Open a notebook's page grid inside the picker (read-only).
    LinkPickerOpenNotebook(String),
    /// Confirm a live target page (not the source page).
    LinkPickerSelectPage {
        notebook_local: String,
        page_local: String,
    },
    /// Jump to a live linked page (local directory names resolved from IDs).
    JumpLink {
        notebook_local: String,
        page_local: String,
    },
    /// Remove the named link from the current page.
    RemoveLink {
        link_id: sync_protocol::LinkId,
    },
    /// Side-button Create Link: open the picker at this canonical point.
    BeginCreateLink {
        x: f32,
        y: f32,
    },
    /// Pen/finger tap on a link icon.
    OpenLinkPopup {
        link_id: sync_protocol::LinkId,
    },
}

impl UiCommand {
    /// 1H-D2: the server-link command of a Restore-from-Server step. `SyncRestoreBegin` carries
    /// the runtime's installation and is built there.
    pub fn restore_link_cmd(&self) -> Option<crate::server_link::LinkCmd> {
        use crate::server_link::LinkCmd;
        Some(match self {
            UiCommand::SyncRestoreServer(id) => LinkCmd::RestoreSelectServer(*id),
            UiCommand::SyncRestoreLibrary(id) => LinkCmd::RestoreSelectLibrary(*id),
            UiCommand::SyncRestoreConfirm => LinkCmd::RestoreConfirm,
            UiCommand::SyncRestoreCancel => LinkCmd::RestoreCancel,
            UiCommand::SyncRestoreDismiss => LinkCmd::RestoreDismiss,
            _ => return None,
        })
    }
}

/// Notebook-view facts needed to resolve chrome and page-menu taps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotebookUiContext {
    pub page: PageChrome,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UiState {
    pub view: AppView,
    pub modal: Option<Modal>,
    pub home: HomeState,
    /// F11.3 transient notebook chrome visibility (never durable).
    pub chrome: NotebookChromeState,
    /// D1: product mode (paired-server sync) is available: Home shows SYNC.
    pub sync_enabled: bool,
    /// D1: the Sync screen snapshot last painted (taps resolve against what was drawn).
    pub sync_view: Option<LinkView>,
    /// Tools screen state (transient; never durable).
    pub tools: ToolsState,
    /// Replica-local side-button preference (durable file is outside the library).
    pub side_button: SideButtonAction,
    /// Link-target picker (only while `AppView::LinkTargetPicker`).
    pub link_picker: Option<LinkPickerState>,
    /// Kindle-local folders (live members, library order), refreshed by the runtime before each
    /// Home paint and tap. Empty = the plain F11 notebook list.
    pub folders: Vec<FolderInfo>,
    /// Default title offered for a new folder.
    pub next_folder_title: String,
    /// Home shows this folder's notebooks (transient; never durable).
    pub home_folder: Option<String>,
}

impl UiState {
    /// Normal interactive launch: Home, with the active notebook selected and visible.
    pub fn launch_home(active_index: usize, total: usize) -> Self {
        Self {
            view: AppView::Home,
            modal: None,
            home: HomeState::selecting(active_index, total),
            chrome: NotebookChromeState::default(),
            sync_enabled: false,
            sync_view: None,
            tools: ToolsState::default(),
            side_button: SideButtonAction::NavigationUndo,
            link_picker: None,
            folders: Vec::new(),
            next_folder_title: crate::folders::default_folder_title(1),
            home_folder: None,
        }
    }

    /// The open folder, if it still exists.
    pub fn open_folder(&self) -> Option<&FolderInfo> {
        let id = self.home_folder.as_deref()?;
        self.folders.iter().find(|f| f.id == id)
    }

    /// Rows of the Home view currently shown.
    pub fn home_rows(&self, cards: &[HomeCard]) -> Vec<HomeRow> {
        home_rows(
            cards,
            &self.folders,
            self.open_folder().map(|f| f.id.as_str()),
        )
    }

    /// Drop a vanished open folder and fit the Home viewport to the current rows.
    pub fn settle_home(&mut self, cards: &[HomeCard]) {
        if self.home_folder.is_some() && self.open_folder().is_none() {
            self.home_folder = None;
            self.home.reveal = true;
        }
        let rows = self.home_rows(cards);
        self.home.settle(&rows, cards, &self.folders);
    }

    fn folder_of(&self, notebook_id: &str) -> Option<&FolderInfo> {
        self.folders
            .iter()
            .find(|f| f.notebook_ids.iter().any(|n| n == notebook_id))
    }

    fn picker(&self, notebook_id: String, title: String, exclude: Option<&str>) -> Modal {
        let options: Vec<(String, String)> = self
            .folders
            .iter()
            .filter(|f| Some(f.id.as_str()) != exclude)
            .map(|f| (f.id.clone(), f.title.clone()))
            .collect();
        if options.is_empty() {
            return if exclude.is_some() {
                Modal::no_other_folder_notice()
            } else {
                Modal::no_folders_notice()
            };
        }
        Modal::FolderPicker {
            notebook_id,
            title,
            options,
            page: 0,
        }
    }

    /// D1: enable the SYNC entry point (product mode).
    pub fn enable_sync(&mut self) {
        self.sync_enabled = true;
        self.home.sync = true;
    }

    /// D1: Home → Sync screen.
    pub fn enter_sync(&mut self) {
        self.view = AppView::Sync;
        self.modal = None;
    }

    /// Home → Tools screen (fresh: nothing scanned yet).
    pub fn enter_tools(&mut self) {
        self.view = AppView::Tools;
        self.modal = None;
        self.tools = ToolsState::default();
        self.link_picker = None;
    }

    /// Settings tab switch to Tools without resetting import state.
    pub fn show_tools_tab(&mut self) {
        self.view = AppView::Tools;
        self.modal = None;
    }

    /// Settings tab: Preferences.
    pub fn enter_preferences(&mut self) {
        self.view = AppView::Preferences;
        self.modal = None;
        self.link_picker = None;
    }

    pub fn enter_link_picker(&mut self, picker: LinkPickerState) {
        self.view = AppView::LinkTargetPicker;
        self.modal = None;
        self.chrome.hide();
        self.link_picker = Some(picker);
    }

    pub fn modal_open(&self) -> bool {
        self.modal.is_some()
    }

    /// Page ink is allowed only in Notebook view with no modal.
    pub fn allows_page_input(&self) -> bool {
        self.view.allows_page_input() && self.modal.is_none()
    }

    /// Enter Home selecting `active_index` (the durable active notebook). Selection is
    /// transient; entering Home never changes `active_notebook_id`.
    pub fn enter_home(&mut self, active_index: usize, total: usize) {
        self.view = AppView::Home;
        self.modal = None;
        self.home = HomeState::selecting(active_index, total);
        self.home.sync = self.sync_enabled;
        self.chrome = NotebookChromeState::default();
        self.link_picker = None;
    }

    /// Normal notebook entry: fullscreen writing (chrome hidden).
    pub fn enter_notebook(&mut self) {
        self.view = AppView::Notebook;
        self.modal = None;
        self.chrome.enter_notebook();
    }

    /// Whether the notebook chrome overlay is (to be) painted on the panel.
    pub fn chrome_overlay_visible(&self) -> bool {
        self.view == AppView::Notebook && self.chrome.is_visible()
    }

    /// Notebook chrome hit test. Hidden chrome has no hit targets: its bands are page content.
    pub fn chrome_hit(&self, geo: &FbGeometry, p: UiPoint) -> Option<ChromeHit> {
        if self.chrome_overlay_visible() {
            ChromeLayout::new(geo).hit(p)
        } else {
            None
        }
    }

    /// Keep the auto-hide timer consistent with modal open/close in Notebook view: opening
    /// suspends it; closing back to the notebook shows chrome with a fresh countdown.
    pub fn note_modal_transition(&mut self, was_open: bool, now_ms: u64) {
        if self.view != AppView::Notebook {
            return;
        }
        match (was_open, self.modal_open()) {
            (false, true) => self.chrome.modal_opened(),
            (true, false) => self.chrome.modal_closed(now_ms),
            _ => {}
        }
    }

    /// Resolve a UI tap (pen or finger) with F11.3 chrome bookkeeping: a tap on visible
    /// chrome restarts the auto-hide countdown; modal open/close suspends/restarts it.
    #[allow(clippy::too_many_arguments)]
    pub fn on_ui_tap(
        &mut self,
        geo: &FbGeometry,
        p: UiPoint,
        cards: &[HomeCard],
        next_default_title: &str,
        notebook: Option<NotebookUiContext>,
        now_ms: u64,
    ) -> UiCommand {
        let was_open = self.modal_open();
        let on_chrome = !was_open && self.chrome_hit(geo, p).is_some();
        let cmd = self.on_tap(geo, p, cards, next_default_title, notebook);
        if on_chrome {
            self.chrome.ui_activity(now_ms);
        }
        self.note_modal_transition(was_open, now_ms);
        cmd
    }

    /// F11.3 finger policy on top of the F11.2 gesture classification.
    ///
    /// Notebook page (no modal): a tap reveals hidden chrome; with chrome visible, a tap on a
    /// control operates it and a tap on content hides chrome; a swipe hides chrome and turns
    /// the page. Reveal/hide return `UiCommand::None` — the runtime re-presents the page from
    /// durable state whenever the panel's chrome differs from this state. Everywhere else
    /// (Home, modals, title keyboard) behavior is unchanged.
    #[allow(clippy::too_many_arguments)]
    pub fn on_finger(
        &mut self,
        geo: &FbGeometry,
        action: FingerAction,
        cards: &[HomeCard],
        next_default_title: &str,
        notebook: Option<NotebookUiContext>,
        now_ms: u64,
    ) -> UiCommand {
        let page_surface = self.allows_page_input();
        match action {
            FingerAction::NextPage | FingerAction::PreviousPage => {
                if page_surface {
                    self.chrome.hide();
                }
                if action == FingerAction::NextPage {
                    UiCommand::NextPage
                } else {
                    UiCommand::PreviousPage
                }
            }
            FingerAction::Tap(p) if page_surface => {
                if !self.chrome.is_visible() {
                    self.chrome.reveal(now_ms);
                    UiCommand::None
                } else if self.chrome_hit(geo, p).is_none() {
                    self.chrome.hide();
                    UiCommand::None
                } else {
                    self.on_ui_tap(geo, p, cards, next_default_title, notebook, now_ms)
                }
            }
            FingerAction::Tap(p) => {
                self.on_ui_tap(geo, p, cards, next_default_title, notebook, now_ms)
            }
            FingerAction::LassoStarted(_)
            | FingerAction::LassoPoint(_)
            | FingerAction::LassoFinished(_)
            | FingerAction::LassoCancelled
            | FingerAction::SelectionMoveStarted(_)
            | FingerAction::SelectionMovePoint(_)
            | FingerAction::SelectionMoveFinished(_)
            | FingerAction::SelectionMoveCancelled
            | FingerAction::SelectionCleared => UiCommand::None,
        }
    }

    pub fn show_error(&mut self, message: impl Into<String>) {
        self.modal = Some(Modal::error(message));
    }

    pub fn show_modal(&mut self, modal: Modal) {
        self.modal = Some(modal);
    }

    /// Resolve one pen tap (Down position, panel pixels).
    pub fn on_tap(
        &mut self,
        geo: &FbGeometry,
        p: UiPoint,
        cards: &[HomeCard],
        next_default_title: &str,
        notebook: Option<NotebookUiContext>,
    ) -> UiCommand {
        if self.modal.is_some() {
            return self.on_modal_tap(geo, p, cards);
        }
        match self.view {
            AppView::Home => self.on_home_tap(geo, p, cards, next_default_title),
            AppView::Sync => self.on_sync_tap(geo, p),
            AppView::Tools => self.on_tools_tap(geo, p),
            AppView::Preferences => self.on_preferences_tap(geo, p),
            AppView::LinkTargetPicker => self.on_link_picker_tap(geo, p, cards),
            AppView::Notebook => match notebook {
                Some(ctx) => self.on_chrome_tap(geo, p, ctx),
                None => UiCommand::None,
            },
        }
    }

    fn on_home_tap(
        &mut self,
        geo: &FbGeometry,
        p: UiPoint,
        cards: &[HomeCard],
        next_default_title: &str,
    ) -> UiCommand {
        let total = cards.len();
        // Resolve against the settled view, but a tap that hits nothing changes no state.
        let before = self.clone();
        self.settle_home(cards);
        let rows = self.home_rows(cards);
        let open = self.open_folder().map(|f| f.id.clone());
        let layout = HomeLayout::for_rows(geo, &self.home, &rows, open.is_some());
        let hit = layout.hit(p, &self.home, total);
        let cmd = match hit {
            // These change no Home state: leave the snapshot exactly as it was.
            None | Some(HomeHit::SearchPlaceholder) => Some(UiCommand::None),
            Some(HomeHit::Exit) => Some(UiCommand::ExitApp),
            Some(HomeHit::Sync) => Some(UiCommand::OpenSync),
            Some(HomeHit::Tools) => Some(UiCommand::OpenTools),
            _ => None,
        };
        if let Some(cmd) = cmd {
            *self = before;
            return cmd;
        }
        match hit {
            None
            | Some(HomeHit::SearchPlaceholder)
            | Some(HomeHit::Exit)
            | Some(HomeHit::Sync)
            | Some(HomeHit::Tools) => UiCommand::None,
            Some(HomeHit::Back) => {
                // Back to the main list, showing the folder just left.
                self.home_folder = None;
                let main = self.home_rows(cards);
                if let Some(fi) = open
                    .as_deref()
                    .and_then(|id| self.folders.iter().position(|f| f.id == id))
                {
                    if let Some(row) = main
                        .iter()
                        .position(|r| matches!(r, HomeRow::Folders(v) if v.contains(&fi)))
                    {
                        self.home.page = row / crate::home::ROWS_PER_PAGE;
                    }
                }
                UiCommand::Redraw(Redraw::Full)
            }
            Some(HomeHit::OpenFolder(fi)) => {
                self.home_folder = Some(self.folders[fi].id.clone());
                self.home.page = 0;
                UiCommand::Redraw(Redraw::Full)
            }
            Some(HomeHit::FolderMenu(fi)) => {
                self.modal = Some(Modal::FolderMenu {
                    folder_id: self.folders[fi].id.clone(),
                    title: self.folders[fi].title.clone(),
                });
                UiCommand::Redraw(Redraw::Full)
            }
            Some(HomeHit::New) => {
                self.modal = Some(match open {
                    // Inside a folder: a notebook, filed in that folder (no nested folders).
                    Some(folder_id) => Modal::TitleEntry {
                        purpose: TitlePurpose::CreateNotebookInFolder { folder_id },
                        editor: TitleEditor::new(next_default_title),
                    },
                    None => Modal::NewChooser {
                        notebook_title: next_default_title.to_string(),
                        folder_title: self.next_folder_title.clone(),
                    },
                });
                UiCommand::Redraw(Redraw::Full)
            }
            Some(HomeHit::OpenCard(i)) => {
                self.home.selected_index = i;
                UiCommand::OpenNotebook(cards[i].notebook_id.clone())
            }
            Some(HomeHit::CardMenu(i)) => {
                self.home.selected_index = i;
                let notebook_id = cards[i].notebook_id.clone();
                let title = cards[i].title.clone();
                self.modal = Some(match self.folder_of(&notebook_id) {
                    Some(f) => Modal::CardMenuInFolder {
                        folder_id: f.id.clone(),
                        notebook_id,
                        title,
                    },
                    None => Modal::CardMenu { notebook_id, title },
                });
                UiCommand::Redraw(Redraw::Full)
            }
            Some(HomeHit::PagePrevious) => {
                self.home.page -= 1;
                UiCommand::Redraw(Redraw::Full)
            }
            Some(HomeHit::PageNext) => {
                self.home.page += 1;
                UiCommand::Redraw(Redraw::Full)
            }
        }
    }

    fn on_tools_tap(&mut self, geo: &FbGeometry, p: UiPoint) -> UiCommand {
        match ToolsLayout::new(geo, &self.tools).hit(p.x, p.y, &self.tools) {
            None => UiCommand::None,
            Some(ToolsHit::Chrome(SettingsChromeHit::Back)) => UiCommand::GoHome,
            Some(ToolsHit::Chrome(SettingsChromeHit::ToolsTab)) => UiCommand::None,
            Some(ToolsHit::Chrome(SettingsChromeHit::PreferencesTab)) => UiCommand::OpenPreferences,
            Some(ToolsHit::Scan) => UiCommand::ToolsScan,
            Some(ToolsHit::ImportAll) => UiCommand::ToolsImportAll,
        }
    }

    fn on_preferences_tap(&mut self, geo: &FbGeometry, p: UiPoint) -> UiCommand {
        match PreferencesLayout::new(geo).hit(p.x, p.y) {
            None => UiCommand::None,
            Some(PreferencesHit::Chrome(SettingsChromeHit::Back)) => UiCommand::GoHome,
            Some(PreferencesHit::Chrome(SettingsChromeHit::ToolsTab)) => UiCommand::ShowToolsTab,
            Some(PreferencesHit::Chrome(SettingsChromeHit::PreferencesTab)) => UiCommand::None,
            Some(PreferencesHit::NavigationUndo) => {
                UiCommand::SetSideButton(SideButtonAction::NavigationUndo)
            }
            Some(PreferencesHit::CreateLink) => {
                UiCommand::SetSideButton(SideButtonAction::CreateLink)
            }
        }
    }

    fn on_link_picker_tap(
        &mut self,
        geo: &FbGeometry,
        p: UiPoint,
        cards: &[HomeCard],
    ) -> UiCommand {
        let Some(picker) = self.link_picker.as_mut() else {
            return UiCommand::GoHome;
        };
        match picker.hit(geo, p, cards) {
            crate::link_picker::LinkPickerHit::None => UiCommand::None,
            crate::link_picker::LinkPickerHit::Cancel => UiCommand::LinkPickerCancel,
            crate::link_picker::LinkPickerHit::BackToNotebooks => {
                picker.show_notebooks();
                UiCommand::Redraw(Redraw::Full)
            }
            crate::link_picker::LinkPickerHit::OpenNotebook(id) => {
                UiCommand::LinkPickerOpenNotebook(id)
            }
            crate::link_picker::LinkPickerHit::SelectPage {
                notebook_local,
                page_local,
            } => UiCommand::LinkPickerSelectPage {
                notebook_local,
                page_local,
            },
            crate::link_picker::LinkPickerHit::PagePrev
            | crate::link_picker::LinkPickerHit::PageNext => UiCommand::Redraw(Redraw::Full),
        }
    }

    fn on_sync_tap(&mut self, geo: &FbGeometry, p: UiPoint) -> UiCommand {
        let Some(view) = &self.sync_view else {
            return UiCommand::None;
        };
        match SyncLayout::new(geo, view).hit_view(view, p.x, p.y) {
            None => UiCommand::None,
            Some(SyncHit::Back) => UiCommand::GoHome,
            Some(SyncHit::Pair(id)) => UiCommand::SyncPair(id),
            Some(SyncHit::CancelPairing) => UiCommand::SyncCancelPairing,
            Some(SyncHit::Restore) => UiCommand::SyncRestoreBegin,
            Some(SyncHit::RestoreServer(id)) => UiCommand::SyncRestoreServer(id),
            Some(SyncHit::RestoreLibrary(id)) => UiCommand::SyncRestoreLibrary(id),
            Some(SyncHit::RestoreConfirm) => UiCommand::SyncRestoreConfirm,
            Some(SyncHit::RestoreCancel) => UiCommand::SyncRestoreCancel,
            Some(SyncHit::RestoreOk) => UiCommand::SyncRestoreDismiss,
            Some(SyncHit::ManualUrl) => {
                self.modal = Some(Modal::TitleEntry {
                    purpose: TitlePurpose::ServerUrl,
                    editor: TitleEditor::new(""),
                });
                UiCommand::Redraw(Redraw::Full)
            }
        }
    }

    fn on_chrome_tap(&mut self, geo: &FbGeometry, p: UiPoint, ctx: NotebookUiContext) -> UiCommand {
        match self.chrome_hit(geo, p) {
            None | Some(ChromeHit::Inert) => UiCommand::None,
            Some(ChromeHit::Home) => UiCommand::GoHome,
            Some(ChromeHit::Undo) => UiCommand::Undo,
            Some(ChromeHit::PreviousPage) => {
                if ctx.page.can_previous() {
                    UiCommand::PreviousPage
                } else {
                    UiCommand::None
                }
            }
            Some(ChromeHit::NextPage) => UiCommand::NextPage,
            Some(ChromeHit::PageMenu) => {
                self.modal = Some(Modal::PageMenu {
                    page_number: ctx.page.index,
                    page_count: ctx.page.count,
                });
                UiCommand::Redraw(Redraw::Full)
            }
        }
    }

    fn on_modal_tap(&mut self, geo: &FbGeometry, p: UiPoint, cards: &[HomeCard]) -> UiCommand {
        let Some(modal) = self.modal.take() else {
            return UiCommand::None;
        };
        if let Modal::TitleEntry {
            purpose,
            mut editor,
        } = modal
        {
            return self.on_editor_tap(geo, p, purpose, &mut editor);
        }
        let Some(spec) = dialog_spec(&modal) else {
            self.modal = Some(modal);
            return UiCommand::None;
        };
        let Some(button) = DialogLayout::new(geo, &spec).hit(p) else {
            // LinkJump: outside tap dismisses. Other modals stay open (no ink underneath).
            if matches!(modal, Modal::LinkJump { .. }) {
                return UiCommand::None;
            }
            self.modal = Some(modal);
            return UiCommand::None;
        };
        match (modal, button) {
            (
                Modal::CardMenu { notebook_id, title }
                | Modal::CardMenuInFolder {
                    notebook_id, title, ..
                },
                ButtonId::Rename,
            ) => {
                self.modal = Some(Modal::TitleEntry {
                    editor: TitleEditor::new(&title),
                    purpose: TitlePurpose::Rename {
                        notebook_id,
                        original: title,
                    },
                });
                UiCommand::Redraw(Redraw::Full)
            }
            (
                Modal::CardMenu { notebook_id, title }
                | Modal::CardMenuInFolder {
                    notebook_id, title, ..
                },
                ButtonId::DeleteNotebook,
            ) => {
                self.modal = Some(if cards.len() < 2 {
                    Modal::sole_notebook_notice()
                } else {
                    Modal::ConfirmDeleteNotebook { notebook_id, title }
                });
                UiCommand::Redraw(Redraw::Full)
            }
            (Modal::ConfirmDeleteNotebook { notebook_id, .. }, ButtonId::ConfirmDelete) => {
                UiCommand::DeleteNotebook(notebook_id)
            }
            (Modal::CardMenu { notebook_id, title }, ButtonId::AddToFolder) => {
                self.modal = Some(self.picker(notebook_id, title, None));
                UiCommand::Redraw(Redraw::Full)
            }
            (
                Modal::CardMenuInFolder {
                    notebook_id,
                    title,
                    folder_id,
                },
                ButtonId::ChangeFolder,
            ) => {
                self.modal = Some(self.picker(notebook_id, title, Some(&folder_id)));
                UiCommand::Redraw(Redraw::Full)
            }
            (Modal::CardMenuInFolder { notebook_id, .. }, ButtonId::RemoveFromFolder) => {
                UiCommand::SetNotebookFolder {
                    notebook_id,
                    folder: None,
                }
            }
            (
                Modal::FolderPicker {
                    notebook_id,
                    mut options,
                    ..
                },
                ButtonId::PickFolder(k),
            ) if k < options.len() => UiCommand::SetNotebookFolder {
                notebook_id,
                folder: Some(options.swap_remove(k).0),
            },
            (
                Modal::FolderPicker {
                    notebook_id,
                    title,
                    options,
                    page,
                },
                ButtonId::PickerPrevious | ButtonId::PickerNext,
            ) => {
                let pages = options.len().div_ceil(PICKER_PAGE).max(1);
                let page = if button == ButtonId::PickerNext {
                    (page + 1).min(pages - 1)
                } else {
                    page.saturating_sub(1)
                };
                self.modal = Some(Modal::FolderPicker {
                    notebook_id,
                    title,
                    options,
                    page,
                });
                UiCommand::Redraw(Redraw::Full)
            }
            (Modal::NewChooser { notebook_title, .. }, ButtonId::NewNotebook) => {
                self.modal = Some(Modal::TitleEntry {
                    purpose: TitlePurpose::Create,
                    editor: TitleEditor::new(&notebook_title),
                });
                UiCommand::Redraw(Redraw::Full)
            }
            (Modal::NewChooser { folder_title, .. }, ButtonId::NewFolder) => {
                self.modal = Some(Modal::TitleEntry {
                    purpose: TitlePurpose::CreateFolder,
                    editor: TitleEditor::new(&folder_title),
                });
                UiCommand::Redraw(Redraw::Full)
            }
            (Modal::FolderMenu { folder_id, title }, ButtonId::Rename) => {
                self.modal = Some(Modal::TitleEntry {
                    editor: TitleEditor::new(&title),
                    purpose: TitlePurpose::RenameFolder {
                        folder_id,
                        original: title,
                    },
                });
                UiCommand::Redraw(Redraw::Full)
            }
            (Modal::FolderMenu { folder_id, title }, ButtonId::DeleteFolder) => {
                self.modal = Some(Modal::ConfirmDeleteFolder { folder_id, title });
                UiCommand::Redraw(Redraw::Full)
            }
            (Modal::ConfirmDeleteFolder { folder_id, .. }, ButtonId::ConfirmDelete) => {
                UiCommand::DeleteFolder(folder_id)
            }
            (Modal::PageMenu { .. }, ButtonId::InsertPageAfter) => UiCommand::InsertPageAfter,
            (Modal::PageMenu { .. }, ButtonId::MovePageLeft) => UiCommand::MovePageLeft,
            (Modal::PageMenu { .. }, ButtonId::MovePageRight) => UiCommand::MovePageRight,
            (
                Modal::PageMenu {
                    page_number,
                    page_count,
                },
                ButtonId::DeletePage,
            ) => {
                self.modal = Some(if page_count < 2 {
                    Modal::sole_page_notice()
                } else {
                    Modal::ConfirmDeletePage { page_number }
                });
                UiCommand::Redraw(Redraw::Full)
            }
            (Modal::ConfirmDeletePage { .. }, ButtonId::ConfirmDelete) => UiCommand::DeletePage,
            (
                Modal::LinkJump {
                    notebook_local,
                    page_local,
                    target_live,
                    ..
                },
                ButtonId::Jump,
            ) if target_live => match (notebook_local, page_local) {
                (Some(notebook_local), Some(page_local)) => UiCommand::JumpLink {
                    notebook_local,
                    page_local,
                },
                _ => UiCommand::None,
            },
            (Modal::LinkJump { link_id, .. }, ButtonId::RemoveLink) => {
                UiCommand::RemoveLink { link_id }
            }
            (Modal::Error { resume, .. }, ButtonId::Ok) => {
                self.modal = resume.map(|m| *m);
                UiCommand::Redraw(Redraw::Full)
            }
            // Cancel / OK on notices, and any other close.
            _ => UiCommand::Redraw(Redraw::Full),
        }
    }

    fn on_editor_tap(
        &mut self,
        geo: &FbGeometry,
        p: UiPoint,
        purpose: TitlePurpose,
        editor: &mut TitleEditor,
    ) -> UiCommand {
        let layout = KeyboardLayout::new(geo, editor.symbols());
        let Some(key) = layout.hit(p) else {
            self.modal = Some(Modal::TitleEntry {
                purpose,
                editor: editor.clone(),
            });
            return UiCommand::None;
        };
        let outcome = editor.apply(key);
        let reopen = |editor: &TitleEditor, purpose: TitlePurpose| Modal::TitleEntry {
            purpose,
            editor: editor.clone(),
        };
        match outcome {
            EditorOutcome::Unchanged => {
                self.modal = Some(reopen(editor, purpose));
                UiCommand::None
            }
            EditorOutcome::TextChanged | EditorOutcome::KeysChanged => {
                // The mode key swaps the whole key grid; recompute with the new mode.
                let region = if key == EditorKey::Mode {
                    KeyboardLayout::new(geo, editor.symbols()).refresh_region(&outcome)
                } else {
                    layout.refresh_region(&outcome)
                };
                self.modal = Some(reopen(editor, purpose));
                match region {
                    Some(r) => UiCommand::Redraw(Redraw::Region(r)),
                    None => UiCommand::None,
                }
            }
            EditorOutcome::Cancel => UiCommand::Redraw(Redraw::Full),
            EditorOutcome::Invalid(message) => {
                self.modal = Some(Modal::Error {
                    message,
                    resume: Some(Box::new(reopen(editor, purpose))),
                });
                UiCommand::Redraw(Redraw::Full)
            }
            EditorOutcome::Submit(title) => match purpose {
                TitlePurpose::Create => UiCommand::CreateNotebook(title),
                TitlePurpose::ServerUrl => UiCommand::SyncManualUrl(title),
                TitlePurpose::CreateFolder => UiCommand::CreateFolder(title),
                TitlePurpose::CreateNotebookInFolder { folder_id } => {
                    UiCommand::CreateNotebookInFolder { title, folder_id }
                }
                TitlePurpose::RenameFolder {
                    folder_id,
                    original,
                } => {
                    if title == original {
                        UiCommand::Redraw(Redraw::Full)
                    } else {
                        UiCommand::RenameFolder { folder_id, title }
                    }
                }
                TitlePurpose::Rename {
                    notebook_id,
                    original,
                } => {
                    if title == original {
                        // Unchanged title: no durable write.
                        UiCommand::Redraw(Redraw::Full)
                    } else {
                        UiCommand::RenameNotebook { notebook_id, title }
                    }
                }
            },
        }
    }
}

/// How a pen/eraser Down is routed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownRoute {
    /// Resolve as a UI tap; the whole contact is consumed until Up.
    UiTap(UiPoint),
    /// Consumed with no action (e.g. eraser on Home); nothing reaches the ink engine.
    Consume,
    /// Normal page content: F0–F7 ink / nav / eraser behavior.
    Page,
}

/// Tracks whether the current contact belongs to the UI so its Move/Up never leak into the
/// page engine, even if the view changes mid-contact (e.g. a tap that opens a notebook).
///
/// F11.1: also gates input on physical display validity. While the Kindle screensaver may own
/// the panel, what is mapped in `/dev/fb0` is not what the user sees, so no event may reach a
/// UI hit target or the ink engine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PenRouter {
    ui_contact: bool,
    display_invalid: bool,
    redraw_requested: bool,
}

impl PenRouter {
    /// Screensaver/suspend entry: the panel no longer shows our UI. Sleep is an input-stream
    /// discontinuity, so any transient UI-contact ownership is dropped (no Up is invented).
    pub fn invalidate_display(&mut self) {
        self.display_invalid = true;
        self.ui_contact = false;
    }

    pub fn display_valid(&self) -> bool {
        !self.display_invalid
    }

    /// Ask the runtime for a full re-present of the current UI state.
    pub fn request_redraw(&mut self) {
        self.redraw_requested = true;
    }

    pub fn redraw_requested(&self) -> bool {
        self.redraw_requested
    }

    /// Apply relayed screensaver events. Invalidate wins first; a restore always requests a
    /// full re-present and keeps input gated until that present completes, even when the
    /// matching invalidate was never seen.
    pub fn on_display_events(&mut self, ev: DisplayEvents) {
        if ev.invalidated {
            self.invalidate_display();
        }
        if ev.restored {
            if !self.display_invalid {
                self.invalidate_display();
            }
            self.request_redraw();
        }
    }

    /// A full present of the current `UiState` completed (GC16 FULL): input is live again.
    /// A contact already consumed by the fail-safe stays owned until its Up.
    pub fn display_presented(&mut self) {
        self.display_invalid = false;
        self.redraw_requested = false;
    }

    pub fn route_down(
        &mut self,
        ui: &UiState,
        geo: &FbGeometry,
        tool: StylusTool,
        p: Option<UiPoint>,
    ) -> DownRoute {
        let pen = tool == StylusTool::Pen;
        if ui.modal_open() || !ui.view.allows_page_input() {
            self.ui_contact = true;
            return match (pen, p) {
                (true, Some(p)) => DownRoute::UiTap(p),
                _ => DownRoute::Consume,
            };
        }
        // Notebook view: only the pen tip operates *visible* chrome. The eraser keeps
        // full-page reach; hidden chrome bands are ordinary writable paper.
        if pen {
            if let Some(p) = p {
                if ui.chrome_hit(geo, p).is_some() {
                    self.ui_contact = true;
                    return DownRoute::UiTap(p);
                }
            }
        }
        self.ui_contact = false;
        DownRoute::Page
    }

    /// Route any stylus event. `page_owned` = the page engine already owns an active contact
    /// (nav/eraser/wet stroke), in which case a Down keeps its sealed page handling.
    /// Only `UiTap` and `Page` differ from "drop the event".
    pub fn route_event(
        &mut self,
        ui: &UiState,
        geo: &FbGeometry,
        kind: StylusEventKind,
        tool: StylusTool,
        p: Option<UiPoint>,
        page_owned: bool,
    ) -> DownRoute {
        if self.display_invalid {
            // First-contact fail-safe (missed/late wake event): consume the whole contact,
            // never resolve its hit target, never ink, and request a full repaint.
            if kind == StylusEventKind::Down {
                self.ui_contact = true;
                self.redraw_requested = true;
            } else if kind == StylusEventKind::Up {
                self.ui_contact = false;
            }
            return DownRoute::Consume;
        }
        let page_input = ui.allows_page_input();
        match kind {
            StylusEventKind::Down if page_input && page_owned => DownRoute::Page,
            StylusEventKind::Down => self.route_down(ui, geo, tool, p),
            StylusEventKind::Up => {
                if self.on_up() || !page_input {
                    DownRoute::Consume
                } else {
                    DownRoute::Page
                }
            }
            _ => {
                if self.ui_contact || !page_input {
                    DownRoute::Consume
                } else {
                    DownRoute::Page
                }
            }
        }
    }

    /// Up event. Returns `true` when the UI owned the contact (consume it).
    pub fn on_up(&mut self) -> bool {
        std::mem::take(&mut self.ui_contact)
    }

    pub fn on_stream_gap(&mut self) {
        self.ui_contact = false;
    }
}
