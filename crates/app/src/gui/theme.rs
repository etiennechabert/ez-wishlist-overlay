//! Theme-aware color helpers + visuals application.
//!
//! egui ships with `Visuals::dark()` / `Visuals::light()`, but most of our
//! panes pick decorative fills (tracked/done cells, row stripes, hover) that
//! don't map cleanly to a stock visual slot. Centralizing the dark/light
//! branch here keeps the panes readable and avoids one-off `dark_mode`
//! checks scattered through the GUI.

use crate::settings::Theme;
use egui::{Color32, Context, Visuals};

/// Apply the user's chosen theme to the egui context.
pub fn apply(ctx: &Context, theme: Theme) {
    match theme {
        Theme::Dark => ctx.set_visuals(Visuals::dark()),
        Theme::Light => ctx.set_visuals(Visuals::light()),
        Theme::System => {
            // Fall back to whatever the OS reports; egui handles the
            // detection itself when we don't force a specific visual.
            let dark = ctx.system_theme().is_none_or(|t| t == egui::Theme::Dark);
            ctx.set_visuals(if dark {
                Visuals::dark()
            } else {
                Visuals::light()
            });
        }
    }
}

pub fn tracked_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(30, 60, 95)
    } else {
        Color32::from_rgb(205, 222, 245)
    }
}

pub fn done_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(30, 70, 40)
    } else {
        Color32::from_rgb(210, 235, 215)
    }
}

pub fn row_hover(dark: bool) -> Color32 {
    if dark {
        Color32::from_white_alpha(22)
    } else {
        Color32::from_black_alpha(18)
    }
}

pub fn row_stripe(dark: bool) -> Color32 {
    if dark {
        Color32::from_white_alpha(3)
    } else {
        Color32::from_black_alpha(8)
    }
}

pub fn placeholder_icon(dark: bool) -> Color32 {
    if dark {
        Color32::from_gray(60)
    } else {
        Color32::from_gray(205)
    }
}

pub fn done_text(dark: bool) -> Color32 {
    if dark {
        Color32::from_gray(140)
    } else {
        Color32::from_gray(115)
    }
}

pub fn done_frame_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_gray(36)
    } else {
        Color32::from_gray(225)
    }
}

/// Warm yellow tint for "ready to claim" tracked upgrades — every required
/// item has been collected, so the user can claim it in-game. Picked to
/// read clearly against both dark and light backgrounds without blowing
/// out the row text.
pub fn ready_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(110, 90, 25)
    } else {
        Color32::from_rgb(248, 225, 130)
    }
}

pub fn source_text(dark: bool) -> Color32 {
    if dark {
        Color32::from_gray(160)
    } else {
        Color32::from_gray(95)
    }
}

/// Warm tint applied to the "Edit" button on recipes the user has corrected,
/// so modified rows stand out in the otherwise neutral grid.
pub fn override_marker(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(140, 90, 30)
    } else {
        Color32::from_rgb(245, 205, 130)
    }
}
