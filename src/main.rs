use anyhow::{Context, Result};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use regex::Regex;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

const HEADER_PEEK_SIZE: usize = 10 * 1024;

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
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled by user"));
        }
        self.inner.read(buf)
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
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

// FIX #7: reuse a single BufReader instead of opening the file twice.
fn parse_metadata(path: &PathBuf) -> Result<PackageMetadata> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);

    // Peek at the header to extract numeric metadata via regex.
    let mut peek = vec![0u8; HEADER_PEEK_SIZE];
    let peeked = reader.read(&mut peek)?;
    let peek_str = String::from_utf8_lossy(&peek[..peeked]);

    let offset_re = Regex::new(r#"offset=`head -n (\d+?) "\$0""#)?;
    let script_line_count: u64 = offset_re
        .captures(&peek_str)
        .context("Could not find 'offset' metadata")?
        .get(1)
        .context("Missing capture group in offset")?
        .as_str()
        .parse()?;

    let filesize_re = Regex::new(r#"filesizes="(\d+?)""#)?;
    let mojosetup_size: u64 = filesize_re
        .captures(&peek_str)
        .context("Could not find 'filesizes' metadata")?
        .get(1)
        .context("Missing capture group in filesize")?
        .as_str()
        .parse()?;

    // Seek back to the start and read exactly `script_line_count` lines —
    // no second file open needed.
    reader.seek(SeekFrom::Start(0))?;
    let mut script_bytes: Vec<u8> = Vec::new();
    for _ in 0..script_line_count {
        let mut line = Vec::new();
        if reader.read_until(b'\n', &mut line)? == 0 {
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
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let args = Args::parse();
    let running = Arc::new(AtomicBool::new(true));

    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    fs::create_dir_all(&args.output_dir)?;
    let meta = parse_metadata(&args.input_file)?;
    let unpacker_path = args.output_dir.join("unpacker.sh");
    fs::write(&unpacker_path, &meta.script_bytes)?;

    println!("Starting extraction. Press Ctrl+C to cancel...\n");
    let m = MultiProgress::new();

    // --- Thread 1: MojoSetup ---
    let (input, out_dir, run1) = (
        args.input_file.clone(),
        args.output_dir.clone(),
        running.clone(),
    );

    // FIX #4: apply a styled progress bar that shows byte progress + ETA.
    let pb_setup = m.add(ProgressBar::new(meta.mojosetup_size));
    pb_setup.set_style(bytes_style("MojoSetup "));

    let handle_setup = thread::spawn(move || -> Result<()> {
        let mut file = File::open(&input)?;
        file.seek(SeekFrom::Start(meta.script_size))?;
        let reader = pb_setup.wrap_read((&mut file).take(meta.mojosetup_size));
        let safe_reader = CancellableReader {
            inner: reader,
            running: &run1,
        };

        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(safe_reader));
        archive.unpack(out_dir.join("mojosetup"))?;
        pb_setup.finish_with_message("✅ done");
        Ok(())
    });

    // --- Thread 2: Game Data ---
    let (input, out_dir, run2) = (
        args.input_file.clone(),
        args.output_dir.clone(),
        running.clone(),
    );

    // FIX #4: count-based bar that also shows the current filename.
    let pb_data = m.add(ProgressBar::new(0));
    pb_data.set_style(count_style("Game Data "));

    let handle_data = thread::spawn(move || -> Result<()> {
        let mut archive = zip::ZipArchive::new(File::open(&input)?)?;
        pb_data.set_length(archive.len() as u64);
        let game_dir = out_dir.join("game_data");

        for i in 0..archive.len() {
            if !run2.load(Ordering::Relaxed) {
                anyhow::bail!("Cancelled");
            }
            let mut zip_file = archive.by_index(i)?;
            let Some(path) = zip_file.enclosed_name() else {
                continue;
            };
            let outpath = game_dir.join(&path);

            // FIX #5: show the current filename in the progress bar message.
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                pb_data.set_message(name.to_owned());
            }

            if zip_file.name().ends_with('/') {
                fs::create_dir_all(&outpath)?;
            } else {
                if let Some(p) = outpath.parent() {
                    fs::create_dir_all(p)?;
                }
                let mut outfile = BufWriter::new(File::create(&outpath)?);
                let mut safe_reader = CancellableReader {
                    inner: &mut zip_file,
                    running: &run2,
                };
                io::copy(&mut safe_reader, &mut outfile)?;
            }
            pb_data.inc(1);
        }
        pb_data.finish_with_message("✅ done");
        Ok(())
    });

    // FIX #1: join threads before inspecting the cancellation flag, and
    // convert panics into proper anyhow errors instead of unwinding.
    let setup_result = handle_setup
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("MojoSetup thread panicked")));
    let data_result = handle_data
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Game data thread panicked")));

    // Now it is safe to check the flag — both threads have fully stopped.
    if !running.load(Ordering::Relaxed) {
        let _ = fs::remove_dir_all(args.output_dir.join("mojosetup"));
        let _ = fs::remove_dir_all(args.output_dir.join("game_data"));
        let _ = fs::remove_file(&unpacker_path);
        println!("\n🚨 Cancelled! Cleaned up.");
        std::process::exit(130);
    }

    setup_result.context("MojoSetup extraction failed")?;
    data_result.context("Game data extraction failed")?;

    println!("\n🎉 Extraction complete!");
    Ok(())
}
