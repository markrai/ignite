# `default-sans-v1` typography contract

The protocol face token `default-sans-v1` is **not** a CSS or system font name. It
names this exact file and the wrap/metrics rules below. Every replica that paints
text must use this file.

## Font file

| | |
|---|---|
| Path | `assets/typography/default-sans-v1.ttf` |
| Family | Source Sans 3 Regular |
| License | SIL Open Font License 1.1 (`OFL.txt`) |
| unitsPerEm | 1000 |

Kindle embeds this file at compile time (`include_bytes!` in `scribe-app`). Do not
copy a third maintained **primary** font into `native/`. Monochrome Noto Emoji is
the exception: it lives under `assets/typography/noto-emoji/` and is a fallback
only. The web bundle may vendor `default-sans-v1` for `@font-face`; that copy is
a build artifact of this file.

`font_size` on `SyncText` is the em size in **canonical page units**
(15624 × 20832). One em at that size maps to `font_size` canonical units.

## Line metrics (v1)

- Line height is **`1.2 × font_size`** (unitless CSS `line-height: 1.2` on the
  same em).
- Height of a text object is derived: it is not stored on the wire.
- **Baseline (Kindle, pinned):** for line index `i` (0-based),
  `line_top = y + i * 1.2 * font_size`.
  Display pixels `px = max(1, font_size * scale_x)` using the same
  `ScribeDisplayTransform` / `ViewportTransform` as ink.
  `ascent_px` comes from the embedded font's horizontal line metrics at `px`
  (fallback `0.8 * px` if the face has none).
  CSS half-leading is `(1.2 * px - px) / 2`.
  Canonical baseline `= line_top + (half_leading + ascent_px) / scale_x`.
- Lines whose `line_top >= CANONICAL_HEIGHT` are not painted. Glyph pixels that
  would land at `y >= panel yres` or `y < 0` are clipped. Text never paints into
  chrome by expanding the page; chrome is a later overlay on the same panel.

## Wrap (v1)

Equivalent to:

```text
white-space: pre-wrap
overflow-wrap: break-word
word-break: normal
```

- U+000A is a forced line break (content is already LF-normalized).
- U+0020 is a wrap opportunity. Spaces are preserved (leading, trailing, and
  between words).
- Advances are the **resolved** font's unkerned `advance_width` at a reference
  size, scaled linearly to `font_size` canonical units (primary face, else Noto
  Emoji, else primary `.notdef`). **No OpenType shaping, no kerning, no
  ligatures** on Kindle. Wrap and paint share this resolver.
- A line is filled until the next scalar would exceed `width`. If a wrap
  opportunity (space) exists in the overflowing run, the break is after that
  space. Otherwise the run is broken so it stays inside the box. A single
  scalar wider than `width` is still taken (paint clips it).

Web implements wrap with the bundled `@font-face` named
`scribe-default-sans-v1` and CSS. Kindle implements wrap with the rules above
in `scribe_app::text_paint`.

## Glyph placement and rounding (Kindle, pinned)

Ink mapping: `display = canonical * panel / page` (`PortraitIdentity`,
`floor` onto the panel). Text uses that same transform.

- Horizontal clip box: `left = floor(to_display(x, y).x)`,
  `right = floor(to_display(x + width, y).x)`, clamped to the panel.
- For each scalar in a wrapped line, choose a glyph from the **font stack**:
  `default-sans-v1` first, then monochrome Noto Emoji if that cmap misses,
  else this face's `.notdef` (never an Amazon/system substitute). Variation
  selectors (U+FE0E, U+FE0F, VS17–VS256) are skipped with zero advance so
  `❤️` does not paint a second box. A base + U+FE0F cluster prefers Noto Emoji
  for the base.
- Destination origin (panel is +Y down; fontdue `ymin` is y-up from the baseline):
  `dest_x = floor(to_display(pen_x, baseline).x) + metrics.xmin`
  `dest_y = floor(to_display(pen_x, baseline).y) + floor(-bounds.height - bounds.ymin)`
  Coverage is max-composited into a panel-sized overlay, then darkened onto
  paper/ink (`coverage` as alpha toward ink 0x00).
- `pen_x` advances by the canonical unkerned width after each scalar.

## Kindle emoji fallback (Noto Emoji, monochrome)

Page text may contain Unicode the primary cmap does not cover. Kindle embeds
[`noto-emoji/NotoEmoji[wght].ttf`](noto-emoji/NotoEmoji[wght].ttf) (Google Fonts
Noto Emoji 3.002, OFL) and selects it only when `default-sans-v1` has no glyph
(or when the cluster is base + U+FE0F). Notices: [`licenses/noto-emoji/`](../../licenses/noto-emoji/).

Line metrics and baseline still come from Source Sans 3. This is not a
`TextFace` and is not synced.

**Not claimed:** GSUB / ZWJ sequences (family emoji, skin-tone modifiers),
regional-indicator flag ligatures, or color (CBDT/COLR) emoji. Those remain
per-scalar; missing pieces still use primary `.notdef`.

## Do not

- Substitute Arial / Liberation / Kindle UI / system fonts for this token.
- Store a CSS `font-family` string on the wire.

## Known web vs Kindle parity limits (not this slice)

Protocol content is not restricted to the primary font cmap. **Kindle** paints
missing glyphs from monochrome Noto Emoji when that face has them, otherwise
primary `.notdef`. **Web CSS** may still substitute a generic fallback for
characters the `@font-face` does not cover, and the browser may kern/shape.
Web zoom also changes CSS wrap in display pixels, which can diverge from
Kindle's canonical wrap at panel scale 8.4.
