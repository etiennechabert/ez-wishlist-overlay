//! Modal that surfaces the in-memory log buffer (see `log_buffer.rs`) plus a
//! "Capture VR Screenshot" button — a developer tool that pulls the SteamVR
//! compositor's left-eye mirror texture as a lossless PNG. We use this to
//! collect clean OCR samples without Steam's F12 JPEG compression.

use crate::log_buffer::{LogBuffer, LogLine};
use crate::ocr::OcrJob;
use crate::vr::runtime::CaptureResult;
use crate::vr::Runtime;
use crossbeam_channel::Sender;
use egui::Color32;
use std::sync::Arc;
use tracing::Level;

pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    log_buf: &LogBuffer,
    vr: &Arc<Runtime>,
    last_capture: Option<&CaptureResult>,
    ocr_job_tx: &Sender<OcrJob>,
) {
    let mut close_now = false;
    egui::Window::new("Debug logs")
        .open(open)
        .collapsible(false)
        .resizable(true)
        .default_width(720.0)
        .default_height(460.0)
        .show(ctx, |ui| {
            let lines = log_buf.snapshot();
            let dark = ui.visuals().dark_mode;
            let weak = ui.visuals().weak_text_color();

            if ui.button("Capture VR screenshot").clicked() {
                vr.request_screenshot();
            }

            // Debug-build helper: push a checked-in fixture PNG through
            // the OCR worker so the feedback overlay can be exercised
            // without SteamVR running. Only compiled in debug builds —
            // shipped users have no use for fixtures, and the directory
            // isn't present in installer builds.
            #[cfg(debug_assertions)]
            render_ocr_fixture_runner(ui, ocr_job_tx, weak);
            #[cfg(not(debug_assertions))]
            let _ = ocr_job_tx;
            match last_capture {
                Some(CaptureResult::Ok(path)) => {
                    ui.label(
                        egui::RichText::new(format!("Saved: {}", path.display()))
                            .small()
                            .color(Color32::from_rgb(80, 180, 100)),
                    );
                }
                Some(CaptureResult::Ephemeral) => {
                    ui.label(
                        egui::RichText::new(
                            "Captured (not written to disk — enable OCR Debug in \
                             Settings to keep the PNG).",
                        )
                        .small()
                        .color(Color32::from_rgb(80, 180, 100)),
                    );
                }
                Some(CaptureResult::Err(msg)) => {
                    ui.label(
                        egui::RichText::new(format!("Capture failed: {msg}"))
                            .small()
                            .color(Color32::from_rgb(220, 100, 90)),
                    );
                }
                None => {
                    ui.label(egui::RichText::new(" ").small());
                }
            }
            ui.separator();

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{} line{} captured this session (oldest first, newest at bottom)",
                        lines.len(),
                        if lines.len() == 1 { "" } else { "s" }
                    ))
                    .small()
                    .color(weak),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        close_now = true;
                    }
                    if ui.button("Clear").clicked() {
                        log_buf.clear();
                    }
                    if ui.button("Copy to clipboard").clicked() {
                        let joined = lines.iter().map(format_line).collect::<Vec<_>>().join("\n");
                        ui.ctx().copy_text(joined);
                    }
                });
            });
            ui.separator();

            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if lines.is_empty() {
                        ui.add_space(20.0);
                        ui.vertical_centered(|ui| {
                            ui.label(egui::RichText::new("No log lines yet.").small().color(weak));
                        });
                        return;
                    }
                    for line in &lines {
                        let color = level_color(line.level, dark);
                        ui.label(
                            egui::RichText::new(format_line(line))
                                .monospace()
                                .small()
                                .color(color),
                        );
                    }
                });
        });

    if close_now {
        *open = false;
    }
}

#[cfg(debug_assertions)]
fn render_ocr_fixture_runner(ui: &mut egui::Ui, ocr_job_tx: &Sender<OcrJob>, weak: Color32) {
    // The hideout fixtures live under `screenshots/hideout/` at repo root. We
    // probe for the directory at runtime; if the dev moved the workspace or the
    // binary is being run far from its build dir the button stays disabled and
    // explains why. The committed fixtures are WebP (q99); `.png` is still
    // accepted for ad-hoc lossless captures dropped in alongside them.
    let fixtures_dir =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../screenshots/hideout");
    let mut fixtures: Vec<std::path::PathBuf> = std::fs::read_dir(&fixtures_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    matches!(
                        p.extension().and_then(|s| s.to_str()),
                        Some("webp") | Some("png")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    fixtures.sort();

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Test OCR on fixture")
                .small()
                .strong()
                .color(weak),
        );
        if fixtures.is_empty() {
            ui.label(
                egui::RichText::new(format!("(no PNGs in {})", fixtures_dir.display()))
                    .small()
                    .color(weak),
            );
            return;
        }
        egui::ComboBox::from_id_salt("ocr-fixture-picker")
            .selected_text("pick a fixture")
            .show_ui(ui, |ui| {
                for fixture in &fixtures {
                    let label = fixture
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if ui.selectable_label(false, label).clicked() {
                        // Decode the fixture on the UI thread (one-off
                        // dev-mode click — a brief freeze is fine) so
                        // the OCR worker receives the same shape of
                        // job it would from a live VR capture. The
                        // fixture path is preserved as `source_path`
                        // so the pipeline's debug dumps still land in
                        // a useful place when `ocr_debug` is on.
                        let job = match image::open(fixture) {
                            Ok(img) => OcrJob {
                                image: img,
                                extra_rounds: Vec::new(),
                                source_path: Some(fixture.clone()),
                                kind: crate::ocr::JobKind::UpgradePanel,
                            },
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    path = %fixture.display(),
                                    "failed to decode OCR fixture",
                                );
                                return;
                            }
                        };
                        if let Err(e) = ocr_job_tx.try_send(job) {
                            tracing::warn!(
                                error = %e,
                                path = %fixture.display(),
                                "failed to enqueue OCR fixture",
                            );
                        }
                    }
                }
            });
        ui.label(
            egui::RichText::new("(needs ocr_enabled in Settings)")
                .small()
                .color(weak),
        );
    });
}

fn format_line(line: &LogLine) -> String {
    format!(
        "{:02}:{:02}:{:02} {:>5} {}",
        line.timestamp.hour(),
        line.timestamp.minute(),
        line.timestamp.second(),
        line.level,
        line.message,
    )
}

fn level_color(level: Level, dark: bool) -> Color32 {
    match level {
        Level::ERROR if dark => Color32::from_rgb(255, 130, 130),
        Level::ERROR => Color32::from_rgb(165, 30, 30),
        Level::WARN if dark => Color32::from_rgb(240, 200, 110),
        Level::WARN => Color32::from_rgb(150, 110, 0),
        Level::INFO if dark => Color32::from_rgb(220, 220, 220),
        Level::INFO => Color32::from_rgb(40, 40, 40),
        Level::DEBUG | Level::TRACE if dark => Color32::from_gray(160),
        Level::DEBUG | Level::TRACE => Color32::from_gray(110),
    }
}
