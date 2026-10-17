# Noto Emoji (monochrome)

Kindle page-text **fallback** face. Not a `SyncText` face token. Ordinary text
still uses `default-sans-v1` (Source Sans 3). This file is used only when that
primary cmap cannot supply a glyph (or when a cluster is base + U+FE0F).

| | |
|---|---|
| File | `NotoEmoji[wght].ttf` (upstream name; do not rename) |
| Family | Noto Emoji (variable `wght`, default Regular) |
| Source | [google/fonts `ofl/notoemoji`](https://github.com/google/fonts/tree/main/ofl/notoemoji) |
| google/fonts commit | `b979dba422e445492b0eb9951ac52ee0b4d648c3` (Noto Emoji 3.002) |
| SHA-256 | `DE6C18832938AFC99CAF132B39D6A30A19BAC7F2E812E28DB2535B4608D27551` |
| License | SIL Open Font License 1.1 — **[`licenses/noto-emoji/`](../../../licenses/noto-emoji/)** |

This copy is unmodified. Do not subset, rename, or regenerate it here.

Kindle embeds the TTF at compile time (`include_bytes!` in `scribe-app`). License
and copyright notices are shipped to the device by the customer Install Ignite
Scriptlet (`deployment/kindle/`) as `/mnt/us/scribe-app/assets/licenses/noto-emoji/`.
