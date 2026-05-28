//! CPU rasterizer for the OCR feedback card, submitted to the
//! head-locked OCR overlay in [`super::overlay`].
//!
//! Mirrors the shape of [`super::render`] — a fresh
//! [`tiny_skia::Pixmap`] built from a [`crate::gui::OcrFeedback`] each
//! time the worker writes a new state. No icons (keeps the renderer
//! contained); item names + before→after counts are enough to verify
//! the pipeline read each cell correctly.
//!
//! Layout knobs (CARD_W, font sizes, paddings) live as `const`s up top
//! so they're easy to retune without hunting through the draw code.

use crate::gui::{OcrFeedback, OcrFeedbackKind, OcrItemDelta};
use crate::vr::text;
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

/// Card width in pixels. The OpenVR overlay's metric width is fixed by
/// [`super::overlay::OcrOverlay::WIDTH_M`]; height-in-metres is derived
/// from the pixmap's aspect ratio, so a tall body card naturally grows
/// down without us having to reconfigure SteamVR.
pub const CARD_W: u32 = 960;
/// Minimum card height — used by the short variants (`Processing`,
/// `NotAPanel`, `Failed`) where the body is one short paragraph.
const CARD_H_MIN: u32 = 280;
/// Hard ceiling — keeps the pixmap allocation bounded for upgrades with
/// many requirements + several progression notes.
const CARD_H_MAX: u32 = 920;

const PAD_X: f32 = 28.0;
const PAD_Y: f32 = 22.0;
const SECTION_GAP: f32 = 14.0;
const ITEM_ROW_H: f32 = 32.0;
const PROG_NOTE_ROW_H: f32 = 28.0;

// Font sizes bumped ~30% over the original (22/16/13 → 30/22/18) so
// the head-locked card stays legible at the head-lock distance — user
// reported squinting at the previous sizes. Row + padding constants
// scale with them to keep the layout proportions intact.
const TITLE_PX: f32 = 30.0;
const ROW_PX: f32 = 22.0;
const SMALL_PX: f32 = 18.0;

// `tiny_skia::Color::from_rgba8` isn't a `const fn`, so these can't be
// real `const`s — fns next best, called at the few sites that need
// them.
fn bg() -> Color {
    Color::from_rgba8(28, 28, 33, 237)
}
fn fg() -> Color {
    Color::from_rgba8(235, 235, 235, 255)
}
fn weak() -> Color {
    Color::from_rgba8(158, 158, 166, 255)
}
fn positive() -> Color {
    Color::from_rgba8(110, 200, 130, 255)
}
fn negative() -> Color {
    Color::from_rgba8(220, 140, 130, 255)
}
fn processing_accent() -> Color {
    Color::from_rgba8(199, 181, 89, 255)
}
fn done_accent() -> Color {
    Color::from_rgba8(89, 150, 220, 255)
}
fn not_panel_accent() -> Color {
    Color::from_rgba8(199, 181, 89, 255)
}
fn failed_accent() -> Color {
    Color::from_rgba8(220, 99, 89, 255)
}

/// Render `feedback` to a fresh RGBA pixmap. Height auto-fits the body
/// content within `[CARD_H_MIN, CARD_H_MAX]`. Always wide enough at
/// `CARD_W` — overlong text gets clipped at the card edge rather than
/// reflowed; the on-screen overlay isn't a place to scroll, so it's
/// better to stay snappy than to wrap unpredictably.
pub fn render(feedback: &OcrFeedback) -> Pixmap {
    let accent = accent_color(&feedback.kind);
    let body_h = measure_body_height(&feedback.kind);
    let total_h = (body_h as u32).clamp(CARD_H_MIN, CARD_H_MAX);
    let mut pix = Pixmap::new(CARD_W, total_h).expect("ocr card pixmap alloc");
    pix.fill(bg());
    draw_border(&mut pix, accent);

    let mut y = PAD_Y;
    y = draw_header(&mut pix, &feedback.kind, accent, y);
    y += SECTION_GAP * 0.4;
    draw_separator(&mut pix, y);
    y += SECTION_GAP;

    y = draw_body(&mut pix, &feedback.kind, y);

    // Footer: ~28 px from the bottom edge so the countdown sits flush
    // even when the body short-circuits the natural Y advance (e.g.
    // Processing variant where the body is a single line).
    let footer_y = (total_h as f32) - PAD_Y;
    draw_separator(&mut pix, footer_y - 14.0);
    draw_footer(&mut pix, feedback, footer_y);

    // `y` is read above to layout the body; the unused var-name keeps
    // the order obvious for future extension. Drop it here to silence
    // the unused-assignment lint without sprinkling `_` over the chain.
    let _ = y;

    pix
}

fn accent_color(kind: &OcrFeedbackKind) -> Color {
    match kind {
        OcrFeedbackKind::Processing => processing_accent(),
        OcrFeedbackKind::Done { .. } => done_accent(),
        OcrFeedbackKind::NotAPanel => not_panel_accent(),
        OcrFeedbackKind::UnknownUpgrade { .. } => not_panel_accent(),
        OcrFeedbackKind::Failed(_) => failed_accent(),
    }
}

/// Compute the y-extent the body content would consume, so we can size
/// the pixmap before drawing. Matches the per-variant body branches in
/// [`draw_body`] — keep in sync.
fn measure_body_height(kind: &OcrFeedbackKind) -> f32 {
    let header_block = PAD_Y + TITLE_PX + SECTION_GAP * 0.4 + SECTION_GAP;
    let footer_block = PAD_Y + SMALL_PX + 18.0;
    let body = match kind {
        OcrFeedbackKind::Processing | OcrFeedbackKind::NotAPanel => ROW_PX + 8.0,
        OcrFeedbackKind::UnknownUpgrade { .. } => ROW_PX * 3.0 + 16.0,
        OcrFeedbackKind::Failed(_) => ROW_PX * 2.0 + 12.0,
        OcrFeedbackKind::Done {
            items,
            progression_notes,
            ..
        } => {
            let items_h = if items.is_empty() {
                ROW_PX + 8.0
            } else {
                items.len() as f32 * ITEM_ROW_H
            };
            let notes_h = if progression_notes.is_empty() {
                0.0
            } else {
                SECTION_GAP * 0.5 + progression_notes.len() as f32 * PROG_NOTE_ROW_H
            };
            items_h + notes_h
        }
    };
    header_block + body + footer_block
}

fn draw_border(pix: &mut Pixmap, accent: Color) {
    // 2 px outline inset by 1 px so it lands fully inside the canvas.
    let Some(rect) = Rect::from_xywh(
        1.0,
        1.0,
        pix.width() as f32 - 2.0,
        pix.height() as f32 - 2.0,
    ) else {
        return;
    };
    let mut pb = PathBuilder::new();
    pb.push_rect(rect);
    let Some(path) = pb.finish() else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(accent);
    paint.anti_alias = true;
    let stroke = Stroke {
        width: 2.0,
        ..Default::default()
    };
    pix.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

fn draw_separator(pix: &mut Pixmap, y: f32) {
    let Some(rect) = Rect::from_xywh(PAD_X, y, pix.width() as f32 - 2.0 * PAD_X, 1.0) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(89, 89, 97, 255));
    pix.fill_rect(rect, &paint, Transform::identity(), None);
}

fn draw_header(pix: &mut Pixmap, kind: &OcrFeedbackKind, accent: Color, y_top: f32) -> f32 {
    let baseline = y_top + TITLE_PX;
    let mut x = PAD_X;
    text::draw_text(pix, "OCR", x, baseline, SMALL_PX, accent);
    x += text::measure_width("OCR", SMALL_PX) + 12.0;

    let title = match kind {
        OcrFeedbackKind::Processing => "Reading panel…".to_string(),
        OcrFeedbackKind::Done { upgrade_name, .. } => upgrade_name.clone(),
        OcrFeedbackKind::NotAPanel => "Not an upgrade panel".to_string(),
        OcrFeedbackKind::UnknownUpgrade { .. } => "Unknown upgrade".to_string(),
        OcrFeedbackKind::Failed(_) => "OCR failed".to_string(),
    };
    text::draw_text(pix, &title, x, baseline, TITLE_PX, fg());

    if let OcrFeedbackKind::Done { level, .. } = kind {
        if *level > 0 {
            let chip = format!("Lv {level}");
            let cw = text::measure_width(&chip, SMALL_PX);
            let chip_x = pix.width() as f32 - PAD_X - cw;
            text::draw_text(pix, &chip, chip_x, baseline, SMALL_PX, weak());
        }
    }

    baseline
}

fn draw_body(pix: &mut Pixmap, kind: &OcrFeedbackKind, y_top: f32) -> f32 {
    match kind {
        OcrFeedbackKind::Processing => {
            text::draw_text(
                pix,
                "Identifying the upgrade and extracting owned counts…",
                PAD_X,
                y_top + ROW_PX,
                ROW_PX,
                weak(),
            );
            y_top + ROW_PX + 8.0
        }
        OcrFeedbackKind::NotAPanel => {
            text::draw_text(
                pix,
                "Open the Facility Upgrade panel and capture again.",
                PAD_X,
                y_top + ROW_PX,
                ROW_PX,
                weak(),
            );
            y_top + ROW_PX + 8.0
        }
        OcrFeedbackKind::UnknownUpgrade {
            module_hint,
            current_level,
            ..
        } => {
            // Spell out the missing upgrade so the user knows what
            // they need to add. Target level = current + 1 because
            // the panel header shows the CURRENT level and the user
            // is buying the next one.
            let label = match module_hint {
                Some(name) => format!("{name} Lv {}", current_level.saturating_add(1)),
                None => format!("(this upgrade) Lv {}", current_level.saturating_add(1)),
            };
            text::draw_text(
                pix,
                &format!("Missing from data: {label}"),
                PAD_X,
                y_top + ROW_PX,
                ROW_PX,
                negative(),
            );
            text::draw_text(
                pix,
                "Add the recipe in the desktop app and",
                PAD_X,
                y_top + ROW_PX * 2.0 + 4.0,
                SMALL_PX,
                weak(),
            );
            text::draw_text(
                pix,
                "file a GitHub issue with the screenshot.",
                PAD_X,
                y_top + ROW_PX * 3.0,
                SMALL_PX,
                weak(),
            );
            y_top + ROW_PX * 3.0 + 16.0
        }
        OcrFeedbackKind::Failed(msg) => {
            text::draw_text(
                pix,
                "Pipeline error:",
                PAD_X,
                y_top + ROW_PX,
                ROW_PX,
                negative(),
            );
            text::draw_text(
                pix,
                msg,
                PAD_X,
                y_top + ROW_PX * 2.0 + 4.0,
                SMALL_PX,
                weak(),
            );
            y_top + ROW_PX * 2.0 + 12.0
        }
        OcrFeedbackKind::Done {
            items,
            progression_notes,
            ..
        } => {
            let mut y = y_top;
            if items.is_empty() {
                text::draw_text(
                    pix,
                    "No items were updated.",
                    PAD_X,
                    y + ROW_PX,
                    ROW_PX,
                    weak(),
                );
                y += ROW_PX + 8.0;
            } else {
                for item in items {
                    draw_item_row(pix, item, y);
                    y += ITEM_ROW_H;
                }
            }
            if !progression_notes.is_empty() {
                y += SECTION_GAP * 0.5;
                for note in progression_notes {
                    text::draw_text(
                        pix,
                        &format!("✓ {note}"),
                        PAD_X,
                        y + PROG_NOTE_ROW_H * 0.7,
                        SMALL_PX,
                        positive(),
                    );
                    y += PROG_NOTE_ROW_H;
                }
            }
            y
        }
    }
}

fn draw_item_row(pix: &mut Pixmap, item: &OcrItemDelta, y_top: f32) {
    let baseline = y_top + ROW_PX * 0.95;
    text::draw_text(pix, &item.item_name, PAD_X, baseline, ROW_PX, fg());

    match item.after {
        Some(after) => {
            let after_text = if item.needed > 0 {
                format!("{after} / {}", item.needed)
            } else {
                format!("{after}")
            };
            let after_color = match after.cmp(&item.before) {
                std::cmp::Ordering::Greater => positive(),
                std::cmp::Ordering::Less => negative(),
                std::cmp::Ordering::Equal => weak(),
            };
            let after_w = text::measure_width(&after_text, ROW_PX);
            let after_x = pix.width() as f32 - PAD_X - after_w;
            text::draw_text(pix, &after_text, after_x, baseline, ROW_PX, after_color);
            if item.before != after {
                let before_text = format!("({}→) ", item.before);
                let before_w = text::measure_width(&before_text, SMALL_PX);
                let before_x = after_x - before_w - 4.0;
                text::draw_text(pix, &before_text, before_x, baseline, SMALL_PX, weak());
            }
        }
        None => {
            // Unread cell — show "kept N / M" in amber so the user
            // can tell the OCR couldn't read the count and we left
            // their existing value intact.
            let kept_text = if item.needed > 0 {
                format!("kept {} / {}", item.before, item.needed)
            } else {
                format!("kept {}", item.before)
            };
            let kept_w = text::measure_width(&kept_text, ROW_PX);
            let kept_x = pix.width() as f32 - PAD_X - kept_w;
            text::draw_text(
                pix,
                &kept_text,
                kept_x,
                baseline,
                ROW_PX,
                not_panel_accent(),
            );
        }
    }
}

fn draw_footer(pix: &mut Pixmap, feedback: &OcrFeedback, baseline_y: f32) {
    // The card renders exactly once per feedback (see
    // `vr::runtime::drive_ocr_overlay`), so a live countdown would
    // freeze on the first-second value and look broken. Static text
    // describing the lifecycle mode instead.
    let manual_dismiss = cfg!(debug_assertions);
    let footer = match (&feedback.kind, manual_dismiss) {
        (OcrFeedbackKind::Processing, _) => "Working… replaced when the pipeline finishes.",
        (_, true) => "Debug build — stays until the next OCR run replaces it.",
        (_, false) => "Fades out in a few seconds.",
    };
    text::draw_text(pix, footer, PAD_X, baseline_y, SMALL_PX, weak());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::{OcrFeedback, OcrFeedbackKind, OcrItemDelta};

    fn done_feedback() -> OcrFeedback {
        OcrFeedback {
            kind: OcrFeedbackKind::Done {
                upgrade_name: "Bitcoin Mine".into(),
                level: 2,
                items: vec![
                    OcrItemDelta {
                        item_name: "BPU".into(),
                        before: 0,
                        after: Some(4),
                        needed: 4,
                    },
                    OcrItemDelta {
                        item_name: "Floppy Disk".into(),
                        before: 3,
                        after: Some(2),
                        needed: 4,
                    },
                    OcrItemDelta {
                        item_name: "PC Fan (unread)".into(),
                        before: 5,
                        after: None,
                        needed: 6,
                    },
                ],
                progression_notes: vec![
                    "Auto-completed Bitcoin Mine Lv 1".into(),
                    "Now tracking Bitcoin Mine Lv 2".into(),
                ],
            },
        }
    }

    #[test]
    fn renders_done_variant_to_expected_size() {
        let pix = render(&done_feedback());
        assert_eq!(pix.width(), CARD_W);
        assert!(pix.height() >= CARD_H_MIN);
        assert!(pix.height() <= CARD_H_MAX);
    }

    #[test]
    fn renders_processing_variant() {
        let pix = render(&OcrFeedback::processing());
        assert_eq!(pix.width(), CARD_W);
        assert_eq!(pix.height(), CARD_H_MIN);
    }

    #[test]
    fn renders_not_a_panel_variant() {
        let pix = render(&OcrFeedback::not_a_panel());
        assert_eq!(pix.width(), CARD_W);
        assert_eq!(pix.height(), CARD_H_MIN);
    }

    #[test]
    fn renders_failed_variant() {
        let pix = render(&OcrFeedback::failed("file not found"));
        assert_eq!(pix.width(), CARD_W);
        assert!(pix.height() >= CARD_H_MIN);
    }

    #[test]
    fn renders_done_with_many_items_within_max_height() {
        let mut fb = done_feedback();
        if let OcrFeedbackKind::Done { items, .. } = &mut fb.kind {
            // Inflate well past anything any real upgrade has.
            for i in 0..30 {
                items.push(OcrItemDelta {
                    item_name: format!("Filler item #{i}"),
                    before: 0,
                    after: Some(1),
                    needed: 5,
                });
            }
        }
        let pix = render(&fb);
        assert_eq!(pix.height(), CARD_H_MAX, "growth should clip at CARD_H_MAX");
    }
}
