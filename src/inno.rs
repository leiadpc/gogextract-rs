use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Windows: suppress the console window that would otherwise appear when
// spawning innoextract from a GUI (windowless) process.
// ---------------------------------------------------------------------------

trait NoConsoleWindow {
    fn no_console_window(&mut self) -> &mut Self;
}

impl NoConsoleWindow for std::process::Command {
    fn no_console_window(&mut self) -> &mut Self {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW (0x08000000) prevents Windows from allocating a
            // new console for the child process when the parent has none.
            self.creation_flags(0x08000000);
        }
        self
    }
}

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

    let resolved_output_dir = output_dir.unwrap_or_else(|| crate::default_output_dir(input_file));

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

    println!("Detected Inno Setup installer — using innoextract.");
    println!("Extracting to: {}\n", resolved_output_dir.display());
    println!("Press Ctrl+C to cancel.\n");

    let mut child = std::process::Command::new("innoextract")
        .arg("--output-dir")
        .arg(&resolved_output_dir)
        .arg(input_file)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .no_console_window()
        .spawn()
        .context("Failed to spawn innoextract")?;

    let child_stdout = child.stdout.take().expect("stdout piped");
    let child_stderr = child.stderr.take().expect("stderr piped");

    // Both stdout and stderr are pumped into a single mpsc channel so the main
    // thread can drive the spinner and poll cancellation without select().
    enum Event {
        /// A filename line from stdout — shown as the spinner message.
        Filename(String),
        /// A stderr line (warnings / info) — printed above the spinner.
        Stderr(String),
    }

    let (tx, rx) = mpsc::channel::<Event>();

    let tx_out = tx.clone();
    let stdout_thread = std::thread::spawn(move || {
        let reader = BufReader::new(child_stdout);
        for line in reader.lines().map_while(|l| l.ok()) {
            let _ = tx_out.send(Event::Filename(line));
        }
    });

    let tx_err = tx;
    let stderr_thread = std::thread::spawn(move || {
        let reader = BufReader::new(child_stderr);
        for line in reader.lines().map_while(|l| l.ok()) {
            let _ = tx_err.send(Event::Stderr(line));
        }
    });

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
                let display = name.trim().trim_start_matches('-').trim();
                pb.set_message(display.to_owned());
            }
            Event::Stderr(line) => {
                pb.println(&line);
            }
        }
    }

    let _ = stdout_thread.join();
    let _ = stderr_thread.join();

    let status = child.wait().context("Failed to wait for innoextract")?;
    if !status.success() {
        // On Windows, Ctrl-C causes innoextract to exit with STATUS_CONTROL_C_EXIT
        // (0xC000013A = -1073741510) before our kill() even fires.
        let code = status.code().unwrap_or(0) as i32;
        if code == -1073741510i32 || !running.load(Ordering::Relaxed) {
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
    let ok = std::process::Command::new("innoextract")
        .arg("--version")
        .no_console_window()
        .output()
        .map_or(false, |o| o.status.success());
    if !ok {
        anyhow::bail!(
            "innoextract not found on PATH.\n\
             Install it (e.g. `apt install innoextract`, `brew install innoextract`, \
             or download from https://constexpr.org/innoextract/) and try again."
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// GUI extraction path
// ---------------------------------------------------------------------------

#[cfg(feature = "gui")]
use crate::gui::GuiEvent;

#[cfg(feature = "gui")]
pub fn extract_gui(
    input_file: &std::path::Path,
    output_dir: Option<std::path::PathBuf>,
    force: bool,
    running: &Arc<AtomicBool>,
    tx: &mpsc::Sender<GuiEvent>,
) -> Result<bool> {
    probe_innoextract()?;

    let resolved_output_dir = output_dir.unwrap_or_else(|| crate::default_output_dir(input_file));

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
        .no_console_window()
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

    // Count files by scanning stdout lines so the Done event carries real stats.
    // innoextract prints one "  - <path>" line per extracted file.
    let extraction_start = std::time::Instant::now();
    let mut file_count: usize = 0;

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
                    file_count += 1;
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

    let elapsed = extraction_start.elapsed().as_secs_f64();
    let _ = tx.send(GuiEvent::Done {
        elapsed_secs: elapsed,
        // innoextract doesn't expose byte totals; report what we have.
        total_mib: 0.0,
        file_count,
    });
    Ok(true)
}
