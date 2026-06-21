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
    // NOTE: list_lines deliberately does NOT call probe_innoextract() itself.
    // All callers (list, extract, extract_gui) already call probe_innoextract()
    // before reaching here. Adding a probe here would cause a redundant
    // subprocess launch for callers that go list_lines → extract path.
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

// ---------------------------------------------------------------------------
// Progress sink abstraction
//
// extract() and extract_gui() differ only in how progress/log lines are
// displayed (an indicatif ProgressBar vs an mpsc channel of GuiEvents) and in
// a couple of one-off log lines around the shared core. `run_innoextract_inner`
// owns spawning innoextract, pumping its stdout/stderr, polling cancellation,
// and waiting on the exit status; `extract`/`extract_gui` are thin wrappers.
// ---------------------------------------------------------------------------

/// Outcome of a completed (non-cancelled, non-error) innoextract run.
struct InnoRunOutcome {
    file_count: u64,
}

trait InnoSink {
    /// A line of innoextract stderr output (warnings / info).
    fn stderr_line(&self, line: &str);
    /// One more file has been extracted; `name` is the cleaned-up filename
    /// from innoextract's stdout ("  - path/to/file" with the prefix stripped).
    fn file_done(&self, files_done: u64, name: &str);
}

/// Runs `innoextract --output-dir <resolved_output_dir> <input_file>`,
/// forwarding progress to `sink` and polling `running` for cancellation.
///
/// Returns `Ok(Some(outcome))` on success, `Ok(None)` if cancelled (the
/// caller is responsible for removing `resolved_output_dir` in that case,
/// since CLI and GUI clean it up at slightly different points), or `Err` on
/// a genuine failure.
fn run_innoextract_inner(
    input_file: &std::path::Path,
    resolved_output_dir: &std::path::Path,
    running: &Arc<AtomicBool>,
    sink: &dyn InnoSink,
) -> Result<Option<InnoRunOutcome>> {
    let mut child = std::process::Command::new("innoextract")
        .arg("--output-dir")
        .arg(resolved_output_dir)
        .arg(input_file)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .no_console_window()
        .spawn()
        .context("Failed to spawn innoextract")?;

    let child_stdout = child.stdout.take().expect("stdout piped");
    let child_stderr = child.stderr.take().expect("stderr piped");

    // Both stdout and stderr are pumped into a single mpsc channel so the main
    // thread can drive progress and poll cancellation without select().
    enum Event {
        /// A filename line from stdout — shown as the progress message.
        ///
        /// innoextract prints one line per extracted file in the form:
        ///   "  - path/to/file"
        /// We strip the leading whitespace and dash prefix so the bar shows
        /// a clean filename. The trim chain is:
        ///   1. trim()                  — removes surrounding whitespace
        ///   2. trim_start_matches('-') — removes the leading dash
        ///   3. trim()                  — removes the space left between dash and path
        Filename(String),
        /// A stderr line (warnings / info).
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

    let mut file_count: u64 = 0;

    for event in rx {
        if !running.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Ok(None);
        }

        match event {
            Event::Filename(name) => {
                let display = name.trim().trim_start_matches('-').trim();
                if !display.is_empty() {
                    file_count += 1;
                    sink.file_done(file_count, display);
                }
            }
            Event::Stderr(line) => {
                sink.stderr_line(&line);
            }
        }
    }

    let _ = stdout_thread.join();
    let _ = stderr_thread.join();

    let status = child.wait().context("Failed to wait for innoextract")?;
    if !status.success() {
        // On Windows, Ctrl-C causes innoextract to exit with STATUS_CONTROL_C_EXIT
        // (0xC000013A = -1073741510) before our kill() even fires. On other
        // platforms a Ctrl-C-triggered exit is detected purely via `running`.
        // NOTE: cfg!(windows) is a *runtime* check — both sides of `&&` are
        // still compiled on every target — so STATUS_CONTROL_C_EXIT must be
        // defined unconditionally (see below) even though it's only ever
        // meaningful on Windows.
        let code = status.code().unwrap_or(0) as i32;
        let cancelled_exit =
            !running.load(Ordering::Relaxed) || (cfg!(windows) && code == STATUS_CONTROL_C_EXIT);

        if cancelled_exit {
            return Ok(None);
        }
        anyhow::bail!("innoextract exited with status {code}");
    }

    Ok(Some(InnoRunOutcome { file_count }))
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

    // Pre-scan with --list to get a file count so we can show a determinate
    // progress bar. --list reads headers only (no decompression) so it's fast.
    // If it fails for any reason we fall back to an indeterminate spinner.
    let known_file_count: Option<u64> = list_lines(input_file)
        .ok()
        .map(|lines| {
            // Count only lines that look like extracted files ("  - path/to/file").
            lines
                .iter()
                .filter(|l| l.trim_start().starts_with('-'))
                .count() as u64
        })
        .filter(|&n| n > 0);

    let extraction_start = std::time::Instant::now();

    // Build either a determinate bar (known total) or a spinner (unknown total).
    let pb = match known_file_count {
        Some(total) => {
            let pb = ProgressBar::new(total);
            pb.set_style(
                ProgressStyle::with_template(
                    "{spinner:.green.bold} Files [{bar:40.green/white.dim}] \
                     {pos}/{len}  {msg:.dim}",
                )
                .unwrap()
                .progress_chars("█▇▆▅▄▃▂ "),
            );
            pb
        }
        None => {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::with_template(
                    "{spinner:.green.bold} Extracting  {pos} files  {msg:.dim}",
                )
                .unwrap(),
            );
            pb
        }
    };
    pb.enable_steady_tick(Duration::from_millis(100));

    let sink = CliInnoSink { pb: pb.clone() };
    let outcome = match run_innoextract_inner(input_file, &resolved_output_dir, running, &sink) {
        Ok(outcome) => outcome,
        Err(e) => {
            pb.abandon_with_message("failed");
            return Err(e);
        }
    };

    let Some(outcome) = outcome else {
        pb.abandon_with_message("cancelled");
        if resolved_output_dir.exists() {
            let _ = fs::remove_dir_all(&resolved_output_dir);
        }
        return Ok(false);
    };

    pb.finish_with_message("done");
    println!(
        "\n  {} files  in {:.1}s",
        outcome.file_count,
        extraction_start.elapsed().as_secs_f64()
    );
    Ok(true)
}

/// `InnoSink` impl backed by an indicatif console progress bar.
struct CliInnoSink {
    pb: ProgressBar,
}

impl InnoSink for CliInnoSink {
    fn stderr_line(&self, line: &str) {
        self.pb.println(line);
    }

    fn file_done(&self, files_done: u64, name: &str) {
        self.pb.set_message(name.to_owned());
        self.pb.set_position(files_done);
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Windows NTSTATUS code delivered when the process receives Ctrl-C
/// (STATUS_CONTROL_C_EXIT = 0xC000013A). Rust represents it as a negative
/// i32 because NTSTATUS values use the high bit to indicate severity.
/// Defined unconditionally (not `#[cfg(windows)]`) because it's compared
/// against at runtime via `cfg!(windows)`, which doesn't strip the other
/// branch at compile time the way `#[cfg(windows)]` would.
const STATUS_CONTROL_C_EXIT: i32 = -1073741510;

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

    // Pre-scan the file list so the GUI has a denominator for the file progress
    // bar. innoextract --list is fast (reads headers only, no decompression).
    // If it fails for any reason we fall back to an indeterminate display.
    let known_file_count: Option<u64> = list_lines(input_file)
        .ok()
        .map(|lines| {
            lines
                .iter()
                .filter(|l| l.trim_start().starts_with('-'))
                .count() as u64
        })
        .filter(|&n| n > 0);

    let _ = tx.send(GuiEvent::Log(format!(
        "Detected {} file(s) to extract.",
        known_file_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown number of".to_owned())
    )));

    let extraction_start = std::time::Instant::now();
    let sink = GuiInnoSink {
        tx: tx.clone(),
        files_total: known_file_count.unwrap_or(0),
    };

    let outcome = run_innoextract_inner(input_file, &resolved_output_dir, running, &sink)?;

    let Some(outcome) = outcome else {
        if resolved_output_dir.exists() {
            let _ = fs::remove_dir_all(&resolved_output_dir);
        }
        // Return Ok(false) — the caller (installer_worker_loop's thread) maps
        // this to GuiEvent::Cancelled; we don't send it here to keep
        // the cancellation path consistent with the mojo extract_gui path.
        return Ok(false);
    };

    let elapsed = extraction_start.elapsed().as_secs_f64();
    let _ = tx.send(GuiEvent::Done {
        elapsed_secs: elapsed,
        file_count: outcome.file_count as usize,
        output_dir: resolved_output_dir,
    });
    Ok(true)
}

/// `InnoSink` impl backed by an `mpsc::Sender<GuiEvent>`.
#[cfg(feature = "gui")]
struct GuiInnoSink {
    tx: mpsc::Sender<GuiEvent>,
    /// Captured once before the run starts — innoextract doesn't report a
    /// per-event total, so each `file_done` call needs this to populate
    /// `GuiEvent::Progress::files_total`.
    files_total: u64,
}

#[cfg(feature = "gui")]
impl InnoSink for GuiInnoSink {
    fn stderr_line(&self, line: &str) {
        let _ = self.tx.send(GuiEvent::Log(line.to_owned()));
    }

    fn file_done(&self, files_done: u64, name: &str) {
        // Combine filename + counter into a single Progress event so the GUI
        // fields are always in sync (see GuiEvent::Progress).
        let _ = self.tx.send(GuiEvent::Progress {
            files_done,
            files_total: self.files_total,
            current_file: Some(name.to_owned()),
        });
    }
}
