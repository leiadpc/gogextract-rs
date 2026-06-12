use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crate::mojo::ensure_output_path_available;

pub fn list_lines(input_file: &std::path::Path) -> Result<Vec<String>> {
    probe_innoextract()?;
    let output = std::process::Command::new("innoextract")
        .arg("--list")
        .arg(input_file)
        .output()
        .context("Failed to run innoextract --list")?;

    let mut lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.to_owned())
        .collect();
    // innoextract sends info/warnings to stderr too
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        if !line.trim().is_empty() {
            lines.push(line.to_owned());
        }
    }
    Ok(lines)
}

// Update the existing list() to delegate:
pub fn list(input_file: &std::path::Path) -> Result<bool> {
    probe_innoextract()?;
    for line in list_lines(input_file)? {
        println!("{line}");
    }
    Ok(true)
}

pub fn extract(
    input_file: &std::path::Path,
    output_dir: Option<PathBuf>,
    force: bool,
    running: &Arc<AtomicBool>,
) -> Result<bool> {
    probe_innoextract()?;

    // Resolve output directory.
    let resolved_output_dir = output_dir.unwrap_or_else(|| {
        let base = input_file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        base.join(
            input_file
                .file_stem()
                .unwrap_or_else(|| std::ffi::OsStr::new("extracted_game_data")),
        )
    });

    ensure_output_path_available(&resolved_output_dir, force)?;

    // If --force, remove the existing directory first so innoextract starts clean.
    if force && resolved_output_dir.exists() {
        fs::remove_dir_all(&resolved_output_dir).with_context(|| {
            format!(
                "Failed to remove existing output directory {}",
                resolved_output_dir.display()
            )
        })?;
    }

    fs::create_dir_all(&resolved_output_dir).with_context(|| {
        format!(
            "Failed to create output directory {}",
            resolved_output_dir.display()
        )
    })?;

    println!("Detected Inno Setup installer — using innoextract.");
    println!("Extracting to: {}\n", resolved_output_dir.display());
    println!("Press Ctrl+C to cancel.\n");

    // Spawn innoextract.  --output-dir puts files directly where we want them.
    let mut child = std::process::Command::new("innoextract")
        .arg("--output-dir")
        .arg(&resolved_output_dir)
        .arg(input_file)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn innoextract")?;

    let child_stdout = child.stdout.take().expect("stdout piped");
    let child_stderr = child.stderr.take().expect("stderr piped");

    // -------------------------------------------------------------------------
    // Both stdout and stderr are pumped into a single mpsc channel so the main
    // thread can drive the spinner and poll cancellation without select().
    // -------------------------------------------------------------------------

    enum Event {
        /// A filename line from stdout — shown as the spinner message.
        Filename(String),
        /// A stderr line (warnings / info) — printed above the spinner.
        Stderr(String),
    }

    let (tx, rx) = mpsc::channel::<Event>();

    // Stdout thread: newline-delimited, BufReader::lines() is fine.
    let tx_out = tx.clone();
    let stdout_thread = std::thread::spawn(move || {
        let reader = BufReader::new(child_stdout);
        for line in reader.lines().map_while(|l| l.ok()) {
            let _ = tx_out.send(Event::Filename(line));
        }
    });

    // Stderr thread: forward all lines as Stderr events.
    let tx_err = tx;
    let stderr_thread = std::thread::spawn(move || {
        let reader = BufReader::new(child_stderr);
        for line in reader.lines().map_while(|l| l.ok()) {
            let _ = tx_err.send(Event::Stderr(line));
        }
    });

    // -------------------------------------------------------------------------
    // Spinner — ticks independently; message shows the current filename.
    // -------------------------------------------------------------------------

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.green.bold} Extracting  {msg:.dim}").unwrap(),
    );
    pb.enable_steady_tick(Duration::from_millis(100));

    for event in rx {
        if !running.load(Ordering::Relaxed) {
            pb.abandon_with_message("cancelled");
            let _ = child.kill();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            if resolved_output_dir.exists() {
                let _ = fs::remove_dir_all(&resolved_output_dir);
            }
            return Ok(false);
        }

        match event {
            Event::Filename(name) => {
                // Trim innoextract's leading "  - " decoration if present.
                let display = name.trim().trim_start_matches('-').trim();
                pb.set_message(display.to_owned());
            }
            Event::Stderr(line) => {
                // Warnings/info: print above the spinner without disturbing it.
                pb.println(&line);
            }
        }
    }

    let _ = stdout_thread.join();
    let _ = stderr_thread.join();

    let status = child.wait().context("Failed to wait for innoextract")?;
    if !status.success() {
        // On Windows, Ctrl-C causes innoextract to exit with STATUS_CONTROL_C_EXIT
        // (0xC000013A = -1073741510) before our kill() even fires.  Treat it as
        // a clean cancellation rather than an error.  Also catch the case where
        // the running flag was cleared (Ctrl-C raced with the last event).
        let code = status.code().unwrap_or(0) as i32;
        if code == -1073741510i32 || !running.load(std::sync::atomic::Ordering::Relaxed) {
            pb.abandon_with_message("cancelled");
            if resolved_output_dir.exists() {
                let _ = fs::remove_dir_all(&resolved_output_dir);
            }
            return Ok(false);
        }
        pb.abandon_with_message("failed");
        anyhow::bail!("innoextract exited with status {code}");
    }

    pb.finish_with_message("done");
    Ok(true)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn probe_innoextract() -> Result<()> {
    let probe = std::process::Command::new("innoextract")
        .arg("--version")
        .output();
    if probe.is_err() || !probe.unwrap().status.success() {
        anyhow::bail!(
            "innoextract not found on PATH.\n\
             Install it (e.g. `apt install innoextract`, `brew install innoextract`, \
             or download from https://constexpr.org/innoextract/) and try again."
        );
    }
    Ok(())
}
#[cfg(feature = "gui")]
use crate::gui::GuiEvent;
#[cfg(feature = "gui")]
#[cfg(feature = "gui")]
pub fn extract_gui(
    input_file: &std::path::Path,
    output_dir: Option<std::path::PathBuf>,
    force: bool,
    running: &Arc<AtomicBool>,
    tx: &mpsc::Sender<GuiEvent>,
) -> Result<bool> {
    probe_innoextract()?;

    let resolved_output_dir = output_dir.unwrap_or_else(|| {
        let base = input_file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        base.join(
            input_file
                .file_stem()
                .unwrap_or_else(|| std::ffi::OsStr::new("extracted_game_data")),
        )
    });

    ensure_output_path_available(&resolved_output_dir, force)?;

    if force && resolved_output_dir.exists() {
        fs::remove_dir_all(&resolved_output_dir).with_context(|| {
            format!(
                "Failed to remove existing output directory {}",
                resolved_output_dir.display()
            )
        })?;
    }

    fs::create_dir_all(&resolved_output_dir).with_context(|| {
        format!(
            "Failed to create output directory {}",
            resolved_output_dir.display()
        )
    })?;

    let _ = tx.send(GuiEvent::Log(format!(
        "Extracting to: {}",
        resolved_output_dir.display()
    )));

    let mut child = std::process::Command::new("innoextract")
        .arg("--output-dir")
        .arg(&resolved_output_dir)
        .arg(input_file)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn innoextract")?;

    let child_stdout = child.stdout.take().expect("stdout piped");
    let child_stderr = child.stderr.take().expect("stderr piped");

    enum Event {
        Filename(String),
        Stderr(String),
    }

    let (etx, erx) = mpsc::channel::<Event>();

    let etx_out = etx.clone();
    let stdout_thread = std::thread::spawn(move || {
        let reader = BufReader::new(child_stdout);
        for line in reader.lines().map_while(|l| l.ok()) {
            let _ = etx_out.send(Event::Filename(line));
        }
    });

    let etx_err = etx;
    let stderr_thread = std::thread::spawn(move || {
        let reader = BufReader::new(child_stderr);
        for line in reader.lines().map_while(|l| l.ok()) {
            let _ = etx_err.send(Event::Stderr(line));
        }
    });

    let start = std::time::Instant::now();

    for event in erx {
        if !running.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            if resolved_output_dir.exists() {
                let _ = fs::remove_dir_all(&resolved_output_dir);
            }
            return Ok(false);
        }

        match event {
            Event::Filename(name) => {
                let display = name.trim().trim_start_matches('-').trim().to_owned();
                if !display.is_empty() {
                    let _ = tx.send(GuiEvent::Filename(display));
                }
            }
            Event::Stderr(line) => {
                let _ = tx.send(GuiEvent::Log(line));
            }
        }
    }

    let _ = stdout_thread.join();
    let _ = stderr_thread.join();

    let status = child.wait().context("Failed to wait for innoextract")?;
    if !status.success() {
        let code = status.code().unwrap_or(0) as i32;
        if code == -1073741510i32 || !running.load(Ordering::Relaxed) {
            if resolved_output_dir.exists() {
                let _ = fs::remove_dir_all(&resolved_output_dir);
            }
            return Ok(false);
        }
        anyhow::bail!("innoextract exited with status {code}");
    }

    let elapsed = start.elapsed().as_secs_f64();
    let _ = tx.send(GuiEvent::Done {
        elapsed_secs: elapsed,
        total_mib: 0.0, // innoextract doesn't expose byte totals easily
        file_count: 0,
    });
    Ok(true)
}
