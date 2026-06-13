use anyhow::{Context, Result};
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

pub const COPY_BUFFER_SIZE: usize = 64 * 1024;

// How many bytes to decompress between cancellation polls.
const CANCEL_CHECK_INTERVAL_BYTES: usize = 1024 * 1024; // 1 MB

// Minimum wall-clock time between progress-bar filename updates.
const PROGRESS_MSG_INTERVAL: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Cancellation error sentinel
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Cancelled;

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
pub struct ArcMmap(pub Arc<memmap2::Mmap>);

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
// Zero-Copy, Allocation-Free Cancellable Reader
// ---------------------------------------------------------------------------

struct CancellableReader<'a, R> {
    inner: R,
    running: &'a Arc<AtomicBool>,
    bytes_since_check: usize,
}

impl<'a, R: Read> CancellableReader<'a, R> {
    fn new(inner: R, running: &'a Arc<AtomicBool>) -> Self {
        Self {
            inner,
            running,
            bytes_since_check: 0,
        }
    }
}

impl<'a, R: Read> Read for CancellableReader<'a, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        // Poll cancellation flag at coarse intervals to minimise atomic overhead.
        if self.bytes_since_check >= CANCEL_CHECK_INTERVAL_BYTES {
            if !self.running.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Other, "cancelled"));
            }
            self.bytes_since_check = 0;
        }

        let n = self.inner.read(out)?;
        self.bytes_since_check += n;
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Output staging
// ---------------------------------------------------------------------------

pub struct OutputLayout {
    pub final_game_dir: PathBuf,
    pub staging_dir: PathBuf,
    pub staging_game_dir: PathBuf,
    pub backup_game_dir: Option<PathBuf>,
}

pub struct StagingCleanup {
    staging_dir: PathBuf,
    armed: bool,
}

impl StagingCleanup {
    pub fn new(staging_dir: &Path) -> Self {
        Self {
            staging_dir: staging_dir.to_path_buf(),
            armed: true,
        }
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingCleanup {
    fn drop(&mut self) {
        if self.armed {
            // Use eprintln! here — by the time Drop runs the progress bars are
            // already abandoned/finished, so there is no display to clobber.
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

pub fn prepare_output_layout(output_dir: &Path, force: bool) -> Result<OutputLayout> {
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

pub fn ensure_output_path_available(path: &Path, force: bool) -> Result<()> {
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

pub fn finalize_output(layout: &OutputLayout) -> Result<()> {
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
// Progress bar styles
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

pub fn list_lines(mmap: &ArcMmap) -> Result<Vec<String>> {
    let cursor = Cursor::new(mmap.clone());
    let mut archive = zip::ZipArchive::new(cursor).context("Failed to open ZIP archive")?;

    let mut lines = Vec::with_capacity(archive.len() + 3);
    lines.push(format!("{:<10}  {:<19}  {}", "Size", "Modified", "Name"));
    lines.push("-".repeat(72));

    let mut total_size: u64 = 0;
    let mut file_count: usize = 0;

    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .with_context(|| format!("Failed to read ZIP entry #{i}"))?;

        let name = entry.name().to_owned();
        let size = entry.size();

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

        lines.push(format!("{:<10}  {}  {}", size, modified, name));

        if !entry.is_dir() {
            total_size += size;
            file_count += 1;
        }
    }

    lines.push("-".repeat(72));
    lines.push(format!(
        "{file_count} file(s), {total_size} bytes uncompressed"
    ));

    Ok(lines)
}

pub fn list(mmap: &ArcMmap) -> Result<()> {
    for line in list_lines(mmap)? {
        println!("{line}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Parallel ZIP extraction — helper that scans metadata in one parallel pass.
// Returns (file_count, total_uncompressed_bytes), skipping directory entries.
// ---------------------------------------------------------------------------

fn scan_zip_metadata(mmap: &ArcMmap, count: usize) -> (usize, u64) {
    (0..count)
        .into_par_iter()
        .map_with(Cursor::new(mmap.clone()), |cursor, i| {
            if let Ok(mut archive) = zip::ZipArchive::new(cursor) {
                if let Ok(entry) = archive.by_index(i) {
                    if !entry.is_dir() {
                        return (1usize, entry.size());
                    }
                }
            }
            // Directory entries and failed reads contribute nothing.
            (0, 0)
        })
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
}

// ---------------------------------------------------------------------------
// CLI extraction
// ---------------------------------------------------------------------------

pub fn extract(
    mmap: &ArcMmap,
    input_file: &Path,
    output_dir: Option<PathBuf>,
    force: bool,
    running: &Arc<AtomicBool>,
    user_cancelled: &Arc<AtomicBool>,
) -> Result<bool> {
    let resolved_output_dir = output_dir.unwrap_or_else(|| crate::default_output_dir(input_file));

    let count = zip::ZipArchive::new(Cursor::new(mmap.clone()))
        .context("Failed to open ZIP archive")?
        .len();

    let (total_files, total_bytes) = scan_zip_metadata(mmap, count);

    let layout = prepare_output_layout(&resolved_output_dir, force)?;
    let mut staging_cleanup = StagingCleanup::new(&layout.staging_dir);

    println!("Extracting to: {}\n", resolved_output_dir.display());
    println!("Press Ctrl+C to cancel.\n");

    fs::create_dir_all(&layout.staging_game_dir).with_context(|| {
        format!(
            "Failed to create staging game-data directory {}",
            layout.staging_game_dir.display()
        )
    })?;

    let multi = MultiProgress::new();
    let pb_files = multi.add(ProgressBar::new(total_files as u64));
    pb_files.set_style(file_count_style());

    let pb_bytes = multi.add(ProgressBar::new(total_bytes));
    pb_bytes.set_style(byte_progress_style());

    let extraction_start = Instant::now();
    let last_msg_nanos = Arc::new(AtomicU64::new(0));
    let bytes_extracted = Arc::new(AtomicU64::new(0));

    let extract_result = (0..count).into_par_iter().try_for_each_init(
        || {
            let cursor = Cursor::new(mmap.clone());
            zip::ZipArchive::new(cursor).context("Failed to open ZIP archive for worker")
        },
        |local_archive, i| -> Result<()> {
            if !running.load(Ordering::Relaxed) {
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
                let mut reader = CancellableReader::new(&mut zip_file, running);

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

                // Fetch clock metadata after I/O to minimise atomic contention.
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

                let current_total_bytes =
                    bytes_extracted.fetch_add(written, Ordering::Relaxed) + written;
                pb_bytes.set_position(current_total_bytes);
            }

            pb_files.inc(1);
            Ok(())
        },
    );

    // Single consolidated cancellation check — no duplicate block needed after
    // this match because the Cancelled arm already returns early.
    match extract_result {
        Ok(()) => {}
        Err(e)
            if e.downcast_ref::<Cancelled>().is_some()
                || user_cancelled.load(Ordering::Relaxed) =>
        {
            pb_files.abandon_with_message("cancelled");
            pb_bytes.abandon_with_message("cancelled");
            drop(staging_cleanup);
            let _ = fs::remove_dir(&resolved_output_dir);
            return Ok(false);
        }
        Err(e) => return Err(e),
    }

    pb_files.finish_with_message("done");
    pb_bytes.finish_with_message("done");

    finalize_output(&layout)?;
    staging_cleanup.disarm();

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

// ---------------------------------------------------------------------------
// GUI extraction path
// ---------------------------------------------------------------------------

#[cfg(feature = "gui")]
use crate::gui::GuiEvent;
#[cfg(feature = "gui")]
use std::sync::mpsc;

#[cfg(feature = "gui")]
pub fn extract_gui(
    mmap: &ArcMmap,
    input_file: &Path,
    output_dir: Option<PathBuf>,
    force: bool,
    running: &Arc<AtomicBool>,
    user_cancelled: &Arc<AtomicBool>,
    tx: &mpsc::Sender<GuiEvent>,
) -> Result<bool> {
    let resolved_output_dir = output_dir.unwrap_or_else(|| crate::default_output_dir(input_file));

    let count = zip::ZipArchive::new(Cursor::new(mmap.clone()))
        .context("Failed to open ZIP archive")?
        .len();

    let (total_files, total_bytes) = scan_zip_metadata(mmap, count);

    let layout = prepare_output_layout(&resolved_output_dir, force)?;
    let mut staging_cleanup = StagingCleanup::new(&layout.staging_dir);

    let _ = tx.send(GuiEvent::Log(format!(
        "Extracting to: {}",
        resolved_output_dir.display()
    )));

    fs::create_dir_all(&layout.staging_game_dir).with_context(|| {
        format!(
            "Failed to create staging game-data directory {}",
            layout.staging_game_dir.display()
        )
    })?;

    let extraction_start = Instant::now();
    let last_msg_nanos = Arc::new(AtomicU64::new(0));
    let bytes_extracted = Arc::new(AtomicU64::new(0));
    let files_extracted = Arc::new(AtomicU64::new(0));

    let tx = Arc::new(tx.clone());

    let extract_result = (0..count).into_par_iter().try_for_each_init(
        || {
            let cursor = std::io::Cursor::new(mmap.clone());
            zip::ZipArchive::new(cursor).context("Failed to open ZIP archive for worker")
        },
        |local_archive, i| -> Result<()> {
            if !running.load(Ordering::Relaxed) {
                return Err(anyhow::anyhow!(Cancelled));
            }

            let local_archive = local_archive
                .as_mut()
                .map_err(|e| anyhow::anyhow!("ZIP archive init failed: {e:#}"))?;

            let mut zip_file = local_archive
                .by_index(i)
                .with_context(|| format!("Failed to read ZIP entry #{i}"))?;

            let entry_name = zip_file.name().to_owned();
            let Some(path) = zip_file.enclosed_name() else {
                let _ = tx.send(GuiEvent::Log(format!(
                    "Skipping unsafe path: {entry_name:?}"
                )));
                return Ok(());
            };
            let outpath = layout.staging_game_dir.join(&path);

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
                let mut reader = CancellableReader::new(&mut zip_file, running);

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

                let bd = bytes_extracted.fetch_add(written, Ordering::Relaxed) + written;
                let fd = files_extracted.fetch_add(1, Ordering::Relaxed) + 1;

                // UI event throttling to prevent channel congestion.
                let now_ns = extraction_start.elapsed().as_nanos() as u64;
                let interval_ns = PROGRESS_MSG_INTERVAL.as_nanos() as u64;
                let last_ns = last_msg_nanos.load(Ordering::Relaxed);
                if now_ns.saturating_sub(last_ns) >= interval_ns
                    && last_msg_nanos
                        .compare_exchange(last_ns, now_ns, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        let _ = tx.send(GuiEvent::Filename(name.to_owned()));
                    }
                    let _ = tx.send(GuiEvent::Progress {
                        files_done: fd,
                        files_total: total_files as u64,
                        bytes_done: bd,
                        bytes_total: total_bytes,
                    });
                }
            }

            Ok(())
        },
    );

    // Single consolidated cancellation check.
    match extract_result {
        Ok(()) => {}
        Err(e)
            if e.downcast_ref::<Cancelled>().is_some()
                || user_cancelled.load(Ordering::Relaxed) =>
        {
            drop(staging_cleanup);
            let _ = fs::remove_dir(&resolved_output_dir);
            return Ok(false);
        }
        Err(e) => return Err(e),
    }

    // Send a final progress tick to ensure the GUI reaches 100%.
    let _ = tx.send(GuiEvent::Progress {
        files_done: total_files as u64,
        files_total: total_files as u64,
        bytes_done: total_bytes,
        bytes_total: total_bytes,
    });

    finalize_output(&layout)?;
    staging_cleanup.disarm();

    let elapsed = extraction_start.elapsed().as_secs_f64();
    let total_mib = total_bytes as f64 / (1024.0 * 1024.0);
    let _ = tx.send(GuiEvent::Done {
        elapsed_secs: elapsed,
        total_mib,
        file_count: total_files,
    });
    Ok(true)
}
