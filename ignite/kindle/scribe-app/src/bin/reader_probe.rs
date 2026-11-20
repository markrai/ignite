use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use scribe_app::reader_engine::Book;

const DEVICE_CORPUS_ROOT: &str = "/mnt/us/ignite-reader-qa/corpus";
const MAX_CYCLES: u32 = 1_000;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const LAYOUT_WIDTH: f32 = 418.0;
const LAYOUT_HEIGHT: f32 = 528.0;
const LAYOUT_EM: f32 = 12.0;

struct Options {
    corpus: PathBuf,
    file: String,
    cycles: u32,
    page: u32,
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
    let mut page = 0;
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
            "--page" => page = parse_number(&value, "page", 0, 1_000_000)?,
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
    if u64::from(width) * u64::from(height) > scribe_app::reader_engine::MAX_RENDER_PIXELS {
        return Err("requested render box exceeds reader pixel budget".into());
    }

    Ok(Options {
        corpus,
        file,
        cycles,
        page,
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

    for cycle in 1..=options.cycles {
        let begin = Instant::now();
        let mut book = Book::open_path(&path).map_err(|e| format!("open: {e}"))?;
        if book.is_reflowable().map_err(|e| format!("reflow: {e}"))? {
            book.layout(LAYOUT_WIDTH, LAYOUT_HEIGHT, LAYOUT_EM)
                .map_err(|e| format!("layout: {e}"))?;
        }
        let pages = book.page_count().map_err(|e| format!("pages: {e}"))?;
        if options.page >= pages {
            return Err(format!(
                "page index {} is outside {pages} pages",
                options.page
            ));
        }
        let open_ms = begin.elapsed().as_millis();
        let render_start = Instant::now();
        let page = book
            .render_page_gray(options.page, options.width, options.height)
            .map_err(|e| format!("render: {e}"))?;
        let width = page.width;
        let height = page.height;
        let checksum = page
            .pixels()
            .iter()
            .fold(0xcbf29ce484222325_u64, |hash, b| {
                (hash ^ u64::from(*b)).wrapping_mul(0x100000001b3)
            });
        let render_ms = render_start.elapsed().as_millis();
        drop(page);
        drop(book);
        let rss = memory_kib("VmRSS:")?;
        let hwm = memory_kib("VmHWM:")?;
        println!(
            "reader_probe_cycle={cycle} pages={pages} width={width} height={height} open_ms={open_ms} render_ms={render_ms} elapsed_ms={} rss_kib={rss} hwm_kib={hwm} checksum={checksum:016x}",
            begin.elapsed().as_millis()
        );
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
    use super::{parse_options, safe_filename, MAX_CYCLES};

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
    fn refuses_unsupported_options_and_missing_file() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--file", "../bad.pdf"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--out", "/mnt/us/scribe-data"]).is_err());
        assert!(parse(&["--file", "a.pdf", "--file", "b.pdf"]).is_err());
    }
}
