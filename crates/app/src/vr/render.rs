//! CPU rasterizer for the overlay's icon grid.
//!
//! Renders a `Vec<u8>` of RGBA pixels that the (future) OpenVR overlay
//! submits via `SetOverlayRaw`. Designed to be unit-testable today: the
//! main entry point takes the `ActiveItem` slice and an icon-bytes resolver,
//! so tests don't need a real `IconCache` or GPU.
//!
//! Layout: a grid of `GRID_COLS` columns × N rows; each cell is `CELL_PX`
//! square with `CELL_PADDING` between. Cells render the item icon, a small
//! progress text, and a "done" overlay (semi-transparent gray) when the
//! item's collected count reaches its needed count.

use crate::state::ActiveItem;
use tiny_skia::{Color, FillRule, IntSize, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

pub const CANVAS_PX: u32 = 1024;
pub const CELL_PX: u32 = 160;
pub const CELL_PADDING: u32 = 8;
pub const GRID_COLS: u32 = 6;
/// Max cells we render in one pass; overflow gets an indicator.
pub const MAX_CELLS: usize = 36;

pub struct CellHit {
    pub item_id: String,
    pub rect: Rect,
}

/// Render `items` to a fresh RGBA pixmap. `resolve_icon` is called with each
/// item's `icon_path` and should return the raw image bytes (PNG/WebP/etc.),
/// or `None` to render a placeholder square.
///
/// Returns the pixmap plus a hit-test table mapping each rendered cell's
/// rect back to its item id (for the VR input handler).
pub fn render<F>(items: &[ActiveItem], mut resolve_icon: F) -> (Pixmap, Vec<CellHit>)
where
    F: FnMut(&str) -> Option<std::borrow::Cow<'static, [u8]>>,
{
    let mut pixmap = Pixmap::new(CANVAS_PX, CANVAS_PX).expect("pixmap alloc");
    // Dark translucent background — readable over any VR scene.
    pixmap.fill(Color::from_rgba8(20, 20, 24, 220));

    let mut hits = Vec::new();

    let visible: Vec<&ActiveItem> = items.iter().take(MAX_CELLS).collect();
    if visible.is_empty() {
        draw_placeholder_text(&mut pixmap);
        return (pixmap, hits);
    }

    for (idx, item) in visible.iter().enumerate() {
        let col = idx as u32 % GRID_COLS;
        let row = idx as u32 / GRID_COLS;
        let x = CELL_PADDING + col * (CELL_PX + CELL_PADDING);
        let y = CELL_PADDING + row * (CELL_PX + CELL_PADDING);

        let Some(rect) = Rect::from_xywh(x as f32, y as f32, CELL_PX as f32, CELL_PX as f32) else {
            continue;
        };
        draw_cell(&mut pixmap, item, rect, &mut resolve_icon);
        hits.push(CellHit {
            item_id: item.item_id.clone(),
            rect,
        });
    }

    if items.len() > MAX_CELLS {
        draw_more_indicator(&mut pixmap, items.len() - MAX_CELLS);
    }

    (pixmap, hits)
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

    // Icon, centered with 12px padding from edges.
    let icon_pad = 12.0;
    let icon_rect = Rect::from_xywh(
        rect.x() + icon_pad,
        rect.y() + icon_pad,
        rect.width() - 2.0 * icon_pad,
        rect.height() - 2.0 * icon_pad - 18.0, // leave room for the bottom progress text
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

fn draw_progress_chip(pixmap: &mut Pixmap, cell: Rect, collected: u32, needed: u32) {
    // Bottom strip with simple filled bar; numbers are visual only here
    // (text rendering is intentionally minimal in the v1 CPU renderer —
    // Phase 3 can add fontdue glyphs when we're ready to ship to VR).
    let strip_h = 14.0;
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
    // Use the parameter to silence dead-code in case borrow-checker shifts.
    let _ = collected;
}

fn draw_more_indicator(pixmap: &mut Pixmap, extra: usize) {
    // A small filled rect bottom-right; full text rendering deferred to Phase 3.
    let badge =
        Rect::from_xywh((CANVAS_PX - 80) as f32, (CANVAS_PX - 30) as f32, 72.0, 22.0).unwrap();
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(160, 90, 70, 255));
    pixmap.fill_rect(badge, &paint, Transform::identity(), None);
    let _ = extra; // shown as a generic indicator until text rendering is added
}

fn draw_placeholder_text(pixmap: &mut Pixmap) {
    // Just paint a centered rect so VR users see "something" when nothing is
    // tracked. Real text comes in Phase 3 with fontdue.
    let r = Rect::from_xywh(
        (CANVAS_PX / 2 - 200) as f32,
        (CANVAS_PX / 2 - 30) as f32,
        400.0,
        60.0,
    )
    .unwrap();
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
    fn renders_empty_to_canvas_sized_pixmap() {
        let (pm, hits) = render(&[], |_| None);
        assert_eq!(pm.width(), CANVAS_PX);
        assert_eq!(pm.height(), CANVAS_PX);
        assert!(hits.is_empty());
    }

    #[test]
    fn renders_grid_with_correct_hit_rects() {
        let items: Vec<ActiveItem> = (0..7)
            .map(|i| item(&format!("i{i}"), &format!("Item {i}"), 5, i as u32))
            .collect();
        let (_pm, hits) = render(&items, |_| None);
        assert_eq!(hits.len(), 7);
        // First cell at (8, 8), 160x160.
        let h0 = &hits[0];
        assert_eq!(h0.item_id, "i0");
        assert!((h0.rect.x() - 8.0).abs() < 0.5);
        assert!((h0.rect.y() - 8.0).abs() < 0.5);
        // 7th cell wraps to row 2 (idx 6 → col 0, row 1).
        let h6 = &hits[6];
        assert_eq!(h6.item_id, "i6");
        assert!((h6.rect.x() - 8.0).abs() < 0.5);
        assert!((h6.rect.y() - (8.0 + CELL_PX as f32 + CELL_PADDING as f32)).abs() < 0.5);
    }

    #[test]
    fn caps_visible_at_max_cells() {
        let items: Vec<ActiveItem> = (0..50)
            .map(|i| item(&format!("i{i}"), &format!("Item {i}"), 1, 0))
            .collect();
        let (_pm, hits) = render(&items, |_| None);
        assert_eq!(hits.len(), MAX_CELLS);
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

        let (pm, hits) = render(&items, assets::read_icon);
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
        let (pm, _) = render(&items, |_| None);
        let path = std::env::temp_dir().join("ez-wishlist-overlay-snapshot.png");
        pm.save_png(&path).expect("save snapshot");
        eprintln!("snapshot saved to {}", path.display());
    }
}
