use anyhow::{Context, Result};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rayon::prelude::*;
use regex::bytes::Regex;
use std::fs::{self, File};
use std::io::{self, BufRead, BufWriter, Cursor, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const HEADER_PEEK_SIZE: usize = 10 * 1024;
const COPY_BUFFER_SIZE: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Thread-Safe Shared Memory-Map Wrapper
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ArcMmap(Arc<memmap2::Mmap>);

// Allows std::io::Cursor to wrap our shared memory map directly
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
// Metadata parsing
// ---------------------------------------------------------------------------

struct PackageMetadata {
    script_size: u64,
    mojosetup_size: u64,
    script_bytes: Vec<u8>,
}

// Parses metadata entirely out of the shared memory map with zero disk I/O
fn parse_metadata(mmap: &[u8]) -> Result<PackageMetadata> {
    let peek_size = std::cmp::min(HEADER_PEEK_SIZE, mmap.len());
    let peek_slice = &mmap[..peek_size];

    let offset_re = Regex::new(r#"offset=`head -n (\d+?) "\$0""#)?;
    let offset_caps = offset_re
        .captures(peek_slice)
        .context("Could not find 'offset' metadata")?;
    let script_line_count: u64 = std::str::from_utf8(
        offset_caps
            .get(1)
            .context("Missing capture group in offset")?
            .as_bytes(),
    )?
    .parse()?;

    let filesize_re = Regex::new(r#"filesizes="(\d+?)""#)?;
    let filesize_caps = filesize_re
        .captures(peek_slice)
        .context("Could not find 'filesizes' metadata")?;
    let mojosetup_size: u64 = std::str::from_utf8(
        filesize_caps
            .get(1)
            .context("Missing capture group in filesize")?
            .as_bytes(),
    )?
    .parse()?;

    // Read the script lines straight out of memory via Cursor — no heap buffer needed
    // since the data is already in memory via mmap. BufReader would add a pointless
    // heap-allocated staging buffer on top of data that's already random-access.
    let mut cursor = Cursor::new(mmap);
    let mut script_bytes: Vec<u8> = Vec::new();
    let mut line = Vec::new();

    for _ in 0..script_line_count {
        line.clear();
        if cursor.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        script_bytes.extend_from_slice(&line);
    }

    Ok(PackageMetadata {
        script_size: script_bytes.len() as u64,
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
    let running = Arc::new(AtomicBool::new(true));

    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    // Global Memory-Map Initialization: File is opened and mapped exactly once
    let file = File::open(&args.input_file).context("Failed to open input file")?;

    // Safeguard for 32-bit systems
    if cfg!(target_pointer_width = "32") && file.metadata()?.len() > (u32::MAX as u64) {
        anyhow::bail!("File is too large to memory map on a 32-bit architecture.");
    }

    let raw_mmap = unsafe { memmap2::Mmap::map(&file).context("Failed to memory-map input file")? };
    let mmap = ArcMmap(Arc::new(raw_mmap));

    fs::create_dir_all(&args.output_dir)?;

    // Parse metadata directly from the memory map
    let meta = parse_metadata(mmap.as_ref())?;
    let unpacker_path = args.output_dir.join("unpacker.sh");
    fs::write(&unpacker_path, &meta.script_bytes)?;

    // Cache the sizes needed for the setup extraction thread
    let script_size = meta.script_size as usize;
    let mojosetup_size = meta.mojosetup_size as usize;

    // Drop the metadata to free up RAM immediately
    drop(meta);

    println!("Starting extraction. Press Ctrl+C to cancel...\n");
    let m = MultiProgress::new();

    // --- Thread 1: MojoSetup (Shared Memmap - Sequential Extraction) ---
    let (mmap_setup, out_dir, run1) = (mmap.clone(), args.output_dir.clone(), running.clone());

    let pb_setup = m.add(ProgressBar::new(mojosetup_size as u64));
    pb_setup.set_style(bytes_style("MojoSetup "));

    let handle_setup = thread::spawn(move || -> Result<()> {
        let end = script_size + mojosetup_size;
        if end > mmap_setup.0.len() {
            anyhow::bail!("MojoSetup metadata sizes exceed total package size");
        }

        // Zero-copy: Extract sub-slice containing only the MojoSetup compressed stream
        let compressed_slice = &mmap_setup.0[script_size..end];
        let reader = pb_setup.wrap_read(compressed_slice);
        let safe_reader = CancellableReader {
            inner: reader,
            running: &run1,
        };

        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(safe_reader));
        archive.unpack(out_dir.join("mojosetup"))?;
        pb_setup.finish_with_message("✅ done");
        Ok(())
    });

    // --- Thread 2: Game Data (Rayon Parallel Extraction) ---
    let (mmap_data, out_dir, run2) = (mmap.clone(), args.output_dir.clone(), running.clone());

    let pb_data = m.add(ProgressBar::new(0));
    pb_data.set_style(count_style("Game Data "));

    let handle_data = thread::spawn(move || -> Result<()> {
        // Parse central directory once to get the total file count
        let total_files = {
            let cursor = Cursor::new(mmap_data.clone());
            let archive = zip::ZipArchive::new(cursor)?;
            archive.len()
        };

        pb_data.set_length(total_files as u64);
        let game_dir = out_dir.join("game_data");

        // Rayon's work-stealing scheduler distributes entries automatically, handling
        // uneven file sizes far better than manual static chunks. Each worker picks
        // up the next available index when it finishes, so a thread stuck on a large
        // file doesn't block others.
        (0..total_files)
            .into_par_iter()
            .try_for_each(|i| -> Result<()> {
                if !run2.load(Ordering::Relaxed) {
                    anyhow::bail!("Cancelled");
                }

                // Each Rayon worker opens its own ZipArchive view into shared memory.
                // Construction only reads the central directory, so it's cheap, and
                // doing it here (rather than once per static chunk) lets Rayon
                // work-steal freely without any per-chunk archive lifetime issues.
                let cursor = Cursor::new(mmap_data.clone());
                let mut local_archive = zip::ZipArchive::new(cursor)?;

                let mut zip_file = local_archive.by_index(i)?;
                let Some(path) = zip_file.enclosed_name() else {
                    return Ok(());
                };
                let outpath = game_dir.join(&path);

                // Update the progress bar text occasionally to avoid high lock contention
                if i % 25 == 0 {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        pb_data.set_message(name.to_owned());
                    }
                }

                if zip_file.name().ends_with('/') {
                    fs::create_dir_all(&outpath)?;
                } else {
                    if let Some(p) = outpath.parent() {
                        fs::create_dir_all(p)?;
                    }

                    let outfile = File::create(&outpath)?;

                    // Pre-allocate disk space to prevent fragmentation
                    outfile.set_len(zip_file.size())?;

                    let mut writer = BufWriter::with_capacity(COPY_BUFFER_SIZE, outfile);
                    let mut safe_reader = CancellableReader {
                        inner: &mut zip_file,
                        running: &run2,
                    };
                    let mut buffer = [0u8; COPY_BUFFER_SIZE];

                    loop {
                        let bytes_read = safe_reader.read(&mut buffer)?;
                        if bytes_read == 0 {
                            break;
                        }
                        writer.write_all(&buffer[..bytes_read])?;
                    }
                    writer.flush()?;

                    #[cfg(unix)]
                    if let Some(mode) = zip_file.unix_mode() {
                        fs::set_permissions(&outpath, fs::Permissions::from_mode(mode))?;
                    }
                }

                // Indicatif handles parallel increments safely
                pb_data.inc(1);
                Ok(())
            })?;

        pb_data.finish_with_message("✅ done");
        Ok(())
    });

    // Join threads and safely process results
    let setup_result = handle_setup
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("MojoSetup thread panicked")));
    let data_result = handle_data
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Game data thread panicked")));

    let cancelled = !running.load(Ordering::Relaxed);
    let failed = setup_result.is_err() || data_result.is_err();

    if cancelled || failed {
        let _ = fs::remove_dir_all(args.output_dir.join("mojosetup"));
        let _ = fs::remove_dir_all(args.output_dir.join("game_data"));
        let _ = fs::remove_file(&unpacker_path);
    }

    if cancelled {
        return Ok(false);
    }

    setup_result.context("MojoSetup extraction failed")?;
    data_result.context("Game data extraction failed")?;

    Ok(true)
}
