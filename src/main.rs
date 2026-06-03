use std::env;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ── Script metadata ──────────────────────────────────────────────────────────

struct ScriptMetadata {
    /// Number of lines in the Makeself shell script header.
    script_line_count: usize,
    /// Byte size of the embedded MojoSetup archive, as declared in the script.
    mojosetup_size: u64,
}

fn extract_number_after_marker(text: &str, marker: &str) -> Option<u64> {
    let start = text.find(marker)? + marker.len();
    let digits: String = text[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Parse both pieces of metadata from the script text in a single pass,
/// returning an error if either marker is absent.
fn parse_script_metadata(script: &str) -> io::Result<ScriptMetadata> {
    let script_line_count = extract_number_after_marker(script, "offset=`head -n ")
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Could not find Makeself script offset line",
            )
        })?
        as usize;

    let mojosetup_size = extract_number_after_marker(script, "filesizes=\"").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Could not find MojoSetup archive size",
        )
    })?;

    Ok(ScriptMetadata {
        script_line_count,
        mojosetup_size,
    })
}

// ── Archive format detection ─────────────────────────────────────────────────

enum ArchiveKind {
    Zip,
    TarGz,
    Unknown,
}

impl ArchiveKind {
    fn extension(&self) -> &'static str {
        match self {
            ArchiveKind::Zip => "zip",
            ArchiveKind::TarGz => "tar.gz",
            ArchiveKind::Unknown => "bin",
        }
    }
}

/// Sniff the first two bytes of a file to determine its archive format.
fn detect_archive_kind(file: &mut File) -> io::Result<ArchiveKind> {
    let mut magic = [0u8; 2];
    let n = file.read(&mut magic)?;
    file.seek(SeekFrom::Current(-(n as i64)))?;

    let kind = match magic {
        [0x50, 0x4B] => ArchiveKind::Zip,   // PK
        [0x1F, 0x8B] => ArchiveKind::TarGz, // gzip
        _ => {
            // (#12) Warn rather than silently writing an unrecognised format.
            eprintln!(
                "Warning: unrecognised archive magic bytes ({:#04x}, {:#04x}), writing as .bin",
                magic[0], magic[1]
            );
            ArchiveKind::Unknown
        }
    };

    Ok(kind)
}

// ── Progress display ─────────────────────────────────────────────────────────

/// Format a byte count as a human-readable string.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;
    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{} {}", bytes, UNITS[unit_index])
    } else {
        format!("{:.2} {}", size, UNITS[unit_index])
    }
}

/// Format a transfer speed, preserving the fractional part.
///
/// (#11) Accepts f64 directly instead of truncating to u64 first, so speeds
/// like 1.8 MB/s are not incorrectly displayed as 1.00 MB/s.
fn format_speed(bytes_per_sec: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes_per_sec;
    let mut unit_index = 0;
    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{:.0} B/s", size)
    } else {
        format!("{:.2} {}/s", size, UNITS[unit_index])
    }
}

fn print_progress(label: &str, copied: u64, total: u64, elapsed: Duration) {
    let percentage = if total > 0 {
        copied as f64 / total as f64 * 100.0
    } else {
        100.0
    };

    let bar_width: usize = 30;
    let filled = ((percentage / 100.0) * bar_width as f64).round() as usize;
    let empty = bar_width.saturating_sub(filled);
    let bar = format!("{}{}", "#".repeat(filled), "-".repeat(empty));

    // Pad label to a fixed width so the bar doesn't jump around between files.
    let secs = elapsed.as_secs_f64();
    let speed = if secs > 0.0 {
        format_speed(copied as f64 / secs)
    } else {
        "---".to_string()
    };

    print!(
        "\r{:<35} [{}] {:>6.2}% ({}/{}) {:>12}",
        label,
        bar,
        percentage,
        format_bytes(copied),
        format_bytes(total),
        speed,
    );
    let _ = io::stdout().flush();
}

fn copy_with_progress<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
    total_size: u64,
    label: &str,
) -> io::Result<u64> {
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut copied = 0u64;
    let start = Instant::now();
    let mut last_update = start;
    let update_interval = Duration::from_millis(100);

    print_progress(label, copied, total_size, start.elapsed());

    loop {
        let bytes_read = reader.read(&mut buffer).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed while reading source data for '{}': {}", label, e),
            )
        })?;

        if bytes_read == 0 {
            break;
        }

        writer.write_all(&buffer[..bytes_read]).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed while writing output data for '{}': {}", label, e),
            )
        })?;

        copied += bytes_read as u64;

        if last_update.elapsed() >= update_interval {
            print_progress(label, copied, total_size, start.elapsed());
            last_update = Instant::now();
        }
    }

    // (#9) One final print to guarantee the 100% line is shown, then newline.
    // The in-loop condition no longer special-cases `copied == total_size`, so
    // this single call is the only end-of-copy render.
    print_progress(label, copied, total_size, start.elapsed());
    println!();

    Ok(copied)
}

// ── File helpers ─────────────────────────────────────────────────────────────

/// Seek `file` to `pos`, attaching `ctx` to any error message.
///
/// (#5) Centralises the repetitive seek + map_err pattern.
fn seek_to(file: &mut File, pos: SeekFrom, ctx: &str) -> io::Result<u64> {
    file.seek(pos)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", ctx, e)))
}

fn open_input_file(input_path: &str) -> io::Result<File> {
    File::open(input_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Failed to open input file '{}': {}", input_path, e),
        )
    })
}

fn create_output_file(path: &Path) -> io::Result<File> {
    // Warn rather than silently overwrite.
    if path.exists() {
        eprintln!(
            "Warning: output file '{}' already exists and will be overwritten",
            path.display()
        );
    }
    File::create(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Failed to create output file '{}': {}", path.display(), e),
        )
    })
}

fn get_file_size(input_path: &str) -> io::Result<u64> {
    let metadata = fs::metadata(input_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Failed to read metadata for '{}': {}", input_path, e),
        )
    })?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Input path '{}' is not a regular file", input_path),
        ));
    }
    Ok(metadata.len())
}

fn ensure_output_dir(output_path: &str) -> io::Result<()> {
    fs::create_dir_all(output_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Failed to create output directory '{}': {}", output_path, e),
        )
    })
}

// ── Main logic ───────────────────────────────────────────────────────────────

fn run() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();

    let (input_path, output_path) = match args.len() {
        2 => (args[1].as_str(), "./"),
        3 => (args[1].as_str(), args[2].as_str()),
        _ => {
            eprintln!("Usage: {} <input_file> [output_dir]", args[0]);
            std::process::exit(1);
        }
    };

    ensure_output_dir(output_path)?;

    // (#5) output_dir is derived immediately from args so it's available
    // throughout run() without a late PathBuf::from() call.
    let output_dir = PathBuf::from(output_path);

    let total_input_size = get_file_size(input_path)?;
    let mut game_bin = open_input_file(input_path)?;

    // ── Read the script header ───────────────────────────────────────────────
    //
    // Makeself installers embed a shell script at the top of the file.  We
    // read the first 10 KB with read_exact; this is a safe upper bound for the
    // script text that contains the two markers we need.  If the file is
    // smaller than 10 KB (i.e. not a real installer) we fail early.
    //
    // HEADER_BUF_SIZE must be large enough to contain both markers; 10 KB has
    // proven sufficient for all known Makeself versions.
    const HEADER_BUF_SIZE: usize = 10_240;

    let mut beginning_buf = vec![0u8; HEADER_BUF_SIZE];
    game_bin.read_exact(&mut beginning_buf).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "Failed to read initial {} bytes from '{}' (is this a valid Makeself installer?): {}",
                HEADER_BUF_SIZE, input_path, e
            ),
        )
    })?;

    // (#2) Reject non-UTF-8 headers rather than silently replacing bytes.
    // A valid Makeself shell script must be valid UTF-8; garbled bytes almost
    // certainly mean this is not the right kind of file.
    let beginning = std::str::from_utf8(&beginning_buf).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Script header in '{}' is not valid UTF-8 (offset {}); \
                 is this a Makeself installer?",
                input_path,
                e.valid_up_to()
            ),
        )
    })?;

    let meta = parse_script_metadata(beginning)?;

    // ── Determine the exact byte size of the script ──────────────────────────

    seek_to(
        &mut game_bin,
        SeekFrom::Start(0),
        &format!("Failed to seek to start of '{}'", input_path),
    )?;

    let (script_size, mut game_bin) = {
        use std::io::BufRead;

        let mut reader = BufReader::new(game_bin);
        let mut line = Vec::new();

        for _ in 0..meta.script_line_count {
            line.clear();
            let bytes = reader.read_until(b'\n', &mut line).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("Failed while reading script lines from '{}': {}", input_path, e),
                )
            })?;
            if bytes == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Reached EOF before reading expected script lines",
                ));
            }
        }

        let script_size = reader.stream_position().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed to determine script size for '{}': {}", input_path, e),
            )
        })?;

        (script_size, reader.into_inner())
    };

    println!("Makeself script size:      {}", format_bytes(script_size));

    // ── Save the unpacker script ─────────────────────────────────────────────

    seek_to(
        &mut game_bin,
        SeekFrom::Start(0),
        &format!("Failed to seek back to start of '{}'", input_path),
    )?;

    // script_size fits in usize on any platform where the script can be mapped;
    // on 32-bit this is theoretically fallible (scripts > 4 GB), but that is
    // not a realistic scenario for a shell script header.
    let script_len: usize = script_size.try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Script size exceeds addressable memory (only possible on 32-bit platforms)",
        )
    })?;

    let mut script_bin = vec![0u8; script_len];
    game_bin.read_exact(&mut script_bin).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Failed to read unpacker script from '{}': {}", input_path, e),
        )
    })?;

    let unpacker_path = output_dir.join("unpacker.sh");
    {
        let mut unpacker_file = create_output_file(&unpacker_path)?;
        unpacker_file.write_all(&script_bin).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed to write '{}': {}", unpacker_path.display(), e),
            )
        })?;
    }
    // script_bin is no longer needed; free it before the large copies below.
    drop(script_bin);

    println!("Wrote {}", unpacker_path.display());

    // ── Compute archive offsets ──────────────────────────────────────────────

    println!(
        "MojoSetup archive size:    {}",
        format_bytes(meta.mojosetup_size)
    );

    let data_offset = script_size
        .checked_add(meta.mojosetup_size)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Calculated data offset overflowed u64",
            )
        })?;

    if data_offset >= total_input_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Calculated data offset {} is at or beyond the end of input file (size {}); \
                 the file may be corrupt or truncated",
                data_offset, total_input_size
            ),
        ));
    }

    let data_size = total_input_size - data_offset;
    println!("Game data archive size:    {}", format_bytes(data_size));

    // ── Extract the MojoSetup archive ────────────────────────────────────────

    seek_to(
        &mut game_bin,
        SeekFrom::Start(script_size),
        &format!("Failed to seek to MojoSetup archive in '{}'", input_path),
    )?;

    let mojosetup_path = output_dir.join("mojosetup.tar.gz");
    let mut mojosetup_file = create_output_file(&mojosetup_path)?;

    {
        let limited_reader = std::io::Read::by_ref(&mut game_bin).take(meta.mojosetup_size);
        copy_with_progress(
            limited_reader,
            &mut mojosetup_file,
            meta.mojosetup_size,
            "Extracting mojosetup.tar.gz",
        )
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed to extract '{}': {}", mojosetup_path.display(), e),
            )
        })?;
    }

    // ── Extract the game data archive ────────────────────────────────────────

    seek_to(
        &mut game_bin,
        SeekFrom::Start(data_offset),
        &format!("Failed to seek to game data archive in '{}'", input_path),
    )?;

    // Sniff the magic bytes to choose the right extension.
    let kind = detect_archive_kind(&mut game_bin)?;
    let data_filename = format!("data.{}", kind.extension());
    let data_path = output_dir.join(&data_filename);
    let mut data_file = create_output_file(&data_path)?;

    let label = format!("Extracting {}", data_filename);
    copy_with_progress(&mut game_bin, &mut data_file, data_size, &label).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Failed to extract '{}': {}", data_path.display(), e),
        )
    })?;

    println!("Done.");
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}