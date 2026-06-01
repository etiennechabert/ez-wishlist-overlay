//! Modal dialog for user-tunable settings.

use crate::settings::{bounds, CaptureEye, ColorScheme, Settings, Theme, VrSettings};
use parking_lot::RwLock;
use std::path::Path;
use std::sync::Arc;

/// Returns `true` if the user closed the dialog this frame (caller persists).
pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    settings: &Arc<RwLock<Settings>>,
    data_dir: &Path,
    debug_dir: &Path,
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

            appearance_section(ui, &mut working.theme, &mut working.color_scheme);

            ui.add_space(12.0);
            updates_section(ui, &mut working.check_for_updates);

            ui.add_space(12.0);
            vr_section(ui, &mut working.vr);

            ui.add_space(12.0);
            ocr_section(
                ui,
                &mut working.ocr_enabled,
                &mut working.capture_eye,
                &mut working.ocr_debug,
                &mut working.ocr_dismiss_seconds,
                &mut working.ocr_auto_track,
                &mut working.ocr_capture_trace,
                &mut working.auto_capture_interval_secs,
            );

            ui.add_space(12.0);
            storage_section(ui, data_dir, debug_dir);

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
            working.sanitize_ocr();
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

fn appearance_section(ui: &mut egui::Ui, theme: &mut Theme, color_scheme: &mut ColorScheme) {
    ui.heading("Appearance");
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Theme");
        ui.selectable_value(theme, Theme::Dark, "Dark");
        ui.selectable_value(theme, Theme::Light, "Light");
        ui.selectable_value(theme, Theme::System, "System");
    });

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Color scheme").on_hover_text(
            "Palette for the hideout status colors (tracked / ready / done / \
             pinned). Both options are colorblind-safe — they keep those states \
             distinct under red-green color vision deficiency. The legend above \
             the hideout grid updates to match your choice.",
        );
        ui.selectable_value(color_scheme, ColorScheme::OkabeIto, "Okabe-Ito")
            .on_hover_text("Color Universal Design palette (Wong 2011). The default.");
        ui.selectable_value(color_scheme, ColorScheme::Ibm, "IBM")
            .on_hover_text("IBM Design Language accessible palette.");
    });
}

#[allow(clippy::too_many_arguments)]
fn ocr_section(
    ui: &mut egui::Ui,
    ocr_enabled: &mut bool,
    capture_eye: &mut CaptureEye,
    ocr_debug: &mut bool,
    ocr_dismiss_seconds: &mut u32,
    ocr_auto_track: &mut bool,
    ocr_capture_trace: &mut bool,
    auto_capture_interval_secs: &mut u32,
) {
    ui.heading("Screenshot OCR");
    ui.add_space(4.0);
    ui.checkbox(ocr_enabled, "Auto-extract counts from VR screenshots")
        .on_hover_text(
            "When you press the screenshot hotkey on the Facility Upgrade \
             panel, the captured image is OCR'd and the owned counts for \
             every required item land in your wishlist. A head-locked \
             feedback card pops up in the headset showing each change. \
             Disable to keep the screenshot trigger but skip the OCR pass.",
        );
    // Inline hotkey hint — the SPACE binding is invisible otherwise
    // (nothing in the UI labels it), and a user can spend a long time
    // figuring out *how* to trigger a capture if the desktop window
    // isn't focused. Mention focus explicitly: the hotkey is gated on
    // `ctx.wants_keyboard_input()` so a focused text field anywhere
    // in the app silently consumes it.
    let weak = ui.visuals().weak_text_color();
    ui.label(
        egui::RichText::new(
            "Hotkey: press SPACE in this desktop window to capture. \
             The window must be focused (no text input active).",
        )
        .small()
        .color(weak),
    );

    ui.add_space(6.0);
    ui.checkbox(ocr_auto_track, "Auto-track the OCR'd upgrade")
        .on_hover_text(
            "When on, a successful OCR adds the matched upgrade to your \
             tracked list and marks every lower-level upgrade in the same \
             module as completed (the game only shows Lv N's panel after \
             Lv (N-1) is claimed, so seeing it is proof). Turn off when \
             you want to bulk-OCR panels just to refresh inventory counts \
             without touching what's tracked. Turn back on before a raid \
             when peeking at a panel should mean \"I'm working on this.\"",
        );

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Capture eye").on_hover_text(
            "Which compositor mirror eye texture to feed into OCR. On most \
             headsets the right eye stays in sync with what you see; the \
             left-eye mirror has been observed lagging by one frame on \
             some setups, which would surface as \"OCR reads the previous \
             panel.\" Try the other side if you see that.",
        );
        ui.selectable_value(capture_eye, CaptureEye::Right, "Right");
        ui.selectable_value(capture_eye, CaptureEye::Left, "Left");
    });

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Dismiss after (s)").on_hover_text(
            "How long the OCR feedback card stays in the headset before \
             fading. Ignored when \"Save OCR debug artifacts\" is on — \
             then the card sticks around until the next capture.",
        );
        stepper_slider_u32(ui, ocr_dismiss_seconds, bounds::OCR_DISMISS_SECS, 1, " s");
    });

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Auto-capture interval (s)").on_hover_text(
            "When the auto-capture loop is running (toggle it from the \
             main window header), it waits this long after each OCR read \
             before grabbing the next frame. While running, a constant \
             \"AUTO-CAPTURE ON\" card stays in the headset so you don't \
             leave it looping into a raid — and the mode always starts \
             OFF on launch.",
        );
        stepper_slider_u32(
            ui,
            auto_capture_interval_secs,
            bounds::AUTO_CAPTURE_INTERVAL_SECS,
            1,
            " s",
        );
    });

    ui.add_space(6.0);
    ui.checkbox(ocr_debug, "Save OCR debug artifacts (for bug reports)")
        .on_hover_text(
            "When on, every OCR pass keeps the full screenshot PNG, drops \
             one binarised strip per cell, writes a debug text file next \
             to the capture, AND keeps the in-headset card visible until \
             the next capture (so you can read it alongside the files). \
             Attach the bundle to a GitHub issue if OCR misreads a panel. \
             When off (default), all OCR artifacts are deleted after the \
             read finishes — screenshots are ~10 MB each.",
        );

    ui.add_space(6.0);
    ui.checkbox(
        ocr_capture_trace,
        "Verbose capture trace (diagnose mirror bugs)",
    )
    .on_hover_text(
        "When on, every capture emits a deep diagnostic trace: \
             per-process capture sequence number, FNV-1a hashes of the \
             raw pixel buffer / RGB strip / encoded PNG / decoded \
             pipeline bytes, compositor frame timing, opposite-eye \
             probe, and the first 12 OCR'd words. Lets you confirm \
             whether the mirror is handing back stale frames or the \
             bug is downstream if 'OCR is reading the previous \
             screenshot'-style issues recur. Off by default — the \
             FNV passes hash ~30 MB per capture and the logs are \
             very chatty.",
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

fn storage_section(ui: &mut egui::Ui, data_dir: &Path, debug_dir: &Path) {
    ui.heading("Storage");
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui
            .button("Open data folder")
            .on_hover_text(
                "Your saved progress and settings (state.json, overrides.json, \
                 settings.json). These are never auto-deleted — leave them be.",
            )
            .clicked()
        {
            let _ = crate::platform::open(data_dir);
        }
        let weak = ui.visuals().weak_text_color();
        ui.label(
            egui::RichText::new(data_dir.display().to_string())
                .small()
                .color(weak),
        );
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui
            .button("Open debug folder")
            .on_hover_text(
                "This session's debug bundle — VR capture screenshots plus the \
                 OCR sidecar files, written only while \"Save OCR debug \
                 artifacts\" is on. Cleared automatically every time the app \
                 starts, so it always holds just the current session. When OCR \
                 misreads a panel, attach the whole folder to a GitHub issue.",
            )
            .clicked()
        {
            let _ = crate::platform::open(debug_dir);
        }
        let weak = ui.visuals().weak_text_color();
        ui.label(
            egui::RichText::new(debug_dir.display().to_string())
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

            ui.label("Max items").on_hover_text(
                "Show at most this many items on the overlay, top priority first \
                 (pinned, then biggest grinds). 'All' shows your whole wishlist; \
                 set a small number to keep the panel to just the next few things \
                 worth grabbing.",
            );
            max_items_slider(ui, &mut vr.max_items);
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
            "Width, pitch thresholds, grid layout, and item cap apply live. \
             Height offset takes effect on the next show.",
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

/// Slider for the overlay item cap. `0` displays as "All" (no cap); the +/-
/// steppers and the slider share that sentinel. Mirrors [`stepper_slider_u32`]
/// but with the custom "All" label, which a plain integer slider can't show.
fn max_items_slider(ui: &mut egui::Ui, value: &mut u32) {
    let range = bounds::OVERLAY_MAX_ITEMS;
    let max = *range.end();
    ui.horizontal(|ui| {
        if ui
            .add_sized([STEPPER_BUTTON_W, 0.0], egui::Button::new("−"))
            .clicked()
        {
            *value = value.saturating_sub(1);
        }
        ui.add(
            egui::Slider::new(value, range)
                .integer()
                .custom_formatter(|n, _| {
                    if n < 1.0 {
                        "All".to_string()
                    } else {
                        (n as u32).to_string()
                    }
                })
                .custom_parser(|s| {
                    let s = s.trim();
                    if s.eq_ignore_ascii_case("all") {
                        Some(0.0)
                    } else {
                        s.parse::<f64>().ok()
                    }
                }),
        );
        if ui
            .add_sized([STEPPER_BUTTON_W, 0.0], egui::Button::new("+"))
            .clicked()
        {
            *value = (*value + 1).min(max);
        }
    });
}
