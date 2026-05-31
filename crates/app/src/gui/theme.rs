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

/// Muted amber for "nearly ready" tracked upgrades — only 1–2 distinct items
/// short of claimable. Sits deliberately *between* `tracked_fill` (cool blue,
/// "on your list") and `ready_fill` (warm yellow, "claim it now"), so the grid
/// reads as a warmth gradient tracked → nearly → ready. Kept dimmer than
/// `ready_fill` so a near-complete cell isn't mistaken for a claimable one.
pub fn nearly_ready_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(85, 72, 30)
    } else {
        Color32::from_rgb(243, 230, 170)
    }
}

/// Violet accent for pinned (user-prioritized) upgrades — drawn as a left
/// stripe on the By-progress row. Deliberately cool/violet, distinct from every
/// warm marker (ready yellow, override/assumed browns) and from the tracked
/// blue, so "this is a priority" never reads as a readiness or warning cue.
pub fn pinned_accent(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(150, 120, 210)
    } else {
        Color32::from_rgb(120, 85, 195)
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

/// Faint stroke/text tint for upgrades whose recipe is *assumed* — the
/// effective recipe is empty, so we don't actually know what the upgrade
/// costs. These cells never turn the "ready" yellow no matter how much the
/// user collects (correct, but otherwise invisible); this gives a quiet
/// "this recipe is a guess, open Edit" cue. Deliberately a muted brick rather
/// than a loud warning color and distinct from `ready_fill`'s warm yellow, so
/// a grid full of Lv3/Lv4 placeholders doesn't become a wall of alarm.
pub fn assumed_marker(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(120, 70, 70)
    } else {
        Color32::from_rgb(225, 180, 175)
    }
}

/// Green fill for the affirmative, recommended button in a confirmation
/// dialog — currently "Consume items required" in the upgrade-completion
/// modal, the path that keeps the app's collected counts in lockstep with the
/// game. More saturated than the muted row tints above so it reads as a
/// call-to-action, while still pairing with the default button text color in
/// both themes. Shares the dark-theme green of the update banner.
pub fn primary_action_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(30, 90, 50)
    } else {
        Color32::from_rgb(160, 205, 170)
    }
}
