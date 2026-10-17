//! Gate F11 modal dialogs: notebook card menu, page menu, confirmations, notices, errors, and
//! the title-entry keyboard. Modals own input while open; nothing underneath sees a tap.

use kindle_platform::FbGeometry;

use crate::keyboard::TitleEditor;
use crate::ui_gfx::{wrap_text, Canvas, Rect, UiPoint, BLACK, GRAY, WHITE};

const BOX_MARGIN_X: u32 = 120;
const BOX_PAD: u32 = 60;
const BOX_BORDER: u32 = 8;
const HEADING_SCALE: u32 = 10;
const BODY_SCALE: u32 = 7;
const LINE_GAP: u32 = 24;
const BUTTON_H: u32 = 170;
const BUTTON_GAP: u32 = 30;
const BUTTON_SCALE: u32 = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TitlePurpose {
    Create,
    Rename {
        notebook_id: String,
        original: String,
    },
    /// D1: manual Scribe Server address (Sync screen fallback).
    ServerUrl,
    /// Kindle-local folders (`folders.rs`).
    CreateFolder,
    RenameFolder {
        folder_id: String,
        original: String,
    },
    /// + NEW inside an open folder: the new notebook goes into that folder.
    CreateNotebookInFolder {
        folder_id: String,
    },
}

/// Folder buttons shown per picker page (PREVIOUS/NEXT page through the rest).
pub const PICKER_PAGE: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Modal {
    /// Notebook menu on the main list (the notebook is in no folder).
    CardMenu {
        notebook_id: String,
        title: String,
    },
    /// Notebook menu for a notebook inside `folder_id`.
    CardMenuInFolder {
        notebook_id: String,
        title: String,
        folder_id: String,
    },
    /// + NEW on the main list: notebook or folder.
    NewChooser {
        notebook_title: String,
        folder_title: String,
    },
    FolderMenu {
        folder_id: String,
        title: String,
    },
    ConfirmDeleteFolder {
        folder_id: String,
        title: String,
    },
    /// Choose the folder for `notebook_id`: `options` are `(folder id, title)`.
    FolderPicker {
        notebook_id: String,
        title: String,
        options: Vec<(String, String)>,
        page: usize,
    },
    ConfirmDeleteNotebook {
        notebook_id: String,
        title: String,
    },
    TitleEntry {
        purpose: TitlePurpose,
        editor: TitleEditor,
    },
    PageMenu {
        page_number: usize,
        page_count: usize,
    },
    ConfirmDeletePage {
        page_number: usize,
    },
    /// Informational refusal (e.g. sole notebook). OK closes.
    Notice {
        heading: String,
        body: Vec<String>,
    },
    /// Jump / remove an explicit page link. Outside tap dismisses (no ink).
    LinkJump {
        link_id: sync_protocol::LinkId,
        /// Primary button label (`Jump to "Title - Page N"` or the missing-dest copy).
        label: String,
        target_live: bool,
        notebook_local: Option<String>,
        page_local: Option<String>,
    },
    /// "Something went wrong". OK closes, or returns to `resume` (e.g. the title editor).
    Error {
        message: String,
        resume: Option<Box<Modal>>,
    },
}

impl Modal {
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
            resume: None,
        }
    }

    pub fn sole_notebook_notice() -> Self {
        Self::Notice {
            heading: "Can't delete".into(),
            body: vec![
                "You can't delete the only notebook.".into(),
                "Create another notebook first.".into(),
            ],
        }
    }

    pub fn sole_page_notice() -> Self {
        Self::Notice {
            heading: "Can't delete".into(),
            body: vec![
                "You can't delete the only page.".into(),
                "Insert another page first.".into(),
            ],
        }
    }

    pub fn no_folders_notice() -> Self {
        Self::Notice {
            heading: "No folders yet".into(),
            body: vec!["Create one with + NEW, then NEW FOLDER.".into()],
        }
    }

    pub fn no_other_folder_notice() -> Self {
        Self::Notice {
            heading: "No other folder".into(),
            body: vec!["Create another folder with + NEW, then NEW FOLDER.".into()],
        }
    }

    pub fn is_title_entry(&self) -> bool {
        matches!(self, Self::TitleEntry { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonId {
    Rename,
    DeleteNotebook,
    InsertPageAfter,
    MovePageLeft,
    MovePageRight,
    DeletePage,
    ConfirmDelete,
    Cancel,
    Ok,
    NewNotebook,
    NewFolder,
    AddToFolder,
    ChangeFolder,
    RemoveFromFolder,
    DeleteFolder,
    /// Index into the picker's `options`.
    PickFolder(usize),
    PickerPrevious,
    PickerNext,
    Jump,
    RemoveLink,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogButton {
    pub id: ButtonId,
    pub label: String,
    pub enabled: bool,
    pub emphasized: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogSpec {
    pub heading: String,
    pub body: Vec<String>,
    pub buttons: Vec<DialogButton>,
    /// Menus stack buttons vertically; confirmations use one horizontal row.
    pub vertical: bool,
}

fn button(id: ButtonId, label: &str) -> DialogButton {
    DialogButton {
        id,
        label: label.into(),
        enabled: true,
        emphasized: false,
    }
}

/// Dialog content for every modal except the full-screen title editor.
pub fn dialog_spec(modal: &Modal) -> Option<DialogSpec> {
    Some(match modal {
        Modal::CardMenu { title, .. } => DialogSpec {
            heading: title.clone(),
            body: Vec::new(),
            buttons: vec![
                button(ButtonId::Rename, "RENAME"),
                button(ButtonId::AddToFolder, "ADD TO FOLDER"),
                button(ButtonId::DeleteNotebook, "DELETE"),
                button(ButtonId::Cancel, "CANCEL"),
            ],
            vertical: true,
        },
        Modal::CardMenuInFolder { title, .. } => DialogSpec {
            heading: title.clone(),
            body: Vec::new(),
            buttons: vec![
                button(ButtonId::Rename, "RENAME"),
                button(ButtonId::ChangeFolder, "CHANGE FOLDER"),
                button(ButtonId::RemoveFromFolder, "REMOVE FROM FOLDER"),
                button(ButtonId::DeleteNotebook, "DELETE"),
                button(ButtonId::Cancel, "CANCEL"),
            ],
            vertical: true,
        },
        Modal::NewChooser { .. } => DialogSpec {
            heading: "Create new".into(),
            body: Vec::new(),
            buttons: vec![
                button(ButtonId::NewNotebook, "NEW NOTEBOOK"),
                button(ButtonId::NewFolder, "NEW FOLDER"),
                button(ButtonId::Cancel, "CANCEL"),
            ],
            vertical: true,
        },
        Modal::FolderMenu { title, .. } => DialogSpec {
            heading: title.clone(),
            body: Vec::new(),
            buttons: vec![
                button(ButtonId::Rename, "RENAME"),
                button(ButtonId::DeleteFolder, "DELETE"),
                button(ButtonId::Cancel, "CANCEL"),
            ],
            vertical: true,
        },
        Modal::ConfirmDeleteFolder { title, .. } => DialogSpec {
            heading: format!("Delete folder \"{title}\"?"),
            body: vec!["Its notebooks move back to the main list. No notebook is deleted.".into()],
            buttons: vec![
                button(ButtonId::Cancel, "CANCEL"),
                DialogButton {
                    emphasized: true,
                    ..button(ButtonId::ConfirmDelete, "DELETE")
                },
            ],
            vertical: false,
        },
        Modal::FolderPicker {
            title,
            options,
            page,
            ..
        } => {
            let start = (page * PICKER_PAGE).min(options.len());
            let end = (start + PICKER_PAGE).min(options.len());
            let mut buttons: Vec<DialogButton> = options[start..end]
                .iter()
                .enumerate()
                .map(|(k, (_, name))| button(ButtonId::PickFolder(start + k), name))
                .collect();
            if options.len() > PICKER_PAGE {
                buttons.push(DialogButton {
                    enabled: start > 0,
                    ..button(ButtonId::PickerPrevious, "PREVIOUS")
                });
                buttons.push(DialogButton {
                    enabled: end < options.len(),
                    ..button(ButtonId::PickerNext, "NEXT")
                });
            }
            buttons.push(button(ButtonId::Cancel, "CANCEL"));
            DialogSpec {
                heading: format!("Move \"{title}\" to"),
                body: Vec::new(),
                buttons,
                vertical: true,
            }
        }
        Modal::ConfirmDeleteNotebook { title, .. } => DialogSpec {
            heading: format!("Delete \"{title}\"?"),
            body: vec!["The notebook will be removed from the library.".into()],
            buttons: vec![
                button(ButtonId::Cancel, "CANCEL"),
                DialogButton {
                    emphasized: true,
                    ..button(ButtonId::ConfirmDelete, "DELETE")
                },
            ],
            vertical: false,
        },
        Modal::PageMenu {
            page_number,
            page_count,
        } => DialogSpec {
            heading: format!("Page {page_number} of {page_count}"),
            body: Vec::new(),
            buttons: vec![
                button(ButtonId::InsertPageAfter, "INSERT PAGE AFTER"),
                DialogButton {
                    enabled: *page_number > 1,
                    ..button(ButtonId::MovePageLeft, "MOVE PAGE LEFT")
                },
                DialogButton {
                    enabled: page_number < page_count,
                    ..button(ButtonId::MovePageRight, "MOVE PAGE RIGHT")
                },
                button(ButtonId::DeletePage, "DELETE PAGE"),
                button(ButtonId::Cancel, "CANCEL"),
            ],
            vertical: true,
        },
        Modal::ConfirmDeletePage { page_number } => DialogSpec {
            heading: format!("Delete page {page_number}?"),
            body: Vec::new(),
            buttons: vec![
                button(ButtonId::Cancel, "CANCEL"),
                DialogButton {
                    emphasized: true,
                    ..button(ButtonId::ConfirmDelete, "DELETE")
                },
            ],
            vertical: false,
        },
        Modal::Notice { heading, body } => DialogSpec {
            heading: heading.clone(),
            body: body.clone(),
            buttons: vec![button(ButtonId::Ok, "OK")],
            vertical: false,
        },
        Modal::LinkJump {
            label, target_live, ..
        } => DialogSpec {
            heading: "LINK".into(),
            body: Vec::new(),
            buttons: vec![
                DialogButton {
                    id: ButtonId::Jump,
                    label: label.clone(),
                    enabled: *target_live,
                    emphasized: true,
                },
                button(ButtonId::RemoveLink, "REMOVE LINK"),
            ],
            vertical: true,
        },
        Modal::Error { message, .. } => DialogSpec {
            heading: "Something went wrong".into(),
            body: vec![message.clone()],
            buttons: vec![button(ButtonId::Ok, "OK")],
            vertical: false,
        },
        Modal::TitleEntry { .. } => return None,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogLayout {
    pub frame: Rect,
    pub heading: Vec<(u32, String)>,
    pub body: Vec<(u32, String)>,
    pub buttons: Vec<(DialogButton, Rect)>,
}

impl DialogLayout {
    pub fn new(geo: &FbGeometry, spec: &DialogSpec) -> Self {
        let box_w = geo.xres.saturating_sub(2 * BOX_MARGIN_X);
        let inner_w = box_w.saturating_sub(2 * BOX_PAD);
        let heading_lines = wrap_text(&spec.heading, HEADING_SCALE, inner_w, 3);
        let body_lines: Vec<String> = spec
            .body
            .iter()
            .flat_map(|p| wrap_text(p, BODY_SCALE, inner_w, 4))
            .collect();
        let heading_h = heading_lines.len() as u32 * (7 * HEADING_SCALE + LINE_GAP);
        let body_h = if body_lines.is_empty() {
            0
        } else {
            LINE_GAP + body_lines.len() as u32 * (7 * BODY_SCALE + LINE_GAP)
        };
        let n = spec.buttons.len() as u32;
        let buttons_h = if spec.vertical {
            n * BUTTON_H + n.saturating_sub(1) * BUTTON_GAP
        } else {
            BUTTON_H
        };
        let box_h = BOX_PAD + heading_h + body_h + BOX_PAD + buttons_h + BOX_PAD;
        let box_x = BOX_MARGIN_X;
        let box_y = geo.yres.saturating_sub(box_h) / 2;
        let frame = Rect::new(box_x, box_y, box_w, box_h);

        let mut y = box_y + BOX_PAD;
        let mut heading = Vec::new();
        for line in heading_lines {
            heading.push((y, line));
            y += 7 * HEADING_SCALE + LINE_GAP;
        }
        let mut body = Vec::new();
        if !body_lines.is_empty() {
            y += LINE_GAP;
            for line in body_lines {
                body.push((y, line));
                y += 7 * BODY_SCALE + LINE_GAP;
            }
        }
        y += BOX_PAD;
        let x0 = box_x + BOX_PAD;
        let mut buttons = Vec::new();
        if spec.vertical {
            for b in &spec.buttons {
                buttons.push((b.clone(), Rect::new(x0, y, inner_w, BUTTON_H)));
                y += BUTTON_H + BUTTON_GAP;
            }
        } else if let Some(w) = inner_w
            .saturating_sub(n.saturating_sub(1) * BUTTON_GAP)
            .checked_div(n)
        {
            for (i, b) in spec.buttons.iter().enumerate() {
                let x = x0 + i as u32 * (w + BUTTON_GAP);
                buttons.push((b.clone(), Rect::new(x, y, w, BUTTON_H)));
            }
        }
        Self {
            frame,
            heading,
            body,
            buttons,
        }
    }

    /// Enabled button under `p`. Disabled buttons and dialog chrome consume taps silently.
    pub fn hit(&self, p: UiPoint) -> Option<ButtonId> {
        self.buttons
            .iter()
            .find(|(b, r)| b.enabled && r.contains(p.x, p.y))
            .map(|(b, _)| b.id)
    }
}

/// Paint a dialog on top of whatever the framebuffer already shows.
pub fn render_dialog(
    fb: &mut [u8],
    geo: &FbGeometry,
    spec: &DialogSpec,
) -> Result<DialogLayout, String> {
    let layout = DialogLayout::new(geo, spec);
    let mut c = Canvas::new(fb, geo)?;
    // A white halo separates the dialog from busy page content underneath.
    let halo = Rect::new(
        layout.frame.x.saturating_sub(24),
        layout.frame.y.saturating_sub(24),
        layout.frame.w + 48,
        layout.frame.h + 48,
    );
    c.fill(halo, WHITE);
    c.frame(layout.frame, BOX_BORDER, BLACK);
    let text_x = layout.frame.x + BOX_PAD;
    for (y, line) in &layout.heading {
        c.text(text_x, *y, HEADING_SCALE, line, BLACK);
    }
    for (y, line) in &layout.body {
        c.text(text_x, *y, BODY_SCALE, line, BLACK);
    }
    for (b, r) in &layout.buttons {
        let (bg, fg, border) = if !b.enabled {
            (WHITE, GRAY, GRAY)
        } else if b.emphasized {
            (BLACK, WHITE, BLACK)
        } else {
            (WHITE, BLACK, BLACK)
        };
        c.fill(*r, bg);
        c.frame(*r, 5, border);
        let label = crate::ui_gfx::fit_text(&b.label, BUTTON_SCALE, r.w.saturating_sub(40));
        c.text_centered(r.inset(10), BUTTON_SCALE, &label, fg);
    }
    Ok(layout)
}
