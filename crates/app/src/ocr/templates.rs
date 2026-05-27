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
}

#[derive(Clone)]
pub struct Component {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub mask: Vec<bool>,
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
        templates.push(Template { label, mask, w, h });
    }
    Ok(templates)
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
            comps.push(Component {
                x: min_x,
                y: min_y,
                w: cw,
                h: ch,
                mask,
            });
        }
    }
    comps
}

/// Score how well a component matches a template by resampling the
/// component to the template's dimensions (nearest-neighbour) and counting
/// pixel agreement. Returns a score in [0, 1].
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
    agree as f32 / total as f32
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
