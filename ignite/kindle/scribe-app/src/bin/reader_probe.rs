use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use scribe_app::reader_engine::Book;

const DEVICE_CORPUS_ROOT: &str = "/mnt/us/ignite-reader-qa/corpus";
const MAX_CYCLES: u32 = 1_000;
const MAX_SEQUENCE_PAGES: usize = 16;
const MAX_TOTAL_RENDERS: u32 = 1_000;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const LAYOUT_WIDTH: f32 = 418.0;
const LAYOUT_HEIGHT: f32 = 528.0;
const LAYOUT_EM: f32 = 12.0;

struct Options {
    corpus: PathBuf,
    file: String,
    cycles: u32,
    pages: Vec<u32>,
    width: u32,
    height: u32,
}

fn parse_number(value: &str, label: &str, minimum: u32, maximum: u32) -> Result<u32, String> {
    let number = value
        .parse::<u32>()
        .map_err(|_| format!("{label} must be an integer"))?;
    if !(minimum..=maximum).contains(&number) {
        return Err(format!("{label} must be between {minimum} and {maximum}"));
    }
    Ok(number)
}

fn parse_page_sequence(value: &str) -> Result<Vec<u32>, String> {
    if value.is_empty() || value.len() > 128 {
        return Err("page sequence must be between 1 and 128 characters".into());
    }
    let pages: Vec<_> = value.split(',').collect();
    if pages.len() > MAX_SEQUENCE_PAGES {
        return Err(format!(
            "page sequence cannot exceed {MAX_SEQUENCE_PAGES} entries"
        ));
    }
    pages
        .into_iter()
        .map(|page| parse_number(page, "page", 0, 1_000_000))
        .collect()
}

fn gray_checksum(pixels: &[u8]) -> u64 {
    pixels.iter().fold(0xcbf29ce484222325_u64, |hash, b| {
        (hash ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

fn safe_filename(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && matches!(
            Path::new(name).extension().and_then(|ext| ext.to_str()),
            Some("pdf" | "epub" | "cbz")
        )
}

fn parse_options(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut corpus = PathBuf::from(DEVICE_CORPUS_ROOT);
    let mut file = None;
    let mut cycles = 1;
    let mut page = None;
    let mut pages = None;
    let mut width = 1_000;
    let mut height = 1_400;
    let mut args = args.into_iter();

    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--file" if file.is_none() => file = Some(value),
            "--cycles" => cycles = parse_number(&value, "cycles", 1, MAX_CYCLES)?,
            "--page" if page.is_none() && pages.is_none() => {
                page = Some(parse_number(&value, "page", 0, 1_000_000)?)
            }
            "--pages" if page.is_none() && pages.is_none() => {
                pages = Some(parse_page_sequence(&value)?)
            }
            "--width" => width = parse_number(&value, "width", 1, 2_480)?,
            "--height" => height = parse_number(&value, "height", 1, 2_480)?,
            "--corpus" if !cfg!(target_arch = "arm") => corpus = PathBuf::from(value),
            _ => return Err(format!("unsupported or repeated option: {flag}")),
        }
    }

    let file = file.ok_or("required option: --file")?;
    if !safe_filename(&file) {
        return Err("file must be a simple lowercase .pdf, .epub or .cbz fixture name".into());
    }
    let pages = pages.unwrap_or_else(|| vec![page.unwrap_or(0)]);
    if cycles > MAX_TOTAL_RENDERS / u32::try_from(pages.len()).unwrap() {
        return Err(format!(
            "total rendered pages cannot exceed {MAX_TOTAL_RENDERS}"
        ));
    }
    if u64::from(width) * u64::from(height) > scribe_app::reader_engine::MAX_RENDER_PIXELS {
        return Err("requested render box exceeds reader pixel budget".into());
    }

    Ok(Options {
        corpus,
        file,
        cycles,
        pages,
        width,
        height,
    })
}

fn fixture_path(options: &Options) -> Result<PathBuf, String> {
    let root = options
        .corpus
        .canonicalize()
        .map_err(|e| format!("cannot resolve corpus: {e}"))?;
    if !root.is_dir() {
        return Err("corpus must be a directory".into());
    }
    if cfg!(target_arch = "arm") && root != Path::new(DEVICE_CORPUS_ROOT) {
        return Err(
            "ARM probe requires the dedicated /mnt/us/ignite-reader-qa/corpus directory".into(),
        );
    }
    let path = root.join(&options.file);
    let metadata =
        fs::symlink_metadata(&path).map_err(|e| format!("cannot inspect fixture: {e}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("fixture must be a regular file, not a symlink".into());
    }
    if metadata.len() == 0 || metadata.len() > MAX_FILE_BYTES {
        return Err("fixture must be between 1 byte and 64 MiB".into());
    }
    Ok(path)
}

fn memory_kib(key: &str) -> Result<u64, String> {
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|e| format!("cannot inspect process memory: {e}"))?;
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|tail| tail.split_whitespace().next())
        .ok_or_else(|| format!("missing {key} from /proc/self/status"))?;
    value.parse::<u64>().map_err(|e| e.to_string())
}

fn run(options: Options) -> Result<(), String> {
    let path = fixture_path(&options)?;
    let start_rss = memory_kib("VmRSS:")?;
    let start_hwm = memory_kib("VmHWM:")?;
    println!("reader_probe_schema=1");
    println!("reader_probe_fixture={}", options.file);
    println!("reader_probe_cycles={}", options.cycles);
    println!("reader_probe_start_rss_kib={start_rss}");
    println!("reader_probe_start_hwm_kib={start_hwm}");
    println!("reader_probe_mode=read_only");
    println!("reader_probe_device_runtime=unqualified");

    if options.pages.len() > 1 {
        let sequence = options
            .pages
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        println!("reader_probe_sequence={sequence}");
    }

    for cycle in 1..=options.cycles {
        let begin = Instant::now();
        let mut book = Book::open_path(&path).map_err(|e| format!("open: {e}"))?;
        if book.is_reflowable().map_err(|e| format!("reflow: {e}"))? {
            book.layout(LAYOUT_WIDTH, LAYOUT_HEIGHT, LAYOUT_EM)
                .map_err(|e| format!("layout: {e}"))?;
        }
        let pages = book.page_count().map_err(|e| format!("pages: {e}"))?;
        for &page_index in &options.pages {
            if page_index >= pages {
                return Err(format!("page index {page_index} is outside {pages} pages"));
            }
        }
        let open_ms = begin.elapsed().as_millis();
        let mut total_render_ms = 0;
        let mut last_width = 0;
        let mut last_height = 0;
        let mut last_checksum = 0;

        for &page_index in &options.pages {
            let render_start = Instant::now();
            let page = book
                .render_page_gray(page_index, options.width, options.height)
                .map_err(|e| format!("render page {page_index}: {e}"))?;
            last_width = page.width;
            last_height = page.height;
            last_checksum = gray_checksum(page.pixels());
            let render_ms = render_start.elapsed().as_millis();
            total_render_ms += render_ms;
            drop(page);

            if options.pages.len() > 1 {
                let rss = memory_kib("VmRSS:")?;
                let hwm = memory_kib("VmHWM:")?;
                println!(
                    "reader_probe_page_cycle={cycle} page={page_index} pages={pages} width={last_width} height={last_height} render_ms={render_ms} rss_kib={rss} hwm_kib={hwm} checksum={last_checksum:016x}"
                );
            }
        }

        drop(book);
        let rss = memory_kib("VmRSS:")?;
        let hwm = memory_kib("VmHWM:")?;
        if options.pages.len() == 1 {
            println!(
                "reader_probe_cycle={cycle} pages={pages} width={last_width} height={last_height} open_ms={open_ms} render_ms={total_render_ms} elapsed_ms={} rss_kib={rss} hwm_kib={hwm} checksum={last_checksum:016x}",
                begin.elapsed().as_millis()
            );
        } else {
            println!(
                "reader_probe_cycle={cycle} pages={pages} rendered_pages={} width={last_width} height={last_height} open_ms={open_ms} render_ms={total_render_ms} elapsed_ms={} rss_kib={rss} hwm_kib={hwm} checksum={last_checksum:016x}",
                options.pages.len(),
                begin.elapsed().as_millis()
            );
        }
    }

    println!("reader_probe_final_rss_kib={}", memory_kib("VmRSS:")?);
    println!("reader_probe_peak_hwm_kib={}", memory_kib("VmHWM:")?);
    println!("reader_probe_result=pass");
    Ok(())
}

fn main() -> ExitCode {
    match parse_options(std::env::args().skip(1)).and_then(run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("reader_probe_result=fail");
            eprintln!("reader_probe_error={error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{gray_checksum, parse_options, run, safe_filename, MAX_CYCLES};

    fn parse(args: &[&str]) -> Result<super::Options, String> {
        parse_options(args.iter().map(|value| (*value).to_string()))
    }

    #[test]
    fn accepts_only_simple_supported_fixture_names() {
        assert!(safe_filename("normal.pdf"));
        assert!(safe_filename("image_heavy.cbz"));
        assert!(safe_filename("reflow.epub"));
        for name in [
            "../notebooks.pdf",
            "/mnt/us/scribe-data/a.pdf",
            ".hidden.pdf",
            "other.txt",
            "file.PDF",
            "folder\\file.pdf",
            "has space.pdf",
        ] {
            assert!(!safe_filename(name), "{name}");
        }
    }

    #[test]
    fn bounds_cycles_page_and_render_box() {
        assert!(parse(&["--file", "a.pdf", "--cycles", "0"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--cycles", "1001"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--page", "-1"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--width", "2480", "--height", "2480"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--cycles", "1000"]).is_ok());
        assert_eq!(MAX_CYCLES, 1_000);
    }

    #[test]
    fn accepts_bounded_navigation_and_preserves_single_page_compatibility() {
        assert_eq!(parse(&["--file", "a.pdf"]).unwrap().pages, vec![0]);
        assert_eq!(
            parse(&["--file", "a.pdf", "--page", "2"]).unwrap().pages,
            vec![2]
        );
        assert_eq!(
            parse(&["--file", "a.pdf", "--pages", "0,1,2,1,0"])
                .unwrap()
                .pages,
            vec![0, 1, 2, 1, 0]
        );
        assert!(parse(&["--file", "a.pdf", "--pages", ""]).is_err());
        assert!(parse(&["--file", "a.pdf", "--pages", "0,,1"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--pages", "0,"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--pages", "0,1000001"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--pages", "0,1", "--page", "0"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--page", "0", "--pages", "0,1"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--page", "0", "--page", "1"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--pages", "0,1", "--pages", "1,0"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--pages", "0,1", "--cycles", "501"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--pages", "0,1", "--cycles", "500"]).is_ok());
        assert!(parse(&["--file", "a.pdf", "--pages", "0,1", "--cycles", "1000"]).is_err());
        let too_many = vec!["0"; 17].join(",");
        assert!(parse(&["--file", "a.pdf", "--pages", &too_many]).is_err());
    }

    fn two_page_pdf() -> Vec<u8> {
        let first = "0 g 25 25 50 50 re f";
        let second = "0 g 125 25 50 50 re f";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 5 0 R >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 6 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{first}\nendstream", first.len()),
            format!(
                "<< /Length {} >>\nstream\n{second}\nendstream",
                second.len()
            ),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
        }
        let startxref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{startxref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn navigation_revisits_identical_page_and_reopens_between_cycles() {
        use scribe_app::reader_engine::Book;
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        struct Corpus(std::path::PathBuf);
        impl Drop for Corpus {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Corpus(std::env::temp_dir().join(format!(
            "ignite-reader-probe-pages-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        std::fs::create_dir(&root.0).unwrap();
        let path = root.0.join("two.pdf");
        std::fs::write(&path, two_page_pdf()).unwrap();

        let book = Book::open_path(&path).unwrap();
        assert_eq!(book.page_count().unwrap(), 2);
        let first = gray_checksum(book.render_page_gray(0, 200, 100).unwrap().pixels());
        let second = gray_checksum(book.render_page_gray(1, 200, 100).unwrap().pixels());
        let again = gray_checksum(book.render_page_gray(0, 200, 100).unwrap().pixels());
        assert_ne!(first, second);
        assert_eq!(first, again);
        drop(book);

        let options = super::Options {
            corpus: root.0.clone(),
            file: "two.pdf".into(),
            cycles: 2,
            pages: vec![0, 1, 0],
            width: 200,
            height: 100,
        };
        run(options).unwrap();

        let missing_page = super::Options {
            corpus: root.0.clone(),
            file: "two.pdf".into(),
            cycles: 1,
            pages: vec![0, 2],
            width: 200,
            height: 100,
        };
        assert!(run(missing_page).unwrap_err().contains("outside 2 pages"));
    }

    #[test]
    fn refuses_unsupported_options_and_missing_file() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--file", "../bad.pdf"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--out", "/mnt/us/scribe-data"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--file", "b.pdf"]).is_err());
    }
}
