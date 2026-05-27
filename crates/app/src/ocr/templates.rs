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
pub fn recognize(strip: &GrayImage, templates: &[Template]) -> String {
    if templates.is_empty() {
        return String::new();
    }
    let img_h = strip.height();
    let mut comps = find_components(strip);
    // Drop tiny noise AND components touching the top/bottom edges (cell
    // row separator lines / strip artefacts). The digit glyphs are
    // roughly square and tall — anything under 4×8 is noise.
    comps.retain(|c| c.w >= 4 && c.h >= 8 && c.y > 0 && c.y + c.h < img_h);
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
    for c in &comps {
        let best = templates
            .iter()
            .map(|t| (t.label, score(c, t)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((label, _s)) = best {
            out.push(label);
        }
    }
    out
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
