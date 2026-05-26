//! Static layout grid built from detected reference points.
//!
//! Strategy (per user feedback):
//!   1. Apply preprocessing + 2× upscale once so all UI text is legible.
//!   2. Run OCR on the resulting image.
//!   3. PRIMARY anchors: BACK and LEVEL UP buttons. They give us the panel's
//!      bottom edge, full width, and tilt angle.
//!   4. SECONDARY anchors found tolerantly in regions derived from the primary:
//!        - "Need to submit items" (anchor / cost line region)
//!        - "FROM RAID" labels (one per cell column, gives count + positions)
//!   5. Title / level chip / cost number / per-cell progress strips are then
//!      derived from those detected points.

use anyhow::{Context, Result};
use image::{imageops::FilterType, GenericImageView, Rgba, RgbaImage};

use crate::engine::{BBox, OcrEngine, OcrOptions, Psm, Word};
use crate::prep;

#[derive(Debug, Clone)]
pub struct Region {
    pub name: String,
    pub rect: BBox,
    pub color: Rgba<u8>,
}

#[derive(Debug, Clone)]
pub struct GridLayout {
    pub button_back: BBox,
    pub button_level_up: BBox,
    pub regions: Vec<Region>,
    pub tilt: f32,
    pub pivot: (i32, i32),
}

const RED: Rgba<u8> = Rgba([220, 50, 50, 255]);
const GREEN: Rgba<u8> = Rgba([50, 200, 80, 255]);
const BLUE: Rgba<u8> = Rgba([50, 100, 220, 255]);
const YELLOW: Rgba<u8> = Rgba([240, 200, 30, 255]);
const MAGENTA: Rgba<u8> = Rgba([220, 50, 200, 255]);
const CYAN: Rgba<u8> = Rgba([30, 200, 220, 255]);
const ORANGE: Rgba<u8> = Rgba([240, 130, 30, 255]);

pub fn build(src: &image::DynamicImage, engine: &dyn OcrEngine) -> Result<GridLayout> {
    let prepped = prep::process(src, prep::DEFAULT);
    let scale = 2u32;
    let (pw, ph) = prepped.dimensions();
    let big = prepped.resize(pw * scale, ph * scale, FilterType::Lanczos3);

    let words = engine.recognize(
        &big,
        &OcrOptions {
            psm: Some(Psm::Sparse),
            whitelist: None,
        },
    )?;
    if std::env::var_os("OCR_LAB_GRID_DUMP").is_some() {
        for w in &words {
            tracing::info!(
                "grid-pass  {:5}x{:<5} +{:3}x{:3} conf={:>5.1}  {:?}",
                w.bbox.x, w.bbox.y, w.bbox.w, w.bbox.h, w.confidence, w.text
            );
        }
    }

    // All bboxes from OCR are in upscaled-image coords; convert to source coords.
    let to_src = |b: &BBox| BBox {
        x: b.x / scale,
        y: b.y / scale,
        w: b.w / scale,
        h: b.h / scale,
    };

    // --- Step 1: PRIMARY anchors — BACK + LEVEL UP.
    let back_word = words
        .iter()
        .find(|w| {
            let up = w.text.to_ascii_uppercase();
            matches!(up.as_str(), "BACK" | "BACK." | "PACK" | "BAGK") && w.confidence > 25.0
        })
        .context("BACK button not found in preprocessed+upscaled image")?;
    let level_word = words.iter().find(|w| {
        let up = w.text.to_ascii_uppercase();
        matches!(up.as_str(), "LEVEL" | "LEUEL" | "1EVEL") && w.confidence > 25.0
    });
    let up_word = words.iter().find(|w| {
        let up = w.text.to_ascii_uppercase();
        matches!(up.as_str(), "UP" | "UP." | "LP") && w.confidence > 25.0
    });

    let back = to_src(&back_word.bbox);
    let level_up_text = match (level_word, up_word) {
        (Some(l), Some(u)) => {
            let bx = BBox {
                x: l.bbox.x.min(u.bbox.x),
                y: l.bbox.y.min(u.bbox.y),
                w: (u.bbox.x + u.bbox.w).saturating_sub(l.bbox.x.min(u.bbox.x)),
                h: l.bbox.h.max(u.bbox.h),
            };
            Some(to_src(&bx))
        }
        (Some(l), None) => Some(to_src(&l.bbox)),
        (None, Some(u)) => Some(to_src(&u.bbox)),
        _ => None,
    };
    // Compute LEVEL UP bounding box. If detected, use it; else mirror BACK across
    // the panel's horizontal center.
    let level_up = level_up_text
        .map(|lu| {
            // Extend horizontally to include both LEVEL and UP if either is missing.
            let right_guess = lu.x + lu.w.max(back.w);
            BBox {
                x: lu.x,
                y: lu.y,
                w: right_guess.saturating_sub(lu.x),
                h: lu.h.max(back.h),
            }
        })
        .unwrap_or(BBox {
            x: back.x + back.w * 7 / 4,
            y: back.y,
            w: back.w,
            h: back.h,
        });

    // --- Geometry: BACK and LEVEL UP are side-by-side at panel bottom.
    let row_top = back.y.min(level_up.y);
    let row_bottom = (back.y + back.h).max(level_up.y + level_up.h);
    let row_h = (row_bottom - row_top).max(20);
    let panel_left = back.x.saturating_sub(row_h / 2);
    let panel_right = level_up.x + level_up.w + row_h / 2;
    let panel_width = panel_right.saturating_sub(panel_left);

    // Tilt: derived from the BACK→LEVEL UP slope (most stable horizontal pair).
    let back_cx = back.x as f32 + back.w as f32 / 2.0;
    let back_cy = back.y as f32 + back.h as f32 / 2.0;
    let lu_cx = level_up.x as f32 + level_up.w as f32 / 2.0;
    let lu_cy = level_up.y as f32 + level_up.h as f32 / 2.0;
    let tilt = (lu_cy - back_cy).atan2(lu_cx - back_cx);

    // --- Step 2: Find "Need to submit items" anchor (tolerantly, look in upper
    // portion of panel — anywhere above the button row).
    let anchor_tokens: Vec<&Word> = words
        .iter()
        .filter(|w| {
            matches!(
                w.text.to_ascii_lowercase().as_str(),
                "need" | "to" | "submit" | "items"
            ) && w.confidence > 40.0
        })
        .collect();
    let (anchor_x_mid, anchor_y, anchor_height) = if anchor_tokens.len() >= 2 {
        let xs: Vec<u32> = anchor_tokens.iter().map(|w| w.bbox.x).collect();
        let xs_r: Vec<u32> = anchor_tokens.iter().map(|w| w.bbox.x + w.bbox.w).collect();
        let ys: Vec<u32> = anchor_tokens.iter().map(|w| w.bbox.y + w.bbox.h / 2).collect();
        let xa = *xs.iter().min().unwrap();
        let xb = *xs_r.iter().max().unwrap();
        let y_mid = ys.iter().sum::<u32>() / ys.len() as u32;
        let median_h = anchor_tokens
            .iter()
            .map(|w| w.bbox.h)
            .max()
            .unwrap_or(30);
        ((xa + xb) / 2 / scale, y_mid / scale, median_h / scale)
    } else {
        // Fallback: estimate based on BACK row.
        let row_h_s = row_h;
        ((panel_left + panel_width / 2), back.y.saturating_sub(row_h_s * 5), row_h_s / 2)
    };

    // --- Step 3: Find FROM RAID labels in the area between the anchor and the
    // button row (cells live there). Tolerant: include lone FROM or lone RAID
    // tokens; cluster by X to count cells.
    let fr_tokens: Vec<&Word> = words
        .iter()
        .filter(|w| {
            let up = w.text.to_ascii_uppercase();
            matches!(
                up.as_str(),
                "FROM" | "FROM." | "FROW" | "FRDM" | "RAID" | "RAID." | "RAIO"
            ) && w.confidence > 25.0
        })
        .collect();
    let from_raid_anchors = cluster_from_raids(&fr_tokens, scale);

    // Cell row Y comes from FROM RAID y if available, else interpolated between
    // anchor_y and back.y.
    let (cell_row_mid_y, cell_centers) = if !from_raid_anchors.is_empty() {
        let mid_y =
            from_raid_anchors.iter().map(|a| a.y).sum::<u32>() / from_raid_anchors.len() as u32;
        let centres: Vec<u32> = from_raid_anchors.iter().map(|a| a.x).collect();
        (mid_y, centres)
    } else {
        // No FROM RAID found — assume 4 evenly-spaced cells across the panel.
        let pitch = panel_width / 4;
        let centres: Vec<u32> = (0..4).map(|i| panel_left + pitch * i + pitch / 2).collect();
        let mid_y = (anchor_y + back.y) / 2;
        (mid_y, centres)
    };
    let n_cells = cell_centers.len() as u32;
    // Pitch from detected centers when ≥2 found, otherwise from panel width.
    let cell_pitch = if cell_centers.len() >= 2 {
        let mut gaps: Vec<u32> = cell_centers.windows(2).map(|w| w[1] - w[0]).collect();
        gaps.sort_unstable();
        gaps[gaps.len() / 2]
    } else {
        panel_width / n_cells.max(1)
    };
    // Inferred cell rectangle dimensions.
    let cell_w = (cell_pitch * 9) / 10;
    let cell_h = row_h * 4;
    let cell_bottom = cell_row_mid_y + row_h / 4;
    let cell_top = cell_bottom.saturating_sub(cell_h);

    // Cost is one short row above the cells, below the anchor.
    let cost_h = (anchor_height * 3 + row_h) / 4;
    let cost_top = (anchor_y + anchor_height) + cost_h / 4;
    let cost_bottom = cell_top.saturating_sub(cost_h / 4);

    // Title chip sits near the top of the panel — variable distance above the
    // anchor (depends on # of upgrade rows). Use a generous band.
    let title_bottom = anchor_y.saturating_sub(row_h);
    let title_top = title_bottom.saturating_sub(row_h * 8);

    let mut regions = Vec::new();
    let cell_margin = cell_pitch / 18;

    regions.push(Region {
        name: "title".into(),
        rect: BBox {
            x: panel_left + cell_margin,
            y: title_top,
            w: (panel_width * 5) / 12,
            h: row_h * 2,
        },
        color: RED,
    });
    regions.push(Region {
        name: "level".into(),
        rect: BBox {
            x: panel_left + cell_margin,
            y: title_top + row_h * 2,
            w: panel_width / 6,
            h: row_h,
        },
        color: ORANGE,
    });
    regions.push(Region {
        name: "need_anchor".into(),
        rect: BBox {
            x: panel_left + panel_width / 4,
            y: anchor_y.saturating_sub(anchor_height / 2),
            w: panel_width / 2,
            h: anchor_height,
        },
        color: CYAN,
    });
    regions.push(Region {
        name: "cost".into(),
        rect: BBox {
            x: panel_left + panel_width / 3,
            y: cost_top,
            w: panel_width / 3,
            h: cost_bottom.saturating_sub(cost_top).max(20),
        },
        color: BLUE,
    });

    for i in 0..n_cells {
        let centre = cell_centers[i as usize];
        let cx = centre.saturating_sub(cell_w / 2);
        regions.push(Region {
            name: format!("cell{i}"),
            rect: BBox {
                x: cx,
                y: cell_top,
                w: cell_w,
                h: cell_bottom.saturating_sub(cell_top),
            },
            color: GREEN,
        });
        // Progress strip: narrow band just above FROM RAID.
        let prog_h = row_h * 3 / 4;
        let prog_top = cell_row_mid_y.saturating_sub(prog_h + row_h / 6);
        regions.push(Region {
            name: format!("cell{i}_progress"),
            rect: BBox {
                x: cx + cell_w / 8,
                y: prog_top,
                w: (cell_w * 3) / 4,
                h: prog_h,
            },
            color: YELLOW,
        });
    }
    regions.push(Region {
        name: "BACK".into(),
        rect: back,
        color: MAGENTA,
    });
    regions.push(Region {
        name: "LEVEL UP".into(),
        rect: level_up,
        color: MAGENTA,
    });

    Ok(GridLayout {
        button_back: back,
        button_level_up: level_up,
        regions,
        tilt,
        pivot: (
            ((back.x + back.w + level_up.x) / 2) as i32,
            ((back.y + back.h / 2 + level_up.y + level_up.h / 2) / 2) as i32,
        ),
    })
}

fn cluster_from_raids(words: &[&Word], scale: u32) -> Vec<BBox> {
    if words.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&Word> = words.to_vec();
    // Filter to dominant Y row (median ± its height).
    let median_y = {
        let mut ys: Vec<u32> = sorted.iter().map(|w| w.bbox.y).collect();
        ys.sort_unstable();
        ys[ys.len() / 2]
    };
    let median_h = {
        let mut hs: Vec<u32> = sorted.iter().map(|w| w.bbox.h).collect();
        hs.sort_unstable();
        hs[hs.len() / 2].max(20)
    };
    sorted.retain(|w| (w.bbox.y as i32 - median_y as i32).abs() < (median_h as i32));
    sorted.sort_by_key(|w| w.bbox.x);

    let cluster_gap = median_h * 3;
    let mut clusters: Vec<Vec<&Word>> = Vec::new();
    for w in sorted {
        let push_new = match clusters.last() {
            None => true,
            Some(c) => {
                let last_right = c.iter().map(|x| x.bbox.x + x.bbox.w).max().unwrap();
                (w.bbox.x as i32) - (last_right as i32) > cluster_gap as i32
            }
        };
        if push_new {
            clusters.push(vec![w]);
        } else {
            clusters.last_mut().unwrap().push(w);
        }
    }
    let mut anchors: Vec<BBox> = clusters
        .iter()
        .map(|c| {
            let left = c.iter().map(|w| w.bbox.x).min().unwrap();
            let right = c.iter().map(|w| w.bbox.x + w.bbox.w).max().unwrap();
            let top = c.iter().map(|w| w.bbox.y).min().unwrap();
            let bottom = c.iter().map(|w| w.bbox.y + w.bbox.h).max().unwrap();
            BBox {
                x: (left + right) / 2 / scale,
                y: (top + bottom) / 2 / scale,
                w: (right - left) / scale,
                h: (bottom - top) / scale,
            }
        })
        .collect();
    anchors.sort_by_key(|b| b.x);
    anchors
}

/// Draw the layout as tilted quadrilaterals on a clone of the source image.
pub fn render_overlay(img: &image::DynamicImage, layout: &GridLayout) -> RgbaImage {
    let mut out = img.to_rgba8();
    let sin = layout.tilt.sin();
    let cos = layout.tilt.cos();
    let (px, py) = layout.pivot;
    let rot = |x: i32, y: i32| -> (i32, i32) {
        let dx = (x - px) as f32;
        let dy = (y - py) as f32;
        let rx = dx * cos - dy * sin;
        let ry = dx * sin + dy * cos;
        ((rx + px as f32) as i32, (ry + py as f32) as i32)
    };
    for region in &layout.regions {
        let r = &region.rect;
        let x0 = r.x as i32;
        let y0 = r.y as i32;
        let x1 = (r.x + r.w) as i32;
        let y1 = (r.y + r.h) as i32;
        let tl = rot(x0, y0);
        let tr = rot(x1, y0);
        let br = rot(x1, y1);
        let bl = rot(x0, y1);
        for thickness in 0..3 {
            draw_line(&mut out, tl, tr, region.color, thickness);
            draw_line(&mut out, tr, br, region.color, thickness);
            draw_line(&mut out, br, bl, region.color, thickness);
            draw_line(&mut out, bl, tl, region.color, thickness);
        }
    }
    out
}

fn draw_line(img: &mut RgbaImage, a: (i32, i32), b: (i32, i32), color: Rgba<u8>, offset: i32) {
    let (mut x0, mut y0) = a;
    let (x1, y1) = b;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (iw, ih) = img.dimensions();
    loop {
        let perp = (-(y1 - a.1), x1 - a.0);
        let plen = ((perp.0 * perp.0 + perp.1 * perp.1) as f32).sqrt().max(1.0);
        let ox = (perp.0 as f32 * offset as f32 / plen) as i32;
        let oy = (perp.1 as f32 * offset as f32 / plen) as i32;
        let px = (x0 + ox).max(0);
        let py = (y0 + oy).max(0);
        if (px as u32) < iw && (py as u32) < ih {
            img.put_pixel(px as u32, py as u32, color);
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}
