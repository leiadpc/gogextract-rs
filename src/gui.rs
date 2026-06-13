use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

// ---------------------------------------------------------------------------
// Events from the extraction worker to the GUI
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum GuiEvent {
    /// Installer type was detected.
    Detected(String),
    /// A filename was extracted.
    Filename(String),
    /// Extraction progress data.
    Progress {
        files_done: u64,
        files_total: u64,
        bytes_done: u64,
        bytes_total: u64,
    },
    /// A log line (warnings, info).
    Log(String),
    /// Extraction finished successfully.
    Done {
        elapsed_secs: f64,
        total_mib: f64,
        file_count: usize,
    },
    /// Extraction failed.
    Failed(String),
    /// Extraction was cancelled.
    Cancelled,
}

// ---------------------------------------------------------------------------
// GUI state machine
// ---------------------------------------------------------------------------

#[derive(PartialEq)]
enum State {
    Idle,
    Running,
    Cancelling,
    Done,
    Failed,
    Cancelled,
}

pub struct App {
    state: State,
    installer_path: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    force_overwrite: bool,

    // Status text & tracking
    detected_kind: String,
    current_file: String,
    files_done: u64,
    files_total: u64,
    bytes_done: u64,
    bytes_total: u64,
    summary_text: String,
    error_text: String,
    log: Vec<String>,

    // Multi-threading communication channels
    rx: mpsc::Receiver<GuiEvent>,
    tx: mpsc::Sender<GuiEvent>,
    running_flag: Arc<AtomicBool>,
    cancelled_flag: Arc<AtomicBool>,
}

impl Default for App {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            state: State::Idle,
            installer_path: None,
            output_dir: None,
            force_overwrite: false,
            detected_kind: "Unknown".to_owned(),
            current_file: String::new(),
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
            summary_text: String::new(),
            error_text: String::new(),
            log: Vec::new(),
            rx,
            tx,
            running_flag: Arc::new(AtomicBool::new(false)),
            cancelled_flag: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl App {
    fn drain_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                GuiEvent::Detected(kind) => {
                    self.detected_kind = kind;
                }
                GuiEvent::Filename(name) => {
                    self.current_file = name;
                }
                GuiEvent::Progress {
                    files_done,
                    files_total,
                    bytes_done,
                    bytes_total,
                } => {
                    self.files_done = files_done;
                    self.files_total = files_total;
                    self.bytes_done = bytes_done;
                    self.bytes_total = bytes_total;
                }
                GuiEvent::Log(line) => {
                    self.log.push(line);
                }
                GuiEvent::Done {
                    elapsed_secs,
                    total_mib,
                    file_count,
                } => {
                    self.state = State::Done;
                    self.summary_text = if total_mib > 0.0 {
                        format!(
                            "✓ Successfully extracted {} files ({:.2} MiB) in {:.1} seconds.",
                            file_count, total_mib, elapsed_secs
                        )
                    } else {
                        // innoextract path: byte totals unavailable.
                        format!(
                            "✓ Successfully extracted {} files in {:.1} seconds.",
                            file_count, elapsed_secs
                        )
                    };
                    self.running_flag.store(false, Ordering::Relaxed);
                }
                GuiEvent::Failed(err) => {
                    self.state = State::Failed;
                    self.error_text = err;
                    self.running_flag.store(false, Ordering::Relaxed);
                }
                // State only transitions to Cancelled when the worker confirms it,
                // not when the button is clicked — avoids a race where a late Done
                // or Failed event would overwrite the terminal state.
                GuiEvent::Cancelled => {
                    self.state = State::Cancelled;
                    self.running_flag.store(false, Ordering::Relaxed);
                }
            }
        }
    }

    fn start_extraction(&mut self) {
        let Some(input_file) = self.installer_path.clone() else {
            return;
        };

        self.state = State::Running;
        self.current_file.clear();
        self.files_done = 0;
        self.files_total = 0;
        self.bytes_done = 0;
        self.bytes_total = 0;
        self.summary_text.clear();
        self.error_text.clear();
        self.log.clear();

        self.running_flag.store(true, Ordering::Relaxed);
        self.cancelled_flag.store(false, Ordering::Relaxed);

        let tx = self.tx.clone();
        let running = self.running_flag.clone();
        let user_cancelled = self.cancelled_flag.clone();
        let out_dir = self.output_dir.clone();
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
                Ok(true) => {} // Done event already sent by extract_gui
                Ok(false) => {
                    // Worker wound down cleanly after cancellation; confirm to GUI.
                    let _ = tx.send(GuiEvent::Cancelled);
                }
                Err(e) => {
                    let _ = tx.send(GuiEvent::Failed(format!("{e:#}")));
                }
            }
        });
    }

    fn cancel_extraction(&mut self) {
        // Signal the worker to stop, then wait for its Cancelled event before
        // transitioning state — this prevents a race with late Done/Failed events.
        self.running_flag.store(false, Ordering::Relaxed);
        self.cancelled_flag.store(true, Ordering::Relaxed);
        self.state = State::Cancelling;
        let _ = self
            .tx
            .send(GuiEvent::Log("⚠️ Cancellation requested...".to_owned()));
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        if let Some(system_theme) = ctx.system_theme() {
            let is_dark_visuals = ctx.style().visuals.dark_mode;

            match system_theme {
                egui::Theme::Dark if !is_dark_visuals => {
                    ctx.set_visuals(egui::Visuals::dark());
                }
                egui::Theme::Light if is_dark_visuals => {
                    ctx.set_visuals(egui::Visuals::light());
                }
                _ => {}
            }
        }

        if self.state == State::Running || self.state == State::Cancelling {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        let bg_dark = Color32::from_rgb(30, 30, 35);
        let success = Color32::from_rgb(75, 210, 115);
        let danger = Color32::from_rgb(235, 85, 85);
        let warn = Color32::from_rgb(240, 170, 55);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 10.0);

            // 2. File Picker Configurations Panel
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                egui::Grid::new("file_picker_grid")
                    .num_columns(3)
                    .spacing([10.0, 8.0])
                    .min_col_width(80.0)
                    .show(ui, |ui| {
                        ui.label(RichText::new("Installer:").strong());
                        if ui.button("📁 Browse...").clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter(
                                    "Game Installers (*.exe, *.bin, *.sh)",
                                    &["exe", "bin", "sh"],
                                )
                                .pick_file()
                            {
                                // Auto-populate output dir with the computed default
                                // so the user can see (and edit) where files will go.
                                self.output_dir = Some(crate::default_output_dir(&path));
                                self.installer_path = Some(path);
                                self.detected_kind = "Unknown".to_owned();
                            }
                        }
                        if let Some(p) = &self.installer_path {
                            ui.label(
                                RichText::new(p.file_name().unwrap_or_default().to_string_lossy())
                                    .monospace(),
                            );
                        } else {
                            ui.label(RichText::new("No file selected").italics().weak());
                        }
                        ui.end_row();

                        ui.label(RichText::new("Output Dir:").strong());
                        ui.horizontal(|ui| {
                            if ui.button("📁 Browse...").clicked() {
                                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                                    self.output_dir = Some(path);
                                }
                            }
                            if ui.button("↺ Reset").clicked() {
                                self.output_dir = self
                                    .installer_path
                                    .as_deref()
                                    .map(crate::default_output_dir);
                            }
                        });
                        if let Some(p) = &self.output_dir {
                            ui.label(RichText::new(p.to_string_lossy()).monospace());
                        } else {
                            ui.label(RichText::new("No file selected yet").italics().weak());
                        }
                        ui.end_row();
                    });

                ui.add_space(4.0);
                ui.checkbox(
                    &mut self.force_overwrite,
                    "Force overwrite existing directory data",
                );
            });

            // 3. Control Layout Panel
            ui.horizontal(|ui| {
                // Guard on running_flag rather than state alone: prevents starting
                // a second worker while a cancelled one is still winding down.
                let can_extract =
                    self.installer_path.is_some() && !self.running_flag.load(Ordering::Relaxed);
                let btn_extract =
                    egui::Button::new(RichText::new("🚀 Extract Game").strong().size(14.0));

                if ui.add_enabled(can_extract, btn_extract).clicked() {
                    self.start_extraction();
                }

                let btn_cancel = egui::Button::new(RichText::new("🛑 Cancel").strong().size(14.0));
                if ui
                    .add_enabled(self.state == State::Running, btn_cancel)
                    .clicked()
                {
                    self.cancel_extraction();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(&self.detected_kind)
                            .monospace()
                            .strong()
                            .color(if self.detected_kind.contains("MojoSetup") {
                                Color32::from_rgb(180, 110, 240)
                            } else if self.detected_kind.contains("Inno") {
                                Color32::from_rgb(90, 165, 235)
                            } else {
                                Color32::from_gray(140)
                            }),
                    );
                    ui.label("Installer Profile:");
                });
            });

            // 4. Progress Container — shown while running or winding down after cancel.
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
                    }

                    if self.bytes_total > 0 {
                        let byte_pct = self.bytes_done as f64 / self.bytes_total as f64;
                        let done_mib = self.bytes_done as f64 / (1024.0 * 1024.0);
                        let total_mib = self.bytes_total as f64 / (1024.0 * 1024.0);

                        ui.horizontal(|ui| {
                            ui.label(format!("Bytes: {:.1} / {:.1} MiB", done_mib, total_mib));
                            ui.add(
                                egui::ProgressBar::new(byte_pct as f32)
                                    .desired_width(ui.available_width()),
                            );
                        });
                    }
                });
            }

            // 5. Extraction Engine Contextual Alerts
            match self.state {
                State::Done => {
                    ui.colored_label(success, &self.summary_text);
                }
                State::Failed => {
                    ui.colored_label(danger, format!("✗ Error: {}", self.error_text));
                }
                State::Cancelling => {
                    ui.colored_label(warn, "⏳ Cancelling — waiting for worker to stop...");
                }
                State::Cancelled => {
                    ui.colored_label(warn, "🚨 Extraction was aborted and cleaned up.");
                }
                _ => {}
            }

            // 6. Elastic Terminal Logs Container
            ui.label(RichText::new("Terminal Logs:").strong());
            let remaining_height = ui.available_height() - 5.0;

            egui::Frame::canvas(ui.style())
                .fill(bg_dark)
                .stroke(Stroke::new(1.0, Color32::from_gray(60)))
                .inner_margin(6.0)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("log")
                        .max_height(remaining_height)
                        .auto_shrink([false; 2])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            for line in &self.log {
                                let color = if line.starts_with('✓') {
                                    success
                                } else if line.starts_with('✗') {
                                    danger
                                } else if line.starts_with('⚠') {
                                    warn
                                } else {
                                    Color32::from_gray(210)
                                };
                                ui.label(RichText::new(line).monospace().size(11.0).color(color));
                            }
                        });
                });
        });
    }
}

pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Game Installer Extractor")
            .with_inner_size([580.0, 480.0])
            .with_min_inner_size([500.0, 400.0])
            .with_resizable(true),
        ..Default::default()
    };
    eframe::run_native(
        "Game Installer Extractor",
        options,
        Box::new(|_cc| Ok(Box::new(App::default()))),
    )
    .map_err(|e| anyhow::anyhow!("eframe native failure: {e}"))
}
