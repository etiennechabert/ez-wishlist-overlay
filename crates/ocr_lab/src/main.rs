//! ocr_lab — isolated OCR test bed for the ez-wishlist-overlay facility-upgrade panel.
//!
//! Usage:
//!   cargo run -p ocr_lab -- score      # run OCR on every image in ocr_data/ and score
//!   cargo run -p ocr_lab -- one <img>  # process a single image, print verbose output

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

mod engine;
mod grid;
mod pipeline;
mod prep;
mod score;
mod templates;

use engine::OcrEngine;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LabelsFile {
    pub schema_version: u32,
    #[serde(default)]
    pub notes: String,
    pub images: BTreeMap<String, Panel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Panel {
    pub title: String,
    pub level: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_next: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<u64>,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub collected: u32,
    pub needed: u32,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ocr_lab=info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("score");

    let repo_root = locate_repo_root()?;
    let data_dir = repo_root.join("ocr_data");
    let labels_path = data_dir.join("labels.json");
    let labels = load_labels(&labels_path)?;

    // Default image (Gunsmith — covers 0,1,2,8,/ digits & 4-cell layout) used
    // when no image arg is given. Normalised to always include the .jpg suffix.
    const DEFAULT_IMAGE: &str = "20260526091314_1.jpg";
    let normalize = |s: &str| -> String {
        if s.ends_with(".jpg") || s.ends_with(".png") {
            s.to_string()
        } else {
            format!("{s}.jpg")
        }
    };

    // Resolve the image arg: missing → default, "all" → every jpg in ocr_data/,
    // otherwise → the single normalised name.
    let resolve_images = |arg: Option<&String>| -> Result<Vec<String>> {
        match arg.map(String::as_str) {
            None => Ok(vec![DEFAULT_IMAGE.to_string()]),
            Some("all") => list_dataset_images(&data_dir),
            Some(s) => Ok(vec![normalize(s)]),
        }
    };

    let engine = engine::build_default_engine()?;

    match mode {
        "score" => run_score(&data_dir, &labels, engine.as_ref()),
        "one" => {
            for img in resolve_images(args.get(2))? {
                run_one(&data_dir, &labels, engine.as_ref(), &img)?;
            }
            Ok(())
        }
        "prep" => {
            for img in resolve_images(args.get(2))? {
                run_prep(&data_dir, engine.as_ref(), &img)?;
            }
            Ok(())
        }
        "digit" => {
            let img = args.get(2).map(|s| normalize(s)).unwrap_or_else(|| DEFAULT_IMAGE.to_string());
            let rect = args.get(3).context("missing <x,y,w,h>")?;
            let expected = args.get(4).cloned().unwrap_or_default();
            run_digit(&data_dir, engine.as_ref(), &img, rect, &expected)
        }
        "tm-extract" => {
            let img = args.get(2).map(|s| normalize(s)).unwrap_or_else(|| DEFAULT_IMAGE.to_string());
            let rect = args.get(3).context("missing <x,y,w,h>")?;
            let expected = args.get(4).context("missing <expected>")?;
            run_tm_extract(&data_dir, &img, rect, expected)
        }
        "tm" => {
            let img = args.get(2).map(|s| normalize(s)).unwrap_or_else(|| DEFAULT_IMAGE.to_string());
            let rect = args.get(3).context("missing <x,y,w,h>")?;
            run_tm(&data_dir, &img, rect)
        }
        "grid" => {
            for img in resolve_images(args.get(2))? {
                run_grid(&data_dir, engine.as_ref(), &img)?;
            }
            Ok(())
        }
        "ocr-debug" => {
            for img in resolve_images(args.get(2))? {
                run_ocr_debug(&data_dir, engine.as_ref(), &img)?;
            }
            Ok(())
        }
        other => anyhow::bail!(
            "unknown mode: {other} (expected: score | one | prep | digit | tm-extract | tm | grid | ocr-debug)"
        ),
    }
}

/// List every `.jpg` directly under `ocr_data/` (no subdirs), sorted.
fn list_dataset_images(data_dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(data_dir)
        .with_context(|| format!("reading {}", data_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.to_ascii_lowercase().ends_with(".jpg") {
            out.push(name);
        }
    }
    out.sort();
    if out.is_empty() {
        anyhow::bail!("no .jpg files found in {}", data_dir.display());
    }
    Ok(out)
}

/// Render the preprocessed + upscaled image with each OCR'd word drawn at its
/// detected location. Box color encodes confidence: red = low, orange = mid,
/// green = high. The recognized text appears next to each box.
fn run_ocr_debug(data_dir: &Path, engine: &dyn OcrEngine, name: &str) -> Result<()> {
    use ab_glyph::{FontRef, PxScale};
    use image::Rgba;
    use imageproc::drawing::{draw_hollow_rect_mut, draw_text_mut};
    use imageproc::rect::Rect;

    let img_path = data_dir.join(name);
    let original = image::open(&img_path)
        .with_context(|| format!("loading {}", img_path.display()))?;
    let prepped = prep::process(&original, prep::DEFAULT);
    let scale = 2u32;
    let (pw, ph) = {
        use image::GenericImageView;
        prepped.dimensions()
    };
    let big = prepped.resize(pw * scale, ph * scale, image::imageops::FilterType::Lanczos3);

    let words = engine.recognize(
        &big,
        &engine::OcrOptions {
            psm: Some(engine::Psm::Sparse),
            whitelist: None,
        },
    )?;

    // Convert big to RGBA so we can draw on it.
    let mut canvas = big.to_rgba8();

    let font_bytes: &[u8] = include_bytes!("consola.ttf");
    let font = FontRef::try_from_slice(font_bytes).context("loading embedded font")?;
    let scale_px = PxScale::from(24.0);

    let mut sorted = words.clone();
    sorted.sort_by_key(|w| (w.bbox.y, w.bbox.x));
    println!("found {} OCR tokens:", sorted.len());
    for w in &sorted {
        // Drop very low confidence noise to keep image readable.
        if w.confidence < 25.0 {
            continue;
        }
        let conf = w.confidence;
        let color = if conf >= 75.0 {
            Rgba([60u8, 200, 80, 255]) // green
        } else if conf >= 50.0 {
            Rgba([240, 200, 30, 255]) // yellow
        } else {
            Rgba([220, 80, 80, 255]) // red
        };
        let rect = Rect::at(w.bbox.x as i32, w.bbox.y as i32)
            .of_size(w.bbox.w.max(1), w.bbox.h.max(1));
        draw_hollow_rect_mut(&mut canvas, rect, color);
        // Label above the box (or below if near the top edge).
        let label = format!("{} [{:.0}]", w.text, conf);
        let label_y = if w.bbox.y > 30 {
            (w.bbox.y as i32) - 26
        } else {
            (w.bbox.y + w.bbox.h) as i32 + 2
        };
        draw_text_mut(
            &mut canvas,
            color,
            w.bbox.x as i32,
            label_y,
            scale_px,
            &font,
            &label,
        );
    }

    let out_dir = data_dir.join("ocr_debug");
    std::fs::create_dir_all(&out_dir)?;
    let stem = std::path::Path::new(name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let out = out_dir.join(format!("{stem}.ocr_debug.png"));
    canvas.save(&out)?;
    println!("wrote {}", out.display());
    Ok(())
}

fn run_grid(data_dir: &Path, engine: &dyn OcrEngine, name: &str) -> Result<()> {
    let img_path = data_dir.join(name);
    let original = image::open(&img_path)
        .with_context(|| format!("loading {}", img_path.display()))?;
    // BACK / LEVEL UP read most reliably from the *raw* image (Tesseract gets them
    // at ~96% conf there). The preprocessing wipes the button background and
    // weakens letterforms enough to confuse the detector.
    let layout = grid::build(&original, engine)?;
    let prepped = prep::process(&original, PREP_FOR_TEMPLATES);

    println!(
        "BACK     at ({}, {}, {}, {})",
        layout.button_back.x, layout.button_back.y, layout.button_back.w, layout.button_back.h
    );
    println!(
        "LEVEL UP at ({}, {}, {}, {})",
        layout.button_level_up.x,
        layout.button_level_up.y,
        layout.button_level_up.w,
        layout.button_level_up.h
    );
    println!("\nRegions ({}):", layout.regions.len());
    for r in &layout.regions {
        println!(
            "  {:<18} ({:>5}, {:>5}, {:>5}, {:>5})",
            r.name, r.rect.x, r.rect.y, r.rect.w, r.rect.h
        );
    }

    // Render on the original image (color overlay) so the colored boxes are easy
    // to see against the screenshot.
    let overlay = grid::render_overlay(&original, &layout);
    let out_dir = data_dir.join("grids");
    std::fs::create_dir_all(&out_dir)?;
    let stem = std::path::Path::new(name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let out_path = out_dir.join(format!("{stem}.grid.png"));
    overlay.save(&out_path)?;
    println!("\nwrote {}", out_path.display());

    // Also save a downscaled version (≤ 1100px) for easy preview.
    let (ow, oh) = overlay.dimensions();
    let max_dim = ow.max(oh);
    if max_dim > 1100 {
        let scale = 1100.0 / max_dim as f32;
        let nw = (ow as f32 * scale) as u32;
        let nh = (oh as f32 * scale) as u32;
        let small = image::DynamicImage::ImageRgba8(overlay).resize(
            nw,
            nh,
            image::imageops::FilterType::Lanczos3,
        );
        let small_path = out_dir.join(format!("{stem}.grid_small.png"));
        small.save(&small_path)?;
        println!("wrote {}", small_path.display());
    }

    Ok(())
}

fn parse_rect(spec: &str) -> Result<(u32, u32, u32, u32)> {
    let parts: Vec<u32> = spec
        .split(',')
        .map(|s| s.trim().parse::<u32>())
        .collect::<std::result::Result<_, _>>()
        .context("rect format: x,y,w,h")?;
    if parts.len() != 4 {
        anyhow::bail!("rect format: x,y,w,h");
    }
    Ok((parts[0], parts[1], parts[2], parts[3]))
}

const PREP_FOR_TEMPLATES: prep::PrepParams = prep::DEFAULT;

fn run_tm_extract(data_dir: &Path, name: &str, rect_spec: &str, expected: &str) -> Result<()> {
    let (x, y, w, h) = parse_rect(rect_spec)?;
    let img_path = data_dir.join(name);
    let original = image::open(&img_path)
        .with_context(|| format!("loading {}", img_path.display()))?;
    let cropped = original.crop_imm(x, y, w, h);
    let prepped = prep::process(&cropped, PREP_FOR_TEMPLATES);
    let gray = prepped.to_luma8();

    // Save preprocessed crop for visual debugging.
    let stem = std::path::Path::new(name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let dbg_dir = data_dir.join("templates_debug");
    std::fs::create_dir_all(&dbg_dir)?;
    prepped.save(dbg_dir.join(format!("{stem}.prepped.png")))?;

    let comps_raw = templates::find_components(&gray);
    // Drop tiny noise, AND drop anything touching the top/bottom edges (cell row lines).
    let img_h = gray.height();
    let mut comps: Vec<templates::Component> = comps_raw
        .into_iter()
        .filter(|c| c.w * c.h >= 4)
        .filter(|c| c.y > 0 && c.y + c.h < img_h)
        .collect();
    comps.sort_by_key(|c| c.x);

    let chars: Vec<char> = expected.chars().collect();
    if comps.len() != chars.len() {
        eprintln!(
            "WARN: found {} components but expected {} chars ({:?}).",
            comps.len(),
            chars.len(),
            expected
        );
        eprintln!("Component positions (x,y,w,h):");
        for (i, c) in comps.iter().enumerate() {
            eprintln!("  {i}: ({}, {}, {}, {})", c.x, c.y, c.w, c.h);
        }
        anyhow::bail!("component count mismatch; tweak the crop");
    }

    let tdir = templates::templates_dir(data_dir);
    for (c, ch) in comps.iter().zip(chars.iter()) {
        let path = templates::save_component_as_template(c, *ch, &tdir)?;
        println!("wrote {} ({}x{})", path.display(), c.w, c.h);
    }
    Ok(())
}

fn run_tm(data_dir: &Path, name: &str, rect_spec: &str) -> Result<()> {
    let (x, y, w, h) = parse_rect(rect_spec)?;
    let img_path = data_dir.join(name);
    let original = image::open(&img_path)
        .with_context(|| format!("loading {}", img_path.display()))?;
    let cropped = original.crop_imm(x, y, w, h);
    let prepped = prep::process(&cropped, PREP_FOR_TEMPLATES);
    let gray = prepped.to_luma8();

    let tdir = templates::templates_dir(data_dir);
    let tmpls = templates::load_templates(&tdir)?;
    if tmpls.is_empty() {
        anyhow::bail!(
            "no templates found in {}. Use tm-extract first.",
            tdir.display()
        );
    }
    let labels: Vec<char> = tmpls.iter().map(|t| t.label).collect();
    println!("loaded {} templates: {:?}", tmpls.len(), labels);

    let img_h = gray.height();
    let comps_raw = templates::find_components(&gray);
    let mut comps: Vec<templates::Component> = comps_raw
        .into_iter()
        .filter(|c| c.w * c.h >= 4 && c.y > 0 && c.y + c.h < img_h)
        .collect();
    comps.sort_by_key(|c| c.x);

    println!("\nfound {} components in strip:", comps.len());
    for c in &comps {
        let mut scored: Vec<(char, f32)> = tmpls
            .iter()
            .map(|t| (t.label, templates::score(c, t)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let top: Vec<String> = scored
            .iter()
            .take(3)
            .map(|(l, s)| format!("{l}={:.2}", s))
            .collect();
        println!(
            "  ({:>3},{:>3}) {:>3}x{:<3}  best={} top3=[{}]",
            c.x,
            c.y,
            c.w,
            c.h,
            scored[0].0,
            top.join(", ")
        );
    }

    let recognized = templates::recognize(&gray, &tmpls);
    println!("\nrecognized: {:?}", recognized);
    Ok(())
}

fn run_digit(
    data_dir: &Path,
    engine: &dyn OcrEngine,
    name: &str,
    rect_spec: &str,
    expected: &str,
) -> Result<()> {
    let parts: Vec<u32> = rect_spec
        .split(',')
        .map(|s| s.trim().parse::<u32>())
        .collect::<std::result::Result<_, _>>()
        .context("rect format: x,y,w,h")?;
    if parts.len() != 4 {
        anyhow::bail!("rect format: x,y,w,h");
    }
    let (x, y, w, h) = (parts[0], parts[1], parts[2], parts[3]);

    let img_path = data_dir.join(name);
    let original = image::open(&img_path)
        .with_context(|| format!("loading {}", img_path.display()))?;
    let cropped = original.crop_imm(x, y, w, h);

    let stem = Path::new(name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let out_dir = data_dir.join("digits");
    std::fs::create_dir_all(&out_dir)?;
    cropped.save(out_dir.join(format!("{stem}.raw.png")))?;

    println!("expected: {:?}\n", expected);
    println!(
        "{:<40}  {:>5}  {:>5}  result",
        "variant", "lum", "scale"
    );

    // Try a matrix of: white_lum threshold × upscale factor × psm × whitelist.
    let lums = [120u8, 140, 160, 180, 200];
    let scales = [2u32, 4, 6, 8];
    let psms = [(engine::Psm::Line, "line"), (engine::Psm::Word, "word"), (engine::Psm::Sparse, "sparse")];

    for &lum in &lums {
        for &scale in &scales {
            let prepped_small = prep::process(
                &cropped,
                prep::PrepParams {
                    white_lum: lum,
                    green_min: 130,
                    green_margin: 30,
                    dilate: false,
                },
            );
            let (pw, ph) = prepped_small.dimensions();
            use image::GenericImageView;
            let up = prepped_small.resize(pw * scale, ph * scale, image::imageops::FilterType::Lanczos3);
            // White-pad: text is already black-on-white, but add a 30-px white border
            // so digits aren't flush against the edge.
            let pad: u32 = 30;
            let mut padded = image::GrayImage::from_pixel(
                up.width() + pad * 2,
                up.height() + pad * 2,
                image::Luma([255]),
            );
            image::imageops::overlay(
                &mut padded,
                &up.to_luma8(),
                pad as i64,
                pad as i64,
            );
            let padded_dyn = image::DynamicImage::ImageLuma8(padded);

            let label = format!("lum{lum}_x{scale}");
            padded_dyn.save(out_dir.join(format!("{stem}.{label}.png")))?;

            for (psm, psm_name) in &psms {
                let ws = engine.recognize(
                    &padded_dyn,
                    &engine::OcrOptions {
                        psm: Some(*psm),
                        whitelist: Some("0123456789/"),
                    },
                )?;
                let text: String = ws
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join("");
                let confs: Vec<String> = ws.iter().map(|w| format!("{:.0}", w.confidence)).collect();
                let match_marker = if !expected.is_empty() && text == expected {
                    " <-- MATCH"
                } else {
                    ""
                };
                println!(
                    "{:<40}  {:>5}  {:>5}  psm={:<6} {:?} confs=[{}]{}",
                    label,
                    lum,
                    scale,
                    psm_name,
                    text,
                    confs.join(","),
                    match_marker
                );
            }
        }
    }

    Ok(())
}

fn run_prep(data_dir: &Path, engine: &dyn OcrEngine, name: &str) -> Result<()> {
    let labels_path = data_dir.join("labels.json");
    let labels = load_labels(&labels_path)?;
    let expected = labels
        .images
        .get(name)
        .with_context(|| format!("no ground truth for {name}"))?;

    let img_path = data_dir.join(name);
    let original = image::open(&img_path)
        .with_context(|| format!("loading {}", img_path.display()))?;
    let out_dir = data_dir.join("prepped");
    std::fs::create_dir_all(&out_dir)?;

    let stem = std::path::Path::new(name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let expected_tokens = expected_tokens_for(expected);
    println!("expected tokens ({}): {}\n", expected_tokens.len(), expected_tokens.join(", "));
    println!(
        "{:<30}  {:>5}  {:>5}  {:>5}  {:>5}  {:>5}  {}",
        "variant", "title", "level", "cost", "names", "prog", "found"
    );

    let mut rows: Vec<(prep::PrepParams, usize, String)> = Vec::new();
    for params in prep::sweep_variants() {
        let result = prep::process(&original, params);
        let label = params.label();
        let out_path = out_dir.join(format!("{stem}.{label}.png"));
        result.save(&out_path)?;

        let words = engine.recognize(
            &result,
            &engine::OcrOptions {
                psm: Some(engine::Psm::Sparse),
                whitelist: None,
            },
        )?;
        let detected: Vec<String> = words
            .iter()
            .filter(|w| w.confidence > 50.0)
            .map(|w| w.text.clone())
            .collect();

        let score = score_against_expected(&detected, expected);
        println!(
            "{:<30}  {:>5}  {:>5}  {:>5}  {:>5}  {:>5}  {}",
            label,
            yn(score.title),
            yn(score.level),
            yn(score.cost),
            format!("{}/{}", score.name_hits, score.name_total),
            format!("{}/{}", score.prog_hits, score.prog_total),
            score.total_found(),
        );
        rows.push((params, score.total_found(), label));
    }

    rows.sort_by(|a, b| b.1.cmp(&a.1));
    println!("\nbest variants:");
    for (params, score, label) in rows.iter().take(3) {
        println!("  {score:>3} matches  {label}  {params:?}");
    }
    Ok(())
}

fn expected_tokens_for(p: &Panel) -> Vec<String> {
    let mut t = vec![p.title.clone(), p.level.clone()];
    if let Some(c) = p.cost {
        t.push(c.to_string());
    }
    for item in &p.items {
        t.push(item.name.clone());
        t.push(format!("{}/{}", item.collected, item.needed));
    }
    t
}

struct VariantScore {
    title: bool,
    level: bool,
    cost: bool,
    name_hits: usize,
    name_total: usize,
    prog_hits: usize,
    prog_total: usize,
}

impl VariantScore {
    fn total_found(&self) -> usize {
        (self.title as usize)
            + (self.level as usize)
            + (self.cost as usize)
            + self.name_hits
            + self.prog_hits
    }
}

fn yn(b: bool) -> &'static str {
    if b { "v" } else { "x" }
}

fn score_against_expected(detected: &[String], expected: &Panel) -> VariantScore {
    let detected_joined = detected.join(" ").to_ascii_lowercase();
    let detected_set: Vec<String> = detected
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();

    let title = expected
        .title
        .split_whitespace()
        .all(|w| detected_joined.contains(&w.to_ascii_lowercase()));
    let level = detected_set
        .iter()
        .any(|d| d.replace('o', "0").replace('i', "1") == expected.level.to_ascii_lowercase());
    let cost = expected
        .cost
        .map(|c| detected_set.iter().any(|d| d == &c.to_string()))
        .unwrap_or(true);

    let mut name_hits = 0;
    for item in &expected.items {
        if item
            .name
            .split_whitespace()
            .all(|w| detected_joined.contains(&w.to_ascii_lowercase()))
        {
            name_hits += 1;
        }
    }
    let mut prog_hits = 0;
    for item in &expected.items {
        let pat = format!("{}/{}", item.collected, item.needed);
        if detected_set.iter().any(|d| d == &pat) {
            prog_hits += 1;
        }
    }

    VariantScore {
        title,
        level,
        cost,
        name_hits,
        name_total: expected.items.len(),
        prog_hits,
        prog_total: expected.items.len(),
    }
}

fn run_score(data_dir: &Path, labels: &LabelsFile, engine: &dyn OcrEngine) -> Result<()> {
    let mut report = score::Report::default();
    let mut predictions: BTreeMap<String, Panel> = BTreeMap::new();

    for (name, expected) in &labels.images {
        let img_path = data_dir.join(name);
        let predicted = pipeline::run(&img_path, engine)
            .with_context(|| format!("pipeline failed for {name}"))?;
        let row = score::compare(expected, &predicted);
        println!("{}", row.render(name));
        report.add(&row);
        predictions.insert(name.clone(), predicted);
    }

    let out_path = data_dir.join("predictions.json");
    let out_file = LabelsFile {
        schema_version: 1,
        notes: "Auto-generated predictions from ocr_lab. Do not commit.".into(),
        images: predictions,
    };
    std::fs::write(&out_path, serde_json::to_string_pretty(&out_file)?)?;
    println!("\nwrote {}", out_path.display());

    println!("\n=== summary ===\n{}", report.render());
    Ok(())
}

fn run_one(
    data_dir: &Path,
    labels: &LabelsFile,
    engine: &dyn OcrEngine,
    name: &str,
) -> Result<()> {
    let img_path = data_dir.join(name);
    let predicted = pipeline::run(&img_path, engine)?;
    println!("predicted = {}", serde_json::to_string_pretty(&predicted)?);
    if let Some(expected) = labels.images.get(name) {
        let row = score::compare(expected, &predicted);
        println!("\n{}", row.render(name));
    } else {
        println!("(no ground-truth for {name})");
    }
    Ok(())
}

fn load_labels(path: &Path) -> Result<LabelsFile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

fn locate_repo_root() -> Result<PathBuf> {
    let here = std::env::current_dir()?;
    for ancestor in here.ancestors() {
        if ancestor.join("Cargo.lock").is_file() && ancestor.join("crates").is_dir() {
            return Ok(ancestor.to_path_buf());
        }
    }
    anyhow::bail!("could not locate repo root from {}", here.display())
}
