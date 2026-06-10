use anyhow::{Context, Result};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{self, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const COPY_BUFFER_SIZE: usize = 64 * 1024;

// How many bytes to decompress between cancellation polls.
const CANCEL_CHECK_INTERVAL_BYTES: usize = 1024 * 1024; // 1 MB

// Minimum wall-clock time between progress-bar filename updates.
const PROGRESS_MSG_INTERVAL: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Cancellation error sentinel
//
// We need to distinguish "user pressed Ctrl-C" from a genuine I/O error so
// that run() can return Ok(false) for the clean cancellation path instead of
// propagating an Err and printing "❌ Extraction failed".
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cancelled by user")
    }
}

impl std::error::Error for Cancelled {}

// ---------------------------------------------------------------------------
// Thread-safe shared memory-map wrapper
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ArcMmap(Arc<memmap2::Mmap>);

impl AsRef<[u8]> for ArcMmap {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl std::ops::Deref for ArcMmap {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Cancellable, buffered reader
//
// Wraps any Read and polls `running` every CANCEL_CHECK_INTERVAL_BYTES bytes.
// Using a fixed read buffer of COPY_BUFFER_SIZE means callers (io::copy) get
// large reads regardless of the inner reader's default chunk size.
// ---------------------------------------------------------------------------

struct CancellableReader<'a, R> {
    inner: R,
    running: &'a Arc<AtomicBool>,
    bytes_since_check: usize,
    buf: Box<[u8; COPY_BUFFER_SIZE]>,
}

impl<'a, R: Read> CancellableReader<'a, R> {
    fn new(inner: R, running: &'a Arc<AtomicBool>) -> Self {
        Self {
            inner,
            running,
            bytes_since_check: 0,
            buf: Box::new([0u8; COPY_BUFFER_SIZE]),
        }
    }
}

impl<'a, R: Read> Read for CancellableReader<'a, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        // Poll cancellation flag before each chunk.
        if self.bytes_since_check >= CANCEL_CHECK_INTERVAL_BYTES {
            if !self.running.load(Ordering::Relaxed) {
                // FIX: Use ErrorKind::Other instead of ErrorKind::Interrupted
                // so io::copy doesn't catch it and infinitely retry.
                return Err(io::Error::new(io::ErrorKind::Other, "cancelled"));
            }
            self.bytes_since_check = 0;
        }

        let want = out.len().min(COPY_BUFFER_SIZE);
        let n = self.inner.read(&mut self.buf[..want])?;
        out[..n].copy_from_slice(&self.buf[..n]);
        self.bytes_since_check += n;
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    input_file: PathBuf,
    /// Optional custom output directory.  If omitted, a folder named after the
    /// input file (without extension) is created in the current directory.
    output_dir: Option<PathBuf>,
    /// Overwrite existing output files if they already exist.
    #[arg(long, short)]
    force: bool,
    /// List archive contents without extracting anything.
    #[arg(long, short)]
    list: bool,
}

// ---------------------------------------------------------------------------
// Output staging
// ---------------------------------------------------------------------------

struct OutputLayout {
    final_game_dir: PathBuf,
    staging_dir: PathBuf,
    staging_game_dir: PathBuf,
    backup_game_dir: Option<PathBuf>,
}

/// RAII guard that removes the staging directory on drop unless disarmed.
struct StagingCleanup {
    staging_dir: PathBuf,
    armed: bool,
}

impl StagingCleanup {
    fn new(staging_dir: &Path) -> Self {
        Self {
            staging_dir: staging_dir.to_path_buf(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingCleanup {
    fn drop(&mut self) {
        if self.armed {
            if let Err(e) = fs::remove_dir_all(&self.staging_dir) {
                eprintln!(
                    "Warning: failed to clean up staging directory {}: {}",
                    self.staging_dir.display(),
                    e
                );
            }
        }
    }
}

fn prepare_output_layout(output_dir: &Path, force: bool) -> Result<OutputLayout> {
    fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create output directory {}", output_dir.display()))?;

    let final_game_dir = output_dir.join("game_data");
    ensure_output_path_available(&final_game_dir, force)?;

    let staging_dir = create_staging_dir(output_dir)?;
    let staging_game_dir = staging_dir.join("game_data");
    let backup_game_dir = force
        .then(|| staging_dir.join("previous-game_data"))
        .filter(|_| final_game_dir.exists());

    Ok(OutputLayout {
        final_game_dir,
        staging_dir,
        staging_game_dir,
        backup_game_dir,
    })
}

fn ensure_output_path_available(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        anyhow::bail!(
            "Output path already exists: {}. Use --force to overwrite.",
            path.display()
        );
    }
    Ok(())
}

fn create_staging_dir(output_dir: &Path) -> Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    for attempt in 0..100 {
        let path = output_dir.join(format!(
            ".gogextract-tmp-{}-{}-{}",
            process::id(),
            timestamp,
            attempt
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Failed to create staging directory {}", path.display())
                })
            }
        }
    }
    anyhow::bail!(
        "Failed to create a unique staging directory under {}",
        output_dir.display()
    );
}

fn finalize_output(layout: &OutputLayout) -> Result<()> {
    if let Some(backup_game_dir) = &layout.backup_game_dir {
        fs::rename(&layout.final_game_dir, backup_game_dir).with_context(|| {
            format!(
                "Failed to move existing {} out of the way",
                layout.final_game_dir.display()
            )
        })?;
    }

    if let Err(e) = fs::rename(&layout.staging_game_dir, &layout.final_game_dir) {
        if let Some(backup_game_dir) = &layout.backup_game_dir {
            fs::rename(backup_game_dir, &layout.final_game_dir).with_context(|| {
                format!(
                    "Failed to restore previous output {} after replacement failed",
                    layout.final_game_dir.display()
                )
            })?;
        }

        return Err(e).with_context(|| {
            format!(
                "Failed to move {} into place",
                layout.final_game_dir.display()
            )
        });
    }

    if let Some(backup_game_dir) = &layout.backup_game_dir {
        remove_existing_output_path(backup_game_dir).with_context(|| {
            format!(
                "Failed to remove previous output {}",
                backup_game_dir.display()
            )
        })?;
    }

    fs::remove_dir(&layout.staging_dir).with_context(|| {
        format!(
            "Failed to remove staging directory {}",
            layout.staging_dir.display()
        )
    })?;
    Ok(())
}

fn remove_existing_output_path(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

// ---------------------------------------------------------------------------
// Progress bar helpers
// ---------------------------------------------------------------------------

fn file_count_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.magenta.bold} Files    [{bar:40.magenta/purple.dim}] {pos}/{len}  {msg:.dim}",
    )
    .unwrap()
    .progress_chars("█▇▆▅▄▃▂ ")
}

fn byte_progress_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.cyan.bold}   Bytes   [{bar:40.cyan/blue.dim}] {bytes}/{total_bytes}  ({bytes_per_sec})",
    )
    .unwrap()
    .progress_chars("█▇▆▅▄▃▂ ")
}

// ---------------------------------------------------------------------------
// --list implementation
// ---------------------------------------------------------------------------

fn list_archive(mmap: &ArcMmap) -> Result<()> {
    let cursor = Cursor::new(mmap.clone());
    let mut archive = zip::ZipArchive::new(cursor).context("Failed to open ZIP archive")?;

    println!("{:<10}  {:<19}  {}", "Size", "Modified", "Name");
    println!("{}", "-".repeat(72));

    let mut total_size: u64 = 0;
    let mut file_count: usize = 0;

    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .with_context(|| format!("Failed to read ZIP entry #{i}"))?;

        let name = entry.name();
        let size = entry.size();

        // Format the last-modified time if available.
        let modified = match entry.last_modified() {
            Some(dt) => format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                dt.year(),
                dt.month(),
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second(),
            ),
            None => "unknown            ".to_owned(),
        };

        println!("{:<10}  {}  {}", size, modified, name);

        if !entry.is_dir() {
            total_size += size;
            file_count += 1;
        }
    }

    println!("{}", "-".repeat(72));
    println!("{file_count} file(s), {total_size} bytes uncompressed");

    Ok(())
}

// ---------------------------------------------------------------------------
// Main & execution
// ---------------------------------------------------------------------------

fn main() {
    match run() {
        Ok(true) => {
            println!("\n🎉 Extraction complete!");
        }
        Ok(false) => {
            println!("\n🚨 Cancelled — cleaned up.");
            std::process::exit(130);
        }
        Err(e) => {
            // Surface the root cause clearly; skip the top-level anyhow chain.
            eprintln!("\n❌ Extraction failed: {e:#}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<bool> {
    let args = Args::parse();

    // --- Open and memory-map the input file ---

    let file = File::open(&args.input_file)
        .with_context(|| format!("Failed to open {}", args.input_file.display()))?;

    if cfg!(target_pointer_width = "32") && file.metadata()?.len() > u32::MAX as u64 {
        anyhow::bail!("File is too large to memory-map on a 32-bit architecture.");
    }

    let raw_mmap = unsafe { memmap2::Mmap::map(&file).context("Failed to memory-map input file")? };
    let mmap = ArcMmap(Arc::new(raw_mmap));

    // --- --list: just enumerate and exit ---

    if args.list {
        return list_archive(&mmap).map(|()| true);
    }

    // --- Set up Ctrl-C handler ---

    let running = Arc::new(AtomicBool::new(true));
    let user_cancelled = Arc::new(AtomicBool::new(false));

    {
        let running = running.clone();
        let user_cancelled = user_cancelled.clone();
        ctrlc::set_handler(move || {
            user_cancelled.store(true, Ordering::SeqCst);
            running.store(false, Ordering::SeqCst);
        })?;
    }

    // --- Resolve output directory ---

    let resolved_output_dir = args.output_dir.unwrap_or_else(|| {
        let mut dir = PathBuf::from("./");
        dir.push(
            args.input_file
                .file_stem()
                .unwrap_or_else(|| std::ffi::OsStr::new("extracted_game_data")),
        );
        dir
    });

    // --- Count files and total uncompressed bytes for progress bars ---

    let (total_files, total_bytes) = {
        let cursor = Cursor::new(mmap.clone());
        let mut archive = zip::ZipArchive::new(cursor).context("Failed to open ZIP archive")?;
        let count = archive.len();
        let mut bytes: u64 = 0;
        for i in 0..count {
            if let Ok(entry) = archive.by_index(i) {
                if !entry.is_dir() {
                    bytes += entry.size();
                }
            }
        }
        (count, bytes)
    };

    // --- Prepare staging layout ---

    let layout = prepare_output_layout(&resolved_output_dir, args.force)?;
    let mut staging_cleanup = StagingCleanup::new(&layout.staging_dir);

    println!("Extracting to: {}\n", resolved_output_dir.display());
    println!("Press Ctrl+C to cancel.\n");

    fs::create_dir_all(&layout.staging_game_dir).with_context(|| {
        format!(
            "Failed to create staging game-data directory {}",
            layout.staging_game_dir.display()
        )
    })?;

    // --- Progress bars (two bars: file count + bytes) ---

    let multi = MultiProgress::new();

    let pb_files = multi.add(ProgressBar::new(total_files as u64));
    pb_files.set_style(file_count_style());

    let pb_bytes = multi.add(ProgressBar::new(total_bytes));
    pb_bytes.set_style(byte_progress_style());

    // Shared state for the throttled filename display.
    let extraction_start = Instant::now();
    let last_msg_nanos = Arc::new(AtomicU64::new(0));
    let bytes_extracted = Arc::new(AtomicU64::new(0));

    // --- Parallel extraction ---

    let extract_result = (0..total_files).into_par_iter().try_for_each_init(
        || {
            let cursor = Cursor::new(mmap.clone());
            zip::ZipArchive::new(cursor).context("Failed to open ZIP archive for worker")
        },
        |local_archive, i| -> Result<()> {
            // Fast cancellation check before opening the entry.
            if !running.load(Ordering::Relaxed) {
                // Return the sentinel error type so run() can distinguish
                // cancellation from a genuine extraction failure.
                return Err(anyhow::anyhow!(Cancelled));
            }

            let local_archive = local_archive
                .as_mut()
                .map_err(|e| anyhow::anyhow!("ZIP archive init failed for worker: {e:#}"))?;

            let mut zip_file = local_archive
                .by_index(i)
                .with_context(|| format!("Failed to read ZIP entry #{i}"))?;

            let entry_name = zip_file.name().to_owned();
            let Some(path) = zip_file.enclosed_name() else {
                eprintln!("Warning: skipping unsafe path in ZIP: {entry_name:?}");
                pb_files.inc(1);
                return Ok(());
            };
            let outpath = layout.staging_game_dir.join(&path);

            // Throttled filename update: use wall-clock nanos to avoid
            // compare_exchange spuriously losing under contention.
            let now_ns = extraction_start.elapsed().as_nanos() as u64;
            let interval_ns = PROGRESS_MSG_INTERVAL.as_nanos() as u64;
            let last_ns = last_msg_nanos.load(Ordering::Relaxed);
            if now_ns.saturating_sub(last_ns) >= interval_ns
                && last_msg_nanos
                    .compare_exchange(last_ns, now_ns, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    pb_files.set_message(name.to_owned());
                }
            }

            if zip_file.is_dir() {
                fs::create_dir_all(&outpath)
                    .with_context(|| format!("Failed to create directory {}", outpath.display()))?;
            } else {
                if let Some(parent) = outpath.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("Failed to create parent directory {}", parent.display())
                    })?;
                }

                let outfile = File::create(&outpath)
                    .with_context(|| format!("Failed to create {}", outpath.display()))?;

                let mut writer = BufWriter::with_capacity(COPY_BUFFER_SIZE, outfile);
                let mut reader = CancellableReader::new(&mut zip_file, &running);

                let written = io::copy(&mut reader, &mut writer)
                    .with_context(|| format!("Failed to write {entry_name}"))?;

                writer
                    .flush()
                    .with_context(|| format!("Failed to flush {}", outpath.display()))?;

                #[cfg(unix)]
                if let Some(mode) = zip_file.unix_mode() {
                    fs::set_permissions(&outpath, fs::Permissions::from_mode(mode)).with_context(
                        || format!("Failed to set permissions on {}", outpath.display()),
                    )?;
                }

                // Update byte-level progress atomically.
                bytes_extracted.fetch_add(written, Ordering::Relaxed);
                pb_bytes.set_position(bytes_extracted.load(Ordering::Relaxed));
            }

            pb_files.inc(1);
            Ok(())
        },
    );

    // --- Resolve extraction outcome ---
    //
    // Three cases:
    //   1. Ok(())            — completed normally
    //   2. Err(Cancelled)    — user pressed Ctrl-C (clean path)
    //   3. Err(other)        — genuine failure

    match extract_result {
        Ok(()) => {}
        Err(e)
            if e.downcast_ref::<Cancelled>().is_some()
                || user_cancelled.load(Ordering::Relaxed) =>
        {
            pb_files.abandon_with_message("cancelled");
            pb_bytes.abandon_with_message("cancelled");
            // staging_cleanup still armed — will remove the partial output on drop
            return Ok(false);
        }
        Err(e) => return Err(e),
    }

    // Double-check: if the Ctrl-C handler fired between the last worker
    // completing and us reaching here, treat it as a cancellation.
    if user_cancelled.load(Ordering::Relaxed) {
        pb_files.abandon_with_message("cancelled");
        pb_bytes.abandon_with_message("cancelled");
        return Ok(false);
    }

    pb_files.finish_with_message("done");
    pb_bytes.finish_with_message("done");

    // --- Move staging output into final location ---

    finalize_output(&layout)?;
    staging_cleanup.disarm();

    // --- Summary ---

    let elapsed = extraction_start.elapsed();
    let total_mib = total_bytes as f64 / (1024.0 * 1024.0);
    println!(
        "\n  {} files  {:.1} MiB  in {:.1}s",
        total_files,
        total_mib,
        elapsed.as_secs_f64(),
    );

    Ok(true)
}
