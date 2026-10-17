//! Gate F11 on-screen title keyboard (framebuffer-native, pen-operated).
//!
//! Deliberately small: append-only editing with the cursor at the end, one-shot Shift, a
//! letters/symbols mode toggle, and the library's title rules (1..=96 chars after trimming,
//! no control characters). Not a general text-editing framework.

use kindle_platform::FbGeometry;

use crate::library::{validate_title, MAX_TITLE_CHARS};
use crate::ui_gfx::{fit_text_tail, Canvas, Rect, UiPoint, BLACK, GRAY, WHITE};

const MARGIN: u32 = 60;
const HEADING_Y: u32 = 70;
const HEADING_SCALE: u32 = 10;
const FIELD_Y: u32 = 220;
const FIELD_H: u32 = 200;
const FIELD_SCALE: u32 = 10;
const STATUS_Y: u32 = 450;
const STATUS_SCALE: u32 = 6;
const KEYS_TOP: u32 = 600;
const KEY_H: u32 = 170;
const KEY_GAP: u32 = 18;
const KEY_SCALE: u32 = 9;
const ACTION_GAP: u32 = 60;
const ACTION_H: u32 = 190;

const LETTER_ROWS: [&str; 3] = ["qwertyuiop", "asdfghjkl", "zxcvbnm"];
const SYMBOL_ROWS: [&str; 3] = ["1234567890", "-_.'&!?,(", ")#+/:;\""];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorKey {
    Char(char),
    Shift,
    Mode,
    Space,
    Backspace,
    Cancel,
    Submit,
}

/// What the display needs after a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorOutcome {
    /// Nothing changed (e.g. typing at the maximum length, backspace on empty).
    Unchanged,
    /// Title text changed: only the field/status area needs a refresh.
    TextChanged,
    /// Shift/mode changed: the key area needs a refresh.
    KeysChanged,
    Cancel,
    /// Validated, trimmed title ready for the durable library operation.
    Submit(String),
    /// Submit refused by the library title rules (short user-facing message).
    Invalid(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TitleEditor {
    text: String,
    shift: bool,
    symbols: bool,
}

impl TitleEditor {
    /// Start editing from `initial` (clipped to the maximum; control characters dropped).
    pub fn new(initial: &str) -> Self {
        let text: String = initial
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_TITLE_CHARS)
            .collect();
        Self {
            text,
            shift: false,
            symbols: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    #[cfg(test)]
    pub fn shift(&self) -> bool {
        self.shift
    }

    pub fn symbols(&self) -> bool {
        self.symbols
    }

    fn push(&mut self, ch: char) -> EditorOutcome {
        if ch.is_control() || self.text.chars().count() >= MAX_TITLE_CHARS {
            return EditorOutcome::Unchanged;
        }
        self.text.push(ch);
        EditorOutcome::TextChanged
    }

    pub fn apply(&mut self, key: EditorKey) -> EditorOutcome {
        match key {
            EditorKey::Char(c) => {
                let ch = if self.shift && !self.symbols {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                let out = self.push(ch);
                if self.shift && !self.symbols && out == EditorOutcome::TextChanged {
                    // One-shot Shift; the key area must redraw back to lowercase.
                    self.shift = false;
                    return EditorOutcome::KeysChanged;
                }
                out
            }
            EditorKey::Space => self.push(' '),
            EditorKey::Backspace => {
                if self.text.pop().is_some() {
                    EditorOutcome::TextChanged
                } else {
                    EditorOutcome::Unchanged
                }
            }
            EditorKey::Shift => {
                if self.symbols {
                    return EditorOutcome::Unchanged;
                }
                self.shift = !self.shift;
                EditorOutcome::KeysChanged
            }
            EditorKey::Mode => {
                self.symbols = !self.symbols;
                self.shift = false;
                EditorOutcome::KeysChanged
            }
            EditorKey::Cancel => EditorOutcome::Cancel,
            EditorKey::Submit => {
                let title = self.text.trim().to_string();
                match validate_title(&title) {
                    Ok(()) => EditorOutcome::Submit(title),
                    Err(_) if title.is_empty() => {
                        EditorOutcome::Invalid("A notebook title can't be empty.".into())
                    }
                    Err(_) => EditorOutcome::Invalid("That title can't be used.".into()),
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyboardLayout {
    pub field: Rect,
    pub status: Rect,
    /// Everything from the first key row to the bottom of the action buttons.
    pub keys_area: Rect,
    pub keys: Vec<(EditorKey, Rect)>,
}

impl KeyboardLayout {
    pub fn new(geo: &FbGeometry, symbols: bool) -> Self {
        let width = geo.xres.saturating_sub(2 * MARGIN);
        let unit = (width.saturating_sub(9 * KEY_GAP)) / 10;
        let rows = if symbols { SYMBOL_ROWS } else { LETTER_ROWS };
        let mut keys = Vec::new();
        let row_y = |i: u32| KEYS_TOP + i * (KEY_H + KEY_GAP);
        let row_w = |n: u32| n * unit + n.saturating_sub(1) * KEY_GAP;

        // Rows 1 and 2: centered character rows.
        for (i, row) in rows.iter().take(2).enumerate() {
            let n = row.chars().count() as u32;
            let x0 = MARGIN + (width.saturating_sub(row_w(n))) / 2;
            for (k, c) in row.chars().enumerate() {
                let x = x0 + k as u32 * (unit + KEY_GAP);
                keys.push((
                    EditorKey::Char(c),
                    Rect::new(x, row_y(i as u32), unit, KEY_H),
                ));
            }
        }
        // Row 3: Shift | 7 chars | Backspace  (wide keys = 1.5 units + half gap).
        let wide = unit + unit / 2 + KEY_GAP / 2;
        let y3 = row_y(2);
        keys.push((EditorKey::Shift, Rect::new(MARGIN, y3, wide, KEY_H)));
        let mid = rows[2];
        let x_mid = MARGIN + wide + KEY_GAP;
        for (k, c) in mid.chars().enumerate() {
            let x = x_mid + k as u32 * (unit + KEY_GAP);
            keys.push((EditorKey::Char(c), Rect::new(x, y3, unit, KEY_H)));
        }
        keys.push((
            EditorKey::Backspace,
            Rect::new(MARGIN + width - wide, y3, wide, KEY_H),
        ));
        // Row 4: mode toggle | space.
        let y4 = row_y(3);
        let mode_w = 2 * unit + KEY_GAP;
        keys.push((EditorKey::Mode, Rect::new(MARGIN, y4, mode_w, KEY_H)));
        keys.push((
            EditorKey::Space,
            Rect::new(
                MARGIN + mode_w + KEY_GAP,
                y4,
                width - mode_w - KEY_GAP,
                KEY_H,
            ),
        ));
        // Action row.
        let y5 = row_y(4) + ACTION_GAP;
        let half = (width - KEY_GAP * 2) / 2;
        keys.push((EditorKey::Cancel, Rect::new(MARGIN, y5, half, ACTION_H)));
        keys.push((
            EditorKey::Submit,
            Rect::new(MARGIN + width - half, y5, half, ACTION_H),
        ));

        let keys_area = Rect::new(0, KEYS_TOP, geo.xres, y5 + ACTION_H + 20 - KEYS_TOP);
        Self {
            field: Rect::new(MARGIN, FIELD_Y, width, FIELD_H),
            status: Rect::new(MARGIN, STATUS_Y, width, 7 * STATUS_SCALE + 20),
            keys_area,
            keys,
        }
    }

    pub fn hit(&self, p: UiPoint) -> Option<EditorKey> {
        self.keys
            .iter()
            .find(|(_, r)| r.contains(p.x, p.y))
            .map(|(k, _)| *k)
    }

    /// Region to refresh after an outcome (`None` = nothing visible changed).
    pub fn refresh_region(&self, outcome: &EditorOutcome) -> Option<Rect> {
        match outcome {
            EditorOutcome::TextChanged => Some(Rect::new(
                0,
                self.field.y,
                self.field.right() + MARGIN,
                self.status.bottom() - self.field.y,
            )),
            // A one-shot Shift consumed by a letter also changes the text.
            EditorOutcome::KeysChanged => Some(Rect::new(
                0,
                self.field.y,
                self.keys_area.w,
                self.keys_area.bottom() - self.field.y,
            )),
            _ => None,
        }
    }
}

fn key_label(key: EditorKey, editor: &TitleEditor, submit_label: &str) -> String {
    match key {
        EditorKey::Char(c) => {
            if editor.shift && !editor.symbols {
                c.to_ascii_uppercase().to_string()
            } else {
                c.to_string()
            }
        }
        EditorKey::Shift => "SHIFT".into(),
        EditorKey::Mode => {
            if editor.symbols {
                "ABC".into()
            } else {
                "123".into()
            }
        }
        EditorKey::Space => "SPACE".into(),
        EditorKey::Backspace => "DEL".into(),
        EditorKey::Cancel => "CANCEL".into(),
        EditorKey::Submit => submit_label.into(),
    }
}

/// Paint the full-screen title editor (white background; no underlying view remains).
pub fn render_title_editor(
    fb: &mut [u8],
    geo: &FbGeometry,
    heading: &str,
    submit_label: &str,
    editor: &TitleEditor,
) -> Result<KeyboardLayout, String> {
    let layout = KeyboardLayout::new(geo, editor.symbols);
    let mut c = Canvas::new(fb, geo)?;
    c.fill_all(WHITE);
    c.text(MARGIN, HEADING_Y, HEADING_SCALE, heading, BLACK);

    // Title field with cursor at the end.
    c.frame(layout.field, 6, BLACK);
    let inner = layout.field.inset(30);
    let cursor_w = 8;
    let shown = fit_text_tail(
        editor.text(),
        FIELD_SCALE,
        inner.w.saturating_sub(cursor_w + FIELD_SCALE * 2),
    );
    let text_y = inner.y + inner.h.saturating_sub(7 * FIELD_SCALE) / 2;
    c.text(inner.x, text_y, FIELD_SCALE, &shown, BLACK);
    let cursor_x = inner.x + crate::ui_gfx::text_width(&shown, FIELD_SCALE) + FIELD_SCALE;
    c.fill(
        Rect::new(cursor_x, text_y - 10, cursor_w, 7 * FIELD_SCALE + 20),
        BLACK,
    );
    let count = format!("{}/{}", editor.text().chars().count(), MAX_TITLE_CHARS);
    c.text(
        layout.status.x,
        layout.status.y,
        STATUS_SCALE,
        &count,
        BLACK,
    );

    for (key, rect) in &layout.keys {
        let label = key_label(*key, editor, submit_label);
        let inverted =
            matches!(key, EditorKey::Submit) || (matches!(key, EditorKey::Shift) && editor.shift);
        let disabled = matches!(key, EditorKey::Shift) && editor.symbols;
        let (bg, fg, border) = if disabled {
            (WHITE, GRAY, GRAY)
        } else if inverted {
            (BLACK, WHITE, BLACK)
        } else {
            (WHITE, BLACK, BLACK)
        };
        c.fill(*rect, bg);
        c.frame(*rect, 5, border);
        let scale = if label.chars().count() > 1 {
            7
        } else {
            KEY_SCALE
        };
        c.text_centered(rect.inset(8), scale, &label, fg);
    }
    Ok(layout)
}
