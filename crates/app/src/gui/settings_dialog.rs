//! Modal dialog for user-tunable settings.

use crate::settings::{bounds, Settings, VrSettings};
use parking_lot::RwLock;
use std::path::Path;
use std::sync::Arc;

/// Returns `true` if the user closed the dialog this frame (caller persists).
pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    settings: &Arc<RwLock<Settings>>,
    data_dir: &Path,
) -> Outcome {
    let mut outcome = Outcome::default();
    let mut close_now = false;

    egui::Window::new("Settings")
        .open(open)
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let before = settings.read().clone();
            let mut working = before.clone();

            vr_section(ui, &mut working.vr);

            ui.add_space(12.0);
            storage_section(ui, data_dir);

            ui.add_space(12.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Restore defaults").clicked() {
                    working = Settings::default();
                }
                ui.add_space(ui.available_width() - 60.0);
                if ui.button("Close").clicked() {
                    close_now = true;
                }
            });

            working.vr.sanitize();
            if working != before {
                *settings.write() = working;
                outcome.changed = true;
            }
        });

    if close_now {
        *open = false;
    }
    outcome.closed = !*open;
    outcome
}

#[derive(Default)]
pub struct Outcome {
    /// User changed any value this frame.
    pub changed: bool,
    /// Window just closed (caller should persist).
    pub closed: bool,
}

fn storage_section(ui: &mut egui::Ui, data_dir: &Path) {
    ui.heading("Storage");
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.button("Open data folder").clicked() {
            let _ = crate::platform::open(data_dir);
        }
        ui.label(
            egui::RichText::new(data_dir.display().to_string())
                .small()
                .color(egui::Color32::GRAY),
        );
    });
}

fn vr_section(ui: &mut egui::Ui, vr: &mut VrSettings) {
    ui.heading("VR Overlay");
    ui.add_space(4.0);

    egui::Grid::new("settings-vr")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .show(ui, |ui| {
            ui.label("Width (m)")
                .on_hover_text("Apparent width of the overlay panel in your VR view.");
            ui.add(
                egui::Slider::new(&mut vr.width_meters, bounds::WIDTH_METERS)
                    .step_by(0.05)
                    .suffix(" m"),
            );
            ui.end_row();

            ui.label("Show pitch (°)").on_hover_text(
                "Tilt your head up at least this many degrees to start the show timer.",
            );
            ui.add(
                egui::Slider::new(&mut vr.show_pitch_deg, bounds::PITCH_DEG)
                    .step_by(1.0)
                    .suffix("°"),
            );
            ui.end_row();

            ui.label("Hide pitch (°)")
                .on_hover_text("If pitch drops below this, the overlay hides immediately.");
            ui.add(
                egui::Slider::new(&mut vr.hide_pitch_deg, bounds::PITCH_DEG)
                    .step_by(1.0)
                    .suffix("°"),
            );
            ui.end_row();

            ui.label("Show dwell (ms)").on_hover_text(
                "How long you must hold the show pitch before the overlay fades in.",
            );
            ui.add(
                egui::Slider::new(&mut vr.show_dwell_ms, bounds::DWELL_MS)
                    .step_by(10.0)
                    .suffix(" ms"),
            );
            ui.end_row();
        });

    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(
            "Width applies live. Pitch and dwell currently persist only — they take \
             effect when the VR pose loop lands in the next phase.",
        )
        .small()
        .color(egui::Color32::GRAY),
    );
}
