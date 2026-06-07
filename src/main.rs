use anyhow::{Context, Result};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rayon::prelude::*;
use regex::bytes::Regex;
use std::fs::{self, File};
use std::io::{self, BufRead, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const HEADER_PEEK_SIZE: usize = 10 * 1024;
const COPY_BUFFER_SIZE: usize = 64 * 1024;

// Minimum time between progress bar message updates, to avoid thrashing under
// Rayon's work-stealing scheduler where index-based throttling is unreliable.
const PROGRESS_MSG_INTERVAL: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Lazily-compiled regexes
// ---------------------------------------------------------------------------

static OFFSET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"offset=`head -n (\d+?) "\$0""#).unwrap());

static FILESIZE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"filesizes="(\d+?)""#).unwrap());

// ---------------------------------------------------------------------------
// Thread-Safe Shared Memory-Map Wrapper
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ArcMmap(Arc<memmap2::Mmap>);

impl AsRef<[u8]> for ArcMmap {
    fn as_ref(&self) -> &[u8] {
        &self.0[..]
    }
}

// ---------------------------------------------------------------------------
// CancellableReader
// ---------------------------------------------------------------------------

struct CancellableReader<'a, R> {
    inner: R,
    running: &'a Arc<AtomicBool>,
}

impl<'a, R: Read> Read for CancellableReader<'a, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.running.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Cancelled by user",
            ));
        }
        self.inner.read(buf)
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    input_file: PathBuf,
    #[arg(default_value = "./")]
    output_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Output staging
// ---------------------------------------------------------------------------

struct OutputLayout {
    final_unpacker_path: PathBuf,
    final_mojosetup_dir: PathBuf,
    final_game_dir: PathBuf,
    staging_dir: PathBuf,
    staging_unpacker_path: PathBuf,
    staging_mojosetup_dir: PathBuf,
    staging_game_dir: PathBuf,
}

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
                    "Warning: Failed to clean up staging directory {}: {}",
                    self.staging_dir.display(),
                    e
                );
            }
        }
    }
}

fn prepare_output_layout(output_dir: &Path) -> Result<OutputLayout> {
    fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create output directory {}", output_dir.display()))?;

    let final_unpacker_path = output_dir.join("unpacker.sh");
    let final_mojosetup_dir = output_dir.join("mojosetup");
    let final_game_dir = output_dir.join("game_data");

    ensure_output_paths_available(&[&final_unpacker_path, &final_mojosetup_dir, &final_game_dir])?;

    let staging_dir = create_staging_dir(output_dir)?;
    let staging_unpacker_path = staging_dir.join("unpacker.sh");
    let staging_mojosetup_dir = staging_dir.join("mojosetup");
    let staging_game_dir = staging_dir.join("game_data");

    Ok(OutputLayout {
        final_unpacker_path,
        final_mojosetup_dir,
        final_game_dir,
        staging_dir,
        staging_unpacker_path,
        staging_mojosetup_dir,
        staging_game_dir,
    })
}

fn ensure_output_paths_available(paths: &[&Path]) -> Result<()> {
    for path in paths {
        if path.exists() {
            anyhow::bail!(
                "Output path already exists: {}. Choose an empty output directory or remove it first.",
                path.display()
            );
        }
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
    // Drop the pre-extraction check here: the paths were verified in
    // prepare_output_layout and re-checking right before rename gives false
    // safety (TOCTOU) without preventing a race. Let fs::rename surface any
    // conflict naturally via its own error.
    fs::rename(&layout.staging_unpacker_path, &layout.final_unpacker_path).with_context(|| {
        format!(
            "Failed to move {} into place",
            layout.final_unpacker_path.display()
        )
    })?;
    fs::rename(&layout.staging_mojosetup_dir, &layout.final_mojosetup_dir).with_context(|| {
        format!(
            "Failed to move {} into place",
            layout.final_mojosetup_dir.display()
        )
    })?;
    fs::rename(&layout.staging_game_dir, &layout.final_game_dir).with_context(|| {
        format!(
            "Failed to move {} into place",
            layout.final_game_dir.display()
        )
    })?;
    fs::remove_dir(&layout.staging_dir).with_context(|| {
        format!(
            "Failed to remove staging directory {}",
            layout.staging_dir.display()
        )
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Metadata parsing
// ---------------------------------------------------------------------------

struct PackageMetadata {
    script_size: u64,
    mojosetup_size: u64,
    script_bytes: Vec<u8>,
}

fn parse_metadata(mmap: &[u8]) -> Result<PackageMetadata> {
    let peek_size = std::cmp::min(HEADER_PEEK_SIZE, mmap.len());
    let peek_slice = &mmap[..peek_size];

    // Use lazily-compiled statics instead of compiling on every call.
    let offset_caps = OFFSET_RE
        .captures(peek_slice)
        .context("Could not find 'offset' metadata")?;
    let script_line_count: u64 = std::str::from_utf8(
        offset_caps
            .get(1)
            .context("Missing capture group in offset")?
            .as_bytes(),
    )?
    .parse()?;

    let filesize_caps = FILESIZE_RE
        .captures(peek_slice)
        .context("Could not find 'filesizes' metadata")?;
    let mojosetup_size: u64 = std::str::from_utf8(
        filesize_caps
            .get(1)
            .context("Missing capture group in filesize")?
            .as_bytes(),
    )?
    .parse()?;

    let mut cursor = Cursor::new(mmap);
    let mut script_bytes: Vec<u8> = Vec::new();
    let mut line = Vec::new();

    for line_number in 0..script_line_count {
        line.clear();
        if cursor.read_until(b'\n', &mut line)? == 0 {
            anyhow::bail!(
                "Installer script ended early: expected {} lines, found {}",
                script_line_count,
                line_number
            );
        }
        script_bytes.extend_from_slice(&line);
    }

    // Use try_from consistently with the rest of the codebase rather than
    // a silent `as u64` cast.
    let script_size = u64::try_from(script_bytes.len()).context("Script size overflows u64")?;

    Ok(PackageMetadata {
        script_size,
        mojosetup_size,
        script_bytes,
    })
}

// ---------------------------------------------------------------------------
// Progress bar helpers
// ---------------------------------------------------------------------------

fn bytes_style(prefix: &str) -> ProgressStyle {
    ProgressStyle::with_template(&format!(
        "{{spinner:.cyan}} {prefix} [{{bar:40.green/dim}}] {{bytes}}/{{total_bytes}} ({{eta}}) {{msg}}"
    ))
    .unwrap()
    .progress_chars("█▉▊▋▌▍▎▏ ")
}

fn count_style(prefix: &str) -> ProgressStyle {
    ProgressStyle::with_template(&format!(
        "{{spinner:.cyan}} {prefix} [{{bar:40.blue/dim}}] {{pos}}/{{len}} files  {{msg:.dim}}"
    ))
    .unwrap()
    .progress_chars("█▉▊▋▌▍▎▏ ")
}

// ---------------------------------------------------------------------------
// Main & Execution
// ---------------------------------------------------------------------------

fn main() {
    match run() {
        Ok(true) => {
            println!("\n🎉 Extraction complete!");
        }
        Ok(false) => {
            println!("\n🚨 Cancelled! Cleaned up.");
            std::process::exit(130);
        }
        Err(e) => {
            eprintln!("\n❌ Extraction failed: {:?}", e);
            std::process::exit(1);
        }
    }
}

fn run() -> Result<bool> {
    let args = Args::parse();

    // `running` is set to false on user cancel only. Threads must NOT set it
    // false on error — that conflates errors with cancellation and causes the
    // wrong exit path to be taken.
    let running = Arc::new(AtomicBool::new(true));
    let user_cancelled = Arc::new(AtomicBool::new(false));

    let r = running.clone();
    let c = user_cancelled.clone();
    ctrlc::set_handler(move || {
        c.store(true, Ordering::SeqCst);
        r.store(false, Ordering::SeqCst);
    })?;

    let file = File::open(&args.input_file)
        .with_context(|| format!("Failed to open input file {}", args.input_file.display()))?;

    if cfg!(target_pointer_width = "32") && file.metadata()?.len() > (u32::MAX as u64) {
        anyhow::bail!("File is too large to memory map on a 32-bit architecture.");
    }

    let raw_mmap = unsafe { memmap2::Mmap::map(&file).context("Failed to memory-map input file")? };
    let mmap = ArcMmap(Arc::new(raw_mmap));

    let meta = parse_metadata(mmap.as_ref())?;
    let layout = prepare_output_layout(&args.output_dir)?;
    let mut staging_cleanup = StagingCleanup::new(&layout.staging_dir);

    fs::write(&layout.staging_unpacker_path, &meta.script_bytes).with_context(|| {
        format!(
            "Failed to write unpacker script {}",
            layout.staging_unpacker_path.display()
        )
    })?;

    #[cfg(unix)]
    fs::set_permissions(
        &layout.staging_unpacker_path,
        fs::Permissions::from_mode(0o755),
    )
    .with_context(|| {
        format!(
            "Failed to mark unpacker script executable {}",
            layout.staging_unpacker_path.display()
        )
    })?;

    let script_size = usize::try_from(meta.script_size)
        .context("Installer script size does not fit in memory address space")?;
    let mojosetup_size = usize::try_from(meta.mojosetup_size)
        .context("MojoSetup size does not fit in memory address space")?;

    drop(meta);

    println!("Starting extraction. Press Ctrl+C to cancel...\n");
    let m = MultiProgress::new();

    // --- Thread 1: MojoSetup (Shared Memmap - Sequential Extraction) ---
    let (mmap_setup, setup_dir, run1) = (
        mmap.clone(),
        layout.staging_mojosetup_dir.clone(),
        running.clone(),
    );

    let pb_setup = m.add(ProgressBar::new(mojosetup_size as u64));
    pb_setup.set_style(bytes_style("MojoSetup "));

    let handle_setup = thread::spawn(move || -> Result<()> {
        let end = script_size
            .checked_add(mojosetup_size)
            .context("MojoSetup metadata sizes overflowed")?;
        if end > mmap_setup.0.len() {
            anyhow::bail!("MojoSetup metadata sizes exceed total package size");
        }

        fs::create_dir_all(&setup_dir).with_context(|| {
            format!(
                "Failed to create MojoSetup output directory {}",
                setup_dir.display()
            )
        })?;

        let compressed_slice = &mmap_setup.0[script_size..end];
        let reader = pb_setup.wrap_read(compressed_slice);
        let safe_reader = CancellableReader {
            inner: reader,
            running: &run1,
        };

        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(safe_reader));
        archive
            .unpack(&setup_dir)
            .with_context(|| format!("Failed to unpack MojoSetup into {}", setup_dir.display()))?;
        pb_setup.finish_with_message("✅ done");
        Ok(())
        // No longer calling run1.store(false) on error — that is the Ctrl+C
        // handler's job. Errors propagate through the join handle instead.
    });

    // --- Thread 2: Game Data (Rayon Parallel Extraction) ---
    let (mmap_data, game_dir, run2) = (
        mmap.clone(),
        layout.staging_game_dir.clone(),
        running.clone(),
    );

    let pb_data = m.add(ProgressBar::new(0));
    pb_data.set_style(count_style("Game Data "));

    let handle_data = thread::spawn(move || -> Result<()> {
        // Open the archive once to read total_files; reuse the same parse
        // rather than opening a second ZipArchive just for the count.
        let cursor = Cursor::new(mmap_data.clone());
        let archive = zip::ZipArchive::new(cursor).context("Failed to open ZIP archive")?;
        let total_files = archive.len();
        drop(archive);

        pb_data.set_length(total_files as u64);
        fs::create_dir_all(&game_dir).with_context(|| {
            format!(
                "Failed to create game data output directory {}",
                game_dir.display()
            )
        })?;

        // Shared timestamp for time-based progress message throttling.
        // Rayon's work-stealing means index % N updates are uneven;
        // a wall-clock gate keeps the display responsive without thrashing.
        let last_msg_time = Arc::new(AtomicU64::new(0));

        (0..total_files).into_par_iter().try_for_each_init(
            || {
                let cursor = Cursor::new(mmap_data.clone());
                zip::ZipArchive::new(cursor).context("Failed to open ZIP archive for worker")
            },
            |local_archive, i| -> Result<()> {
                if !run2.load(Ordering::Relaxed) {
                    anyhow::bail!("Cancelled");
                }

                let local_archive = match local_archive {
                    Ok(archive) => archive,
                    Err(e) => anyhow::bail!("Failed to open ZIP archive for worker: {e:#}"),
                };

                let mut zip_file = local_archive
                    .by_index(i)
                    .with_context(|| format!("Failed to read ZIP entry #{i}"))?;

                let entry_name = zip_file.name().to_owned();
                let Some(path) = zip_file.enclosed_name() else {
                    pb_data.inc(1);
                    return Ok(());
                };
                let outpath = game_dir.join(&path);

                // Time-based throttle: update the message at most every
                // PROGRESS_MSG_INTERVAL regardless of which Rayon thread fires.
                let now_ms = Instant::now().elapsed().as_millis() as u64;
                let interval_ms = PROGRESS_MSG_INTERVAL.as_millis() as u64;
                let last = last_msg_time.load(Ordering::Relaxed);
                if now_ms.saturating_sub(last) >= interval_ms {
                    if last_msg_time
                        .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            pb_data.set_message(name.to_owned());
                        }
                    }
                }

                // Robust platform-agnostic check using the zip library directly
                if zip_file.is_dir() {
                    fs::create_dir_all(&outpath).with_context(|| {
                        format!("Failed to create directory {}", outpath.display())
                    })?;
                } else {
                    if let Some(p) = outpath.parent() {
                        fs::create_dir_all(p).with_context(|| {
                            format!("Failed to create parent directory {}", p.display())
                        })?;
                    }

                    let outfile = File::create(&outpath).with_context(|| {
                        format!("Failed to create output file {}", outpath.display())
                    })?;

                    outfile.set_len(zip_file.size()).with_context(|| {
                        format!("Failed to pre-allocate output file {}", outpath.display())
                    })?;

                    let mut writer = BufWriter::with_capacity(COPY_BUFFER_SIZE, outfile);
                    let mut safe_reader = CancellableReader {
                        inner: &mut zip_file,
                        running: &run2,
                    };

                    // Highly optimized internal copy leverages OS mechanics when possible
                    // and handles standard read/write byte boundaries much better.
                    io::copy(&mut safe_reader, &mut writer).with_context(|| {
                        format!("Failed to copy data for ZIP entry {entry_name}")
                    })?;

                    writer.flush().with_context(|| {
                        format!("Failed to flush output file {}", outpath.display())
                    })?;

                    #[cfg(unix)]
                    if let Some(mode) = zip_file.unix_mode() {
                        fs::set_permissions(&outpath, fs::Permissions::from_mode(mode))
                            .with_context(|| {
                                format!("Failed to set permissions on {}", outpath.display())
                            })?;
                    }
                }

                pb_data.inc(1);
                Ok(())
                // Errors propagate through try_for_each; no run2.store(false)
                // here — that is solely the Ctrl+C handler's responsibility.
            },
        )?;

        pb_data.finish_with_message("✅ done");
        Ok(())
        // Same as Thread 1: no run2.store(false) on error at the outer level.
    });

    let setup_result = handle_setup
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("MojoSetup thread panicked")));
    let data_result = handle_data
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Game data thread panicked")));

    // Only treat as a clean cancel if the user actually hit Ctrl+C.
    // If threads failed for other reasons, fall through to the error match
    // so the real error message is surfaced rather than a misleading "Cancelled".
    if user_cancelled.load(Ordering::Relaxed) {
        return Ok(false);
    }

    match (setup_result, data_result) {
        (Ok(()), Ok(())) => {}
        (Err(e), Ok(())) => return Err(e).context("MojoSetup extraction failed"),
        (Ok(()), Err(e)) => return Err(e).context("Game data extraction failed"),
        (Err(setup_err), Err(data_err)) => {
            anyhow::bail!(
                "Extraction failed:\n  MojoSetup: {setup_err:#}\n  Game data: {data_err:#}"
            );
        }
    }

    finalize_output(&layout)?;
    staging_cleanup.disarm();

    Ok(true)
}
