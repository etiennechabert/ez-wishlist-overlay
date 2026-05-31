//! Theme-aware color helpers + visuals application.
//!
//! egui ships with `Visuals::dark()` / `Visuals::light()`, but most of our
//! panes pick decorative fills (tracked/done cells, row stripes, hover) that
//! don't map cleanly to a stock visual slot. Centralizing the dark/light
//! branch here keeps the panes readable and avoids one-off `dark_mode`
//! checks scattered through the GUI.
//!
//! The *status* colors (tracked / nearly / ready / done / pinned / customized /
//! assumed) also vary by the user's [`ColorScheme`] — `Default` plus two
//! colorblind-safe palettes. Rather than thread a palette through the ~30 call
//! sites, the active scheme lives in a thread-local set once per frame
//! ([`set_scheme`]); every status-color helper reads it. The neutral helpers
//! (`row_hover`, `row_stripe`, `done_text`, …) are scheme-independent and stay
//! plain `fn(dark)`.

use crate::settings::{ColorScheme, Theme};
use egui::{Color32, Context, Visuals};
use std::cell::Cell;

thread_local! {
    /// Active status-color palette for this (UI) thread. Defaults to the
    /// hand-tuned set so any helper called before the first `set_scheme`
    /// (or off the UI thread, e.g. a unit test) still returns sane colors.
    static ACTIVE_SCHEME: Cell<ColorScheme> = const { Cell::new(ColorScheme::Default) };
}

/// Point the status-color helpers at `scheme`. Called once per frame from the
/// GUI loop with the user's setting — cheap (a `Cell` write), so it runs
/// unconditionally each frame. Changing the setting recolors the whole app on
/// the next frame without touching any call site.
pub fn set_scheme(scheme: ColorScheme) {
    ACTIVE_SCHEME.with(|s| s.set(scheme));
}

fn scheme() -> ColorScheme {
    ACTIVE_SCHEME.with(|s| s.get())
}

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

// --- Status colors (palette-aware) -----------------------------------------

/// One resolved palette for a given (scheme, dark) pair. Single source of truth
/// shared by the public helpers below, the hideout legend, and the
/// distinctness test, so the seven status colors can never silently drift apart
/// between those readers.
struct Semantic {
    tracked: Color32,
    nearly: Color32,
    ready: Color32,
    done: Color32,
    pinned: Color32,
    /// Tint for the "Edit" button on a customized recipe.
    customized: Color32,
    /// Thin stroke on cells whose recipe is a guess (empty/assumed).
    assumed: Color32,
    /// Fill for the affirmative dialog button ("Consume items required").
    primary_action: Color32,
}

/// Vivid categorical hue → muted cell fill. Dark themes darken the hue (so the
/// light row text stays readable on top); light themes pastel it toward white.
/// Used to derive the colorblind palettes from their canonical foreground hues.
fn cell_fill(base: Color32, dark: bool) -> Color32 {
    if dark {
        scale(base, 42)
    } else {
        toward_white(base, 62)
    }
}

/// Vivid hue → accent (thin stripe / small button): a touch stronger than a
/// [`cell_fill`] so it keeps its identity at small sizes.
fn accent(base: Color32, dark: bool) -> Color32 {
    if dark {
        scale(base, 66)
    } else {
        toward_white(base, 32)
    }
}

/// Multiply each channel by `pct`/100 (darken toward black, keep hue).
fn scale(c: Color32, pct: u16) -> Color32 {
    let f = |v: u8| ((v as u16 * pct) / 100).min(255) as u8;
    Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

/// Blend each channel toward white by `pct`/100 (pastel tint, keep hue).
fn toward_white(c: Color32, pct: u16) -> Color32 {
    let f = |v: u8| {
        let v = v as u16;
        (v + ((255 - v) * pct) / 100).min(255) as u8
    };
    Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

/// Okabe-Ito (Color Universal Design, Wong 2011) canonical hues. Chosen so the
/// categories stay distinguishable under protan/deutan/tritan vision.
mod okabe {
    use egui::Color32;
    pub const ORANGE: Color32 = Color32::from_rgb(230, 159, 0);
    pub const GREEN: Color32 = Color32::from_rgb(0, 158, 115);
    pub const YELLOW: Color32 = Color32::from_rgb(240, 228, 66);
    pub const BLUE: Color32 = Color32::from_rgb(0, 114, 178);
    pub const VERMILION: Color32 = Color32::from_rgb(213, 94, 0);
    pub const PURPLE: Color32 = Color32::from_rgb(204, 121, 167);
}

/// IBM Design Language accessible palette — only five hues, so the two *accent*
/// roles (pinned, customized) share magenta, separated by lightness and by
/// placement (left stripe vs. a small button); the four readiness *fills* each
/// get their own hue, which is the distinction that actually matters in use.
mod ibm {
    use egui::Color32;
    pub const BLUE: Color32 = Color32::from_rgb(100, 143, 255);
    pub const PURPLE: Color32 = Color32::from_rgb(120, 94, 240);
    pub const MAGENTA: Color32 = Color32::from_rgb(220, 38, 127);
    pub const ORANGE: Color32 = Color32::from_rgb(254, 97, 0);
    pub const YELLOW: Color32 = Color32::from_rgb(255, 176, 0);
}

/// Hand-tuned `Default` palette. Deliberately keeps tracked-blue, ready-yellow,
/// nearly-amber and done-green, but pulls *customized* and *assumed* out of the
/// warm cluster (teal and neutral slate) so they stop reading as "ready" — the
/// confusion that motivated the colorblind work.
fn default_semantic(dark: bool) -> Semantic {
    if dark {
        Semantic {
            tracked: Color32::from_rgb(30, 60, 95),
            nearly: Color32::from_rgb(85, 72, 30),
            ready: Color32::from_rgb(110, 90, 25),
            done: Color32::from_rgb(30, 70, 40),
            pinned: Color32::from_rgb(150, 120, 210),
            customized: Color32::from_rgb(30, 95, 105),
            assumed: Color32::from_rgb(95, 100, 110),
            primary_action: Color32::from_rgb(30, 90, 50),
        }
    } else {
        Semantic {
            tracked: Color32::from_rgb(205, 222, 245),
            nearly: Color32::from_rgb(243, 230, 170),
            ready: Color32::from_rgb(248, 225, 130),
            done: Color32::from_rgb(210, 235, 215),
            pinned: Color32::from_rgb(120, 85, 195),
            customized: Color32::from_rgb(165, 212, 218),
            assumed: Color32::from_rgb(198, 203, 210),
            primary_action: Color32::from_rgb(160, 205, 170),
        }
    }
}

/// Neutral slate for the "assumed recipe" marker in the colorblind palettes —
/// "we don't know the cost" reads best as a quiet grey, and it keeps the
/// scarce categorical hues free for the readiness states.
fn neutral_assumed(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(95, 100, 110)
    } else {
        Color32::from_rgb(198, 203, 210)
    }
}

fn semantic(dark: bool) -> Semantic {
    match scheme() {
        ColorScheme::Default => default_semantic(dark),
        ColorScheme::OkabeIto => Semantic {
            tracked: cell_fill(okabe::BLUE, dark),
            nearly: cell_fill(okabe::ORANGE, dark),
            ready: cell_fill(okabe::YELLOW, dark),
            done: cell_fill(okabe::GREEN, dark),
            pinned: accent(okabe::PURPLE, dark),
            customized: accent(okabe::VERMILION, dark),
            assumed: neutral_assumed(dark),
            primary_action: accent(okabe::GREEN, dark),
        },
        ColorScheme::Ibm => Semantic {
            tracked: cell_fill(ibm::BLUE, dark),
            nearly: cell_fill(ibm::ORANGE, dark),
            ready: cell_fill(ibm::YELLOW, dark),
            // IBM ships no green: "done" takes purple instead. The completed
            // state is also carried by the ✓ checkbox and the dimmed text, so
            // losing the green-as-done convention costs little here.
            done: cell_fill(ibm::PURPLE, dark),
            pinned: accent(ibm::MAGENTA, dark),
            // Shares magenta with `pinned` (only five hues) but darker, and it
            // surfaces as a button rather than a stripe.
            customized: if dark {
                scale(ibm::MAGENTA, 50)
            } else {
                toward_white(ibm::MAGENTA, 22)
            },
            assumed: neutral_assumed(dark),
            primary_action: accent(ibm::PURPLE, dark),
        },
    }
}

/// Blue cell tint for "tracked" (on your list) upgrades.
pub fn tracked_fill(dark: bool) -> Color32 {
    semantic(dark).tracked
}

/// Cell tint for "nearly ready" tracked upgrades — only 1–2 distinct items
/// short of claimable. In `Default` it sits between `tracked_fill` and
/// `ready_fill` as a warmth gradient (tracked → nearly → ready), kept dimmer
/// than `ready_fill` so a near-complete cell isn't mistaken for a claimable
/// one; the colorblind palettes give it its own distinct hue instead.
pub fn nearly_ready_fill(dark: bool) -> Color32 {
    semantic(dark).nearly
}

/// Cell tint for "ready to claim" tracked upgrades — every required item has
/// been collected, so the user can claim it in-game.
pub fn ready_fill(dark: bool) -> Color32 {
    semantic(dark).ready
}

/// Cell tint for completed ("done") upgrades.
pub fn done_fill(dark: bool) -> Color32 {
    semantic(dark).done
}

/// Accent for pinned (user-prioritized) upgrades — drawn as a left stripe on
/// the By-progress row. Distinct from every readiness color so "this is a
/// priority" never reads as a readiness cue.
pub fn pinned_accent(dark: bool) -> Color32 {
    semantic(dark).pinned
}

/// Tint applied to the "Edit" button on recipes the user has corrected, so
/// modified rows stand out. Deliberately *not* in the warm/ready family.
pub fn override_marker(dark: bool) -> Color32 {
    semantic(dark).customized
}

/// Faint stroke for upgrades whose recipe is *assumed* (the effective recipe is
/// empty, so we don't actually know the cost). A quiet neutral — "this recipe
/// is a guess, open Edit" — rather than a loud warning, so a grid full of
/// Lv3/Lv4 placeholders doesn't become a wall of alarm.
pub fn assumed_marker(dark: bool) -> Color32 {
    semantic(dark).assumed
}

/// Fill for the affirmative, recommended button in a confirmation dialog —
/// currently "Consume items required" in the upgrade-completion modal. More
/// saturated than the muted row tints so it reads as a call-to-action.
pub fn primary_action_fill(dark: bool) -> Color32 {
    semantic(dark).primary_action
}

/// Outline drawn around the upgrade cell currently open in the recipe editor.
/// Scheme-independent on purpose — a bright neutral ring is unambiguously
/// "this is the one I'm editing" and never collides with any palette hue
/// (the gap the user hit: focus had no color of its own).
pub fn selected_outline(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(236, 239, 246)
    } else {
        Color32::from_rgb(40, 44, 52)
    }
}

// --- Neutral helpers (scheme-independent) ----------------------------------

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

pub fn source_text(dark: bool) -> Color32 {
    if dark {
        Color32::from_gray(160)
    } else {
        Color32::from_gray(95)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMES: [ColorScheme; 3] = [
        ColorScheme::Default,
        ColorScheme::OkabeIto,
        ColorScheme::Ibm,
    ];

    /// The four readiness *fills* are the primary signal — they must never
    /// collide within a scheme, in either theme. (The accents may share a hue;
    /// see the IBM note above.)
    #[test]
    fn readiness_fills_distinct_in_every_scheme() {
        for sc in SCHEMES {
            set_scheme(sc);
            for dark in [true, false] {
                let fills = [
                    tracked_fill(dark),
                    nearly_ready_fill(dark),
                    ready_fill(dark),
                    done_fill(dark),
                ];
                for i in 0..fills.len() {
                    for j in (i + 1)..fills.len() {
                        assert_ne!(
                            fills[i], fills[j],
                            "scheme {sc:?} dark={dark}: readiness fills {i} and {j} collide"
                        );
                    }
                }
            }
        }
        set_scheme(ColorScheme::Default);
    }

    /// Switching the scheme actually changes the resolved colors (guards the
    /// thread-local wiring).
    #[test]
    fn set_scheme_changes_palette() {
        set_scheme(ColorScheme::Default);
        let d = ready_fill(true);
        set_scheme(ColorScheme::OkabeIto);
        let o = ready_fill(true);
        set_scheme(ColorScheme::Ibm);
        let i = ready_fill(true);
        assert_ne!(d, o);
        assert_ne!(o, i);
        set_scheme(ColorScheme::Default);
    }

    /// The whole point of the colorblind palettes: "customized" must not read
    /// as "ready". (This was the reported confusion in `Default` too.)
    #[test]
    fn customized_never_equals_ready() {
        for sc in SCHEMES {
            set_scheme(sc);
            for dark in [true, false] {
                assert_ne!(
                    override_marker(dark),
                    ready_fill(dark),
                    "scheme {sc:?} dark={dark}: customized must differ from ready"
                );
            }
        }
        set_scheme(ColorScheme::Default);
    }
}
