//! Experimental book reader engine: MuPDF behind the off-by-default `reader` feature.
//!
//! Build plumbing and validation only. Nothing in the app calls this module yet, and ordinary
//! builds do not compile it or link MuPDF. It opens controlled PDF, EPUB and CBZ files without
//! copying their complete contents, while retaining an in-memory fixture path for tests.
//!
//! Everything is rendered as 8-bit gray, the Kindle panel's format, scaled to fit a box given
//! in pixels. Book identity, reading positions and any UI are deliberately out of scope here.

use std::fs::{self, File, Metadata};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use mupdf::{Colorspace, Document, Matrix, Pixmap, Rect};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

pub const MAX_RENDER_DIMENSION: u32 = 2_480;
pub const MAX_RENDER_PIXELS: u64 = 1_860 * 2_480;
pub const MAX_RENDER_BYTES: u64 = MAX_RENDER_PIXELS;

const MAX_LAYOUT_DIMENSION_POINTS: f32 = 4_096.0;
const MIN_LAYOUT_DIMENSION_POINTS: f32 = 1.0;
const MIN_LAYOUT_EM_POINTS: f32 = 1.0;
const MAX_LAYOUT_EM_POINTS: f32 = 512.0;

/// A rendered page: `width * height` gray pixels, row-major, 0 = black, 255 = white.
#[derive(Debug)]
pub struct GrayPage {
    pub width: u32,
    pub height: u32,
    pixels: Pixmap,
}

impl GrayPage {
    pub fn pixels(&self) -> &[u8] {
        self.pixels.samples()
    }
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

    pub fn open_path(path: impl AsRef<Path>) -> Result<Self, ReaderError> {
        let path = absolute_path(path.as_ref())?;
        reject_symlink_components(&path)?;
        let path_metadata = regular_file_metadata(&path)?;
        validate_document_extension(&path)?;

        let file = File::open(&path)
            .map_err(|error| path_error(&path, format!("cannot open for reading: {error}")))?;
        let opened_metadata = file
            .metadata()
            .map_err(|error| path_error(&path, format!("cannot inspect opened file: {error}")))?;
        if !opened_metadata.is_file() {
            return Err(path_error(&path, "opened input is not a regular file"));
        }
        let path_identity = FileIdentity::from_metadata(&path_metadata);
        let opened_identity = FileIdentity::from_metadata(&opened_metadata);
        if path_identity != opened_identity {
            return Err(path_error(&path, "input changed before it could be opened"));
        }

        let doc = open_mupdf_document(&path)?;
        reject_symlink_components(&path)?;
        let final_metadata = regular_file_metadata(&path)?;
        if FileIdentity::from_metadata(&final_metadata) != opened_identity {
            return Err(path_error(
                &path,
                "input changed while MuPDF was opening it",
            ));
        }
        drop(file);

        Ok(Self { doc })
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
        validate_render_box(box_w, box_h)?;
        let index = i32::try_from(index).map_err(|_| ReaderError("page index too large".into()))?;
        let page = self.doc.load_page(index)?;
        let plan = render_plan(page.bounds()?, box_w, box_h)?;
        let pix = page.to_pixmap(
            &Matrix::new_scale(plan.scale, plan.scale),
            &Colorspace::device_gray(),
            false,
            true,
        )?;

        validate_returned_pixmap(
            &plan,
            pix.origin(),
            pix.width(),
            pix.height(),
            pix.n(),
            pix.alpha(),
            pix.stride(),
            pix.samples().len(),
        )?;
        let (width, height) = (plan.width, plan.height);
        let pixels = pix;
        Ok(GrayPage {
            width,
            height,
            pixels,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    mtime: i64,
    #[cfg(unix)]
    mtime_nsec: i64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
}

impl FileIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
            #[cfg(unix)]
            mtime: metadata.mtime(),
            #[cfg(unix)]
            mtime_nsec: metadata.mtime_nsec(),
            #[cfg(unix)]
            ctime: metadata.ctime(),
            #[cfg(unix)]
            ctime_nsec: metadata.ctime_nsec(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RenderPlan {
    scale: f32,
    origin: (i32, i32),
    width: u32,
    height: u32,
    stride: usize,
    bytes: usize,
}

fn absolute_path(path: &Path) -> Result<PathBuf, ReaderError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(|error| ReaderError(format!("cannot resolve document path: {error}")))
    }
}

fn reject_symlink_components(path: &Path) -> Result<(), ReaderError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            path_error(
                path,
                format!(
                    "cannot inspect path component {}: {error}",
                    current.display()
                ),
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(path_error(
                path,
                format!(
                    "symlink path component is not allowed: {}",
                    current.display()
                ),
            ));
        }
    }
    Ok(())
}

fn regular_file_metadata(path: &Path) -> Result<Metadata, ReaderError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| path_error(path, format!("cannot inspect input: {error}")))?;
    if metadata.file_type().is_symlink() {
        return Err(path_error(path, "symlink input is not allowed"));
    }
    if !metadata.is_file() {
        return Err(path_error(path, "input is not a regular file"));
    }
    Ok(metadata)
}

fn validate_document_extension(path: &Path) -> Result<(), ReaderError> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| path_error(path, "input has no supported UTF-8 extension"))?;
    if ["pdf", "epub", "cbz"]
        .iter()
        .any(|supported| extension.eq_ignore_ascii_case(supported))
    {
        Ok(())
    } else {
        Err(path_error(
            path,
            format!("unsupported document extension: {extension}"),
        ))
    }
}

fn path_error(path: &Path, message: impl Into<String>) -> ReaderError {
    ReaderError(format!("{}: {}", path.display(), message.into()))
}

fn open_mupdf_document(path: &Path) -> Result<Document, ReaderError> {
    #[cfg(windows)]
    let document = path
        .to_str()
        .ok_or_else(|| path_error(path, "Windows document path is not valid UTF-8"))
        .and_then(|path_text| {
            Document::open(path_text).map_err(|error| {
                path_error(path, format!("MuPDF could not open document: {error}"))
            })
        });
    #[cfg(not(windows))]
    let document = Document::open(path)
        .map_err(|error| path_error(path, format!("MuPDF could not open document: {error}")));
    document
}

fn validate_render_box(width: u32, height: u32) -> Result<(), ReaderError> {
    if width == 0 || height == 0 {
        return Err(ReaderError("empty render box".into()));
    }
    if width > MAX_RENDER_DIMENSION || height > MAX_RENDER_DIMENSION {
        return Err(ReaderError(format!(
            "render dimension exceeds {MAX_RENDER_DIMENSION} pixels: {width}x{height}"
        )));
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| ReaderError("render box dimensions overflow".into()))?;
    if pixels > MAX_RENDER_PIXELS {
        return Err(ReaderError(format!(
            "render box exceeds {MAX_RENDER_PIXELS} gray pixels: {width}x{height}"
        )));
    }
    Ok(())
}

fn render_plan(bounds: Rect, box_w: u32, box_h: u32) -> Result<RenderPlan, ReaderError> {
    validate_render_box(box_w, box_h)?;
    if ![bounds.x0, bounds.y0, bounds.x1, bounds.y1]
        .into_iter()
        .all(f32::is_finite)
        || bounds.x0 >= bounds.x1
        || bounds.y0 >= bounds.y1
    {
        return Err(ReaderError("page has invalid bounds".into()));
    }
    let (page_w, page_h) = (bounds.x1 - bounds.x0, bounds.y1 - bounds.y0);
    if !(page_w.is_finite() && page_h.is_finite() && page_w > 0.0 && page_h > 0.0) {
        return Err(ReaderError("page has an invalid size".into()));
    }
    let mut scale = (box_w as f32 / page_w).min(box_h as f32 / page_h);
    if !(scale.is_finite() && scale > 0.0) {
        return Err(ReaderError("page has an invalid scale".into()));
    }

    for _ in 0..8 {
        let (origin, width, height) = rounded_pixmap_geometry(bounds, scale)?;
        if width <= box_w && height <= box_h {
            return finish_render_plan(scale, origin, width, height);
        }
        let width_factor = f64::from(box_w) / f64::from(width);
        let height_factor = f64::from(box_h) / f64::from(height);
        let mut next = (f64::from(scale) * width_factor.min(height_factor)) as f32;
        if next >= scale {
            next = f32::from_bits(scale.to_bits().saturating_sub(1));
        }
        if !(next.is_finite() && next > 0.0) {
            return Err(ReaderError("rounded page cannot fit the render box".into()));
        }
        scale = next;
    }
    Err(ReaderError(
        "rounded page geometry did not converge within the render box".into(),
    ))
}

fn rounded_pixmap_geometry(
    bounds: Rect,
    scale: f32,
) -> Result<((i32, i32), u32, u32), ReaderError> {
    let transformed = bounds.transform(&Matrix::new_scale(scale, scale));
    if ![
        transformed.x0,
        transformed.y0,
        transformed.x1,
        transformed.y1,
    ]
    .into_iter()
    .all(f32::is_finite)
    {
        return Err(ReaderError("transformed page bounds are invalid".into()));
    }
    let rounded = transformed.round();
    let width = i64::from(rounded.x1) - i64::from(rounded.x0);
    let height = i64::from(rounded.y1) - i64::from(rounded.y0);
    if width <= 0 || height <= 0 {
        return Err(ReaderError("transformed page has an empty pixmap".into()));
    }
    let width = u32::try_from(width)
        .map_err(|_| ReaderError("transformed pixmap width cannot be represented".into()))?;
    let height = u32::try_from(height)
        .map_err(|_| ReaderError("transformed pixmap height cannot be represented".into()))?;
    Ok(((rounded.x0, rounded.y0), width, height))
}

fn finish_render_plan(
    scale: f32,
    origin: (i32, i32),
    width: u32,
    height: u32,
) -> Result<RenderPlan, ReaderError> {
    if width > MAX_RENDER_DIMENSION || height > MAX_RENDER_DIMENSION {
        return Err(ReaderError(format!(
            "rounded pixmap exceeds {MAX_RENDER_DIMENSION} pixels per dimension: {width}x{height}"
        )));
    }
    let stride = usize::try_from(width)
        .map_err(|_| ReaderError("pixmap stride cannot be represented".into()))?;
    let rows = usize::try_from(height)
        .map_err(|_| ReaderError("pixmap height cannot be represented".into()))?;
    let bytes = stride
        .checked_mul(rows)
        .ok_or_else(|| ReaderError("pixmap byte count overflows".into()))?;
    let bytes_u64 = u64::try_from(bytes)
        .map_err(|_| ReaderError("pixmap byte count cannot be represented".into()))?;
    if bytes_u64 > MAX_RENDER_BYTES {
        return Err(ReaderError(format!(
            "rounded pixmap exceeds the {MAX_RENDER_BYTES}-byte output budget: {width}x{height}"
        )));
    }
    Ok(RenderPlan {
        scale,
        origin,
        width,
        height,
        stride,
        bytes,
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_returned_pixmap(
    plan: &RenderPlan,
    origin: (i32, i32),
    width: u32,
    height: u32,
    components: u8,
    alpha: bool,
    stride: isize,
    sample_len: usize,
) -> Result<(), ReaderError> {
    if alpha {
        return Err(ReaderError("gray pixmap unexpectedly has alpha".into()));
    }
    if components != 1 {
        return Err(ReaderError(format!(
            "gray pixmap has {components} components"
        )));
    }
    if origin != plan.origin || width != plan.width || height != plan.height {
        return Err(ReaderError(format!(
            "MuPDF returned unexpected pixmap geometry: origin={origin:?} size={width}x{height}, expected origin={:?} size={}x{}",
            plan.origin, plan.width, plan.height
        )));
    }
    let stride = usize::try_from(stride)
        .map_err(|_| ReaderError("pixmap stride cannot be represented".into()))?;
    if stride != plan.stride {
        return Err(ReaderError(format!(
            "MuPDF returned unexpected pixmap stride: {stride}, expected {}",
            plan.stride
        )));
    }
    if sample_len != plan.bytes {
        return Err(ReaderError(format!(
            "MuPDF returned unexpected sample length: {sample_len}, expected {}",
            plan.bytes
        )));
    }
    Ok(())
}

fn validate_layout(width: f32, height: f32, em: f32) -> Result<(), ReaderError> {
    if [width, height].into_iter().all(|value| {
        value.is_finite()
            && (MIN_LAYOUT_DIMENSION_POINTS..=MAX_LAYOUT_DIMENSION_POINTS).contains(&value)
    }) && em.is_finite()
        && (MIN_LAYOUT_EM_POINTS..=MAX_LAYOUT_EM_POINTS).contains(&em)
    {
        Ok(())
    } else {
        Err(ReaderError(format!(
            "layout requires width and height in {MIN_LAYOUT_DIMENSION_POINTS}..={MAX_LAYOUT_DIMENSION_POINTS} points and em in {MIN_LAYOUT_EM_POINTS}..={MAX_LAYOUT_EM_POINTS} points"
        )))
    }
}

fn page_count_from_mupdf(count: i32) -> Result<u32, ReaderError> {
    u32::try_from(count).map_err(|_| ReaderError(format!("invalid page count: {count}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDocumentDir(PathBuf);

    impl TempDocumentDir {
        fn new(label: &str) -> Self {
            let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ignite-reader-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for TempDocumentDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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

    fn image_pdf(width: u32, height: u32) -> Vec<u8> {
        let sample_count = usize::try_from(u64::from(width) * u64::from(height)).unwrap();
        let samples = vec![0x80; sample_count];
        let content = format!("q {width} 0 0 {height} 0 0 cm /Im0 Do Q");
        let objects = [
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] /Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>"
            )
            .into_bytes(),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            )
            .into_bytes(),
            {
                let mut image = format!(
                    "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceGray /BitsPerComponent 8 /Length {} >>\nstream\n",
                    samples.len()
                )
                .into_bytes();
                image.extend_from_slice(&samples);
                image.extend_from_slice(b"\nendstream");
                image
            },
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
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

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }

    fn push_u16(bytes: &mut Vec<u8>, value: u16) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut output = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let name = name.as_bytes();
            let offset = u32::try_from(output.len()).unwrap();
            let size = u32::try_from(data.len()).unwrap();
            let name_len = u16::try_from(name.len()).unwrap();
            let crc = crc32(data);

            push_u32(&mut output, 0x0403_4b50);
            push_u16(&mut output, 20);
            push_u16(&mut output, 0);
            push_u16(&mut output, 0);
            push_u16(&mut output, 0);
            push_u16(&mut output, 0);
            push_u32(&mut output, crc);
            push_u32(&mut output, size);
            push_u32(&mut output, size);
            push_u16(&mut output, name_len);
            push_u16(&mut output, 0);
            output.extend_from_slice(name);
            output.extend_from_slice(data);

            push_u32(&mut central, 0x0201_4b50);
            push_u16(&mut central, 20);
            push_u16(&mut central, 20);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u32(&mut central, crc);
            push_u32(&mut central, size);
            push_u32(&mut central, size);
            push_u16(&mut central, name_len);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u16(&mut central, 0);
            push_u32(&mut central, 0);
            push_u32(&mut central, offset);
            central.extend_from_slice(name);
        }

        let central_offset = u32::try_from(output.len()).unwrap();
        let central_size = u32::try_from(central.len()).unwrap();
        output.extend_from_slice(&central);
        push_u32(&mut output, 0x0605_4b50);
        push_u16(&mut output, 0);
        push_u16(&mut output, 0);
        push_u16(&mut output, u16::try_from(entries.len()).unwrap());
        push_u16(&mut output, u16::try_from(entries.len()).unwrap());
        push_u32(&mut output, central_size);
        push_u32(&mut output, central_offset);
        push_u16(&mut output, 0);
        output
    }

    fn epub_fixture() -> Vec<u8> {
        let container = br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;
        let package = br#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="bookid">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="bookid">ignite-test</dc:identifier><dc:title>Ignite Test</dc:title><dc:language>en</dc:language></metadata>
<manifest><item id="page" href="page.xhtml" media-type="application/xhtml+xml"/></manifest>
<spine><itemref idref="page"/></spine>
</package>"#;
        let page = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Test</title></head><body><p>File-backed EPUB test.</p></body></html>"#;
        stored_zip(&[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", container),
            ("OEBPS/content.opf", package),
            ("OEBPS/page.xhtml", page),
        ])
    }

    fn cbz_fixture() -> Vec<u8> {
        stored_zip(&[("page.pgm", b"P5\n2 2\n255\n\x00\xff\x80\x40")])
    }

    fn pixel(p: &GrayPage, x: u32, y: u32) -> u8 {
        p.pixels()[(y * p.width + x) as usize]
    }

    #[test]
    fn pdf_page_renders_to_fit_the_box_in_gray() {
        let book = Book::open_bytes(&rectangle_pdf(), "application/pdf").expect("open pdf");
        assert!(!book.is_reflowable().unwrap());
        assert_eq!(book.page_count().unwrap(), 1);

        // 200 x 100 pt into 400 x 400 px: width-limited, zoom 2 → 400 x 200.
        let page = book.render_page_gray(0, 400, 400).expect("render");
        assert_eq!((page.width, page.height), (400, 200));
        assert_eq!(page.pixels().len(), 400 * 200);
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
        assert!(book
            .render_page_gray(0, MAX_RENDER_DIMENSION + 1, 1)
            .is_err());
        assert!(book.render_page_gray(0, 2_480, 2_480).is_err());
        assert!(book.render_page_gray(5, 100, 100).is_err());
    }

    #[test]
    fn layout_rejects_nonfinite_and_nonpositive_dimensions() {
        let mut book = Book::open_bytes(&long_html(), "text/html").unwrap();
        assert!(book.layout(f32::NAN, 100.0, 10.0).is_err());
        assert!(book.layout(100.0, f32::INFINITY, 10.0).is_err());
        assert!(book.layout(100.0, 100.0, 0.0).is_err());
        assert!(book.layout(0.5, 100.0, 10.0).is_err());
        assert!(book.layout(4_097.0, 100.0, 10.0).is_err());
        assert!(book.layout(100.0, 100.0, 513.0).is_err());
    }

    #[test]
    fn invalid_page_counts_are_errors() {
        assert_eq!(page_count_from_mupdf(3).unwrap(), 3);
        assert!(page_count_from_mupdf(-1).is_err());
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
            page.pixels().iter().any(|&v| v < 128),
            "text should leave dark pixels on the page"
        );
    }

    #[test]
    fn file_backed_pdf_epub_and_cbz_open_and_render() {
        let dir = TempDocumentDir::new("formats");
        let pdf_path = dir.write("fixture.PDF", &rectangle_pdf());
        let epub_path = dir.write("fixture.epub", &epub_fixture());
        let cbz_path = dir.write("fixture.cbz", &cbz_fixture());

        let pdf = Book::open_path(&pdf_path).expect("open file-backed PDF");
        assert_eq!(pdf.page_count().unwrap(), 1);
        assert_eq!(
            pdf.render_page_gray(0, 400, 400).unwrap().pixels().len(),
            80_000
        );

        let mut epub = Book::open_path(&epub_path).expect("open file-backed EPUB");
        assert!(epub.is_reflowable().unwrap());
        epub.layout(418.0, 528.0, 12.0).unwrap();
        assert!(epub.page_count().unwrap() >= 1);
        assert!(!epub
            .render_page_gray(0, 400, 500)
            .unwrap()
            .pixels()
            .is_empty());

        let cbz = Book::open_path(&cbz_path).expect("open file-backed CBZ");
        assert!(!cbz.is_reflowable().unwrap());
        assert_eq!(cbz.page_count().unwrap(), 1);
        assert!(!cbz
            .render_page_gray(0, 400, 400)
            .unwrap()
            .pixels()
            .is_empty());
    }

    #[test]
    fn image_heavy_pdf_renders_within_output_budget() {
        let dir = TempDocumentDir::new("image-heavy");
        let path = dir.write("image-heavy.pdf", &image_pdf(2_048, 2_048));
        let book = Book::open_path(&path).expect("open image-heavy PDF");
        let page = book
            .render_page_gray(0, 1_860, 1_860)
            .expect("render image-heavy PDF");
        assert_eq!((page.width, page.height), (1_860, 1_860));
        assert_eq!(page.pixels().len(), 1_860 * 1_860);
        assert!(page.pixels().len() as u64 <= MAX_RENDER_BYTES);
    }

    #[test]
    fn file_backed_open_rejects_invalid_inputs() {
        let dir = TempDocumentDir::new("invalid-paths");
        let unsupported = dir.write("fixture.txt", &rectangle_pdf());
        let corrupt = dir.write("corrupt.pdf", b"%PDF-1.4\ntruncated");
        let missing = dir.0.join("missing.pdf");

        assert!(Book::open_path(&missing)
            .err()
            .unwrap()
            .0
            .contains("cannot inspect"));
        assert!(Book::open_path(&dir.0)
            .err()
            .unwrap()
            .0
            .contains("not a regular file"));
        assert!(Book::open_path(&unsupported)
            .err()
            .unwrap()
            .0
            .contains("unsupported document extension"));
        assert!(Book::open_path(&corrupt)
            .err()
            .unwrap()
            .0
            .contains("MuPDF could not open document"));
    }

    #[test]
    fn file_identity_detects_replacement() {
        let dir = TempDocumentDir::new("replacement");
        let path = dir.write("fixture.pdf", &rectangle_pdf());
        let before = FileIdentity::from_metadata(&fs::metadata(&path).unwrap());
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();
        let after = FileIdentity::from_metadata(&fs::metadata(&path).unwrap());
        assert_ne!(before, after);
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_open_rejects_symlink_components() {
        use std::os::unix::fs::symlink;

        let dir = TempDocumentDir::new("symlink");
        let target = dir.write("target.pdf", &rectangle_pdf());
        let direct = dir.0.join("direct.pdf");
        symlink(&target, &direct).unwrap();
        assert!(Book::open_path(&direct)
            .err()
            .unwrap()
            .0
            .contains("symlink"));

        let real_parent = dir.0.join("real");
        fs::create_dir(&real_parent).unwrap();
        let nested = real_parent.join("nested.pdf");
        fs::write(&nested, rectangle_pdf()).unwrap();
        let linked_parent = dir.0.join("linked");
        symlink(&real_parent, &linked_parent).unwrap();
        assert!(Book::open_path(linked_parent.join("nested.pdf"))
            .err()
            .unwrap()
            .0
            .contains("symlink"));
    }

    #[test]
    fn render_plan_bounds_rounding_dimensions_and_memory() {
        let full = render_plan(Rect::new(0.0, 0.0, 1_860.0, 2_480.0), 1_860, 2_480).unwrap();
        assert_eq!((full.width, full.height), (1_860, 2_480));
        assert_eq!(full.bytes as u64, MAX_RENDER_BYTES);

        let fractional =
            render_plan(Rect::new(0.25, 0.25, 200.25, 100.25), 400, 400).expect("rounded plan");
        assert_eq!(
            fractional.bytes,
            fractional.stride * fractional.height as usize
        );
        assert!(fractional.bytes as u64 <= MAX_RENDER_BYTES);
        assert!(fractional.width <= 400);
        assert!(fractional.height <= 400);

        assert!(render_plan(Rect::new(0.0, 0.0, 100.0, 100.0), 2_480, 2_480).is_err());
        assert!(render_plan(Rect::new(f32::NAN, 0.0, 100.0, 100.0), 100, 100).is_err());
        assert!(render_plan(Rect::new(-f32::MAX, 0.0, f32::MAX, 100.0), 100, 100).is_err());
    }

    #[test]
    fn returned_pixmap_geometry_stride_and_buffer_are_checked() {
        let plan = render_plan(Rect::new(0.0, 0.0, 200.0, 100.0), 400, 400).unwrap();
        assert!(validate_returned_pixmap(
            &plan,
            plan.origin,
            plan.width,
            plan.height,
            1,
            false,
            plan.stride as isize,
            plan.bytes
        )
        .is_ok());
        assert!(validate_returned_pixmap(
            &plan,
            (plan.origin.0 + 1, plan.origin.1),
            plan.width,
            plan.height,
            1,
            false,
            plan.stride as isize,
            plan.bytes
        )
        .is_err());
        assert!(validate_returned_pixmap(
            &plan,
            plan.origin,
            plan.width,
            plan.height,
            1,
            false,
            -1,
            plan.bytes
        )
        .is_err());
        assert!(validate_returned_pixmap(
            &plan,
            plan.origin,
            plan.width + 1,
            plan.height,
            1,
            false,
            plan.stride as isize,
            plan.bytes
        )
        .is_err());
        assert!(validate_returned_pixmap(
            &plan,
            plan.origin,
            plan.width,
            plan.height,
            2,
            false,
            plan.stride as isize,
            plan.bytes
        )
        .is_err());
        assert!(validate_returned_pixmap(
            &plan,
            plan.origin,
            plan.width,
            plan.height,
            1,
            true,
            plan.stride as isize,
            plan.bytes
        )
        .is_err());
        assert!(validate_returned_pixmap(
            &plan,
            plan.origin,
            plan.width,
            plan.height,
            1,
            false,
            plan.stride as isize + 1,
            plan.bytes
        )
        .is_err());
        assert!(validate_returned_pixmap(
            &plan,
            plan.origin,
            plan.width,
            plan.height,
            1,
            false,
            plan.stride as isize,
            plan.bytes - 1
        )
        .is_err());
    }

    #[test]
    fn repeated_file_open_render_and_close_cycles_recover() {
        let dir = TempDocumentDir::new("cycles");
        let path = dir.write("fixture.pdf", &rectangle_pdf());
        for _ in 0..16 {
            let book = Book::open_path(&path).unwrap();
            let page = book.render_page_gray(0, 400, 400).unwrap();
            assert_eq!(
                (page.width, page.height, page.pixels().len()),
                (400, 200, 80_000)
            );
        }
    }

    #[test]
    fn garbage_is_an_error_not_a_crash() {
        assert!(Book::open_bytes(b"not a document", "application/pdf").is_err());
    }
}
