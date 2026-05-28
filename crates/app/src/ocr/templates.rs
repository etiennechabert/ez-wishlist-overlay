//! Template-matching digit recognizer for the chunky pixel-art game font.
//!
//! Ported from `ocr_lab/src/templates.rs` on the `add_ocr_data` branch.
//! Tesseract reads the stylized digits as letters ("2" → "e", "8" → "6")
//! and even a whitelist can't recover them because Tesseract's model has
//! no training examples that look like this font. Template matching
//! solves the problem cleanly: there are only 11 glyphs (0-9, "/"), each
//! renders pixel-identical across every image, so a stored reference per
//! glyph + connected-components per input character is enough.

use crate::assets;
use anyhow::{Context, Result};
use image::GrayImage;
use once_cell::sync::Lazy;

#[derive(Clone)]
pub struct Template {
    pub label: char,
    pub mask: Vec<bool>,
    pub w: u32,
    pub h: u32,
    /// Number of background regions fully enclosed by foreground.
    /// Computed once at load time and used by [`score`] as a
    /// size-invariant topology discriminator: pixel-agreement
    /// scaling collapses closed-loop digits ('0', '6', '8') into
    /// each other at small rendered sizes, but the *number* of
    /// holes survives the scaling — `8` always has 2, `0`/`6`
    /// always have 1, the rest have 0.
    pub holes: u32,
}

#[derive(Clone)]
pub struct Component {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub mask: Vec<bool>,
    /// Same topology feature as [`Template::holes`], computed once
    /// when the component is built from the binarised strip.
    pub holes: u32,
}

/// Embedded templates loaded once at first use. Sourced from
/// `crates/app/src/assets/ocr_templates/*.png`. Until the user extracts a
/// fresh set from native-resolution captures, this directory may be empty
/// and digit recognition will return empty strings — upgrade
/// identification still works, but owned counts will all read as 0.
pub static EMBEDDED: Lazy<Vec<Template>> = Lazy::new(|| match load_embedded() {
    Ok(t) => {
        tracing::debug!(count = t.len(), "loaded embedded digit templates");
        t
    }
    Err(e) => {
        tracing::warn!(error = %e, "failed to load embedded digit templates — owned counts will read as 0");
        Vec::new()
    }
});

fn load_embedded() -> Result<Vec<Template>> {
    let mut templates = Vec::new();
    for entry in assets::ocr_template_files() {
        let stem = entry.0;
        let bytes = entry.1;
        let label = match stem.as_str() {
            "slash" => '/',
            s if s.chars().count() == 1 => s.chars().next().unwrap(),
            _ => continue,
        };
        let gray = image::load_from_memory(&bytes)
            .with_context(|| format!("decoding template {stem}"))?
            .to_luma8();
        let (w, h) = gray.dimensions();
        let mask: Vec<bool> = gray.pixels().map(|p| p.0[0] < 128).collect();
        let holes = count_holes(&mask, w, h);
        templates.push(Template {
            label,
            mask,
            w,
            h,
            holes,
        });
    }
    Ok(templates)
}

/// Count background "holes" — connected regions of background pixels
/// fully enclosed by foreground. Same algorithm for both templates
/// and live components: flood-fill the background from every bounding-
/// box edge, then count connected components of the un-reached
/// background pixels.
///
/// This is the topology feature the score function leans on to
/// discriminate digits that the pixel-agreement metric can't separate
/// at small rendered sizes ('0' vs '8', '3' vs '8', '1' vs '0').
/// Closed-loop digits ('0','6','9') have 1 hole, '8' has 2, the rest
/// have 0 — invariant of size, so it survives the small-component
/// regime where nearest-neighbour resample loses the distinguishing
/// fine features.
fn count_holes(mask: &[bool], w: u32, h: u32) -> u32 {
    if w == 0 || h == 0 {
        return 0;
    }
    let n = (w * h) as usize;
    let idx = |x: u32, y: u32| (y * w + x) as usize;
    let is_bg = |x: u32, y: u32| !mask[idx(x, y)];

    // Phase 1: flood-fill the exterior background from every edge.
    let mut reached = vec![false; n];
    let mut stack: Vec<(u32, u32)> = Vec::new();
    for x in 0..w {
        if is_bg(x, 0) {
            stack.push((x, 0));
        }
        if h > 1 && is_bg(x, h - 1) {
            stack.push((x, h - 1));
        }
    }
    for y in 0..h {
        if is_bg(0, y) {
            stack.push((0, y));
        }
        if w > 1 && is_bg(w - 1, y) {
            stack.push((w - 1, y));
        }
    }
    while let Some((x, y)) = stack.pop() {
        let i = idx(x, y);
        if reached[i] || !is_bg(x, y) {
            continue;
        }
        reached[i] = true;
        for (dx, dy) in [(-1i32, 0), (1, 0), (0, -1), (0, 1)] {
            let nx = x as i32 + dx;
            let ny = y as i32 + dy;
            if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                continue;
            }
            stack.push((nx as u32, ny as u32));
        }
    }

    // Phase 2: count connected components of unreached background
    // pixels. Holes smaller than `MIN_HOLE_PIXELS` don't count — at
    // the 5-7 px wide range where most live digits sit, single-pixel
    // gaps inside a '2' / '5' / '3' (e.g. a tiny binarisation
    // artefact that closes a curve into a loop) would otherwise read
    // as a fake hole and push the score function to pick a
    // closed-loop digit. Real closed-loop holes in '0' / '6' / '8'
    // at these sizes are always ≥ 2 px because the loop spans the
    // full inner cavity of the glyph.
    const MIN_HOLE_PIXELS: u32 = 2;
    let mut visited = reached.clone();
    let mut holes = 0u32;
    for sy in 0..h {
        for sx in 0..w {
            if visited[idx(sx, sy)] || !is_bg(sx, sy) {
                continue;
            }
            let mut size = 0u32;
            let mut hs = vec![(sx, sy)];
            while let Some((x, y)) = hs.pop() {
                let i = idx(x, y);
                if visited[i] || !is_bg(x, y) {
                    continue;
                }
                visited[i] = true;
                size += 1;
                for (dx, dy) in [(-1i32, 0), (1, 0), (0, -1), (0, 1)] {
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                        continue;
                    }
                    hs.push((nx as u32, ny as u32));
                }
            }
            if size >= MIN_HOLE_PIXELS {
                holes += 1;
            }
        }
    }
    holes
}

/// 4-connected connected-components labelling on a binary mask. The input
/// is a grayscale image where black (value < 128) is foreground (text)
/// and white is background. One `Component` per cluster of touching
/// foreground pixels.
pub fn find_components(img: &GrayImage) -> Vec<Component> {
    let (w, h) = img.dimensions();
    let n = (w * h) as usize;
    let mut visited = vec![false; n];
    let mut comps: Vec<Component> = Vec::new();

    let idx = |x: u32, y: u32| (y * w + x) as usize;
    let is_text = |x: u32, y: u32| img.get_pixel(x, y).0[0] < 128;

    for sy in 0..h {
        for sx in 0..w {
            if visited[idx(sx, sy)] || !is_text(sx, sy) {
                visited[idx(sx, sy)] = true;
                continue;
            }
            let mut stack = vec![(sx, sy)];
            let mut pixels: Vec<(u32, u32)> = Vec::new();
            while let Some((x, y)) = stack.pop() {
                let i = idx(x, y);
                if visited[i] {
                    continue;
                }
                visited[i] = true;
                if !is_text(x, y) {
                    continue;
                }
                pixels.push((x, y));
                for (dx, dy) in [(-1i32, 0), (1, 0), (0, -1), (0, 1)] {
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                        continue;
                    }
                    let (nx, ny) = (nx as u32, ny as u32);
                    if !visited[idx(nx, ny)] {
                        stack.push((nx, ny));
                    }
                }
            }
            if pixels.is_empty() {
                continue;
            }
            let min_x = pixels.iter().map(|p| p.0).min().unwrap();
            let max_x = pixels.iter().map(|p| p.0).max().unwrap();
            let min_y = pixels.iter().map(|p| p.1).min().unwrap();
            let max_y = pixels.iter().map(|p| p.1).max().unwrap();
            let cw = max_x - min_x + 1;
            let ch = max_y - min_y + 1;
            let mut mask = vec![false; (cw * ch) as usize];
            for (px, py) in &pixels {
                let lx = px - min_x;
                let ly = py - min_y;
                mask[(ly * cw + lx) as usize] = true;
            }
            let holes = count_holes(&mask, cw, ch);
            comps.push(Component {
                x: min_x,
                y: min_y,
                w: cw,
                h: ch,
                mask,
                holes,
            });
        }
    }
    comps
}

/// Score how well a component matches a template. The base signal is
/// pixel agreement after nearest-neighbour resample to the template's
/// dimensions; on top of that we apply a topology discriminator
/// (hole count) because the base signal can't separate closed-loop
/// digits at small rendered sizes.
///
/// **Why the hole-count term exists.** The pixel-agreement score
/// gives `'8'=0.702, '2'=0.676` for a component that's clearly a
/// `2` (game UI captured at distance, digit ≈ 5×11). Closed-loop
/// digits and non-loop digits collapse into the same coarse grid
/// after nearest-neighbour scaling, and whichever template happens
/// to have a slightly fuller foreground at the sample points wins.
/// `comp.holes` and `t.holes` survive scaling unchanged — '0' always
/// has 1 hole, '8' always has 2, '2' always has 0 — so a mismatch
/// is strong evidence the template is wrong.
///
/// **Penalty schedule.** Asymmetric on purpose:
/// - Equal hole counts → no change. Most common case.
/// - Component has FEWER holes than template → mild penalty. Could
///   legitimately be a small-size degradation (a barely-closed loop
///   that broke open during binarisation), so we don't punish hard.
/// - Component has MORE holes than template → stronger penalty.
///   Templates are clean references; if the component shows extra
///   enclosed background, it's either noise OR a genuinely
///   higher-topology digit being matched to a lower-topology one
///   (e.g. component '8' matched against template '0'). Either way
///   the template is unlikely to be right.
///
/// Constants tuned by sweeping the fixture suite — a vertical-mass
/// distribution discriminator was prototyped on top of this but
/// regressed (-3 cells) because live components are noisy enough at
/// these sizes for the top/bottom mass split to flip false-positive
/// for legitimate matches. Hole count alone is the discriminator
/// that survives.
pub fn score(comp: &Component, t: &Template) -> f32 {
    let mut agree = 0u32;
    let total = t.w * t.h;
    for ty in 0..t.h {
        for tx in 0..t.w {
            let sx = tx * comp.w / t.w;
            let sy = ty * comp.h / t.h;
            let cv = comp.mask[(sy * comp.w + sx) as usize];
            let tv = t.mask[(ty * t.w + tx) as usize];
            if cv == tv {
                agree += 1;
            }
        }
    }
    let base = agree as f32 / total as f32;

    // Hole-count discriminator. Caps the diff at 2 so a very noisy
    // component can't drag the penalty to zero — even with bad
    // binarisation the base pixel score still has signal.
    let comp_h = comp.holes as i32;
    let templ_h = t.holes as i32;
    let penalty = if comp_h == templ_h {
        1.0
    } else if comp_h < templ_h {
        // Component lost a hole — possible at small sizes. Mild.
        let diff = ((templ_h - comp_h) as f32).min(2.0);
        1.0 - 0.10 * diff
    } else {
        // Component has extra holes — wrong digit or noise. Stronger.
        let diff = ((comp_h - templ_h) as f32).min(2.0);
        1.0 - 0.18 * diff
    };

    base * penalty
}

/// Match every component in the binary strip against the templates and
/// return the recognised characters, sorted left-to-right.
///
/// The cell strip is intentionally tall (about 3 text-rows) so head tilt
/// can't shift the digits out of frame — but that means the strip can
/// also contain the "FROM RAID" label sitting below the count. We
/// cluster by Y and keep only the topmost row (where the digits are).
// Thin wrapper around `recognize_with_debug` for callers that don't
// need the per-component score breakdown. The pipeline now always
// goes through the debug variant (it's nearly free), but external
// tests + ignored diagnostics still call this convenient version.
#[allow(dead_code)]
pub fn recognize(strip: &GrayImage, templates: &[Template]) -> String {
    recognize_with_debug(strip, templates).recognised
}

/// Bag of intermediate state that [`recognize_with_debug`] returns so
/// the debug-dump writer can show exactly what the matcher saw, what
/// it kept after filtering, and which templates each kept component
/// scored against. The pipeline's [`recognize`] wrapper drops this.
///
/// `raw_components` is only consumed by the debug-dump writer, which
/// is `#[cfg(debug_assertions)]`-gated — release builds never touch
/// it, hence the `dead_code` opt-out so `-D warnings` doesn't fail
/// the MSI job.
pub struct RecognizeDebug {
    pub recognised: String,
    #[allow(dead_code)]
    pub raw_components: Vec<(u32, u32, u32, u32)>,
    pub kept_components: Vec<KeptComponent>,
}

pub struct KeptComponent {
    // x/y/w/h are read only by the debug-dump writer (debug builds);
    // release builds never observe these, hence the opt-out. Keeping
    // them as plain `pub` fields keeps the dump construction site
    // identical across cfgs.
    #[allow(dead_code)]
    pub x: u32,
    #[allow(dead_code)]
    pub y: u32,
    #[allow(dead_code)]
    pub w: u32,
    #[allow(dead_code)]
    pub h: u32,
    /// All template scores, sorted desc by score. First entry is the
    /// winner that ended up in the recognised string.
    pub scores: Vec<(char, f32)>,
}

/// Variant of [`recognize_with_debug`] that uses the **known Y** value
/// from `data.json` to disambiguate the layout. The cell always shows
/// `X/Y` in left-to-right order: `x_n` X-digit components, then exactly
/// one `/`, then `y_n` Y-digit components, where `y_n = len(str(Y))`.
///
/// The blind template matcher routinely confuses the `/` glyph with
/// `1` (both are narrow vertical bars at this font size), turning a
/// real `2/6` into recognised `"216"` — which then fails
/// `split_progress` and silently drops the read. Knowing where the
/// slash MUST sit lets us force-assign it, then template-match the
/// other positions against digits only.
///
/// Falls back to the unconstrained read when the kept-component count
/// is too small to fit even `/<Y>` — that's a sign the strip is on the
/// wrong row or the panel layout differs from what we expect, and
/// forcing structure would just produce nonsense.
pub fn recognize_with_known_needed(
    strip: &GrayImage,
    templates: &[Template],
    needed: u32,
) -> RecognizeDebug {
    /// Cap on how many digits the X (owned-count) part is allowed to
    /// have. Real-world hideout inventories sit in 0–99; a 3+ digit
    /// X read is overwhelmingly noise — extra components that
    /// survived row clustering (FROM RAID letter fragments, icon
    /// edges, etc.). Reject those rather than emit garbage like
    /// "844/1" or "58161/6".
    const MAX_X_DIGITS: usize = 2;
    let mut base = recognize_with_debug(strip, templates);
    let total = base.kept_components.len();
    let y_n = needed.to_string().chars().count();
    if total < y_n + 1 {
        return base;
    }
    let x_n = total - y_n - 1;
    if x_n > MAX_X_DIGITS {
        return base;
    }
    let slash_idx = x_n;
    let mut out = String::with_capacity(total);
    for (i, k) in base.kept_components.iter().enumerate() {
        if i == slash_idx {
            out.push('/');
        } else {
            // Best NON-slash label from the pre-computed scores. If
            // every score is for '/' (impossible with the bundled
            // templates) we'd emit '?', which split_progress rejects
            // — fine, the existing collected value survives.
            let best_digit = k
                .scores
                .iter()
                .find(|(c, _)| *c != '/')
                .map(|(c, _)| *c)
                .unwrap_or('?');
            out.push(best_digit);
        }
    }
    base.recognised = out;
    base
}

pub fn recognize_with_debug(strip: &GrayImage, templates: &[Template]) -> RecognizeDebug {
    if templates.is_empty() {
        return RecognizeDebug {
            recognised: String::new(),
            raw_components: Vec::new(),
            kept_components: Vec::new(),
        };
    }
    let img_h = strip.height();
    let mut comps = find_components(strip);
    let raw_components: Vec<(u32, u32, u32, u32)> =
        comps.iter().map(|c| (c.x, c.y, c.w, c.h)).collect();
    // Drop tiny noise AND components touching the top/bottom edges (cell
    // row separator lines / strip artefacts). The "1" glyph in this
    // pixel-art font is a 2-px-wide vertical bar — using c.w >= 4 here
    // silently dropped every leading "1" and turned "1/5" reads into
    // "/5", which split_progress then fell back to (0, 5). The slash
    // is similarly narrow.
    // Digit shape gate. Width/height bounds + a w<=1.5*h aspect cap:
    //   - w >= 2 keeps narrow "1" / "/" glyphs (a w>=4 cap silently
    //     dropped every leading "1" in 1/5 reads).
    //   - h >= 8 drops single-pixel scratches.
    //   - y/img_h edge guard drops separator lines hugging the strip
    //     top or bottom.
    //   - w <= 1.5 * h drops horizontal lines (cell-border artifacts
    //     spanning ~150 px wide that survived the edge guard at strip
    //     mid-Y — KitchenArea cell 2 had a 149×17 border that turned
    //     "1/1" into "11/1" by inflating the kept-component count).
    comps.retain(|c| {
        c.w >= 2 && c.h >= 8 && c.y > 0 && c.y + c.h < img_h && (c.w as u64) <= (c.h as u64) * 3 / 2
    });
    // Y-row clustering: keep only the topmost row. The digits live above
    // the FROM RAID label; clustering by min_y + (tallest component's
    // height) gives a tight bound around the digit row.
    if !comps.is_empty() {
        let min_y = comps.iter().map(|c| c.y).min().unwrap();
        let max_h = comps.iter().map(|c| c.h).max().unwrap();
        let row_cutoff = min_y + max_h;
        comps.retain(|c| c.y <= row_cutoff);
    }
    comps.sort_by_key(|c| c.x);
    let mut out = String::new();
    let mut kept_components = Vec::with_capacity(comps.len());
    for c in &comps {
        let mut scores: Vec<(char, f32)> =
            templates.iter().map(|t| (t.label, score(c, t))).collect();
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some(&(label, _)) = scores.first() {
            out.push(label);
        }
        kept_components.push(KeptComponent {
            x: c.x,
            y: c.y,
            w: c.w,
            h: c.h,
            scores,
        });
    }
    RecognizeDebug {
        recognised: out,
        raw_components,
        kept_components,
    }
}

/// Split a string like "3/8" or "12/20" into `(owned, needed)` integers.
/// Handles missing slash by halving the digit run (e.g. "28" → (2, 8))
/// when exactly two digits were recognised.
pub fn split_progress(s: &str) -> Option<(u32, u32)> {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '/')
        .collect();
    if let Some((a, b)) = cleaned.split_once('/') {
        let a: u32 = a.parse().ok()?;
        let b: u32 = b.parse().ok()?;
        return Some((a, b));
    }
    if cleaned.len() == 2 {
        let a = cleaned[..1].parse().ok()?;
        let b = cleaned[1..].parse().ok()?;
        return Some((a, b));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_with_slash() {
        assert_eq!(split_progress("3/8"), Some((3, 8)));
        assert_eq!(split_progress("12/20"), Some((12, 20)));
        assert_eq!(split_progress(" 0 / 4 "), Some((0, 4)));
    }

    #[test]
    fn split_two_digits_no_slash() {
        assert_eq!(split_progress("28"), Some((2, 8)));
    }

    #[test]
    fn split_garbage() {
        assert_eq!(split_progress(""), None);
        assert_eq!(split_progress("abc"), None);
        assert_eq!(split_progress("123"), None); // ambiguous, no slash
    }

    #[test]
    fn recognize_empty_when_no_templates() {
        let img = GrayImage::from_pixel(10, 10, image::Luma([255]));
        assert_eq!(recognize(&img, &[]), String::new());
    }
}
