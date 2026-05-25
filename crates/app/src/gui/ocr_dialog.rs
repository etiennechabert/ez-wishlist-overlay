//! "Import upgrade from screenshot" dialog — POC v1.
//!
//! Flow: user clicks **Pick image…**, picks a PNG/JPG via the native
//! file dialog, the OCR engine runs synchronously on the GUI thread,
//! and the raw recognized text + word-box dump appears in a scrollable
//! pane. This is intentionally low-fi: the goal of v1 is to validate
//! that Windows.Media.Ocr produces usable output on the in-game
//! stylized font. Once we trust the OCR output, v2 wires the
//! [`crate::ocr::parse::CapturedUpgrade`] structured parser + the
//! wishlist write + a confirmation step.
//!
//! Runs on the GUI thread because OCR is fast (~100 ms typical on a
//! 1080p frame) and the file picker is already modal. If that ever
//! becomes a problem we'd move it behind a worker thread + channel,
//! same pattern the updater uses.

use crate::ocr;
use std::path::PathBuf;

pub struct OcrDialogState {
    /// Most recent OCR run, if any — image path + result. `None` until
    /// the user picks their first image.
    pub last_run: Option<LastRun>,
    /// Sticky error message from the most recent failed pick / OCR
    /// attempt. Cleared on the next successful run.
    pub last_error: Option<String>,
}

pub struct LastRun {
    pub image_path: PathBuf,
    pub result: ocr::OcrResult,
}

impl Default for OcrDialogState {
    fn default() -> Self {
        Self {
            last_run: None,
            last_error: None,
        }
    }
}

/// Render the dialog. Returns whether it's still open (the caller flips
/// its show-state bool to match).
pub fn show(ctx: &egui::Context, open: &mut bool, state: &mut OcrDialogState) {
    egui::Window::new("OCR POC — Import upgrade from screenshot")
        .open(open)
        .default_width(720.0)
        .default_height(560.0)
        .vscroll(false) // inner scroll on the result pane
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Pick image…").clicked() {
                    pick_and_recognize(state);
                }
                ui.add_space(8.0);
                if let Some(run) = &state.last_run {
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  ·  {}×{} px  ·  {} words",
                            run.image_path
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            run.result.image_width,
                            run.result.image_height,
                            run.result.words.len(),
                        ))
                        .small(),
                    );
                }
            });

            if let Some(err) = &state.last_error {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::from_rgb(220, 100, 90), err);
            }

            ui.separator();

            let Some(run) = &state.last_run else {
                ui.add_space(8.0);
                ui.label(
                    "Pick a screenshot of an in-game upgrade panel \
                     (e.g. the Storage Room / hideout submit screens). \
                     OCR runs locally via Windows.Media.Ocr — no network.",
                );
                return;
            };

            egui::CollapsingHeader::new("Recognized text")
                .default_open(true)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("ocr-text-scroll")
                        .max_height(220.0)
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut run.result.text.as_str())
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(10)
                                    .font(egui::TextStyle::Monospace),
                            );
                        });
                });

            egui::CollapsingHeader::new(format!("Word boxes ({})", run.result.words.len()))
                .default_open(false)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("ocr-words-scroll")
                        .max_height(220.0)
                        .show(ui, |ui| {
                            // Plain text dump — visual overlay on the source
                            // image is a v2 feature; for now we just need to
                            // see the (text, x, y, w, h) for each word so we
                            // can sanity-check the parser inputs.
                            for w in &run.result.words {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "[{:>4.0},{:>4.0}  {:>4.0}×{:>4.0}]  {}",
                                        w.rect.x,
                                        w.rect.y,
                                        w.rect.width,
                                        w.rect.height,
                                        w.text,
                                    ))
                                    .monospace()
                                    .small(),
                                );
                            }
                        });
                });
        });
}

fn pick_and_recognize(state: &mut OcrDialogState) {
    let Some(path) = rfd::FileDialog::new()
        .add_filter("Image", &["png", "jpg", "jpeg", "webp", "bmp"])
        .pick_file()
    else {
        // User cancelled — leave previous state intact.
        return;
    };

    match ocr::recognize_file(&path) {
        Ok(result) => {
            tracing::info!(
                path = %path.display(),
                words = result.words.len(),
                chars = result.text.len(),
                "OCR finished",
            );
            state.last_run = Some(LastRun {
                image_path: path,
                result,
            });
            state.last_error = None;
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "OCR failed");
            state.last_error = Some(format!("{e:#}"));
        }
    }
}
