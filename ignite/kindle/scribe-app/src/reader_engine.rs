//! Experimental book reader engine: MuPDF behind the off-by-default `reader` feature.
//!
//! Build plumbing and validation only. Nothing in the app calls this module yet, and ordinary
//! builds do not compile it or link MuPDF. It proves that MuPDF builds for the host and the
//! Kindle target and that its two paths Ignite needs work: fixed-layout pages (PDF, CBZ) and
//! reflowable layout (EPUB uses the same HTML layout engine that the tests exercise).
//!
//! Everything is rendered as 8-bit gray, the Kindle panel's format, scaled to fit a box given
//! in pixels. Book identity, reading positions and any UI are deliberately out of scope here.

use mupdf::{Colorspace, Document, Matrix};

/// A rendered page: `width * height` gray pixels, row-major, 0 = black, 255 = white.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrayPage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// An error from MuPDF, reduced to its message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderError(pub String);

impl std::fmt::Display for ReaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "reader: {}", self.0)
    }
}

impl std::error::Error for ReaderError {}

impl From<mupdf::Error> for ReaderError {
    fn from(e: mupdf::Error) -> Self {
        Self(e.to_string())
    }
}

/// An open document.
pub struct Book {
    doc: Document,
}

impl Book {
    /// Opens a document from memory. `magic` is a MIME type or file extension that tells MuPDF
    /// the format (for example `"application/pdf"`, `"epub"`, `"cbz"`, `"text/html"`).
    pub fn open_bytes(bytes: &[u8], magic: &str) -> Result<Self, ReaderError> {
        Ok(Self {
            doc: Document::from_bytes(bytes, magic)?,
        })
    }

    /// True for formats whose pages depend on the layout (EPUB, HTML), false for PDF and CBZ.
    pub fn is_reflowable(&self) -> Result<bool, ReaderError> {
        Ok(self.doc.is_reflowable()?)
    }

    /// Lays out a reflowable document for a page of `width` x `height` points with a font size
    /// of `em` points. Fixed-layout documents ignore it.
    pub fn layout(&mut self, width: f32, height: f32, em: f32) -> Result<(), ReaderError> {
        validate_layout(width, height, em)?;
        Ok(self.doc.layout(width, height, em)?)
    }

    pub fn page_count(&self) -> Result<u32, ReaderError> {
        page_count_from_mupdf(self.doc.page_count()?)
    }

    /// Renders page `index` (0-based) as large as fits in `box_w` x `box_h` pixels, keeping its
    /// aspect ratio.
    pub fn render_page_gray(
        &self,
        index: u32,
        box_w: u32,
        box_h: u32,
    ) -> Result<GrayPage, ReaderError> {
        if box_w == 0 || box_h == 0 {
            return Err(ReaderError("empty render box".into()));
        }
        let index = i32::try_from(index).map_err(|_| ReaderError("page index too large".into()))?;
        let page = self.doc.load_page(index)?;
        let b = page.bounds()?;
        let (pw, ph) = (b.x1 - b.x0, b.y1 - b.y0);
        if !(pw.is_finite() && ph.is_finite() && pw > 0.0 && ph > 0.0) {
            return Err(ReaderError("page has an invalid size".into()));
        }
        let zoom = (box_w as f32 / pw).min(box_h as f32 / ph);
        if !(zoom.is_finite() && zoom > 0.0) {
            return Err(ReaderError("page has an invalid scale".into()));
        }
        let pix = page.to_pixmap(
            &Matrix::new_scale(zoom, zoom),
            &Colorspace::device_gray(),
            false,
            true,
        )?;

        // Rounding can make the pixmap one pixel larger than the box; never exceed it.
        let (w, h) = (pix.width().min(box_w), pix.height().min(box_h));
        if pix.alpha() {
            return Err(ReaderError("gray pixmap unexpectedly has alpha".into()));
        }
        let pixels = copy_gray_samples(w, h, pix.n(), pix.stride(), pix.samples())?;
        Ok(GrayPage {
            width: w,
            height: h,
            pixels,
        })
    }
}

fn validate_layout(width: f32, height: f32, em: f32) -> Result<(), ReaderError> {
    if [width, height, em]
        .into_iter()
        .all(|value| value.is_finite() && value > 0.0)
    {
        Ok(())
    } else {
        Err(ReaderError(
            "layout dimensions must be finite and positive".into(),
        ))
    }
}

fn page_count_from_mupdf(count: i32) -> Result<u32, ReaderError> {
    u32::try_from(count).map_err(|_| ReaderError(format!("invalid page count: {count}")))
}

fn copy_gray_samples(
    width: u32,
    height: u32,
    components: u8,
    stride: isize,
    samples: &[u8],
) -> Result<Vec<u8>, ReaderError> {
    if width == 0 || height == 0 {
        return Err(ReaderError("MuPDF returned an empty pixmap".into()));
    }
    if components != 1 {
        return Err(ReaderError(format!(
            "gray pixmap has {components} components"
        )));
    }
    let width = usize::try_from(width)
        .map_err(|_| ReaderError("pixmap width cannot be represented".into()))?;
    let height = usize::try_from(height)
        .map_err(|_| ReaderError("pixmap height cannot be represented".into()))?;
    let stride = usize::try_from(stride)
        .map_err(|_| ReaderError("pixmap stride cannot be represented".into()))?;
    if stride < width {
        return Err(ReaderError("pixmap stride is shorter than a row".into()));
    }
    let (pixel_count, required) = gray_buffer_layout(width, height, stride)?;
    if samples.len() < required {
        return Err(ReaderError(format!(
            "pixmap sample buffer is too short: {} < {required}",
            samples.len()
        )));
    }
    let mut pixels = Vec::with_capacity(pixel_count);
    for row in samples.chunks(stride).take(height) {
        pixels.extend_from_slice(&row[..width]);
    }
    if pixels.len() != pixel_count {
        return Err(ReaderError("pixmap row count is inconsistent".into()));
    }
    Ok(pixels)
}

fn gray_buffer_layout(
    width: usize,
    height: usize,
    stride: usize,
) -> Result<(usize, usize), ReaderError> {
    if width == 0 || height == 0 {
        return Err(ReaderError("MuPDF returned an empty pixmap".into()));
    }
    let pixel_count = width
        .checked_mul(height)
        .ok_or_else(|| ReaderError("pixmap dimensions overflow".into()))?;
    let required = (height - 1)
        .checked_mul(stride)
        .and_then(|offset| offset.checked_add(width))
        .ok_or_else(|| ReaderError("pixmap sample span overflows".into()))?;
    Ok((pixel_count, required))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-page PDF (200 x 100 pt) with a black rectangle at x 50..150, y 25..75 (PDF
    /// coordinates, origin bottom left), with a correct cross-reference table.
    fn rectangle_pdf() -> Vec<u8> {
        let content = "0 g 50 25 100 50 re f";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R >>".to_string(),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for off in offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    fn long_html() -> Vec<u8> {
        let para = "<p>Ignite renders this paragraph to check that reflowable layout works. \
                    A larger font has to spread the same text over more pages.</p>";
        format!("<html><body>{}</body></html>", para.repeat(60)).into_bytes()
    }

    fn pixel(p: &GrayPage, x: u32, y: u32) -> u8 {
        p.pixels[(y * p.width + x) as usize]
    }

    #[test]
    fn pdf_page_renders_to_fit_the_box_in_gray() {
        let book = Book::open_bytes(&rectangle_pdf(), "application/pdf").expect("open pdf");
        assert!(!book.is_reflowable().unwrap());
        assert_eq!(book.page_count().unwrap(), 1);

        // 200 x 100 pt into 400 x 400 px: width-limited, zoom 2 → 400 x 200.
        let page = book.render_page_gray(0, 400, 400).expect("render");
        assert_eq!((page.width, page.height), (400, 200));
        assert_eq!(page.pixels.len(), 400 * 200);
        assert!(
            pixel(&page, 200, 100) < 32,
            "rectangle centre should be black"
        );
        assert!(pixel(&page, 10, 10) > 223, "corner should be white");
        assert!(pixel(&page, 390, 190) > 223, "corner should be white");
    }

    #[test]
    fn render_rejects_an_empty_box_and_a_missing_page() {
        let book = Book::open_bytes(&rectangle_pdf(), "application/pdf").unwrap();
        assert!(book.render_page_gray(0, 0, 100).is_err());
        assert!(book.render_page_gray(5, 100, 100).is_err());
    }

    #[test]
    fn layout_rejects_nonfinite_and_nonpositive_dimensions() {
        let mut book = Book::open_bytes(&long_html(), "text/html").unwrap();
        assert!(book.layout(f32::NAN, 100.0, 10.0).is_err());
        assert!(book.layout(100.0, f32::INFINITY, 10.0).is_err());
        assert!(book.layout(100.0, 100.0, 0.0).is_err());
    }

    #[test]
    fn invalid_page_counts_are_errors() {
        assert_eq!(page_count_from_mupdf(3).unwrap(), 3);
        assert!(page_count_from_mupdf(-1).is_err());
    }

    #[test]
    fn gray_sample_copy_validates_layout_and_buffer_size() {
        assert_eq!(
            copy_gray_samples(2, 2, 1, 3, &[1, 2, 9, 3, 4]).unwrap(),
            vec![1, 2, 3, 4]
        );
        assert!(copy_gray_samples(2, 2, 2, 4, &[0; 8]).is_err());
        assert!(copy_gray_samples(2, 2, 1, -1, &[0; 4]).is_err());
        assert!(copy_gray_samples(2, 2, 1, 1, &[0; 4]).is_err());
        assert!(copy_gray_samples(2, 2, 1, 2, &[0; 3]).is_err());
        assert!(gray_buffer_layout(usize::MAX, 2, usize::MAX).is_err());
        assert!(gray_buffer_layout(1, 2, usize::MAX).is_err());
    }

    #[test]
    fn reflowable_text_repaginates_with_font_size() {
        let mut book = Book::open_bytes(&long_html(), "text/html").expect("open html");
        assert!(book.is_reflowable().unwrap());

        // Roughly the Kindle Scribe's safe area at 300 dpi, in points.
        let (w, h) = (418.0, 528.0);
        book.layout(w, h, 10.0).unwrap();
        let small = book.page_count().unwrap();
        book.layout(w, h, 20.0).unwrap();
        let large = book.page_count().unwrap();
        assert!(small >= 1);
        assert!(
            large > small,
            "a larger font should need more pages ({small} at 10 pt, {large} at 20 pt)"
        );

        let page = book.render_page_gray(0, 1740, 2200).expect("render");
        assert!(
            page.pixels.iter().any(|&v| v < 128),
            "text should leave dark pixels on the page"
        );
    }

    #[test]
    fn garbage_is_an_error_not_a_crash() {
        assert!(Book::open_bytes(b"not a document", "application/pdf").is_err());
    }
}
