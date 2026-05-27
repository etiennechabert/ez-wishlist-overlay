//! Modal dialog for user-tunable settings.

use crate::settings::{bounds, Settings, Theme, VrSettings};
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

            appearance_section(ui, &mut working.theme);

            ui.add_space(12.0);
            updates_section(ui, &mut working.check_for_updates);

            ui.add_space(12.0);
            vr_section(ui, &mut working.vr);

            ui.add_space(12.0);
            ocr_section(ui, &mut working.ocr_enabled);

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

fn appearance_section(ui: &mut egui::Ui, theme: &mut Theme) {
    ui.heading("Appearance");
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Theme");
        ui.selectable_value(theme, Theme::Dark, "Dark");
        ui.selectable_value(theme, Theme::Light, "Light");
        ui.selectable_value(theme, Theme::System, "System");
    });
}

fn ocr_section(ui: &mut egui::Ui, ocr_enabled: &mut bool) {
    ui.heading("Screenshot OCR");
    ui.add_space(4.0);
    ui.checkbox(ocr_enabled, "Auto-extract counts from VR screenshots")
        .on_hover_text(
            "When you press the screenshot hotkey on the Facility Upgrade \
             panel, the saved image is OCR'd and the owned counts for that \
             upgrade's required items are written to your wishlist. \
             Disable to keep the screenshot trigger but skip the OCR pass.",
        );
}

fn updates_section(ui: &mut egui::Ui, check_for_updates: &mut bool) {
    ui.heading("Updates");
    ui.add_space(4.0);
    ui.checkbox(check_for_updates, "Check for updates on startup")
        .on_hover_text(
            "Once per launch, queries the GitHub releases API to compare \
             against the version you're running. Turning this off is fine \
             if you'd rather the app stay fully offline.",
        );
    // Re-prompt for a previously-dismissed version if the user changes their
    // mind — they'll get the banner again next time an update check runs.
}

fn storage_section(ui: &mut egui::Ui, data_dir: &Path) {
    ui.heading("Storage");
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.button("Open data folder").clicked() {
            let _ = crate::platform::open(data_dir);
        }
        let weak = ui.visuals().weak_text_color();
        ui.label(
            egui::RichText::new(data_dir.display().to_string())
                .small()
                .color(weak),
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
            stepper_slider_f32(ui, &mut vr.width_meters, bounds::WIDTH_METERS, 0.05, " m");
            ui.end_row();

            ui.label("Show pitch (°)").on_hover_text(
                "Tilt your head up at least this many degrees to start the show timer.",
            );
            stepper_slider_f32(ui, &mut vr.show_pitch_deg, bounds::PITCH_DEG, 1.0, "°");
            ui.end_row();

            ui.label("Hide pitch (°)")
                .on_hover_text("If pitch drops below this, the overlay hides immediately.");
            stepper_slider_f32(ui, &mut vr.hide_pitch_deg, bounds::PITCH_DEG, 1.0, "°");
            ui.end_row();

            ui.label("Items per row").on_hover_text(
                "Number of icon columns on the overlay grid. Rows are derived \
                 from the size of your wishlist, so a narrower grid produces a \
                 taller panel.",
            );
            stepper_slider_u32(ui, &mut vr.grid_cols, bounds::GRID_COLS, 1, "");
            ui.end_row();

            ui.label("Height offset (m)").on_hover_text(
                "How far above the HMD the overlay sits when it shows. Higher \
                 values push the panel up so you have to crane further to see \
                 it; lower values bring it down toward eye level. Takes effect \
                 on the next show — already-visible overlays keep their spot.",
            );
            stepper_slider_f32(
                ui,
                &mut vr.height_offset_m,
                bounds::HEIGHT_OFFSET_M,
                0.05,
                " m",
            );
            ui.end_row();
        });

    ui.add_space(4.0);
    let weak = ui.visuals().weak_text_color();
    ui.label(
        egui::RichText::new(
            "Width, pitch thresholds, and grid layout apply live. Height offset \
             takes effect on the next show.",
        )
        .small()
        .color(weak),
    );
}

/// Width of the +/- buttons next to each slider. Chosen so they're large
/// enough to land a VR-laser-pointer click on at typical overlay scale,
/// while still leaving the slider track readable.
const STEPPER_BUTTON_W: f32 = 24.0;

fn stepper_slider_f32(
    ui: &mut egui::Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    step: f32,
    suffix: &str,
) {
    let (min, max) = (*range.start(), *range.end());
    ui.horizontal(|ui| {
        if ui
            .add_sized([STEPPER_BUTTON_W, 0.0], egui::Button::new("−"))
            .clicked()
        {
            *value = (*value - step).max(min);
        }
        ui.add(
            egui::Slider::new(value, range)
                .step_by(step as f64)
                .suffix(suffix),
        );
        if ui
            .add_sized([STEPPER_BUTTON_W, 0.0], egui::Button::new("+"))
            .clicked()
        {
            *value = (*value + step).min(max);
        }
    });
}

fn stepper_slider_u32(
    ui: &mut egui::Ui,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    step: u32,
    suffix: &str,
) {
    let (min, max) = (*range.start(), *range.end());
    ui.horizontal(|ui| {
        if ui
            .add_sized([STEPPER_BUTTON_W, 0.0], egui::Button::new("−"))
            .clicked()
        {
            *value = value.saturating_sub(step).max(min);
        }
        let mut slider = egui::Slider::new(value, range).integer();
        if !suffix.is_empty() {
            slider = slider.suffix(suffix);
        }
        ui.add(slider);
        if ui
            .add_sized([STEPPER_BUTTON_W, 0.0], egui::Button::new("+"))
            .clicked()
        {
            *value = value.saturating_add(step).min(max);
        }
    });
}
