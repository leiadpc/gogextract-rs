#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]

#[cfg(feature = "gui")]
mod gui;
mod inno;
mod mojo;

use anyhow::{Context, Result};
use clap::Parser;
use mojo::ArcMmap;
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(feature = "gui")]
use std::sync::mpsc;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Installer file to extract (EXE, BIN, or SH script; omit to launch the GUI).
    input_file: Option<PathBuf>,
    /// Optional custom output directory. If omitted, a folder named after the
    /// input file (without extension) is created next to it.
    output_dir: Option<PathBuf>,
    /// Overwrite existing output files if they already exist.
    #[arg(long, short)]
    force: bool,
    /// List archive contents without extracting anything.
    #[arg(long, short)]
    list: bool,
    /// Launch the graphical interface instead of the CLI.
    #[cfg(feature = "gui")]
    #[arg(long, short = 'g')]
    gui: bool,
}

// ---------------------------------------------------------------------------
// Shared helper — deduplicates the default output-dir logic used in all four
// extract entry points (inno CLI, inno GUI, mojo CLI, mojo GUI).
// ---------------------------------------------------------------------------

pub fn default_output_dir(input: &Path) -> PathBuf {
    let base = input
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    base.join(
        input
            .file_stem()
            .unwrap_or(OsStr::new("extracted_game_data")),
    )
}

// ---------------------------------------------------------------------------
// Installer type detection (shared by CLI and GUI worker thread)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum InstallerKind {
    Inno,
    MojoSetup,
}

pub fn detect_installer_kind(mmap: &ArcMmap) -> Result<InstallerKind> {
    let data = mmap.as_ref();

    // 1. Check for Inno Setup Windows installer signatures.
    //    Inno data is appended after the PE stub and can sit beyond 1 MiB on
    //    larger titles, so we scan up to 8 MiB.
    const INNO_MAGIC: &[u8] = b"Inno Setup Setup Data";
    const SCAN_LIMIT: usize = 8 * 1024 * 1024;
    let scan_end = data.len().min(SCAN_LIMIT);
    if data[..scan_end]
        .windows(INNO_MAGIC.len())
        .any(|w| w == INNO_MAGIC)
    {
        return Ok(InstallerKind::Inno);
    }

    // 2. Check for traditional ZIP archives or appended-script ZIP structures (.sh).
    const EOCD_SIG: &[u8] = b"PK\x05\x06";
    const MAX_EOCD_SEARCH: usize = 65535 + 22;
    let search_start = data.len().saturating_sub(MAX_EOCD_SEARCH);

    if data.starts_with(b"PK\x03\x04")
        || data[search_start..]
            .windows(EOCD_SIG.len())
            .any(|w| w == EOCD_SIG)
    {
        return Ok(InstallerKind::MojoSetup);
    }

    anyhow::bail!(
        "Unrecognised installer format — expected a MojoSetup installer \
         (.sh/.exe) or an Inno Setup executable."
    );
}

// ---------------------------------------------------------------------------
// GUI Worker Thread Dispatcher
// ---------------------------------------------------------------------------

#[cfg(feature = "gui")]
pub fn installer_worker_loop(
    input_file: &std::path::Path,
    output_dir: Option<PathBuf>,
    force: bool,
    running: &Arc<AtomicBool>,
    user_cancelled: &Arc<AtomicBool>,
    tx: &mpsc::Sender<gui::GuiEvent>,
) -> Result<bool> {
    let file = File::open(input_file)
        .with_context(|| format!("Failed to open input file {}", input_file.display()))?;

    let mmap = unsafe { memmap2::Mmap::map(&file) }
        .with_context(|| format!("Failed to memory-map {}", input_file.display()))?;
    let mmap = ArcMmap(Arc::new(mmap));

    let kind = detect_installer_kind(&mmap)?;

    match kind {
        InstallerKind::Inno => {
            let _ = tx.send(gui::GuiEvent::Detected("Inno Setup".to_owned()));
            inno::extract_gui(input_file, output_dir, force, running, tx)
        }
        InstallerKind::MojoSetup => {
            let _ = tx.send(gui::GuiEvent::Detected("MojoSetup".to_owned()));
            mojo::extract_gui(
                &mmap,
                input_file,
                output_dir,
                force,
                running,
                user_cancelled,
                tx,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Runner logic
// ---------------------------------------------------------------------------

// Returned by run() to tell main() how to print the final status line.
enum RunOutcome {
    /// CLI extraction finished successfully — print the completion banner.
    CliSuccess,
    /// CLI extraction was cancelled — print the cancellation notice.
    CliCancelled,
    /// GUI exited cleanly — no banner needed.
    #[cfg(feature = "gui")]
    GuiExited,
}

fn run() -> Result<RunOutcome> {
    let args = Args::parse();

    #[cfg(feature = "gui")]
    {
        let force_gui = args.gui;
        let no_args =
            args.input_file.is_none() && args.output_dir.is_none() && !args.force && !args.list;

        if force_gui || no_args {
            gui::run().map(|()| RunOutcome::GuiExited)
        } else {
            run_cli(args)
        }
    }

    #[cfg(not(feature = "gui"))]
    {
        run_cli(args)
    }
}

fn run_cli(args: Args) -> Result<RunOutcome> {
    let Some(input_file) = args.input_file else {
        anyhow::bail!("No input file provided. Use --help for usage details.");
    };

    let file = File::open(&input_file)
        .with_context(|| format!("Failed to open installer file: {}", input_file.display()))?;

    // On 32-bit targets, the usable virtual address space is typically 2–3 GiB,
    // so even files well under 4 GiB can fail to map. Warn early rather than
    // letting the OS reject the mmap call with a cryptic error.
    if cfg!(target_pointer_width = "32") && file.metadata()?.len() > 2u64 * 1024 * 1024 * 1024 {
        anyhow::bail!(
            "File may be too large to memory-map on a 32-bit architecture \
             (usable address space is typically ~2–3 GiB)."
        );
    }

    let raw_mmap = unsafe { memmap2::Mmap::map(&file).context("Failed to memory-map file")? };
    let mmap = ArcMmap(Arc::new(raw_mmap));

    let installer_kind = detect_installer_kind(&mmap)?;

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

    let ok = match installer_kind {
        InstallerKind::Inno => {
            if args.list {
                inno::list(&input_file)
            } else {
                inno::extract(&input_file, args.output_dir, args.force, &running)
            }
        }
        InstallerKind::MojoSetup => {
            if args.list {
                mojo::list(&mmap).map(|()| true)
            } else {
                mojo::extract(
                    &mmap,
                    &input_file,
                    args.output_dir,
                    args.force,
                    &running,
                    &user_cancelled,
                )
            }
        }
    }?;

    if ok {
        Ok(RunOutcome::CliSuccess)
    } else {
        Ok(RunOutcome::CliCancelled)
    }
}

fn main() {
    match run() {
        Ok(RunOutcome::CliSuccess) => {
            println!("\n🎉 Extraction complete!");
        }
        Ok(RunOutcome::CliCancelled) => {
            println!("\n🚨 Cancelled — cleaned up.");
            std::process::exit(130);
        }
        #[cfg(feature = "gui")]
        Ok(RunOutcome::GuiExited) => {
            // GUI handles its own feedback; no console output needed.
        }
        Err(e) => {
            eprintln!("\n❌ Extraction failed: {e:#}");
            std::process::exit(1);
        }
    }
}
