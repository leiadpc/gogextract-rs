use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use std::collections::VecDeque;
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
    Progress { files_done: u64, files_total: u64 },
    /// A log line (warnings, info).
    Log(String),
    /// Extraction finished successfully.
    Done {
        elapsed_secs: f64,
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
    installer_path_str: String,
    output_dir_str: String,
    /// The output dir that was actually used by the last successful extraction,
    /// kept separately so the "Open Output Folder" button survives state resets.
    last_output_dir: Option<PathBuf>,
    force_overwrite: bool,

    // Status text & tracking
    detected_kind: String,
    current_file: String,
    files_done: u64,
    files_total: u64,
    summary_text: String,
    error_text: String,
    log: VecDeque<String>,

    // Multi-threading communication channels
    rx: mpsc::Receiver<GuiEvent>,
    tx: mpsc::Sender<GuiEvent>,
    running_flag: Arc<AtomicBool>,
    cancelled_flag: Arc<AtomicBool>,
}

impl App {
    /// Creates a state container.
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = mpsc::channel();

        Self {
            state: State::Idle,
            installer_path_str: String::new(),
            output_dir_str: String::new(),
            last_output_dir: None,
            force_overwrite: false,
            detected_kind: "Unknown".to_owned(),
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
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        if self.state != State::Running && self.state != State::Cancelling {
            return;
        }

        let mut repainted = false;

        while let Ok(event) = self.rx.try_recv() {
            repainted = true;
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
                } => {
                    self.files_done = files_done;
                    self.files_total = files_total;
                }
                GuiEvent::Log(line) => {
                    if self.log.len() >= 1000 {
                        self.log.pop_front();
                    }
                    self.log.push_back(line);
                }
                GuiEvent::Done {
                    elapsed_secs,
                    file_count,
                } => {
                    self.state = State::Done;

                    let out_path = PathBuf::from(&self.output_dir_str);
                    if !self.output_dir_str.trim().is_empty() {
                        self.last_output_dir = Some(out_path);
                    } else if !self.installer_path_str.trim().is_empty() {
                        self.last_output_dir = Some(crate::default_output_dir(&PathBuf::from(
                            &self.installer_path_str,
                        )));
                    } else {
                        self.last_output_dir = None;
                    }

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

        if repainted {
            ctx.request_repaint();
        }
    }

    fn start_extraction(&mut self, ctx: &egui::Context) {
        if self.installer_path_str.trim().is_empty() {
            return;
        }
        let input_file = PathBuf::from(&self.installer_path_str);

        let out_dir = if self.output_dir_str.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(&self.output_dir_str))
        };

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
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }

    fn log_line_color(
        line: &str,
        success: Color32,
        danger: Color32,
        warn: Color32,
        default_text: Color32,
    ) -> Color32 {
        if line.starts_with('✓') {
            return success;
        }
        if line.starts_with('✗') {
            return danger;
        }
        if line.starts_with("⚠️") || line.starts_with('⚠') {
            return warn;
        }

        let lower = line.to_ascii_lowercase();
        if lower.contains("error") || lower.contains("failed") || lower.contains("abort") {
            return danger;
        }
        if lower.contains("warning") || lower.contains("warn") || lower.contains("skipping") {
            return warn;
        }

        default_text
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        if self.state == State::Running || self.state == State::Cancelling {
            return;
        }

        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if let Some(first) = dropped.into_iter().next() {
            if let Some(path) = first.path {
                self.output_dir_str = crate::default_output_dir(&path)
                    .to_string_lossy()
                    .into_owned();
                self.installer_path_str = path.to_string_lossy().into_owned();
                self.detected_kind = "Unknown".to_owned();
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_dropped_files(ctx);
        self.drain_events(ctx);

        // Derive color standards by matching the active egui-configured system context
        let is_dark = ctx.style().visuals.dark_mode;

        let success_col = if is_dark {
            Color32::from_rgb(158, 206, 106)
        } else {
            Color32::from_rgb(72, 94, 28)
        };
        let danger_col = if is_dark {
            Color32::from_rgb(247, 118, 142)
        } else {
            Color32::from_rgb(140, 43, 62)
        };
        let warn_col = if is_dark {
            Color32::from_rgb(224, 175, 104)
        } else {
            Color32::from_rgb(143, 91, 0)
        };
        let log_text_default = if is_dark {
            Color32::from_gray(210)
        } else {
            Color32::from_rgb(52, 53, 64)
        };

        let canvas_bg = ctx.style().visuals.extreme_bg_color;
        let border_color = ctx.style().visuals.widgets.noninteractive.bg_stroke.color;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 10.0);

            if self.state == State::Idle && self.installer_path_str.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(
                            "💡 Tip: drag and drop an installer file anywhere onto this window.",
                        )
                        .italics()
                        .weak()
                        .size(11.0),
                    );
                });
            }

            ui.group(|ui| {
                ui.set_width(ui.available_width());
                egui::Grid::new("file_picker_grid")
                    .num_columns(3)
                    .spacing([10.0, 8.0])
                    .min_col_width(80.0)
                    .show(ui, |ui| {
                        // --- ROW 1: Installer Entry ---
                        ui.label(RichText::new("Installer:").strong());
                        if ui.button("📁 Browse...").clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter(
                                    "Game Installers (*.exe, *.bin, *.sh)",
                                    &["exe", "bin", "sh"],
                                )
                                .pick_file()
                            {
                                self.installer_path_str = path.to_string_lossy().into_owned();
                                self.output_dir_str = crate::default_output_dir(&path)
                                    .to_string_lossy()
                                    .into_owned();
                                self.detected_kind = "Unknown".to_owned();
                            }
                        }

                        // Textbox width takes up the rest of the layout space
                        let edit_width = ui.available_width();
                        ui.add(
                            egui::TextEdit::singleline(&mut self.installer_path_str)
                                .hint_text("/path/to/installer.sh")
                                .desired_width(edit_width),
                        );
                        ui.end_row();

                        // --- ROW 2: Output Dir Entry ---
                        ui.label(RichText::new("Output Dir:").strong());
                        ui.horizontal(|ui| {
                            if ui.button("📁 Browse...").clicked() {
                                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                                    self.output_dir_str = path.to_string_lossy().into_owned();
                                }
                            }
                            if ui.button("↺ Reset").clicked() {
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
                    egui::Button::new(RichText::new("🚀 Extract Game").strong().size(14.0));

                if ui.add_enabled(can_extract, btn_extract).clicked() {
                    self.start_extraction(ctx);
                }

                let btn_cancel = egui::Button::new(RichText::new("🛑 Cancel").strong().size(14.0));
                if ui
                    .add_enabled(self.state == State::Running, btn_cancel)
                    .clicked()
                {
                    self.cancel_extraction(ctx);
                }

                if self.state == State::Done {
                    if let Some(dir) = self.last_output_dir.clone() {
                        if ui
                            .button(RichText::new("📂 Open Output Folder").size(14.0))
                            .clicked()
                        {
                            Self::open_folder(&dir);
                        }
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(&self.detected_kind)
                            .monospace()
                            .strong()
                            .color(if self.detected_kind.contains("MojoSetup") {
                                if is_dark {
                                    Color32::from_rgb(189, 147, 249)
                                } else {
                                    Color32::from_rgb(52, 90, 182)
                                }
                            } else if self.detected_kind.contains("Inno") {
                                if is_dark {
                                    Color32::from_rgb(139, 233, 253)
                                } else {
                                    Color32::from_rgb(181, 137, 0)
                                }
                            } else {
                                if is_dark {
                                    Color32::from_gray(140)
                                } else {
                                    Color32::from_gray(100)
                                }
                            }),
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
                    ui.colored_label(warn_col, "⏳ Cancelling — waiting for worker to stop...");
                }
                State::Cancelled => {
                    ui.colored_label(warn_col, "🚨 Extraction was aborted and cleaned up.");
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
                    egui::ScrollArea::vertical()
                        .id_salt("log")
                        .max_height(remaining_height)
                        .auto_shrink([false; 2])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            for line in &self.log {
                                let color = Self::log_line_color(
                                    line,
                                    success_col,
                                    danger_col,
                                    warn_col,
                                    log_text_default,
                                );
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
