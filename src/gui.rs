use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::InstallerKind;

// ---------------------------------------------------------------------------
// Events from the extraction worker to the GUI
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum GuiEvent {
    /// Installer type was detected.
    Detected(InstallerKind),
    /// A filename was extracted, along with current progress counts.
    /// Combining these into one event prevents the filename and counter
    /// from becoming out of sync due to channel-level throttling.
    Progress {
        files_done: u64,
        files_total: u64,
        /// `None` when the progress tick doesn't carry a new filename (e.g.
        /// the Inno path emits progress on every file; throttling is done
        /// inside the worker).
        current_file: Option<String>,
    },
    /// A log line (warnings, info).
    Log(String),
    /// Extraction finished successfully.
    Done {
        elapsed_secs: f64,
        file_count: usize,
        /// The output directory actually used, captured at extraction start
        /// so it is correct even if the user edits the path field mid-run.
        output_dir: PathBuf,
    },
    /// Extraction failed.
    Failed(String),
    /// Extraction was cancelled.
    Cancelled,
}

// ---------------------------------------------------------------------------
// GUI state machine
// ---------------------------------------------------------------------------

const MAX_LOG_LINES: usize = 1000;

#[derive(PartialEq)]
enum State {
    Idle,
    Running,
    Cancelling,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Success,
    Error,
    Warning,
    Info,
}

/// Theme-derived colors, cached per-frame to avoid recomputing on every repaint.
/// Rebuilt only when `is_dark` flips (i.e. the user switches the OS theme).
struct ThemeColors {
    is_dark: bool,
    success: Color32,
    danger: Color32,
    warn: Color32,
    log_text_default: Color32,
    canvas_bg: Color32,
    border: Color32,
    mojo_kind: Color32,
    inno_kind: Color32,
    pkg_kind: Color32,
    unknown_kind: Color32,
}

impl ThemeColors {
    fn build(ctx: &egui::Context) -> Self {
        let is_dark = ctx.style().visuals.dark_mode;
        Self {
            is_dark,
            success: if is_dark {
                Color32::from_rgb(158, 206, 106)
            } else {
                Color32::from_rgb(72, 94, 28)
            },
            danger: if is_dark {
                Color32::from_rgb(247, 118, 142)
            } else {
                Color32::from_rgb(140, 43, 62)
            },
            warn: if is_dark {
                Color32::from_rgb(224, 175, 104)
            } else {
                Color32::from_rgb(143, 91, 0)
            },
            log_text_default: if is_dark {
                Color32::from_gray(210)
            } else {
                Color32::from_rgb(52, 53, 64)
            },
            canvas_bg: ctx.style().visuals.extreme_bg_color,
            border: ctx.style().visuals.widgets.noninteractive.bg_stroke.color,
            mojo_kind: if is_dark {
                Color32::from_rgb(189, 147, 249)
            } else {
                Color32::from_rgb(52, 90, 182)
            },
            inno_kind: if is_dark {
                Color32::from_rgb(139, 233, 253)
            } else {
                Color32::from_rgb(181, 137, 0)
            },
            pkg_kind: if is_dark {
                Color32::from_rgb(158, 206, 106)
            } else {
                Color32::from_rgb(58, 132, 64)
            },
            unknown_kind: if is_dark {
                Color32::from_gray(140)
            } else {
                Color32::from_gray(100)
            },
        }
    }

    /// Rebuild only when the dark-mode flag has changed.
    fn refresh(&mut self, ctx: &egui::Context) {
        if ctx.style().visuals.dark_mode != self.is_dark {
            *self = Self::build(ctx);
        }
    }
}

pub struct App {
    state: State,
    installer_path_str: String,
    output_dir_str: String,
    /// The output dir that was actually used by the last successful extraction,
    /// kept separately so the "Open Output Folder" button survives state resets.
    last_output_dir: Option<PathBuf>,
    force_overwrite: bool,

    // Status text & tracking
    /// `None` = not yet detected / unknown; avoids fragile string comparisons.
    detected_kind: Option<InstallerKind>,
    current_file: String,
    files_done: u64,
    files_total: u64,
    summary_text: String,
    error_text: String,
    log: VecDeque<(LogLevel, String)>,

    // Multi-threading communication channels
    rx: mpsc::Receiver<GuiEvent>,
    tx: mpsc::Sender<GuiEvent>,
    running_flag: Arc<AtomicBool>,
    cancelled_flag: Arc<AtomicBool>,

    // Render limits
    last_repaint: Instant,

    /// Cached theme colors — rebuilt only when the dark/light mode flips.
    theme: ThemeColors,
}

impl App {
    /// Creates a state container.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = mpsc::channel();

        Self {
            state: State::Idle,
            installer_path_str: String::new(),
            output_dir_str: String::new(),
            last_output_dir: None,
            force_overwrite: false,
            detected_kind: None,
            current_file: String::new(),
            files_done: 0,
            files_total: 0,
            summary_text: String::new(),
            error_text: String::new(),
            log: VecDeque::new(),
            rx,
            tx,
            running_flag: Arc::new(AtomicBool::new(false)),
            cancelled_flag: Arc::new(AtomicBool::new(false)),
            last_repaint: Instant::now(),
            theme: ThemeColors::build(&cc.egui_ctx),
        }
    }

    fn parse_log_level(line: &str) -> LogLevel {
        if line.starts_with('✓') {
            return LogLevel::Success;
        }
        if line.starts_with('✗') {
            return LogLevel::Error;
        }
        if line.starts_with("⚠️") || line.starts_with('⚠') {
            return LogLevel::Warning;
        }

        let lower = line.to_ascii_lowercase();
        if lower.contains("error") || lower.contains("failed") || lower.contains("abort") {
            return LogLevel::Error;
        }
        if lower.contains("warning") || lower.contains("warn") || lower.contains("skipping") {
            return LogLevel::Warning;
        }

        LogLevel::Info
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        let mut repainted = false;

        while let Ok(event) = self.rx.try_recv() {
            repainted = true;
            match event {
                // GuiEvent::Detected is emitted by the background detection thread
                // and must be processed in any state so the installer profile label
                // updates as soon as a file is loaded, before extraction starts.
                GuiEvent::Detected(kind) => {
                    self.detected_kind = Some(kind);
                }

                // All other events are only meaningful while a worker is active.
                _ if self.state != State::Running && self.state != State::Cancelling => {}

                GuiEvent::Progress {
                    files_done,
                    files_total,
                    current_file,
                } => {
                    self.files_done = files_done;
                    self.files_total = files_total;
                    if let Some(name) = current_file {
                        self.current_file = name;
                    }
                }
                GuiEvent::Log(line) => {
                    if self.log.len() >= MAX_LOG_LINES {
                        self.log.pop_front();
                    }
                    let level = Self::parse_log_level(&line);
                    self.log.push_back((level, line));
                }
                GuiEvent::Done {
                    elapsed_secs,
                    file_count,
                    output_dir,
                } => {
                    self.state = State::Done;
                    // Use the path captured at extraction start — not the
                    // current field value, which the user may have edited.
                    self.last_output_dir = Some(output_dir);
                    self.summary_text = format!(
                        "✓ Successfully extracted {} files in {:.1} seconds.",
                        file_count, elapsed_secs
                    );
                    self.running_flag.store(false, Ordering::Relaxed);
                }
                GuiEvent::Failed(err) => {
                    self.state = State::Failed;
                    self.error_text = err;
                    self.running_flag.store(false, Ordering::Relaxed);
                }
                GuiEvent::Cancelled => {
                    self.state = State::Cancelled;
                    self.running_flag.store(false, Ordering::Relaxed);
                }
            }
        }

        // Throttle UI repaints driven by high-frequency background channel events
        // down to roughly ~30 FPS (32 ms interval) to save CPU/GPU cycles.
        if repainted {
            let now = Instant::now();
            if now.duration_since(self.last_repaint) >= Duration::from_millis(32) {
                ctx.request_repaint();
                self.last_repaint = now;
            } else {
                ctx.request_repaint_after(Duration::from_millis(32));
            }
        }
    }

    fn start_extraction(&mut self, ctx: &egui::Context) {
        if self.installer_path_str.trim().is_empty() {
            return;
        }
        let input_file = PathBuf::from(&self.installer_path_str);

        // Resolve the output dir now, before the worker thread starts, so the
        // Done event carries the path that was actually used regardless of any
        // subsequent edits the user makes to the output_dir field.
        let resolved_output_dir = if self.output_dir_str.trim().is_empty() {
            crate::default_output_dir(&input_file)
        } else {
            PathBuf::from(&self.output_dir_str)
        };

        // Pass None to the worker so it doesn't re-derive the path; we already
        // have the resolved dir and will embed it in the Done event.
        let out_dir = Some(resolved_output_dir.clone());

        self.state = State::Running;
        self.current_file.clear();
        self.files_done = 0;
        self.files_total = 0;
        self.summary_text.clear();
        self.error_text.clear();
        self.log.clear();

        self.running_flag.store(true, Ordering::Relaxed);
        self.cancelled_flag.store(false, Ordering::Relaxed);

        let tx = self.tx.clone();
        let running = self.running_flag.clone();
        let user_cancelled = self.cancelled_flag.clone();
        let force = self.force_overwrite;

        std::thread::spawn(move || {
            let res = crate::installer_worker_loop(
                &input_file,
                out_dir,
                force,
                &running,
                &user_cancelled,
                &tx,
            );
            match res {
                // extract_gui functions send GuiEvent::Done themselves, so
                // Ok(true) has already been communicated to the GUI.
                Ok(true) => {}
                Ok(false) => {
                    let _ = tx.send(GuiEvent::Cancelled);
                }
                Err(e) => {
                    let _ = tx.send(GuiEvent::Failed(format!("{e:#}")));
                }
            }
        });

        ctx.request_repaint();
    }

    fn cancel_extraction(&mut self, ctx: &egui::Context) {
        self.running_flag.store(false, Ordering::Relaxed);
        self.cancelled_flag.store(true, Ordering::Relaxed);
        self.state = State::Cancelling;
        let _ = self
            .tx
            .send(GuiEvent::Log("⚠️ Cancellation requested...".to_owned()));

        ctx.request_repaint();
    }

    fn open_folder(dir: &std::path::Path) {
        #[cfg(target_os = "windows")]
        let _ = std::process::Command::new("explorer").arg(dir).spawn();

        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(dir).spawn();

        #[cfg(target_os = "linux")]
        {
            // Prefer $OPENER if set (common on tiling/headless setups), fall
            // back to xdg-open which is available on most desktop Linux distros.
            let opener = std::env::var("OPENER").unwrap_or_else(|_| "xdg-open".to_owned());
            let _ = std::process::Command::new(&opener).arg(dir).spawn();
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        if self.state == State::Running || self.state == State::Cancelling {
            return;
        }

        let first_path = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()));
        if let Some(path) = first_path {
            self.output_dir_str = crate::default_output_dir(&path)
                .to_string_lossy()
                .into_owned();
            self.installer_path_str = path.to_string_lossy().into_owned();

            self.detect_installer_async(ctx);
        }
    }

    /// Spawns a short-lived background thread to detect the installer kind,
    /// keeping the UI thread responsive on slow or network-mounted paths.
    /// The result arrives as a `GuiEvent::Detected` on the normal event channel,
    /// and the thread requests a repaint so `drain_events` picks it up immediately.
    fn detect_installer_async(&mut self, ctx: &egui::Context) {
        use std::fs::File;

        let path = PathBuf::from(&self.installer_path_str);
        if !path.is_file() {
            self.detected_kind = None;
            return;
        }

        // Optimistically clear while we wait for the result.
        self.detected_kind = None;

        let tx = self.tx.clone();
        // egui::Context is cheaply cloneable (Arc internally) and Send, so we
        // can move it into the thread and call request_repaint() after the event
        // is queued — otherwise the result would sit unseen until the next
        // user-triggered repaint.
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<InstallerKind> {
                let file = File::open(&path)?;
                let mmap = unsafe { memmap2::Mmap::map(&file)? };
                let mmap = crate::mojo::ArcMmap(Arc::new(mmap));
                crate::detect_installer_kind(&mmap)
            })();

            if let Ok(kind) = result {
                let _ = tx.send(GuiEvent::Detected(kind));
                // Wake the UI thread so drain_events processes the event
                // without waiting for the next user interaction.
                ctx.request_repaint();
            }
            // On failure leave detected_kind as None (unknown) — no event sent.
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_dropped_files(ctx);
        self.drain_events(ctx);

        // Rebuild theme colors only when dark/light mode has flipped, then copy
        // all Color32 values into plain locals. Color32 is Copy, so this is
        // zero-cost and avoids holding a borrow of `self.theme` across the
        // `CentralPanel::show` closure (which also needs `&mut self`).
        self.theme.refresh(ctx);
        let success_col = self.theme.success;
        let danger_col = self.theme.danger;
        let warn_col = self.theme.warn;
        let log_text_default = self.theme.log_text_default;
        let canvas_bg = self.theme.canvas_bg;
        let border_color = self.theme.border;
        let mojo_kind_col = self.theme.mojo_kind;
        let inno_kind_col = self.theme.inno_kind;
        let pkg_kind_col = self.theme.pkg_kind;
        let unknown_kind_col = self.theme.unknown_kind;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 10.0);

            ui.group(|ui| {
                ui.set_width(ui.available_width());
                egui::Grid::new("file_picker_grid")
                    .num_columns(3)
                    .spacing([10.0, 8.0])
                    .min_col_width(80.0)
                    .show(ui, |ui| {
                        // --- ROW 1: Installer Entry ---
                        ui.label(RichText::new("Installer:").strong());
                        if ui.button("\u{1F5C0} Browse...").clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter(
                                    "Game Installers (*.exe, *.sh, *.pkg)",
                                    &["exe", "sh", "pkg"],
                                )
                                .pick_file()
                            {
                                self.installer_path_str = path.to_string_lossy().into_owned();
                                self.output_dir_str = crate::default_output_dir(&path)
                                    .to_string_lossy()
                                    .into_owned();

                                self.detect_installer_async(ctx);
                            }
                        }

                        // Textbox width takes up the rest of the layout space
                        let edit_width = ui.available_width();
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.installer_path_str)
                                .hint_text("/path/to/installer.exe, .sh, or .pkg")
                                .desired_width(edit_width),
                        );

                        if response.lost_focus() && response.changed() {
                            self.detect_installer_async(ctx);

                            if !self.installer_path_str.trim().is_empty() {
                                let path = PathBuf::from(&self.installer_path_str);

                                if path.exists() {
                                    self.output_dir_str = crate::default_output_dir(&path)
                                        .to_string_lossy()
                                        .into_owned();
                                }
                            }
                        }
                        ui.end_row();

                        // --- ROW 2: Output Dir Entry ---
                        ui.label(RichText::new("Output Dir:").strong());
                        ui.horizontal(|ui| {
                            if ui.button("\u{1F5C0} Browse...").clicked() {
                                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                                    self.output_dir_str = path.to_string_lossy().into_owned();
                                }
                            }
                            if ui.button("\u{21BA} Reset").clicked() {
                                if !self.installer_path_str.trim().is_empty() {
                                    let path = PathBuf::from(&self.installer_path_str);
                                    self.output_dir_str = crate::default_output_dir(&path)
                                        .to_string_lossy()
                                        .into_owned();
                                } else {
                                    self.output_dir_str.clear();
                                }
                            }
                        });

                        ui.add(
                            egui::TextEdit::singleline(&mut self.output_dir_str)
                                .hint_text("/path/to/output_dir")
                                .desired_width(edit_width),
                        );
                        ui.end_row();
                    });

                ui.add_space(4.0);
                ui.checkbox(
                    &mut self.force_overwrite,
                    "Force overwrite existing directory data",
                );
            });

            ui.horizontal(|ui| {
                let can_extract = !self.installer_path_str.trim().is_empty()
                    && !self.running_flag.load(Ordering::Relaxed);
                let btn_extract =
                    egui::Button::new(RichText::new("\u{1F4E4} Extract Game").strong().size(14.0));

                if ui.add_enabled(can_extract, btn_extract).clicked() {
                    self.start_extraction(ctx);
                }

                let btn_cancel =
                    egui::Button::new(RichText::new("\u{274C} Cancel").strong().size(14.0));
                if ui
                    .add_enabled(self.state == State::Running, btn_cancel)
                    .clicked()
                {
                    self.cancel_extraction(ctx);
                }

                if self.state == State::Done {
                    if let Some(ref dir) = self.last_output_dir {
                        if ui
                            .button(RichText::new("\u{1F5C1} Open Output Folder").size(14.0))
                            .clicked()
                        {
                            Self::open_folder(dir);
                        }
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Color and label derived from typed InstallerKind — no string matching.
                    let (kind_label, kind_color) = match self.detected_kind {
                        Some(InstallerKind::MojoSetup) => ("MojoSetup", mojo_kind_col),
                        Some(InstallerKind::Inno) => ("Inno Setup", inno_kind_col),
                        Some(InstallerKind::Pkg) => ("pkg", pkg_kind_col),
                        None => ("Unknown", unknown_kind_col),
                    };

                    ui.label(
                        RichText::new(kind_label)
                            .monospace()
                            .strong()
                            .color(kind_color),
                    );
                    ui.label("Installer Profile:");
                });
            });

            if self.state == State::Running || self.state == State::Cancelling {
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    if !self.current_file.is_empty() {
                        ui.label(
                            RichText::new(format!("Processing: {}", self.current_file))
                                .monospace()
                                .weak(),
                        );
                    }

                    if self.files_total > 0 {
                        let file_pct = self.files_done as f32 / self.files_total as f32;
                        ui.horizontal(|ui| {
                            ui.label(format!("Files: {} / {}", self.files_done, self.files_total));
                            ui.add(
                                egui::ProgressBar::new(file_pct)
                                    .desired_width(ui.available_width()),
                            );
                        });
                    } else {
                        ui.horizontal(|ui| {
                            ui.label("Files:");
                            ui.add(
                                egui::ProgressBar::new(0.0)
                                    .desired_width(ui.available_width())
                                    .animate(true),
                            );
                        });
                    }
                });
            }

            match self.state {
                State::Done => {
                    ui.colored_label(success_col, &self.summary_text);
                }
                State::Failed => {
                    ui.colored_label(danger_col, format!("✗ Error: {}", self.error_text));
                }
                State::Cancelling => {
                    ui.colored_label(
                        warn_col,
                        "\u{231B} Cancelling — waiting for worker to stop...",
                    );
                }
                State::Cancelled => {
                    ui.colored_label(warn_col, "Extraction was aborted and cleaned up.");
                }
                _ => {}
            }

            ui.label(RichText::new("Terminal Logs:").strong());
            let remaining_height = ui.available_height() - 5.0;

            egui::Frame::canvas(ui.style())
                .fill(canvas_bg)
                .stroke(Stroke::new(1.0, border_color))
                .inner_margin(6.0)
                .show(ui, |ui| {
                    // Dynamically acquire the height of the custom text style to ensure accurate virtualization
                    let row_height = ui.fonts(|f| f.row_height(&egui::FontId::monospace(11.0)));

                    egui::ScrollArea::vertical()
                        .id_salt("log")
                        .max_height(remaining_height)
                        .auto_shrink([false; 2])
                        .stick_to_bottom(true)
                        .show_rows(ui, row_height, self.log.len(), |ui, row_range| {
                            ui.set_width(ui.available_width());

                            // Iterate strictly over the visible subset of rows
                            for row in row_range {
                                if let Some((level, line)) = self.log.get(row) {
                                    let color = match level {
                                        LogLevel::Success => success_col,
                                        LogLevel::Error => danger_col,
                                        LogLevel::Warning => warn_col,
                                        LogLevel::Info => log_text_default,
                                    };
                                    ui.label(
                                        RichText::new(line).monospace().size(11.0).color(color),
                                    );
                                }
                            }
                        });
                });
        });
    }
}

pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("gogextract")
            .with_inner_size([580.0, 480.0])
            .with_min_inner_size([500.0, 400.0])
            .with_resizable(true)
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "gogextract",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe native failure: {e}"))
}
