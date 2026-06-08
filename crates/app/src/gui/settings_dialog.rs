//! Modal dialog for user-tunable settings.

use crate::settings::{
    bounds, CaptureEye, CaptureHand, ColorScheme, OcrFeedbackStyle, Settings, Theme, VrSettings,
};
use parking_lot::RwLock;
use std::path::Path;
use std::sync::Arc;

/// Which group of settings is visible. The dialog is split into tabs so it
/// stays a fixed, scannable height instead of one ever-growing column. The
/// selection is transient UI state (per session, never persisted), so it lives
/// in egui's temporary memory rather than on `Settings` or the caller's `App`.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SettingsTab {
    #[default]
    General,
    Overlay,
    Capture,
}

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

            // Tab selection is transient UI state, not a setting — stash it in
            // egui memory so `show` can stay a stateless free function. Mirrors
            // the main view's `selectable_value` tab strip (see gui::mod).
            let tab_id = egui::Id::new("settings_dialog_tab");
            let mut tab: SettingsTab = ui.data_mut(|d| d.get_temp(tab_id).unwrap_or_default());
            ui.horizontal(|ui| {
                ui.selectable_value(&mut tab, SettingsTab::General, "General");
                ui.selectable_value(&mut tab, SettingsTab::Overlay, "Overlay");
                ui.selectable_value(&mut tab, SettingsTab::Capture, "Capture");
            });
            ui.data_mut(|d| d.insert_temp(tab_id, tab));

            ui.separator();

            // Cap the body height so no single tab can push the footer off the
            // bottom of the screen; a tab taller than this scrolls instead of
            // growing the window. Headroom is left for the tab strip, footer,
            // and window chrome. `auto_shrink` keeps the width pinned (so notes
            // wrap consistently) while letting short tabs keep a short window.
            let max_body_h = (ctx.screen_rect().height() - 160.0).clamp(240.0, 720.0);
            egui::ScrollArea::vertical()
                .max_height(max_body_h)
                .auto_shrink([false, true])
                .show(ui, |ui| match tab {
                    SettingsTab::General => {
                        appearance_section(ui, &mut working.theme, &mut working.color_scheme);
                        ui.add_space(12.0);
                        updates_section(ui, &mut working.check_for_updates);
                        ui.add_space(12.0);
                        storage_section(ui, data_dir, debug_dir);
                    }
                    SettingsTab::Overlay => {
                        overlay_section(ui, &mut working.vr);
                    }
                    SettingsTab::Capture => {
                        capture_guide_section(
                            ui,
                            &mut working.vr.capture_trigger,
                            &mut working.capture_eye,
                            &mut working.vr.guide_eye_only,
                        );
                        ui.add_space(12.0);
                        ocr_section(
                            ui,
                            &mut working.ocr_enabled,
                            &mut working.ocr_dismiss_seconds,
                            &mut working.ocr_auto_track,
                            &mut working.ocr_feedback_style,
                        );
                        ui.add_space(12.0);
                        debug_section(ui, &mut working.ocr_debug, &mut working.ocr_capture_trace);
                    }
                });

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

fn ocr_section(
    ui: &mut egui::Ui,
    ocr_enabled: &mut bool,
    ocr_dismiss_seconds: &mut u32,
    ocr_auto_track: &mut bool,
    ocr_feedback_style: &mut OcrFeedbackStyle,
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
    ui.label("In-headset feedback style").on_hover_text(
        "How each capture's per-item OCR read is shown in the headset. The guide \
         box's aiming reticle + a short \"Saved …\" / \"Stash: N\" status chip \
         confirm every capture regardless — this only picks how the per-item \
         detail is presented. Errors / \"reading…\" always use the centered card.",
    );
    ui.horizontal(|ui| {
        ui.selectable_value(
            ocr_feedback_style,
            OcrFeedbackStyle::OnItems,
            "On the items",
        )
        .on_hover_text(
            "Paint the read directly on the items through the guide box — the \
                 owned count on each hideout cell, a green ✓ / red ✗ on each \
                 box/stash tile. Nothing occludes the panel or grid. The default.",
        );
        ui.selectable_value(ocr_feedback_style, OcrFeedbackStyle::Card, "Card")
            .on_hover_text(
                "The original centered text card: each item's before→after count \
                 plus the running series tally + weight checksum. Occludes the \
                 panel / grid while it's up.",
            );
        ui.selectable_value(ocr_feedback_style, OcrFeedbackStyle::Grid, "Mini-grid")
            .on_hover_text(
                "A centered mini-grid diagram (#138): the per-item marks laid out \
                 in the same relative positions as on screen, read at a glance \
                 instead of scanning names.",
            );
        ui.selectable_value(ocr_feedback_style, OcrFeedbackStyle::Off, "Off")
            .on_hover_text(
                "No detailed per-item feedback — only the guide-box status chip \
                 confirms the capture. The lightest overlay.",
            );
    });

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Hide feedback after (s)").on_hover_text(
            "How long the in-headset OCR feedback lingers after a capture before \
             fading — both the per-item marks + result chip painted on the guide \
             box and, when enabled, the centered card. Raise it to read a busy \
             box grid without recapturing. Ignored while \"Save OCR debug \
             artifacts\" is on: the card then stays up until the next capture.",
        );
        stepper_slider_u32(ui, ocr_dismiss_seconds, bounds::OCR_DISMISS_SECS, 1, " s");
    });
}

/// Debug toggles, split out from the everyday Screenshot-OCR controls. Both are
/// off by default and make each capture much heavier (full-frame PNG + per-cell
/// strips on disk; FNV hashing of ~30 MB per capture) — only worth turning on to
/// gather material for a bug report.
fn debug_section(ui: &mut egui::Ui, ocr_debug: &mut bool, ocr_capture_trace: &mut bool) {
    ui.heading("Debug");
    ui.add_space(4.0);
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

fn overlay_section(ui: &mut egui::Ui, vr: &mut VrSettings) {
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

/// Capture-flow controls for the Capture tab. The trigger (which controller
/// fires a capture), the eye (which mirror texture OCR reads), and the
/// guide-box display toggle live together here: the two Right/Left side-choices
/// sit side by side, and the eye-only box option references the eye selected
/// just above it.
fn capture_guide_section(
    ui: &mut egui::Ui,
    capture_trigger: &mut CaptureHand,
    capture_eye: &mut CaptureEye,
    guide_eye_only: &mut bool,
) {
    ui.heading("Capture guide (OCR)");
    ui.add_space(4.0);
    let weak = ui.visuals().weak_text_color();
    ui.label(
        egui::RichText::new(
            "While a capture mode is armed (Hideout, or a container scan), a \
             guide box appears in the headset and the chosen controller trigger \
             takes one screenshot per pull. The cropped region OCR'd is fixed \
             per screen (the app knows each screen's shape) — aim the guide box \
             at the panel/container so it lands inside.",
        )
        .small()
        .color(weak),
    );
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Capture trigger").on_hover_text(
            "Which controller trigger takes the OCR screenshot while a capture \
             mode is armed. The other trigger is left free for in-game menu \
             navigation. Shown on the guide box in the headset.",
        );
        ui.selectable_value(capture_trigger, CaptureHand::Right, "Right");
        ui.selectable_value(capture_trigger, CaptureHand::Left, "Left");
    });

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
    ui.checkbox(guide_eye_only, "Show capture box in the capture eye only")
        .on_hover_text(
            "The aiming box is a head-locked overlay at a fixed depth, so when \
             your eyes focus on the panel it can ghost into a doubled image \
             (one box per eye). Turn this on to draw it only in the capture eye \
             (selected above) — no double box. Off by default: a one-eye HUD \
             element can feel odd (binocular rivalry) for some people, so try it \
             and see which you prefer.",
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

#[cfg(test)]
mod tests {
    //! Headless GUI tests for the Settings dialog's tab strip (the dialog was
    //! split into tabs so it stops growing into one tall column). They drive the
    //! real [`show`] through `egui_kittest` — extending the pane test pattern
    //! from #103/#106 — and pin the wiring the reorg introduced:
    //!   * selecting a tab reveals that tab's widgets and hides the others'
    //!     (the always-visible tab strip + the `match tab` routing);
    //!   * the capture-guide trigger now lives on the Capture tab next to the
    //!     OCR toggles, not with the overlay-layout sliders;
    //!   * navigating tabs (transient egui-memory state) never mutates
    //!     `Settings`, so it can't spuriously mark settings dirty.
    use super::*;
    use egui_kittest::kittest::Queryable;
    use egui_kittest::Harness;
    use std::path::PathBuf;

    /// Headless harness driving [`show`] with the given `Settings`. The closure
    /// owns the open flag, a settings clone, and a throwaway dir for the
    /// data/debug paths (only touched if the Storage folder buttons are
    /// *clicked*, which these tests don't), so the harness is `'static` and the
    /// caller keeps the original `Arc` for assertions. Sized tall enough that
    /// the tallest tab lays out unclipped, so every widget is reachable by the
    /// AccessKit queries.
    fn harness(settings: &Arc<RwLock<Settings>>) -> Harness<'static> {
        let settings = Arc::clone(settings);
        let dir = PathBuf::from(".");
        let mut open = true;
        Harness::builder()
            .with_size(egui::vec2(640.0, 900.0))
            .build_ui(move |ui| {
                let _ = show(ui.ctx(), &mut open, &settings, &dir, &dir);
            })
    }

    #[test]
    fn tab_strip_swaps_the_visible_section() {
        let settings = Arc::new(RwLock::new(Settings::default()));
        let mut h = harness(&settings);
        h.run();

        // General is the default tab: appearance/updates *and* the folded-in
        // storage controls render; the other tabs' widgets don't.
        assert!(h.query_by_label("Check for updates on startup").is_some());
        assert!(h.query_by_label("Open data folder").is_some());
        assert!(h.query_by_label("Width (m)").is_none());
        assert!(h
            .query_by_label("Auto-extract counts from VR screenshots")
            .is_none());

        // Overlay → the VR layout sliders; the General widgets are gone.
        h.get_by_label("Overlay").click();
        h.run();
        assert!(h.query_by_label("Width (m)").is_some());
        assert!(h.query_by_label("Check for updates on startup").is_none());
        assert!(h.query_by_label("Open data folder").is_none());

        // Capture → the relocated capture-guide trigger sits with the OCR
        // toggles (it used to render under the overlay-layout sliders). The
        // storage controls live on General now, not here.
        h.get_by_label("Capture").click();
        h.run();
        assert!(h.query_by_label("Capture trigger").is_some());
        assert!(h
            .query_by_label("Auto-extract counts from VR screenshots")
            .is_some());
        assert!(h.query_by_label("Width (m)").is_none());
        assert!(h.query_by_label("Open data folder").is_none());
    }

    #[test]
    fn switching_tabs_does_not_mutate_settings() {
        // Tab selection lives in egui memory, not `Settings`, so navigating
        // between tabs must leave `Settings` untouched — otherwise `show` would
        // report `changed` and the caller would mark settings dirty + persist.
        let settings = Arc::new(RwLock::new(Settings::default()));
        let before = settings.read().clone();
        let mut h = harness(&settings);
        h.run();
        h.get_by_label("Overlay").click();
        h.run();
        h.get_by_label("Capture").click();
        h.run();
        assert!(
            *settings.read() == before,
            "tab navigation must not mutate Settings"
        );
    }
}
