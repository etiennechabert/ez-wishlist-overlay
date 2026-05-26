//! Template-matching digit recognizer for the chunky pixel-art game font.
//!
//! Tesseract reads these stylized digits as letters ("2" → "e", "8" → "6") and
//! its character whitelist can't recover them because the model has no training
//! examples that look like this font. Template matching solves the problem
//! cleanly: there are only 11 glyphs (0-9, "/"), each renders pixel-identical
//! across every image, so a stored reference per glyph + connected-components
//! per input character is enough.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use image::{GenericImageView, GrayImage, Luma};

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

pub fn templates_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("templates")
}

/// Load all `<char>.png` (and `slash.png`) files from `dir` as templates.
pub fn load_templates(dir: &Path) -> Result<Vec<Template>> {
    let mut templates = Vec::new();
    if !dir.exists() {
        return Ok(templates);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("png") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let label = match stem.as_str() {
            "slash" => '/',
            s if s.chars().count() == 1 => s.chars().next().unwrap(),
            _ => continue,
        };
        let gray = image::open(&path)
            .with_context(|| format!("opening template {}", path.display()))?
            .to_luma8();
        let (w, h) = gray.dimensions();
        let mask: Vec<bool> = gray.pixels().map(|p| p.0[0] < 128).collect();
        templates.push(Template {
            label,
            mask,
            w,
            h,
        });
    }
    Ok(templates)
}

/// 4-connected connected-components labelling on a binary mask. The input is a
/// grayscale image where black (value < 128) is foreground (text), white is
/// background. Returns one component per cluster of touching foreground pixels.
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

/// Score how well a component matches a template. Resamples the component to
/// the template's dimensions (nearest-neighbour) and counts pixel agreement.
/// Returns a score in [0, 1].
pub fn score(comp: &Component, t: &Template) -> f32 {
    let mut agree = 0u32;
    let total = (t.w * t.h) as u32;
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

/// Match every component in the binary strip against the templates and return
/// the recognised characters, sorted left-to-right.
pub fn recognize(strip: &GrayImage, templates: &[Template]) -> String {
    if templates.is_empty() {
        return String::new();
    }
    let img_h = strip.height();
    let mut comps = find_components(strip);
    // Drop tiny noise AND components touching the top/bottom edges (cell row separator lines).
    comps.retain(|c| c.w * c.h >= 4 && c.y > 0 && c.y + c.h < img_h);
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

/// Save one component as a small black-on-white PNG template.
pub fn save_component_as_template(comp: &Component, label: char, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let filename = if label == '/' {
        "slash.png".to_string()
    } else {
        format!("{label}.png")
    };
    let path = dir.join(filename);
    let mut img = GrayImage::from_pixel(comp.w, comp.h, Luma([255]));
    for y in 0..comp.h {
        for x in 0..comp.w {
            if comp.mask[(y * comp.w + x) as usize] {
                img.put_pixel(x, y, Luma([0]));
            }
        }
    }
    img.save(&path)
        .with_context(|| format!("saving template {}", path.display()))?;
    Ok(path)
}
