//! Image → Panel pipeline.
//!
//! Two-stage anchoring (FROM RAID is too small at native resolution to detect):
//!
//! Pass 1: full-image sparse OCR → find big text that gives us a panel crop.
//!   Anchor: "Need to submit items" row (high-conf in most images), or fallback to
//!   the largest Y-cluster of high-confidence words.
//!
//! Pass 2: OCR the cropped + 2x upscaled panel. At this scale Tesseract sees:
//!   - the LV0/LV1 chip next to the title (top of panel)
//!   - the cost row (digit run after coin glyph)
//!   - the 3 or 4 "FROM RAID" labels at the bottom of each item cell
//!   - the X/Y progress digits directly above each FROM RAID
//!   - the item names above each progress strip
//!
//! Use pass-2 anchors to precisely locate cells and parse the panel.

use std::path::Path;

use anyhow::{Context, Result};
use image::{imageops::FilterType, DynamicImage, GenericImageView, GrayImage, Luma};

use crate::engine::{BBox, OcrEngine, OcrOptions, Psm, Word};
use crate::prep;
use crate::templates::{self, Template};
use crate::{Item, Panel};

const PANEL_UPSCALE: u32 = 2;

/// Preprocessing params used by both Tesseract passes and the template matcher.
const PREP: prep::PrepParams = prep::DEFAULT;

pub fn run(image_path: &Path, engine: &dyn OcrEngine) -> Result<Panel> {
    let img = image::open(image_path)
        .with_context(|| format!("loading {}", image_path.display()))?;

    // Preprocess once: keep only white/green text pixels (UI text) and invert
    // to black-on-white. Tesseract reads this dramatically better than the raw
    // VR screenshot, and template matching needs the same binarization.
    let prepped = prep::process(&img, PREP);

    let pass1_words = engine.recognize(
        &prepped,
        &OcrOptions {
            psm: Some(Psm::Sparse),
            whitelist: None,
        },
    )?;
    dump_words("pass1", &pass1_words, "OCR_LAB_DUMP_PASS1");

    let panel_rect = approximate_panel_rect(&prepped, &pass1_words)
        .context("pass1 could not locate the panel (no 'Need to submit items' anchor and no dense text cluster)")?;

    let panel_img = crop_and_upscale(&prepped, &panel_rect);
    dump_panel(image_path, &panel_img);

    let pass2_words = engine.recognize(
        &panel_img,
        &OcrOptions {
            psm: Some(Psm::Block),
            whitelist: None,
        },
    )?;
    dump_words("pass2", &pass2_words, "OCR_LAB_DUMP_PASS2");

    // Load templates (digits 0-9 + slash) for the per-cell progress reader.
    let tdir = image_path
        .parent()
        .map(|p| p.join("templates"))
        .unwrap_or_else(|| std::path::PathBuf::from("templates"));
    let tmpls = templates::load_templates(&tdir).unwrap_or_default();

    parse_panel(&panel_img, &pass2_words, engine, &tmpls)
}

/// Pass-1 panel localization. Two strategies, primary then fallback.
fn approximate_panel_rect(img: &DynamicImage, words: &[Word]) -> Option<BBox> {
    let (img_w, img_h) = img.dimensions();
    if let Some(b) = anchor_by_submit_phrase(img_w, img_h, words) {
        return Some(b);
    }
    anchor_by_text_cluster(img_w, img_h, words)
}

fn anchor_by_submit_phrase(img_w: u32, img_h: u32, words: &[Word]) -> Option<BBox> {
    let row: Vec<&Word> = words
        .iter()
        .filter(|w| {
            matches!(
                w.text.to_ascii_lowercase().as_str(),
                "need" | "to" | "submit" | "items"
            )
        })
        .collect();
    if row.len() < 2 {
        return None;
    }
    let median_y = median_of(row.iter().map(|w| w.bbox.y));
    let median_h = median_of(row.iter().map(|w| w.bbox.h)).max(10);
    let on_row: Vec<&Word> = row
        .into_iter()
        .filter(|w| (w.bbox.y as i32 - median_y as i32).abs() < median_h as i32)
        .collect();
    if on_row.len() < 2 {
        return None;
    }
    let left = on_row.iter().map(|w| w.bbox.x).min().unwrap();
    let right = on_row.iter().map(|w| w.bbox.x + w.bbox.w).max().unwrap();
    let top = on_row.iter().map(|w| w.bbox.y).min().unwrap();
    let bottom = on_row.iter().map(|w| w.bbox.y + w.bbox.h).max().unwrap();
    let row_h = (bottom - top).max(median_h);

    // The "Need to submit items" row sits roughly mid-panel horizontally and below
    // mid-panel vertically. Pad asymmetrically. pad_above ≥ 20x row_h to capture
    // the title chip + all upgrade rows; pad_below ≥ 9x row_h to capture the cost,
    // item cells, and FROM RAID labels at the bottom.
    let pad_x = (row_h as i32) * 18;
    let pad_above = (row_h as i32) * 20;
    let pad_below = (row_h as i32) * 14;

    Some(clamp_bbox(
        img_w,
        img_h,
        left as i32 - pad_x,
        top as i32 - pad_above,
        right as i32 + pad_x,
        bottom as i32 + pad_below,
    ))
}

fn anchor_by_text_cluster(img_w: u32, img_h: u32, words: &[Word]) -> Option<BBox> {
    let mut keepers: Vec<&Word> = words
        .iter()
        .filter(|w| w.confidence > 70.0 && w.bbox.w >= 20 && w.bbox.h >= 10)
        .collect();
    if keepers.is_empty() {
        return None;
    }
    keepers.sort_by_key(|w| w.bbox.y);
    let median_h = median_of(keepers.iter().map(|w| w.bbox.h)).max(10);

    let gap_thresh = (median_h * 5) as i32;
    let mut clusters: Vec<Vec<&Word>> = Vec::new();
    for w in keepers {
        let push_new = match clusters.last() {
            None => true,
            Some(c) => {
                let last_bottom = c.iter().map(|x| x.bbox.y + x.bbox.h).max().unwrap() as i32;
                (w.bbox.y as i32) - last_bottom > gap_thresh
            }
        };
        if push_new {
            clusters.push(vec![w]);
        } else {
            clusters.last_mut().unwrap().push(w);
        }
    }
    let panel = clusters.into_iter().max_by_key(|c| c.len())?;
    let left = panel.iter().map(|w| w.bbox.x).min().unwrap();
    let right = panel.iter().map(|w| w.bbox.x + w.bbox.w).max().unwrap();
    let top = panel.iter().map(|w| w.bbox.y).min().unwrap();
    let bottom = panel.iter().map(|w| w.bbox.y + w.bbox.h).max().unwrap();

    let pad_x = (median_h * 8) as i32;
    let pad_y = (median_h * 6) as i32;
    Some(clamp_bbox(
        img_w,
        img_h,
        left as i32 - pad_x,
        top as i32 - pad_y,
        right as i32 + pad_x,
        bottom as i32 + pad_y,
    ))
}

fn clamp_bbox(img_w: u32, img_h: u32, x0: i32, y0: i32, x1: i32, y1: i32) -> BBox {
    let x0 = x0.max(0) as u32;
    let y0 = y0.max(0) as u32;
    let x1 = (x1.max(0) as u32).min(img_w);
    let y1 = (y1.max(0) as u32).min(img_h);
    BBox {
        x: x0,
        y: y0,
        w: x1.saturating_sub(x0),
        h: y1.saturating_sub(y0),
    }
}

fn median_of<I: Iterator<Item = u32>>(it: I) -> u32 {
    let mut v: Vec<u32> = it.collect();
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

fn crop_and_upscale(img: &DynamicImage, rect: &BBox) -> DynamicImage {
    let cropped = img.crop_imm(rect.x, rect.y, rect.w, rect.h);
    cropped.resize(
        rect.w * PANEL_UPSCALE,
        rect.h * PANEL_UPSCALE,
        FilterType::Lanczos3,
    )
}

fn dump_words(tag: &str, words: &[Word], env: &str) {
    if std::env::var_os(env).is_none() {
        return;
    }
    for w in words {
        tracing::info!(
            "{tag}  {:4}x{:4} +{:3}x{:3} conf={:>5.1}  {:?}",
            w.bbox.x,
            w.bbox.y,
            w.bbox.w,
            w.bbox.h,
            w.confidence,
            w.text
        );
    }
}

fn dump_panel(image_path: &Path, panel_img: &DynamicImage) {
    let Some(dump_dir) = std::env::var_os("OCR_LAB_DUMP_CROPS") else {
        return;
    };
    let stem = image_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let dir = std::path::PathBuf::from(dump_dir);
    let _ = panel_img.save(dir.join(format!("{stem}.panel.png")));
}

/// Parse a panel from pass-2 OCR words (in panel-image coordinates).
fn parse_panel(
    panel_img: &DynamicImage,
    words: &[Word],
    engine: &dyn OcrEngine,
    tmpls: &[Template],
) -> Result<Panel> {
    let (pw, ph) = panel_img.dimensions();

    let cells = locate_cells_via_from_raid(words, pw, ph);

    // Level chip: LV0/LV1 token, upper half of panel.
    let level = words
        .iter()
        .filter(|w| w.bbox.y < ph / 2)
        .find(|w| is_level_token(&w.text))
        .map(|w| normalize_level(&w.text))
        .unwrap_or_default();

    // Title: largest words near the level chip — skip header bleed at top.
    let title = pick_title(words, ph);

    // Cost: digit-only re-OCR of the strip just above the cells.
    let cell_top = cells.iter().map(|c| c.y).min().unwrap_or(ph);
    let cost = pick_cost(panel_img, words, cell_top, engine);

    let items: Vec<Item> = cells
        .iter()
        .map(|c| cell_to_item(panel_img, words, c, engine, tmpls))
        .collect();

    Ok(Panel {
        title,
        level,
        description: None,
        description_next: None,
        cost,
        items,
    })
}

fn locate_cells_via_from_raid(words: &[Word], pw: u32, ph: u32) -> Vec<BBox> {
    // Tesseract may read "FROM" cleanly or as "FROM."/"FROW"; RAID similarly.
    let froms: Vec<&Word> = words
        .iter()
        .filter(|w| looks_like_from(&w.text))
        .collect();
    let raids: Vec<&Word> = words
        .iter()
        .filter(|w| looks_like_raid(&w.text))
        .collect();

    let mut pairs: Vec<(&Word, &Word)> = Vec::new();
    for f in &froms {
        let mid_y = f.bbox.y as i32 + f.bbox.h as i32 / 2;
        let f_right = (f.bbox.x + f.bbox.w) as i32;
        let cand = raids
            .iter()
            .filter(|r| {
                let r_mid_y = r.bbox.y as i32 + r.bbox.h as i32 / 2;
                (r_mid_y - mid_y).abs() < f.bbox.h as i32
            })
            .filter(|r| {
                let dx = (r.bbox.x as i32) - f_right;
                dx > -((f.bbox.w as i32) / 2) && dx < (f.bbox.w as i32) * 3
            })
            .min_by_key(|r| (r.bbox.x as i32 - f_right).abs());
        if let Some(r) = cand {
            pairs.push((f, r));
        }
    }
    if pairs.is_empty() {
        // Fallback: equal-width split of the bottom half of the panel into
        // 4 columns. Better than nothing for badly-cropped images.
        tracing::info!("locate_cells: no FROM/RAID pairs → fallback grid");
        return fallback_cells(pw, ph);
    }
    tracing::info!("locate_cells: found {} FROM/RAID pairs", pairs.len());
    pairs.sort_by_key(|(f, _)| f.bbox.x);
    pairs.dedup_by_key(|(f, _)| f.bbox.x);

    let cell_top = pairs
        .iter()
        .map(|(f, _)| f.bbox.y)
        .min()
        .unwrap();
    let cell_h = pairs
        .iter()
        .map(|(f, r)| (f.bbox.y + f.bbox.h).max(r.bbox.y + r.bbox.h) - f.bbox.y)
        .max()
        .unwrap();
    // Each cell extends UP from the FROM RAID strip by ~5x text-height to capture
    // icon + name + X/Y progress row. Tight enough to exclude "Need to submit items".
    let cell_full_top = cell_top.saturating_sub(cell_h * 5);
    let cell_full_bottom = cell_top + cell_h;

    let mut centers: Vec<u32> = pairs
        .iter()
        .map(|(f, r)| (f.bbox.x + (r.bbox.x + r.bbox.w)) / 2)
        .collect();
    centers.sort_unstable();
    let pitch = if centers.len() >= 2 {
        let gaps: Vec<u32> = centers.windows(2).map(|w| w[1] - w[0]).collect();
        median_of(gaps.into_iter())
    } else {
        (cell_h * 7).max(40)
    };

    // Infer missing cells. We expect 3 or 4 cells total. If we found N < 4, check
    // if extending one cell-pitch beyond the leftmost or rightmost center stays
    // within the panel image — if so, add it.
    if centers.len() < 4 && pitch > 0 {
        let leftmost = *centers.first().unwrap();
        let rightmost = *centers.last().unwrap();
        let inferred_right = rightmost + pitch;
        let inferred_left = leftmost.saturating_sub(pitch);
        // Add inferred only if the extra cell fits comfortably (≥ half-pitch margin).
        if inferred_right + pitch / 2 < pw {
            centers.push(inferred_right);
        }
        if leftmost > pitch && centers.len() < 4 {
            centers.insert(0, inferred_left);
        }
        centers.sort_unstable();
    }

    let mut cells = Vec::new();
    for (i, &c) in centers.iter().enumerate() {
        let left = if i == 0 {
            c.saturating_sub(pitch / 2)
        } else {
            (centers[i - 1] + c) / 2
        };
        let right = if i + 1 < centers.len() {
            (c + centers[i + 1]) / 2
        } else {
            c + pitch / 2
        };
        cells.push(clamp_bbox(
            pw,
            ph,
            left as i32,
            cell_full_top as i32,
            right as i32,
            cell_full_bottom as i32,
        ));
    }
    cells
}

fn fallback_cells(pw: u32, ph: u32) -> Vec<BBox> {
    let cell_top = ph / 2;
    let cell_bot = ph * 9 / 10;
    let n = 4;
    (0..n)
        .map(|i| {
            let l = pw * i as u32 / n as u32;
            let r = pw * (i + 1) as u32 / n as u32;
            clamp_bbox(pw, ph, l as i32, cell_top as i32, r as i32, cell_bot as i32)
        })
        .collect()
}

fn looks_like_from(t: &str) -> bool {
    let up = t.to_ascii_uppercase();
    up == "FROM" || up == "FROM." || up == "FRDM" || up == "FROW"
}

fn looks_like_raid(t: &str) -> bool {
    let up = t.to_ascii_uppercase();
    up == "RAID" || up == "RAID." || up == "RAIO" || up == "RAID:"
}

fn pick_title(words: &[Word], panel_h: u32) -> String {
    let top = words
        .iter()
        .filter(|w| w.bbox.y < panel_h / 4)
        .filter(|w| !is_level_token(&w.text))
        .filter(|w| {
            // Drop pure punctuation / single chars Tesseract emits.
            w.text.chars().filter(|c| c.is_alphanumeric()).count() >= 2
        })
        .collect::<Vec<_>>();
    if top.is_empty() {
        return String::new();
    }
    let mut heights: Vec<u32> = top.iter().map(|w| w.bbox.h).collect();
    heights.sort_unstable();
    let cutoff = heights[heights.len() * 6 / 10];
    let mut tall: Vec<&Word> = top.iter().copied().filter(|w| w.bbox.h >= cutoff).collect();
    tall.sort_by_key(|w| (w.bbox.y, w.bbox.x));
    let Some(seed) = tall.first().copied() else {
        return String::new();
    };
    let band_y = seed.bbox.y as i32;
    let band_h = seed.bbox.h as i32;
    let mut band: Vec<&Word> = top
        .iter()
        .copied()
        .filter(|w| (w.bbox.y as i32 - band_y).abs() < band_h)
        .collect();
    band.sort_by_key(|w| w.bbox.x);
    band.iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn pick_cost(
    panel_img: &DynamicImage,
    words: &[Word],
    cells_top: u32,
    engine: &dyn OcrEngine,
) -> Option<u64> {
    // First try: find a 4-6 digit token in the strip just above the cells.
    // Apply digit-mistake normalization (Tesseract reads stylized 8→s, 0→o, etc).
    let candidate = words
        .iter()
        .filter(|w| w.bbox.y < cells_top && w.bbox.y > cells_top.saturating_sub(cells_top))
        .filter_map(|w| {
            digitize(&w.text).and_then(|d| {
                if (4..=7).contains(&d.len()) {
                    Some((w, d))
                } else {
                    None
                }
            })
        })
        .max_by_key(|(w, _)| w.bbox.y);

    if let Some((_, d)) = candidate {
        if let Ok(n) = d.parse::<u64>() {
            return Some(n);
        }
    }

    // Second try: targeted digit-only OCR of the cost strip (mid-panel, above cells).
    let strip_top = cells_top.saturating_sub(cells_top / 6);
    let strip_h = cells_top - strip_top;
    let (pw, _ph) = panel_img.dimensions();
    let strip = panel_img.crop_imm(
        pw / 4,
        strip_top.saturating_sub(strip_h / 2),
        pw / 2,
        strip_h,
    );
    let prepped = binarize_for_digits(&strip);
    let ws = engine
        .recognize(
            &prepped,
            &OcrOptions {
                psm: Some(Psm::Line),
                whitelist: Some("0123456789"),
            },
        )
        .ok()?;
    ws.iter()
        .filter_map(|w| w.text.parse::<u64>().ok())
        .filter(|n| *n >= 1000 && *n <= 999_999)
        .max()
}

/// Map common Tesseract misreads back to digits. Used for cost & progress tokens.
fn digitize(s: &str) -> Option<String> {
    let mapped: String = s
        .chars()
        .filter_map(|c| match c {
            '0'..='9' => Some(c),
            'o' | 'O' | 'D' => Some('0'),
            'l' | 'I' | 'i' | '|' => Some('1'),
            's' | 'S' | 'B' | 'e' | 'E' => Some('8'),
            'g' | 'q' | 'Q' => Some('9'),
            'z' | 'Z' => Some('2'),
            'A' => Some('4'),
            'b' => Some('6'),
            _ => None,
        })
        .collect();
    if mapped.is_empty() {
        None
    } else {
        Some(mapped)
    }
}

fn is_level_token(t: &str) -> bool {
    let upper = t
        .to_ascii_uppercase()
        .replace('O', "0")
        .replace('I', "1");
    upper.starts_with("LV") && upper.len() >= 3 && upper[2..].chars().all(|c| c.is_ascii_digit())
}

fn normalize_level(t: &str) -> String {
    t.to_ascii_uppercase().replace('O', "0").replace('I', "1")
}

fn cell_to_item(
    panel_img: &DynamicImage,
    words: &[Word],
    cell: &BBox,
    _engine: &dyn OcrEngine,
    tmpls: &[Template],
) -> Item {
    let in_cell: Vec<&Word> = words
        .iter()
        .filter(|w| {
            let cx = w.bbox.x + w.bbox.w / 2;
            let cy = w.bbox.y + w.bbox.h / 2;
            cx >= cell.x
                && cx < cell.x + cell.w
                && cy >= cell.y
                && cy < cell.y + cell.h
        })
        .filter(|w| !is_chrome(&w.text))
        .collect();

    // Template-match the X/Y digit strip — Tesseract can't read the stylized
    // chunky-pixel digit font but stored per-digit templates match it exactly.
    let (collected, needed) = read_progress_via_templates(panel_img, cell, tmpls)
        .unwrap_or((0, 0));

    // Name: words in the upper half of the cell, excluding any digit-only tokens.
    let mut name_words: Vec<&&Word> = in_cell
        .iter()
        .filter(|w| w.bbox.y < cell.y + cell.h * 2 / 3)
        .filter(|w| {
            w.text.chars().filter(|c| c.is_alphabetic()).count() >= 2
        })
        .collect();
    name_words.sort_by_key(|w| (w.bbox.y, w.bbox.x));
    let name = name_words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");

    Item {
        name,
        collected,
        needed,
    }
}

/// Crop the X/Y digit strip from a cell and recognize it via template matching.
/// The panel image is already binarized (from prep::process at the top of the
/// pipeline), so we just need to slice the strip, find connected components,
/// and match each one against the stored digit templates.
fn read_progress_via_templates(
    panel_img: &DynamicImage,
    cell: &BBox,
    tmpls: &[Template],
) -> Option<(u32, u32)> {
    if tmpls.is_empty() {
        return None;
    }
    // The cell rect spans icon + name + X/Y; the X/Y strip is in roughly the
    // bottom third (above the FROM RAID label which is just below the cell box).
    let h = cell.h;
    let strip_top = cell.y + h * 55 / 100;
    let strip_bottom = cell.y + h * 88 / 100;
    if strip_bottom <= strip_top {
        return None;
    }
    let strip = panel_img.crop_imm(cell.x, strip_top, cell.w, strip_bottom - strip_top);
    let gray = strip.to_luma8();
    let recognised = templates::recognize(&gray, tmpls);
    split_progress(&recognised)
}

/// Legacy Tesseract-based progress reader — kept for the digit-strip dump path.
#[allow(dead_code)]
fn read_progress(
    panel_img: &DynamicImage,
    cell: &BBox,
    engine: &dyn OcrEngine,
) -> Option<(u32, u32)> {
    let h = cell.h;
    let strip_top = cell.y + h * 60 / 100;
    let strip_bottom = cell.y + h * 88 / 100;
    if strip_bottom <= strip_top {
        return None;
    }
    let strip_h = strip_bottom - strip_top;
    let strip = panel_img.crop_imm(cell.x, strip_top, cell.w, strip_h);
    let prepped = binarize_for_digits(&strip);
    if let Some(dir) = std::env::var_os("OCR_LAB_DUMP_DIGITS") {
        let dir = std::path::PathBuf::from(dir);
        let name = format!(
            "cell_{}_{}.png",
            cell.x,
            cell.y
        );
        let _ = prepped.save(dir.join(name));
    }
    let ws = engine
        .recognize(
            &prepped,
            &OcrOptions {
                psm: Some(Psm::Line),
                whitelist: Some("0123456789/"),
            },
        )
        .ok()?;
    if ws.is_empty() {
        return None;
    }
    let mut combined = String::new();
    for w in &ws {
        combined.push_str(&w.text);
    }
    if std::env::var_os("OCR_LAB_DUMP_DIGITS").is_some() {
        tracing::info!("read_progress cell({},{}) -> {:?}", cell.x, cell.y, combined);
    }
    split_progress(&combined)
}

/// Preprocessing pipeline for small stylized game-UI digits:
/// 1. Upscale 3× with Lanczos — gives Tesseract more pixels to work with.
/// 2. Convert to grayscale.
/// 3. Threshold at the image's 65th-percentile luminance → pure black/white.
/// 4. Invert (text was light-on-dark; Tesseract wants dark-on-light).
/// 5. Pad with 12 px of white border so text isn't flush against the edge.
fn binarize_for_digits(strip: &DynamicImage) -> DynamicImage {
    let upscaled = strip.resize(
        strip.width() * 3,
        strip.height() * 3,
        FilterType::Lanczos3,
    );
    let gray = upscaled.to_luma8();

    // Threshold: pick the 65th-percentile luminance as the cutoff. The text
    // pixels are the brightest, so anything above is "text" → black after inversion;
    // anything below is background → white.
    let mut samples: Vec<u8> = gray.pixels().map(|p| p.0[0]).collect();
    samples.sort_unstable();
    let cutoff = samples[samples.len() * 65 / 100];

    let (w, h) = gray.dimensions();
    let pad: u32 = 12;
    let mut out = GrayImage::from_pixel(w + pad * 2, h + pad * 2, Luma([255]));
    for (x, y, p) in gray.enumerate_pixels() {
        let v = if p.0[0] > cutoff { 0 } else { 255 };
        out.put_pixel(x + pad, y + pad, Luma([v]));
    }
    DynamicImage::ImageLuma8(out)
}

fn split_progress(s: &str) -> Option<(u32, u32)> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '/').collect();
    if let Some((a, b)) = cleaned.split_once('/') {
        let a: u32 = a.parse().ok()?;
        let b: u32 = b.parse().ok()?;
        return Some((a, b));
    }
    // No slash — try splitting in halves (handles "28" → 2/8).
    if cleaned.len() == 2 {
        let a = cleaned[..1].parse().ok()?;
        let b = cleaned[1..].parse().ok()?;
        return Some((a, b));
    }
    None
}

fn is_chrome(t: &str) -> bool {
    let up = t.to_ascii_uppercase();
    matches!(
        up.as_str(),
        "FROM" | "RAID" | "FROM." | "RAID." | "BACK" | "LEVEL" | "UP"
    )
}

