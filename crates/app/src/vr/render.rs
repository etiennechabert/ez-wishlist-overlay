//! CPU rasterizer for the overlay's icon grid.
//!
//! Renders a `Vec<u8>` of RGBA pixels that the (future) OpenVR overlay
//! submits via `SetOverlayRaw`. Designed to be unit-testable today: the
//! main entry point takes the `ActiveItem` slice and an icon-bytes resolver,
//! so tests don't need a real `IconCache` or GPU.
//!
//! Layout: a grid of `cols` columns × N rows; each cell is `CELL_PX` square
//! with `CELL_PADDING` between. The number of columns is configured by the
//! user (see `VrSettings::grid_cols`); the number of rows is derived from
//! the wishlist size, so the panel is exactly as tall as it needs to be.
//! Cells render the item icon, a small progress text, and a "done" overlay
//! (semi-transparent gray) when the item's collected count reaches its
//! needed count.

use crate::state::ActiveItem;
use tiny_skia::{Color, FillRule, IntSize, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

pub const CELL_PX: u32 = 160;
pub const CELL_PADDING: u32 = 8;
/// Vertical space reserved at the top of each cell for the item name.
/// Sized to comfortably fit a 14px font with breathing room above/below.
const NAME_BAND_H: f32 = 22.0;
/// Pixel size of the item name font.
const NAME_FONT_PX: f32 = 14.0;
/// Horizontal padding inside the name band — leaves the truncation
/// ellipsis room before it bumps against the cell border.
const NAME_SIDE_PAD: f32 = 6.0;
/// Hard cap on rendered rows. Beyond this we fall through to the "+N more"
/// indicator instead of growing the canvas without bound (a wishlist with
/// 200 items at 2 cols would otherwise blow up the texture). 8 keeps the
/// canvas well under 2k pixels tall even at the smallest `cols`.
pub const MAX_ROWS: u32 = 8;

/// Dimensions used for the "no items tracked" placeholder — the canvas
/// shouldn't reserve space for a grid that isn't there.
const PLACEHOLDER_WIDTH: u32 = 600;
const PLACEHOLDER_HEIGHT: u32 = 120;

pub struct CellHit {
    pub item_id: String,
    /// Where to draw the hover-highlight border. Tight to the visible icon
    /// so the yellow box always traces the icon's outline — never bleeds
    /// into the name band above or the progress chip below.
    pub rect: Rect,
    /// Where the click/hover detection fires. Extends past the icon into
    /// the name band and chip area so users don't fall into a 68px dead
    /// zone between vertically-stacked rows. The 8px CELL_PADDING between
    /// cells is the only true gap.
    pub hit_rect: Rect,
    /// Target quantity at render time. Cached here so the click handler
    /// doesn't have to re-derive it from `AppState::active_items()` —
    /// any staleness between render and click is bounded by the debounce
    /// window and the version-driven re-render.
    pub needed: u32,
}

/// Render `items` to a fresh RGBA pixmap with a `cols`-wide grid.
/// `resolve_icon` is called with each item's `icon_path` and should return
/// the raw image bytes (PNG/WebP/etc.), or `None` to render a placeholder
/// square.
///
/// Returns the pixmap plus a hit-test table mapping each rendered cell's
/// rect back to its item id (for the VR input handler). The pixmap's
/// width/height are derived from `cols` and the item count; callers that
/// need to translate texture-space input coordinates into pixel coordinates
/// should read those from the returned pixmap.
pub fn render<F>(items: &[ActiveItem], cols: u32, mut resolve_icon: F) -> (Pixmap, Vec<CellHit>)
where
    F: FnMut(&str) -> Option<std::borrow::Cow<'static, [u8]>>,
{
    // Defensive: `Settings::sanitize` clamps to `bounds::GRID_COLS`, but a
    // caller passing 0 would otherwise divide-by-zero below.
    let cols = cols.max(1);
    let max_cells = (cols * MAX_ROWS) as usize;
    let visible_count = items.len().min(max_cells);

    if visible_count == 0 {
        let mut pixmap = Pixmap::new(PLACEHOLDER_WIDTH, PLACEHOLDER_HEIGHT).expect("pixmap alloc");
        pixmap.fill(Color::from_rgba8(20, 20, 24, 220));
        draw_placeholder_text(&mut pixmap);
        return (pixmap, Vec::new());
    }

    let rows_needed = (visible_count as u32).div_ceil(cols);
    let canvas_w = CELL_PADDING + cols * (CELL_PX + CELL_PADDING);
    let canvas_h = CELL_PADDING + rows_needed * (CELL_PX + CELL_PADDING);

    let mut pixmap = Pixmap::new(canvas_w, canvas_h).expect("pixmap alloc");
    pixmap.fill(Color::from_rgba8(20, 20, 24, 220));

    let mut hits = Vec::with_capacity(visible_count);

    for (idx, item) in items.iter().take(visible_count).enumerate() {
        let col = idx as u32 % cols;
        let row = idx as u32 / cols;
        let x = CELL_PADDING + col * (CELL_PX + CELL_PADDING);
        let y = CELL_PADDING + row * (CELL_PX + CELL_PADDING);

        let Some(cell_rect) = Rect::from_xywh(x as f32, y as f32, CELL_PX as f32, CELL_PX as f32)
        else {
            continue;
        };
        draw_cell(&mut pixmap, item, cell_rect, &mut resolve_icon);
        // Visual highlight rect: traces the visible icon band only. Must
        // stay in sync with `draw_cell`'s `icon_rect`.
        let Some(visual_rect) = Rect::from_xywh(
            x as f32,
            y as f32 + NAME_BAND_H,
            CELL_PX as f32,
            CELL_PX as f32 - NAME_BAND_H - 12.0 - 26.0,
        ) else {
            continue;
        };
        hits.push(CellHit {
            item_id: item.item_id.clone(),
            rect: visual_rect,
            // Collision rect = full cell. The 8px CELL_PADDING gap between
            // cells stays as the only "miss" zone, so sweeping the laser
            // from row N to row N+1 doesn't fall through ~68px of nothing.
            hit_rect: cell_rect,
            needed: item.needed,
        });
    }

    if items.len() > visible_count {
        draw_more_indicator(&mut pixmap, items.len() - visible_count);
    }

    (pixmap, hits)
}

/// Paint a bright hover border on top of an already-rendered pixmap. Cheap
/// — just one stroked rectangle — so we can apply it per frame without
/// re-rendering the whole grid (icon decoding is the slow path).
pub fn apply_hover_highlight(pixmap: &mut Pixmap, hits: &[CellHit], hover_id: &str) {
    let Some(hit) = hits.iter().find(|h| h.item_id == hover_id) else {
        return;
    };
    let mut pb = PathBuilder::new();
    pb.push_rect(hit.rect);
    let Some(path) = pb.finish() else { return };
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(255, 220, 60, 255));
    pixmap.stroke_path(
        &path,
        &paint,
        &Stroke {
            width: 4.0,
            ..Stroke::default()
        },
        Transform::identity(),
        None,
    );
}

fn draw_cell<F>(pixmap: &mut Pixmap, item: &ActiveItem, rect: Rect, resolve_icon: &mut F)
where
    F: FnMut(&str) -> Option<std::borrow::Cow<'static, [u8]>>,
{
    let done = item.needed > 0 && item.collected >= item.needed;

    // Cell background.
    let bg_color = if done {
        Color::from_rgba8(40, 40, 44, 220)
    } else {
        Color::from_rgba8(50, 52, 58, 255)
    };
    let mut paint = Paint::default();
    paint.set_color(bg_color);
    pixmap.fill_rect(rect, &paint, Transform::identity(), None);

    // Border.
    let mut border_paint = Paint::default();
    border_paint.set_color(Color::from_rgba8(80, 84, 90, 255));
    let mut pb = PathBuilder::new();
    pb.push_rect(rect);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(
            &path,
            &border_paint,
            &Stroke {
                width: 2.0,
                ..Stroke::default()
            },
            Transform::identity(),
            None,
        );
    }

    // Item name across the top of the cell. Truncates with an ellipsis
    // when wider than the cell can fit at the chosen font size.
    draw_cell_name(pixmap, rect, &item.name);

    // Icon, centered with 12px side padding. Top is offset to leave room
    // for the name band (NAME_BAND_H); bottom reserves room for the
    // progress chip (22px strip + 4px chrome).
    let icon_pad = 12.0;
    let icon_rect = Rect::from_xywh(
        rect.x() + icon_pad,
        rect.y() + NAME_BAND_H,
        rect.width() - 2.0 * icon_pad,
        rect.height() - NAME_BAND_H - icon_pad - 26.0,
    )
    .unwrap_or(rect);
    if let Some(bytes) = resolve_icon(&item.icon_path) {
        if let Some(icon_pm) = decode_icon(&bytes) {
            blit_icon(pixmap, &icon_pm, icon_rect, done);
        } else {
            draw_icon_placeholder(pixmap, icon_rect);
        }
    } else {
        draw_icon_placeholder(pixmap, icon_rect);
    }

    // Progress block bottom-right.
    draw_progress_chip(
        pixmap,
        rect,
        item.collected.min(item.needed.max(1)),
        item.needed,
    );

    if done {
        // Desaturate overlay.
        let mut overlay = Paint::default();
        overlay.set_color(Color::from_rgba8(0, 0, 0, 110));
        pixmap.fill_rect(rect, &overlay, Transform::identity(), None);
    }
}

fn decode_icon(bytes: &[u8]) -> Option<Pixmap> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let pm = Pixmap::from_vec(rgba.into_raw(), IntSize::from_wh(w, h)?)?;
    Some(pm)
}

fn blit_icon(dest: &mut Pixmap, icon: &Pixmap, target: Rect, dimmed: bool) {
    let scale_x = target.width() / icon.width() as f32;
    let scale_y = target.height() / icon.height() as f32;
    let scale = scale_x.min(scale_y);
    let draw_w = icon.width() as f32 * scale;
    let draw_h = icon.height() as f32 * scale;
    let tx = target.x() + (target.width() - draw_w) / 2.0;
    let ty = target.y() + (target.height() - draw_h) / 2.0;

    let mut paint = tiny_skia::PixmapPaint::default();
    if dimmed {
        paint.opacity = 0.5;
    }
    let transform = Transform::from_translate(tx, ty)
        .post_scale(scale, scale)
        .post_translate(-tx * (scale - 1.0) / scale, -ty * (scale - 1.0) / scale);
    // Simpler approach: pre-resize the icon via image crate instead of skia.
    // tiny-skia's draw_pixmap with scale is fiddly with origins.
    let _ = transform;

    let resized = if (scale - 1.0).abs() > 0.001 {
        resize_pixmap(icon, draw_w.round() as u32, draw_h.round() as u32)
    } else {
        icon.clone()
    };
    dest.draw_pixmap(
        tx.round() as i32,
        ty.round() as i32,
        resized.as_ref(),
        &paint,
        Transform::identity(),
        None,
    );
}

fn resize_pixmap(src: &Pixmap, w: u32, h: u32) -> Pixmap {
    // Hop through `image` for a quality resize. tiny-skia has no built-in
    // image resampler.
    let raw = src.data().to_vec();
    let img = image::RgbaImage::from_raw(src.width(), src.height(), raw).expect("rgba dims");
    let resized = image::imageops::resize(
        &img,
        w.max(1),
        h.max(1),
        image::imageops::FilterType::Lanczos3,
    );
    Pixmap::from_vec(
        resized.into_raw(),
        IntSize::from_wh(w.max(1), h.max(1)).expect("size"),
    )
    .expect("pixmap from resized")
}

fn draw_icon_placeholder(pixmap: &mut Pixmap, rect: Rect) {
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(70, 74, 80, 255));
    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    // Diagonal slash, indicating missing icon.
    let mut pb = PathBuilder::new();
    pb.move_to(rect.x(), rect.y());
    pb.line_to(rect.right(), rect.bottom());
    if let Some(path) = pb.finish() {
        let mut p = Paint::default();
        p.set_color(Color::from_rgba8(40, 44, 50, 255));
        pixmap.stroke_path(
            &path,
            &p,
            &Stroke {
                width: 3.0,
                ..Stroke::default()
            },
            Transform::identity(),
            None,
        );
    }
}

fn draw_cell_name(pixmap: &mut Pixmap, cell: Rect, full_name: &str) {
    let max_w = cell.width() - 2.0 * NAME_SIDE_PAD;
    let label = truncate_to_width(full_name, max_w, NAME_FONT_PX);
    if label.is_empty() {
        return;
    }
    let text_w = super::text::measure_width(&label, NAME_FONT_PX);
    let text_x = cell.x() + (cell.width() - text_w) * 0.5;
    // Baseline ~70% down the band so the name sits closer to the icon than
    // to the cell border (cap height + a hair of leading above).
    let baseline_y = cell.y() + NAME_BAND_H * 0.70;
    super::text::draw_text(
        pixmap,
        &label,
        text_x,
        baseline_y,
        NAME_FONT_PX,
        Color::from_rgba8(225, 228, 234, 255),
    );
}

/// Trim `text` to whatever fits in `max_w` at the given font size, appending
/// "…" when truncated. Returns the original string unchanged if it already
/// fits, and an empty string if even the ellipsis alone is too wide (very
/// narrow cells — shouldn't happen at our sizes but stays defensive).
fn truncate_to_width(text: &str, max_w: f32, font_px: f32) -> String {
    if super::text::measure_width(text, font_px) <= max_w {
        return text.to_string();
    }
    let ellipsis = "…";
    let ellipsis_w = super::text::measure_width(ellipsis, font_px);
    if ellipsis_w > max_w {
        return String::new();
    }
    let mut chars: Vec<char> = text.chars().collect();
    // Drop chars from the end until "{prefix}…" fits.
    while !chars.is_empty() {
        chars.pop();
        let prefix: String = chars.iter().collect();
        if super::text::measure_width(&prefix, font_px) + ellipsis_w <= max_w {
            return format!("{prefix}{ellipsis}");
        }
    }
    ellipsis.to_string()
}

fn draw_progress_chip(pixmap: &mut Pixmap, cell: Rect, collected: u32, needed: u32) {
    // Bottom strip with filled progress bar + "collected/needed" text
    // centered over it. Bar grows from left at the collected/needed ratio;
    // text reads white over both the dark background and the colored fill,
    // so it stays legible across the whole range.
    let strip_h = 22.0;
    let strip = Rect::from_xywh(
        cell.x() + 4.0,
        cell.bottom() - strip_h - 4.0,
        cell.width() - 8.0,
        strip_h,
    )
    .unwrap_or(cell);

    let mut bg = Paint::default();
    bg.set_color(Color::from_rgba8(20, 22, 26, 230));
    pixmap.fill_rect(strip, &bg, Transform::identity(), None);

    if needed > 0 {
        let ratio = (collected as f32 / needed as f32).clamp(0.0, 1.0);
        if ratio > 0.0 {
            let fill = Rect::from_xywh(strip.x(), strip.y(), strip.width() * ratio, strip.height())
                .unwrap_or(strip);
            let mut fg = Paint::default();
            let color = if ratio >= 1.0 {
                Color::from_rgba8(70, 140, 80, 255)
            } else {
                Color::from_rgba8(120, 160, 200, 255)
            };
            fg.set_color(color);
            pixmap.fill_rect(fill, &fg, Transform::identity(), None);
        }
    }

    // A border on the strip for legibility.
    let mut border = Paint::default();
    border.set_color(Color::from_rgba8(120, 124, 130, 255));
    let mut pb = PathBuilder::new();
    pb.push_rect(strip);
    if let Some(p) = pb.finish() {
        pixmap.stroke_path(
            &p,
            &border,
            &Stroke {
                width: 1.0,
                ..Stroke::default()
            },
            Transform::identity(),
            None,
        );
    }

    // "collected/needed" centered on the strip. Capped collected at
    // `needed` so a stale state.json doesn't show "11/10" mid-debounce.
    let shown_collected = collected.min(needed.max(1));
    let label = if needed == 0 {
        "—".to_string()
    } else {
        format!("{}/{}", shown_collected, needed)
    };
    let font_px = 14.0;
    let text_w = super::text::measure_width(&label, font_px);
    let text_x = strip.x() + (strip.width() - text_w) * 0.5;
    // Baseline ~75% down the strip — empirical, lines up the digit caps
    // with the vertical center of the bar at the font sizes we use.
    let baseline_y = strip.y() + strip.height() * 0.75;
    super::text::draw_text(
        pixmap,
        &label,
        text_x,
        baseline_y,
        font_px,
        Color::from_rgba8(245, 247, 250, 255),
    );
}

fn draw_more_indicator(pixmap: &mut Pixmap, extra: usize) {
    // A small filled rect bottom-right; full text rendering deferred to
    // Phase 3. Position is relative to the actual pixmap size so it lands
    // in the bottom-right corner regardless of grid_cols / rows.
    let Some(badge) = Rect::from_xywh(
        (pixmap.width().saturating_sub(80)) as f32,
        (pixmap.height().saturating_sub(30)) as f32,
        72.0,
        22.0,
    ) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(160, 90, 70, 255));
    pixmap.fill_rect(badge, &paint, Transform::identity(), None);
    let _ = extra; // shown as a generic indicator until text rendering is added
}

fn draw_placeholder_text(pixmap: &mut Pixmap) {
    // Centered rect so VR users see "something" when nothing is tracked.
    // Sized relative to the placeholder canvas. Real text comes in Phase 3
    // with fontdue.
    let w = pixmap.width() as f32;
    let h = pixmap.height() as f32;
    let rect_w = (w * 0.66).min(400.0);
    let rect_h = (h * 0.5).min(60.0);
    let Some(r) = Rect::from_xywh((w - rect_w) * 0.5, (h - rect_h) * 0.5, rect_w, rect_h) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(50, 52, 58, 255));
    pixmap.fill_rect(r, &paint, Transform::identity(), None);
    let _ = FillRule::Winding; // touch the import so it's used
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, name: &str, needed: u32, collected: u32) -> ActiveItem {
        ActiveItem {
            item_id: id.to_string(),
            name: name.to_string(),
            icon_path: format!("icons/{id}.png"),
            needed,
            collected,
            sources: vec!["test".into()],
        }
    }

    #[test]
    fn renders_empty_to_placeholder_pixmap() {
        let (pm, hits) = render(&[], 6, |_| None);
        assert!(hits.is_empty());
        // Placeholder is small — we don't reserve grid-sized space when the
        // wishlist is empty.
        assert!(pm.width() < CELL_PX * 6);
        assert!(pm.height() < CELL_PX);
    }

    #[test]
    fn renders_grid_with_correct_hit_rects() {
        let items: Vec<ActiveItem> = (0..7)
            .map(|i| item(&format!("i{i}"), &format!("Item {i}"), 5, i as u32))
            .collect();
        let (_pm, hits) = render(&items, 6, |_| None);
        assert_eq!(hits.len(), 7);
        // Hit rect is the icon band only: y starts NAME_BAND_H below the
        // cell top so it doesn't catch rays pointed at the name above the
        // icon. First cell at (8, 8 + NAME_BAND_H).
        let h0 = &hits[0];
        assert_eq!(h0.item_id, "i0");
        assert!((h0.rect.x() - 8.0).abs() < 0.5);
        assert!((h0.rect.y() - (8.0 + NAME_BAND_H)).abs() < 0.5);
        // 7th cell wraps to row 2 (idx 6 → col 0, row 1) at the default 6 cols.
        let h6 = &hits[6];
        assert_eq!(h6.item_id, "i6");
        assert!((h6.rect.x() - 8.0).abs() < 0.5);
        let expected_y6 = 8.0 + CELL_PX as f32 + CELL_PADDING as f32 + NAME_BAND_H;
        assert!((h6.rect.y() - expected_y6).abs() < 0.5);
    }

    #[test]
    fn rows_derive_from_cols() {
        // 7 items at 3 cols → 3 rows (3+3+1). Last cell should be on row 2.
        let items: Vec<ActiveItem> = (0..7)
            .map(|i| item(&format!("i{i}"), &format!("Item {i}"), 1, 0))
            .collect();
        let (pm, hits) = render(&items, 3, |_| None);
        assert_eq!(hits.len(), 7);
        let last = &hits[6];
        let row_2_y = (CELL_PADDING + 2 * (CELL_PX + CELL_PADDING)) as f32 + NAME_BAND_H;
        assert!((last.rect.y() - row_2_y).abs() < 0.5);
        // Canvas height matches exactly the rows we rendered.
        let expected_h = CELL_PADDING + 3 * (CELL_PX + CELL_PADDING);
        assert_eq!(pm.height(), expected_h);
        // Width matches 3 cols.
        let expected_w = CELL_PADDING + 3 * (CELL_PX + CELL_PADDING);
        assert_eq!(pm.width(), expected_w);
    }

    #[test]
    fn caps_visible_at_max_rows_times_cols() {
        // 100 items at 5 cols, MAX_ROWS=8 → cap at 40 cells.
        let items: Vec<ActiveItem> = (0..100)
            .map(|i| item(&format!("i{i}"), &format!("Item {i}"), 1, 0))
            .collect();
        let (_pm, hits) = render(&items, 5, |_| None);
        assert_eq!(hits.len(), (5 * MAX_ROWS) as usize);
    }

    /// End-to-end snapshot using real embedded icons + the loaded data.json.
    /// Set RENDER_SNAPSHOT=1 to save the PNG; otherwise this test just runs
    /// the render path to make sure embedded icons decode.
    #[test]
    fn snapshot_with_real_icons() {
        use crate::assets;
        use crate::state::AppState;
        use std::sync::Arc;

        let data = Arc::new(assets::load_game_data().expect("embedded data.json"));
        let mut state = AppState::new(data);
        // Track every upgrade in the first three modules so we get a varied grid.
        let ids: Vec<String> = state
            .data
            .modules
            .iter()
            .take(3)
            .flat_map(|m| m.upgrades.iter().map(|u| u.id.clone()))
            .collect();
        for id in &ids {
            state.set_tracked_upgrade(id, true);
        }
        let items = state.active_items();
        assert!(!items.is_empty(), "real data should produce active items");

        let (pm, hits) = render(&items, 6, assets::read_icon);
        assert!(!hits.is_empty());

        if std::env::var("RENDER_SNAPSHOT").as_deref() == Ok("1") {
            let path = std::env::temp_dir().join("ez-wishlist-overlay-real-snapshot.png");
            pm.save_png(&path).expect("save real snapshot");
            eprintln!("real snapshot saved to {}", path.display());
        }
    }

    /// Snapshot: save a PNG to disk for manual inspection if RENDER_SNAPSHOT=1.
    #[test]
    fn snapshot_for_manual_review() {
        if std::env::var("RENDER_SNAPSHOT").as_deref() != Ok("1") {
            return;
        }
        let items: Vec<ActiveItem> = vec![
            item("bolts", "Bolts", 20, 12),
            item("screws", "Screws", 15, 8),
            item("wire", "Wire", 3, 0),
            item("battery", "Battery", 5, 5), // done
            item("intel", "Intel", 9, 9),     // done
            item("paint", "Paint", 4, 2),
        ];
        let (pm, _) = render(&items, 6, |_| None);
        let path = std::env::temp_dir().join("ez-wishlist-overlay-snapshot.png");
        pm.save_png(&path).expect("save snapshot");
        eprintln!("snapshot saved to {}", path.display());
    }
}
