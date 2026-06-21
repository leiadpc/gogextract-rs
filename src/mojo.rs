use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
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

// How many bytes to decompress between cancellation polls. Kept at 256 KiB so
// that cancellation feels snappy even on archives with very large single files,
// while the atomic load overhead remains negligible compared to I/O cost.
const CANCEL_CHECK_INTERVAL_BYTES: usize = 256 * 1024;

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
        // Only accumulate actual bytes — zero-length reads (EOF probes) must
        // not advance the counter, otherwise the threshold can be hit on an
        // empty stream and fire a spurious cancellation check.
        if n > 0 {
            self.bytes_since_check += n;
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Output staging
//
// ZIP contents are extracted into a hidden staging directory that sits next to
// the intended output directory. On success the staging directory is atomically
// renamed into place as the final output directory. This ensures the output
// path is either fully populated or absent — never half-written — even if the
// process is killed mid-extraction.
//
// Layout (force=false, output_dir = /dest/mygame):
//   /dest/.gogextract-tmp-<pid>-<ms>/   ← staging_dir  (written during extraction)
//   /dest/mygame/                        ← output_dir   (appears only on success)
//
// Layout (force=true, output_dir already exists):
//   /dest/.gogextract-tmp-<pid>-<ms>/   ← staging_dir  (written during extraction)
//   /dest/.gogextract-tmp-<pid>-<ms>-prev/ ← backup_dir (old output moved here)
//   /dest/mygame/                        ← output_dir   (replaced on success)
// ---------------------------------------------------------------------------

pub struct OutputLayout {
    /// The final destination — what the user asked for.
    pub output_dir: PathBuf,
    /// Hidden sibling directory where files are written during extraction.
    pub staging_dir: PathBuf,
    /// If force=true and output_dir already existed, the old contents are
    /// moved here before the rename so they can be restored on failure.
    pub backup_dir: Option<PathBuf>,
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

/// Prepares the staging layout for an extraction run.
///
/// Creates the parent of `output_dir` (if needed) and a hidden staging
/// sibling directory. Does NOT create `output_dir` itself — that happens via
/// the atomic rename in `finalize_output`.
pub fn prepare_output_layout(output_dir: &Path, force: bool) -> Result<OutputLayout> {
    // Ensure the parent exists so the staging dir can be created next to it.
    let parent = output_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create parent directory {}", parent.display()))?;

    ensure_output_path_available(output_dir, force)?;

    let staging_dir = create_staging_dir(parent)?;

    // If force=true and the output already exists, remember where to move the
    // old contents so we can restore them if the rename later fails.
    let backup_dir = force
        .then(|| {
            // Place the backup alongside the staging dir so both are on the
            // same filesystem — guaranteeing the restore rename is also atomic.
            let mut name = staging_dir
                .file_name()
                .expect("staging dir always has a name")
                .to_os_string();
            name.push("-prev");
            staging_dir.with_file_name(name)
        })
        .filter(|_| output_dir.exists());

    Ok(OutputLayout {
        output_dir: output_dir.to_path_buf(),
        staging_dir,
        backup_dir,
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

fn create_staging_dir(parent: &Path) -> Result<PathBuf> {
    // PID + millisecond timestamp gives a unique-enough name without retrying.
    // Two processes would need the same PID and start within the same millisecond
    // in the same directory to collide — effectively impossible in practice.
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let path = parent.join(format!(".gogextract-tmp-{}-{}", process::id(), timestamp));

    fs::create_dir(&path)
        .with_context(|| format!("Failed to create staging directory {}", path.display()))?;

    Ok(path)
}

/// Atomically moves the staging directory into place as the final output.
///
/// Steps:
///   1. If a backup exists (force run with pre-existing output), move the old
///      output aside into the backup slot.
///   2. Rename staging → output.
///   3. If step 2 fails and a backup exists, restore the old output.
///   4. Remove the backup slot (now empty after step 2 succeeded).
///   5. Remove the staging dir itself (now empty after step 2 succeeded).
pub fn finalize_output(layout: &OutputLayout) -> Result<()> {
    // Step 1 — move existing output aside so step 2's rename target is clear.
    if let Some(backup_dir) = &layout.backup_dir {
        fs::rename(&layout.output_dir, backup_dir).with_context(|| {
            format!(
                "Failed to move existing {} out of the way",
                layout.output_dir.display()
            )
        })?;
    }

    // Step 2 — atomic rename of the fully-written staging dir into place.
    if let Err(rename_err) = fs::rename(&layout.staging_dir, &layout.output_dir) {
        // Step 3 — restore old output if we have a backup.
        if let Some(backup_dir) = &layout.backup_dir {
            fs::rename(backup_dir, &layout.output_dir).with_context(|| {
                format!(
                    "Failed to restore previous output {} after replacement failed \
                     (original rename error: {rename_err:#})",
                    layout.output_dir.display()
                )
            })?;
        }

        return Err(rename_err).with_context(|| {
            format!(
                "Failed to move staging directory into place as {}",
                layout.output_dir.display()
            )
        });
    }

    // Step 4 — clean up the now-empty backup slot.
    if let Some(backup_dir) = &layout.backup_dir {
        remove_existing_output_path(backup_dir).with_context(|| {
            format!(
                "Failed to remove previous output backup {}",
                backup_dir.display()
            )
        })?;
    }

    // Step 5 — staging_dir was renamed away in step 2, so there is nothing
    // left to remove. The StagingCleanup RAII guard must be disarmed by the
    // caller after this returns successfully.
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
        "{spinner:.green.bold} Files [{bar:40.green/white.dim}] {pos}/{len}  {msg:.dim}",
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
// ZIP metadata scan
//
// Opens the archive once and returns both the raw entry count (directories
// included, needed to drive the Rayon index range) and the file-only count
// (used as the progress bar total). ZipArchive::new() parses the central
// directory into memory so by_index_raw() here is metadata-only — no seeking
// to compressed data. Running this in parallel would spawn N ZipArchive
// instances, each re-parsing the central directory, which is slower for large
// entry counts and wastes memory.
// ---------------------------------------------------------------------------

pub struct ZipCounts {
    /// Total entries including directories — used as the Rayon iteration bound.
    pub entry_count: usize,
    /// Files only (no directories) — used as the progress bar total.
    pub file_count: usize,
}

fn scan_zip_counts(mmap: &ArcMmap) -> Result<ZipCounts> {
    let cursor = Cursor::new(mmap.clone());
    let mut archive = zip::ZipArchive::new(cursor).context("Failed to open ZIP archive")?;

    let entry_count = archive.len();
    let mut file_count = 0usize;
    for i in 0..entry_count {
        if let Ok(entry) = archive.by_index_raw(i) {
            if !entry.is_dir() {
                file_count += 1;
            }
        }
    }

    Ok(ZipCounts {
        entry_count,
        file_count,
    })
}

// ---------------------------------------------------------------------------
// Progress sink abstraction
//
// The CLI and GUI extraction paths are identical in every respect except how
// progress is reported and how the final outcome is surfaced to the user
// (a console ProgressBar vs an mpsc channel of GuiEvents). `extract_zip_inner`
// drives the actual rayon extraction loop once; `extract` and `extract_gui`
// are thin wrappers that build the appropriate ProgressSink impl and forward
// to it.
// ---------------------------------------------------------------------------

/// Receives progress notifications from the extraction loop. Implementations
/// own their own display/transport (a console ProgressBar, an mpsc channel,
/// etc.) and are called from worker threads, so methods take `&self`.
trait ProgressSink: Sync {
    /// A non-fatal warning, e.g. an unsafe path being skipped.
    fn warn(&self, message: String);
    /// A file has just been written, with the current running total. Called
    /// on every file — implementations needing a low-frequency counter (e.g.
    /// a progress bar position) can update unconditionally here.
    fn file_written(&self, files_done: u64, files_total: u64);
    /// Periodically called with the current file name, already throttled by
    /// the caller so implementations can update a "now extracting" label
    /// without flooding a console redraw or GUI channel.
    fn current_file(&self, files_done: u64, files_total: u64, name: &str);
}

/// Drives the parallel ZIP extraction loop shared by the CLI and GUI paths.
///
/// Returns `Ok(true)` on success, `Ok(false)` if cancelled (with the staging
/// dir cleaned up and `output_dir` left untouched), or `Err` on a real failure.
fn extract_zip_inner(
    mmap: &ArcMmap,
    layout: &OutputLayout,
    counts: &ZipCounts,
    running: &Arc<AtomicBool>,
    user_cancelled: &Arc<AtomicBool>,
    sink: &dyn ProgressSink,
) -> Result<bool> {
    let extraction_start = Instant::now();
    let last_msg_nanos = Arc::new(AtomicU64::new(0));
    let files_extracted = Arc::new(AtomicU64::new(0));

    let thread_count = rayon::current_num_threads();
    let min_chunk_size = (counts.entry_count / thread_count).max(1);

    let extract_result = (0..counts.entry_count)
        .into_par_iter()
        .with_min_len(min_chunk_size)
        .try_for_each_init(
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
                    // Skip entries with unsafe paths (e.g. path traversal) without
                    // incrementing progress — the file count total was computed
                    // from safe entries only.
                    sink.warn(format!("Skipping unsafe path in ZIP: {entry_name:?}"));
                    return Ok(());
                };
                // Write directly into the staging dir — no game_data subfolder.
                let outpath = layout.staging_dir.join(&path);

                if zip_file.is_dir() {
                    fs::create_dir_all(&outpath).with_context(|| {
                        format!("Failed to create directory {}", outpath.display())
                    })?;
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

                    let _written = io::copy(&mut reader, &mut writer)
                        .with_context(|| format!("Failed to write {entry_name}"))?;

                    writer
                        .flush()
                        .with_context(|| format!("Failed to flush {}", outpath.display()))?;

                    #[cfg(unix)]
                    if let Some(mode) = zip_file.unix_mode() {
                        fs::set_permissions(&outpath, fs::Permissions::from_mode(mode))
                            .with_context(|| {
                                format!("Failed to set permissions on {}", outpath.display())
                            })?;
                    }

                    let fd = files_extracted.fetch_add(1, Ordering::Relaxed) + 1;
                    sink.file_written(fd, counts.file_count as u64);

                    // Fetch clock metadata after I/O to minimise atomic contention,
                    // and throttle how often we update the displayed filename so
                    // fast extractions don't flood a console redraw / GUI channel.
                    let now_ns = extraction_start.elapsed().as_nanos() as u64;
                    let interval_ns = PROGRESS_MSG_INTERVAL.as_nanos() as u64;
                    let last_ns = last_msg_nanos.load(Ordering::Relaxed);
                    if now_ns.saturating_sub(last_ns) >= interval_ns
                        && last_msg_nanos
                            .compare_exchange(last_ns, now_ns, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                    {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            sink.current_file(fd, counts.file_count as u64, name);
                        }
                    }
                }

                Ok(())
            },
        );

    // Single consolidated cancellation check, covering both the in-loop
    // Cancelled sentinel and the out-of-band user_cancelled flag (set e.g.
    // by Ctrl+C handling, which may race ahead of the loop noticing `running`).
    match extract_result {
        Ok(()) => {}
        Err(e)
            if e.downcast_ref::<Cancelled>().is_some()
                || user_cancelled.load(Ordering::Relaxed) =>
        {
            // The output_dir was never populated (finalize_output was never
            // called), so it is safe to remove if it exists. It may exist if
            // prepare_output_layout created the parent but not the output dir
            // itself — which it doesn't — so this is a no-op in the normal
            // case. We still attempt removal defensively.
            let _ = fs::remove_dir_all(&layout.output_dir);
            return Ok(false);
        }
        Err(e) => return Err(e),
    }

    Ok(true)
}

// ---------------------------------------------------------------------------
// CLI extraction
// ---------------------------------------------------------------------------

/// `ProgressSink` impl backed by an indicatif console progress bar.
struct CliSink {
    pb: ProgressBar,
}

impl ProgressSink for CliSink {
    fn warn(&self, message: String) {
        self.pb.println(format!("Warning: {message}"));
    }

    fn file_written(&self, files_done: u64, _files_total: u64) {
        self.pb.set_position(files_done);
    }

    fn current_file(&self, _files_done: u64, _files_total: u64, name: &str) {
        self.pb.set_message(name.to_owned());
    }
}

pub fn extract(
    mmap: &ArcMmap,
    input_file: &Path,
    output_dir: Option<PathBuf>,
    force: bool,
    running: &Arc<AtomicBool>,
    user_cancelled: &Arc<AtomicBool>,
) -> Result<bool> {
    let resolved_output_dir = output_dir.unwrap_or_else(|| crate::default_output_dir(input_file));

    let counts = scan_zip_counts(mmap)?;

    let layout = prepare_output_layout(&resolved_output_dir, force)?;
    let mut staging_cleanup = StagingCleanup::new(&layout.staging_dir);

    println!("Extracting to: {}\n", resolved_output_dir.display());
    println!("Press Ctrl+C to cancel.\n");

    let pb = ProgressBar::new(counts.file_count as u64);
    pb.set_style(file_count_style());
    pb.enable_steady_tick(Duration::from_millis(100));

    let extraction_start = Instant::now();
    let sink = CliSink { pb: pb.clone() };

    let ok = extract_zip_inner(mmap, &layout, &counts, running, user_cancelled, &sink)?;

    if !ok {
        pb.abandon_with_message("cancelled");
        drop(staging_cleanup);
        return Ok(false);
    }

    pb.finish_with_message("done");

    finalize_output(&layout)?;
    staging_cleanup.disarm();

    println!(
        "\n  {} files  in {:.1}s",
        counts.file_count,
        extraction_start.elapsed().as_secs_f64()
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

/// `ProgressSink` impl backed by an `mpsc::Sender<GuiEvent>`.
#[cfg(feature = "gui")]
struct GuiSink {
    tx: mpsc::Sender<GuiEvent>,
}

#[cfg(feature = "gui")]
impl ProgressSink for GuiSink {
    fn warn(&self, message: String) {
        let _ = self
            .tx
            .send(GuiEvent::Log(format!("Skipping unsafe path: {message}")));
    }

    fn file_written(&self, _files_done: u64, _files_total: u64) {
        // The GUI only needs the throttled `current_file` tick below — sending
        // a Progress event on every single file would flood the channel.
    }

    fn current_file(&self, files_done: u64, files_total: u64, name: &str) {
        // Emit filename + progress atomically in one event so the GUI fields
        // never become out of sync.
        let _ = self.tx.send(GuiEvent::Progress {
            files_done,
            files_total,
            current_file: Some(name.to_owned()),
        });
    }
}

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

    let counts = scan_zip_counts(mmap)?;

    let layout = prepare_output_layout(&resolved_output_dir, force)?;
    let mut staging_cleanup = StagingCleanup::new(&layout.staging_dir);

    let _ = tx.send(GuiEvent::Log(format!(
        "Extracting to: {}",
        resolved_output_dir.display()
    )));

    let extraction_start = Instant::now();
    let sink = GuiSink { tx: tx.clone() };

    let ok = extract_zip_inner(mmap, &layout, &counts, running, user_cancelled, &sink)?;

    if !ok {
        // Drop the RAII guard first to clean up the staging dir.
        // output_dir was never finalized so it is safe to remove if it
        // exists. Because prepare_output_layout no longer creates
        // output_dir itself, this is a no-op in the normal case.
        drop(staging_cleanup);
        return Ok(false);
    }

    // Send a final progress tick to ensure the GUI reaches 100%.
    let _ = tx.send(GuiEvent::Progress {
        files_done: counts.file_count as u64,
        files_total: counts.file_count as u64,
        current_file: None,
    });

    finalize_output(&layout)?;
    staging_cleanup.disarm();

    let elapsed = extraction_start.elapsed().as_secs_f64();
    let _ = tx.send(GuiEvent::Done {
        elapsed_secs: elapsed,
        file_count: counts.file_count,
        output_dir: resolved_output_dir,
    });
    Ok(true)
}
