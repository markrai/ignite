//! Slice 19B.3B: small-durable-write benchmark for the proposed changed-page ledger.
//!
//! Measures, in a **fresh disposable subdirectory** it creates under `--dir`, the cost of:
//! 100-byte appends without/with fsync, reopen-per-record appends, first-file creation with a
//! directory fsync, today's first-mutation generation bump (tmp + fsync + rename + dir fsync),
//! and the modelled first mutation with ledger records (session + page record + bump).
//!
//! Safety rules (it never touches Ignite data or user files):
//! - On the Kindle (ARM Linux build) `--dir` must be exactly `/mnt/us/ignite-bench`, a real
//!   directory (not a symlink, no symlinked ancestor). Elsewhere any scratch directory outside an
//!   Ignite library / sync state (and outside `/mnt/us/scribe-data` / `/mnt/us/scribe-app`).
//! - `--dir` must be empty or hold only recognised earlier bench runs (`ignite-durable-bench-*`
//!   directories containing nothing but the bench's own file names); anything else is refused.
//! - It writes only inside its own new `ignite-durable-bench-*` subdirectory. Cleanup removes
//!   only the bench's own file names from that subdirectory and then the (empty) subdirectory,
//!   after re-checking (on unix) that neither it nor `--dir` was swapped; `--dir` itself is never
//!   removed. A cleanup failure is reported (`durable_bench cleanup=failed`, exit 3).
//! - Not part of the release package (`package-release.ps1` copies only scribe-app and
//!   scribe-lifecycle; `validate_stage.py` rejects anything else).
//!
//! Usage: `durable-write-bench --dir <scratch dir> [--iterations N] [--keep]`

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One ~100-byte ledger-like record (newline-terminated JSON line).
const RECORD: &[u8] =
    b"{\"v\":1,\"g\":123456,\"k\":\"page\",\"nb\":\"notebook-12\",\"page\":\"page-34\",\"pad\":\"xxxxxxxxxxxxxxxxxxxxxxxxx\"}\n";

/// The only scratch directory the Kindle build accepts.
const KINDLE_DIR: &str = "/mnt/us/ignite-bench";
const WORK_PREFIX: &str = "ignite-durable-bench-";
const FORBIDDEN_PREFIXES: &[&str] = &["/mnt/us/scribe-data", "/mnt/us/scribe-app"];
const FORBIDDEN_MARKERS: &[&str] = &[
    "library.meta",
    ".scribe-sync",
    "sync-state.json",
    "outbox.jsonl",
    "confirmed.log",
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args).and_then(|o| run(&o, required_dir())) {
        Ok(out) => {
            for l in &out.lines {
                println!("{l}");
            }
            if let Err(e) = out.cleanup {
                eprintln!("durable_bench cleanup=failed {e}");
                std::process::exit(3);
            }
        }
        Err(e) => {
            eprintln!("durable_bench error: {e}");
            std::process::exit(2);
        }
    }
}

#[derive(Debug)]
struct Opts {
    dir: PathBuf,
    iterations: usize,
    keep: bool,
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut dir = None;
    let mut iterations = 100;
    let mut keep = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dir" => dir = Some(PathBuf::from(it.next().ok_or("--dir needs a path")?)),
            "--iterations" => {
                iterations = it
                    .next()
                    .ok_or("--iterations needs a number")?
                    .parse()
                    .map_err(|_| "--iterations must be a positive integer")?;
                if iterations == 0 || iterations > 10_000 {
                    return Err("--iterations must be 1..=10000".into());
                }
            }
            "--keep" => keep = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Opts {
        dir: dir.ok_or("--dir is required")?,
        iterations,
        keep,
    })
}

/// The Kindle build pins the scratch directory; desktop builds do not.
fn required_dir() -> Option<&'static Path> {
    if cfg!(all(target_os = "linux", target_arch = "arm")) {
        Some(Path::new(KINDLE_DIR))
    } else {
        None
    }
}

/// A bench-created file name: `append.jsonl`, `gen`, `gen.tmp`, `ledger.jsonl`, `created-N.jsonl`.
fn is_bench_file(name: &str) -> bool {
    matches!(name, "append.jsonl" | "gen" | "gen.tmp" | "ledger.jsonl")
        || name
            .strip_prefix("created-")
            .and_then(|r| r.strip_suffix(".jsonl"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// An earlier run's leftover: a real `ignite-durable-bench-*` directory holding only bench files.
fn check_leftover(path: &Path) -> Result<(), String> {
    let refuse = |why: String| format!("refusing: unrecognised content {}: {why}", path.display());
    let m = fs::symlink_metadata(path).map_err(|e| refuse(e.to_string()))?;
    if !m.is_dir() {
        return Err(refuse("not a plain directory".into()));
    }
    for e in fs::read_dir(path).map_err(|e| refuse(e.to_string()))? {
        let e = e.map_err(|e| refuse(e.to_string()))?;
        let name = e.file_name().to_string_lossy().into_owned();
        let ft = e.file_type().map_err(|e| refuse(e.to_string()))?;
        if !ft.is_file() || !is_bench_file(&name) {
            return Err(refuse(format!("contains {name:?}")));
        }
    }
    Ok(())
}

/// Refuse any location that is, or is inside, Ignite data, that is not the pinned Kindle
/// directory (when `required`), that is a symlink or reached through one, or that holds anything
/// but recognised bench leftovers.
fn check_location(dir: &Path, required: Option<&Path>) -> Result<(), String> {
    let lmeta = fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if lmeta.file_type().is_symlink() {
        return Err(format!("refusing {}: is a symlink", dir.display()));
    }
    if !lmeta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let canon = fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if let Some(req) = required {
        // Exactly the approved path, spelled as approved, and resolving to itself (no symlinked
        // ancestor, no `..`, no redirect).
        if dir != req || canon != req {
            return Err(format!(
                "refusing {} (resolves to {}): only {} is allowed on this device",
                dir.display(),
                canon.display(),
                req.display()
            ));
        }
    }
    // Only a dedicated scratch directory: empty, or holding nothing but earlier bench runs.
    // (A directory with any other content — a backup folder, documents — is refused.)
    for e in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let name = e.map_err(|e| e.to_string())?.file_name();
        if !name.to_string_lossy().starts_with(WORK_PREFIX) {
            return Err(format!(
                "refusing {}: not an empty scratch directory (contains {:?})",
                dir.display(),
                name
            ));
        }
        check_leftover(&dir.join(&name))?;
    }
    let shown = canon.to_string_lossy().replace('\\', "/");
    for p in FORBIDDEN_PREFIXES {
        if shown == *p || shown.starts_with(&format!("{p}/")) {
            return Err(format!("refusing Ignite data location {shown}"));
        }
    }
    let mut cur = Some(canon.as_path());
    while let Some(d) = cur {
        for m in FORBIDDEN_MARKERS {
            if d.join(m).exists() {
                return Err(format!(
                    "refusing {}: {} contains {m} (Ignite library or sync state)",
                    dir.display(),
                    d.display()
                ));
            }
        }
        cur = d.parent();
    }
    Ok(())
}

/// Filesystem identity of a directory (device, inode, owner uid), used to detect a swap between
/// creation and cleanup. `None` where the platform does not expose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirId(u64, u64, u32);

fn dir_id(path: &Path) -> Result<Option<DirId>, String> {
    let m = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !m.is_dir() {
        return Err(format!("{} is not a plain directory", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Some(DirId(m.dev(), m.ino(), m.uid())))
    }
    #[cfg(not(unix))]
    {
        Ok(None)
    }
}

/// Remove only the bench's own files from `work`, then `work` itself (non-recursive). Removes
/// nothing if `work` or its parent changed identity since creation; never removes the parent.
/// Returns the number of files removed.
fn cleanup(work: &Path, parent_id: Option<DirId>, work_id: Option<DirId>) -> Result<usize, String> {
    let parent = work.parent().ok_or("work dir has no parent")?;
    if dir_id(parent)? != parent_id || dir_id(work)? != work_id {
        return Err(format!(
            "{} or its parent changed identity; nothing removed",
            work.display()
        ));
    }
    let mut removed = 0;
    let mut problems = Vec::new();
    for e in fs::read_dir(work).map_err(|e| format!("{}: {e}", work.display()))? {
        let e = e.map_err(|e| e.to_string())?;
        let name = e.file_name().to_string_lossy().into_owned();
        let is_file = e.file_type().map(|t| t.is_file()).unwrap_or(false);
        if !is_file || !is_bench_file(&name) {
            problems.push(format!("left unrecognised {name:?}"));
            continue;
        }
        match fs::remove_file(e.path()) {
            Ok(()) => removed += 1,
            Err(err) => problems.push(format!("remove {name}: {err}")),
        }
    }
    if !problems.is_empty() {
        return Err(format!("{}: {}", work.display(), problems.join("; ")));
    }
    fs::remove_dir(work).map_err(|e| format!("remove {}: {e}", work.display()))?;
    Ok(removed)
}

fn fsync_dir(dir: &Path) -> Result<bool, String> {
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| format!("fsync dir {}: {e}", dir.display()))?;
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(false) // directories cannot be fsynced here; reported as dirsync=unsupported
    }
}

struct Stats {
    name: &'static str,
    samples: Vec<Duration>,
}

impl Stats {
    fn line(mut self, dirsync: bool) -> String {
        self.samples.sort();
        let n = self.samples.len();
        let at = |q: f64| self.samples[((q * n as f64).ceil() as usize).clamp(1, n) - 1];
        let total: Duration = self.samples.iter().sum();
        let us = |d: Duration| d.as_micros();
        format!(
            "durable_bench case={} n={n} min_us={} median_us={} p90_us={} max_us={} mean_us={} dirsync={}",
            self.name,
            us(self.samples[0]),
            us(at(0.5)),
            us(at(0.9)),
            us(self.samples[n - 1]),
            us(total / n as u32),
            if dirsync { "yes" } else { "unsupported" }
        )
    }
}

fn time(mut f: impl FnMut() -> Result<(), String>) -> Result<Duration, String> {
    let t = Instant::now();
    f()?;
    Ok(t.elapsed())
}

fn open_append(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))
}

/// Today's first-mutation generation bump, reproduced on a bench file.
fn gen_bump(work: &Path, gen: u64) -> Result<(), String> {
    let tmp = work.join("gen.tmp");
    let dest = work.join("gen");
    {
        let mut f = File::create(&tmp).map_err(|e| e.to_string())?;
        writeln!(f, "{gen}").map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    #[cfg(windows)]
    if dest.exists() {
        fs::remove_file(&dest).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    fsync_dir(work)?;
    Ok(())
}

struct Outcome {
    lines: Vec<String>,
    cleanup: Result<(), String>,
}

fn run(o: &Opts, required: Option<&Path>) -> Result<Outcome, String> {
    check_location(&o.dir, required)?;
    let parent_id = dir_id(&o.dir)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let work = o
        .dir
        .join(format!("{WORK_PREFIX}{}-{nonce}", std::process::id()));
    // create_dir (not create_dir_all) fails if the name exists, so `work` is ours.
    fs::create_dir(&work).map_err(|e| format!("create {}: {e}", work.display()))?;
    let work_id = dir_id(&work)?;
    if let (Some(p), Some(w)) = (parent_id, work_id) {
        if p.2 != w.2 {
            // The scratch directory belongs to someone else: ownership is ambiguous.
            let _ = fs::remove_dir(&work);
            return Err(format!(
                "refusing {}: owned by uid {} but the bench runs as uid {}",
                o.dir.display(),
                p.2,
                w.2
            ));
        }
    }
    let result = measure(&work, o.iterations);
    let (cleanup, status) = if o.keep {
        (Ok(()), "durable_bench cleanup=kept".to_string())
    } else {
        match cleanup(&work, parent_id, work_id) {
            Ok(n) => (
                Ok(()),
                format!("durable_bench cleanup=ok removed_files={n}"),
            ),
            Err(e) => (Err(e), "durable_bench cleanup=failed".to_string()),
        }
    };
    let mut lines = vec![format!(
        "durable_bench dir={} iterations={} record_bytes={}",
        work.display(),
        o.iterations,
        RECORD.len()
    )];
    match result {
        Ok(l) => lines.extend(l),
        Err(e) => {
            // Measurement failed; still surface the cleanup outcome.
            return Err(match cleanup {
                Ok(()) => e,
                Err(c) => format!("{e}; cleanup failed: {c}"),
            });
        }
    }
    lines.push(status);
    Ok(Outcome { lines, cleanup })
}

fn measure(work: &Path, n: usize) -> Result<Vec<String>, String> {
    let dirsync = fsync_dir(work)?;
    let mut out = Vec::new();

    // 1. Append, no fsync (page-cache cost only).
    let path = work.join("append.jsonl");
    let mut f = open_append(&path)?;
    let mut s = Stats {
        name: "append_nosync",
        samples: Vec::new(),
    };
    for _ in 0..n {
        s.samples
            .push(time(|| f.write_all(RECORD).map_err(|e| e.to_string()))?);
    }
    out.push(s.line(dirsync));

    // 2. Append + fsync on a held-open file (the ledger's steady state).
    let mut s = Stats {
        name: "append_fsync",
        samples: Vec::new(),
    };
    for _ in 0..n {
        s.samples.push(time(|| {
            f.write_all(RECORD).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())
        })?);
    }
    drop(f);
    out.push(s.line(dirsync));

    // 3. Reopen + append + fsync per record (a writer that does not keep the file open).
    let mut s = Stats {
        name: "reopen_append_fsync",
        samples: Vec::new(),
    };
    for _ in 0..n {
        s.samples.push(time(|| {
            let mut f = open_append(&path)?;
            f.write_all(RECORD).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())
        })?);
    }
    out.push(s.line(dirsync));

    // 4. First-file creation + record + fsync + directory fsync (ledger creation).
    let mut s = Stats {
        name: "create_fsync_dirsync",
        samples: Vec::new(),
    };
    for i in 0..n {
        let p = work.join(format!("created-{i}.jsonl"));
        s.samples.push(time(|| {
            let mut f = File::create(&p).map_err(|e| e.to_string())?;
            f.write_all(RECORD).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
            fsync_dir(work).map(|_| ())
        })?);
    }
    out.push(s.line(dirsync));

    // 5. Today's first mutation of a session: the generation bump alone.
    let mut s = Stats {
        name: "gen_bump_today",
        samples: Vec::new(),
    };
    for i in 0..n {
        s.samples.push(time(|| gen_bump(work, i as u64 + 1))?);
    }
    out.push(s.line(dirsync));

    // 6. Proposed first mutation of a session: session record + generation bump + page record.
    let ledger = work.join("ledger.jsonl");
    let mut s = Stats {
        name: "first_mutation_with_ledger",
        samples: Vec::new(),
    };
    for i in 0..n {
        s.samples.push(time(|| {
            let mut l = open_append(&ledger)?;
            l.write_all(RECORD).map_err(|e| e.to_string())?;
            l.sync_all().map_err(|e| e.to_string())?;
            gen_bump(work, i as u64 + 1_000_000)?;
            l.write_all(RECORD).map_err(|e| e.to_string())?;
            l.sync_all().map_err(|e| e.to_string())
        })?);
    }
    out.push(s.line(dirsync));

    // 7. Proposed first write to another page later in the session: one page record.
    let mut l = open_append(&ledger)?;
    let mut s = Stats {
        name: "next_page_record",
        samples: Vec::new(),
    };
    for _ in 0..n {
        s.samples.push(time(|| {
            l.write_all(RECORD).map_err(|e| e.to_string())?;
            l.sync_all().map_err(|e| e.to_string())
        })?);
    }
    out.push(s.line(dirsync));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "durable-bench-test-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn opts(dir: &Path) -> Opts {
        Opts {
            dir: dir.to_path_buf(),
            iterations: 3,
            keep: false,
        }
    }

    #[test]
    fn runs_every_case_and_cleans_up() {
        let d = tmp("run");
        let out = run(&opts(&d), None).unwrap();
        let lines = &out.lines;
        for case in [
            "append_nosync",
            "append_fsync",
            "reopen_append_fsync",
            "create_fsync_dirsync",
            "gen_bump_today",
            "first_mutation_with_ledger",
            "next_page_record",
        ] {
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains(&format!("case={case} n=3 "))),
                "{case}: {lines:?}"
            );
        }
        assert!(out.cleanup.is_ok());
        // append.jsonl + 3 created + gen + ledger.jsonl
        assert_eq!(
            lines.last().unwrap(),
            "durable_bench cleanup=ok removed_files=6"
        );
        assert!(RECORD.len() >= 90 && RECORD.len() <= 110);
        assert_eq!(fs::read_dir(&d).unwrap().count(), 0, "work dir removed");
        assert!(d.is_dir(), "the scratch directory itself is never removed");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn refuses_ignite_library_and_sync_locations() {
        let lib = tmp("lib");
        fs::write(lib.join("library.meta"), b"{}").unwrap();
        let inside = lib.join("notebooks");
        fs::create_dir_all(&inside).unwrap();
        assert!(check_location(&inside, None)
            .unwrap_err()
            .contains("library.meta"));
        let sync = tmp("sync");
        fs::write(sync.join("sync-state.json"), b"{}").unwrap();
        assert!(check_location(&sync, None)
            .unwrap_err()
            .contains("sync-state.json"));
        assert!(check_location(&lib.join("missing"), None).is_err());
        let ok = tmp("ok");
        assert!(check_location(&ok, None).is_ok());
        // Any other content (e.g. a personal backup folder) is refused...
        let backup = tmp("backup");
        fs::create_dir_all(backup.join("notebooks")).unwrap();
        fs::write(backup.join("state-pc.sha256"), b"x").unwrap();
        assert!(check_location(&backup, None)
            .unwrap_err()
            .contains("not an empty scratch directory"));
        // ...while earlier bench leftovers are fine.
        let left = ok.join("ignite-durable-bench-1-2");
        fs::create_dir_all(&left).unwrap();
        for f in [
            "append.jsonl",
            "gen",
            "gen.tmp",
            "ledger.jsonl",
            "created-17.jsonl",
        ] {
            fs::write(left.join(f), b"x").unwrap();
        }
        assert!(check_location(&ok, None).is_ok());
        for d in [lib, sync, ok, backup] {
            let _ = fs::remove_dir_all(d);
        }
    }

    #[test]
    fn refuses_bench_named_directories_with_foreign_content() {
        for (label, foreign) in [
            ("file", "notes.txt"),
            ("near", "created-.jsonl"),
            ("sub", "nested"),
        ] {
            let d = tmp(label);
            let left = d.join("ignite-durable-bench-9-9");
            fs::create_dir_all(&left).unwrap();
            fs::write(left.join("gen"), b"1").unwrap();
            if foreign == "nested" {
                fs::create_dir(left.join(foreign)).unwrap();
            } else {
                fs::write(left.join(foreign), b"user data").unwrap();
            }
            let err = check_location(&d, None).unwrap_err();
            assert!(err.contains("unrecognised content"), "{label}: {err}");
            // A plain file with the bench prefix is not a leftover either.
            let _ = fs::remove_dir_all(&left);
            fs::write(&left, b"x").unwrap();
            assert!(check_location(&d, None)
                .unwrap_err()
                .contains("not a plain directory"));
            let _ = fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn device_rule_accepts_only_the_pinned_directory() {
        let d = tmp("pinned");
        // A different directory is refused when a pinned path is required...
        let other = tmp("other");
        let err = check_location(&other, Some(&d)).unwrap_err();
        assert!(err.contains("only"), "{err}");
        // ...as is the pinned directory spelled another way (redirect through `..`).
        let child = d.join("x");
        fs::create_dir(&child).unwrap();
        assert!(check_location(&child.join(".."), Some(&d)).is_err());
        fs::remove_dir(&child).unwrap();
        // The pinned directory itself passes when it is its own canonical form.
        let canon = fs::canonicalize(&d).unwrap();
        assert!(check_location(&canon, Some(&canon)).is_ok());
        assert!(run(&opts(&canon), Some(&other)).is_err());
        assert_eq!(fs::read_dir(&canon).unwrap().count(), 0, "nothing created");
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&other);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_scratch_directory_and_ancestor() {
        let real = tmp("real");
        let base = tmp("links");
        let link = base.join("bench-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(check_location(&link, None).unwrap_err().contains("symlink"));
        // A pinned path reached through a symlinked ancestor resolves elsewhere: refused.
        let inner = real.join("ignite-bench");
        fs::create_dir(&inner).unwrap();
        let via = link.join("ignite-bench");
        assert!(check_location(&via, Some(&via)).is_err());
        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&real);
    }

    #[test]
    fn cleanup_leaves_unknown_files_and_reports_failure() {
        let d = tmp("cleanup");
        let work = d.join("ignite-durable-bench-5-5");
        fs::create_dir(&work).unwrap();
        let pid = dir_id(&d).unwrap();
        let wid = dir_id(&work).unwrap();
        fs::write(work.join("gen"), b"1").unwrap();
        fs::write(work.join("precious.txt"), b"user").unwrap();
        let err = cleanup(&work, pid, wid).unwrap_err();
        assert!(err.contains("precious.txt"), "{err}");
        assert!(work.join("precious.txt").exists(), "unknown file kept");
        assert!(!work.join("gen").exists(), "bench file removed");
        assert!(
            work.is_dir() && d.is_dir(),
            "directories kept when not empty"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_removes_nothing_after_an_identity_swap() {
        let d = tmp("swap");
        let work = d.join("ignite-durable-bench-6-6");
        fs::create_dir(&work).unwrap();
        let pid = dir_id(&d).unwrap();
        let wid = dir_id(&work).unwrap();
        // Swap in a different directory (kept alive elsewhere, so its inode differs) holding a
        // bench-named file.
        let other = tmp("swap-other");
        fs::write(other.join("gen"), b"theirs").unwrap();
        fs::remove_dir(&work).unwrap();
        fs::rename(&other, &work).unwrap();
        assert_ne!(dir_id(&work).unwrap(), wid);
        let err = cleanup(&work, pid, wid).unwrap_err();
        assert!(err.contains("changed identity"), "{err}");
        assert!(work.join("gen").exists(), "nothing removed");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn rejects_bad_arguments() {
        let a = |v: &[&str]| parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(a(&[]).is_err(), "--dir required");
        assert!(a(&["--dir", "x", "--iterations", "0"]).is_err());
        assert!(a(&["--dir", "x", "--bogus"]).is_err());
        let o = a(&["--dir", "x", "--iterations", "5", "--keep"]).unwrap();
        assert_eq!((o.iterations, o.keep), (5, true));
    }

    #[test]
    fn bench_file_names_are_exact() {
        for ok in [
            "append.jsonl",
            "gen",
            "gen.tmp",
            "ledger.jsonl",
            "created-0.jsonl",
        ] {
            assert!(is_bench_file(ok), "{ok}");
        }
        for bad in [
            "created-.jsonl",
            "created-1a.jsonl",
            "created-1.jsonl.bak",
            "library.meta",
            "outbox.jsonl",
            "Gen",
        ] {
            assert!(!is_bench_file(bad), "{bad}");
        }
    }
}
