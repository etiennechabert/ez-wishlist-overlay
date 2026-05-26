//! Image preprocessing for the white-text-on-dark game UI.

use image::{DynamicImage, GenericImageView, GrayImage, Luma};

/// Tunable preprocessing parameters.
#[derive(Debug, Clone, Copy)]
pub struct PrepParams {
    /// Minimum luminance for a pixel to count as "white-ish" text.
    pub white_lum: u8,
    /// Minimum green-channel value AND green-vs-other-channels gap for "green text".
    pub green_min: u8,
    pub green_margin: u8,
    /// Whether to dilate the text mask by 1 pixel (fattens thin strokes).
    pub dilate: bool,
}

impl PrepParams {
    pub fn label(&self) -> String {
        // dil first so all dil0 files cluster together when sorted, then lum.
        format!(
            "dil{}_lum{:03}_grn{:03}m{:02}",
            if self.dilate { 1 } else { 0 },
            self.white_lum,
            self.green_min,
            self.green_margin
        )
    }
}

/// Default preset used by the rest of the pipeline.
pub const DEFAULT: PrepParams = PrepParams {
    white_lum: 175,
    green_min: 130,
    green_margin: 30,
    dilate: false,
};

/// Keep text-colored pixels and invert: text → black, everything else → white.
pub fn keep_white_invert(img: &DynamicImage) -> DynamicImage {
    process(img, DEFAULT)
}

pub fn process(img: &DynamicImage, params: PrepParams) -> DynamicImage {
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();

    let mut mask: Vec<bool> = vec![false; (w * h) as usize];
    for (x, y, p) in rgb.enumerate_pixels() {
        if is_text_pixel(p.0, params) {
            mask[(y * w + x) as usize] = true;
        }
    }

    let mask = if params.dilate {
        dilate_1px(&mask, w, h)
    } else {
        mask
    };

    let mut out = GrayImage::from_pixel(w, h, Luma([255]));
    for y in 0..h {
        for x in 0..w {
            if mask[(y * w + x) as usize] {
                out.put_pixel(x, y, Luma([0]));
            }
        }
    }
    DynamicImage::ImageLuma8(out)
}

fn is_text_pixel([r, g, b]: [u8; 3], p: PrepParams) -> bool {
    let lum = ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u8;
    let near_white = lum >= p.white_lum;

    let green = g >= p.green_min
        && g as i16 - r as i16 >= p.green_margin as i16
        && g as i16 - b as i16 >= p.green_margin as i16;

    near_white || green
}

fn dilate_1px(mask: &[bool], w: u32, h: u32) -> Vec<bool> {
    let mut out = vec![false; mask.len()];
    let wi = w as i32;
    let hi = h as i32;
    for y in 0..hi {
        for x in 0..wi {
            let mut hit = false;
            'outer: for dy in -1..=1 {
                for dx in -1..=1 {
                    let nx = x + dx;
                    let ny = y + dy;
                    if nx < 0 || ny < 0 || nx >= wi || ny >= hi {
                        continue;
                    }
                    if mask[(ny * w as i32 + nx) as usize] {
                        hit = true;
                        break 'outer;
                    }
                }
            }
            out[(y * w as i32 + x) as usize] = hit;
        }
    }
    out
}

/// Variants to sweep in `ocr_lab prep` for visual comparison. Step size of 5
/// across the useful luminance range. Dilation is dropped — it merges letters
/// in the chunky-pixel font more than it helps fill thin slashes.
pub fn sweep_variants() -> Vec<PrepParams> {
    let mut v = Vec::new();
    let mut white_lum = 150;
    while white_lum <= 200 {
        v.push(PrepParams {
            white_lum,
            green_min: 130,
            green_margin: 30,
            dilate: false,
        });
        white_lum += 1;
    }
    v
}
