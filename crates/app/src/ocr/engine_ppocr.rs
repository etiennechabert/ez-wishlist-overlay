//! PP-OCRv4 (PaddleOCR) detection + recognition on ONNX Runtime — the OCR
//! engine behind [`crate::ocr::engine::recognize_image`] (issue #181).
//!
//! Why not `Windows.Media.Ocr`: the WinRT engine is a closed document-OCR
//! whose text-line detector deterministically suppresses short names next to
//! busy item icons (a RAM stick's chip rows read as a garbage "text line" and
//! the real `RAM` print below dies with it — 4/17 RAM tiles captured). PP-OCR's
//! segmentation detector (DBNet) has no such failure mode; measured on the
//! fixture library it lifts the stash scan 178→195+/200 and reads every
//! previously-unreadable `#hard` unit tile. It also runs fully local (models
//! embedded below, CPU-only) and needs no OS language pack.
//!
//! The pipeline mirrors RapidOCR's defaults, which the offline spike validated
//! against the fixture corpus:
//!
//! 1. **det** — resize (short side ≥ 736, long side ≤ 2000, /32 aligned),
//!    `(x/127.5 - 1)` normalize, BGR CHW → DBNet → text-probability map.
//! 2. **DB postprocess** — binarize at 0.3, 2×2 dilate, 8-connected
//!    components, mean-probability box score ≥ 0.5, unclip each box by
//!    `area·1.6/perimeter` (DB predicts shrunken text kernels; unclip restores
//!    the full extent), map back to source pixel space.
//! 3. **rec** — crop each line from the *original* image (full glyph
//!    resolution), resize to h=48 / w≥320 padded, same normalize → CTC
//!    softmax over the embedded 6625-class dict.
//! 4. **decode** — greedy CTC collapse, drop lines under the 0.5 text score,
//!    then split each line into **words** at dict spaces, deriving per-word x
//!    ranges from the CTC column indices. Downstream consumers
//!    (`anchor::detect_panel` token windows, `box_scan::read_tiles` tile
//!    clustering) were built on the WinRT engine's word granularity, so the
//!    adapter preserves it.

use crate::ocr::{OcrRect, OcrWord};
use anyhow::{Context, Result};
use image::{imageops::FilterType, DynamicImage, RgbImage};
use once_cell::sync::OnceCell;
use ort::session::Session;
use parking_lot::Mutex;

static DET_MODEL: &[u8] = include_bytes!("../assets/ocr/ch_PP-OCRv4_det_infer.onnx");
static REC_MODEL: &[u8] = include_bytes!("../assets/ocr/ch_PP-OCRv4_rec_infer.onnx");
/// Microsoft's official ONNX Runtime (MIT). Embedded and extracted to the app
/// data dir at first init because `ort` is built `load-dynamic`: its prebuilt
/// *static* libs are /MD and clash with our `+crt-static` binary (see
/// Cargo.toml), while a dll keeps its own CRT behind the C ABI.
static ORT_DLL: &[u8] = include_bytes!("../assets/ocr/onnxruntime.dll");
/// Bump together with the embedded dll — the versioned filename keeps an
/// in-use older dll from being overwritten across app updates.
const ORT_DLL_VERSION: &str = "1.24.2";
/// Recognition dictionary, extracted from the rec model's own `character`
/// metadata (one entry per line; CTC class `i+1` = line `i`). Class 0 is the
/// CTC blank and the last class (`len+1`) is the space appended at runtime.
static REC_DICT: &str = include_str!("../assets/ocr/ppocr_keys_v4.txt");

// --- det parameters (RapidOCR config.yaml, validated by the #181 spike) ---
/// Upscale so the short side is at least this (limit_type "min").
const DET_LIMIT_SIDE: u32 = 736;
/// Downscale first if the long side exceeds this (global max_side_len).
const DET_MAX_SIDE: u32 = 2000;
/// Binarization threshold on the probability map.
const DB_THRESH: f32 = 0.3;
/// Minimum mean probability over a component's bbox for it to be a text box.
const DB_BOX_THRESH: f32 = 0.5;
/// DB unclip ratio: boxes grow by `area * ratio / perimeter` per side.
const DB_UNCLIP_RATIO: f32 = 1.6;
/// Components with a side smaller than this (in det-map pixels) are noise.
const DB_MIN_SIZE: u32 = 3;

// --- rec parameters ---
/// Recognition input height; width scales proportionally.
const REC_H: u32 = 48;
/// Minimum (padded) recognition width — matches RapidOCR, whose batch padding
/// floors the width at the canonical 320 even for narrow crops.
const REC_MIN_W: u32 = 320;
/// Drop a recognized line when its mean per-glyph confidence is below this
/// (RapidOCR's global `text_score`).
const TEXT_SCORE: f32 = 0.5;
/// Cap intra-op threads: OCR runs on one background worker and must stay a
/// good neighbour to the VR compositor, so we trade a little latency for a
/// bounded CPU footprint instead of letting ORT fan out across every core.
const INTRA_THREADS: usize = 4;

struct Engine {
    det: Session,
    rec: Session,
    /// CTC class index → glyph. `dict[0]` is unused (blank); the trailing
    /// entry is the space class.
    dict: Vec<String>,
}

static ENGINE: OnceCell<Mutex<Engine>> = OnceCell::new();

fn engine() -> Result<&'static Mutex<Engine>> {
    ENGINE.get_or_try_init(|| {
        let dll = extract_ort_dylib()?;
        ort::init_from(dll.to_string_lossy())
            .commit()
            .map_err(|e| anyhow::anyhow!("load onnxruntime from {}: {e}", dll.display()))?;
        // `ort`'s builder errors aren't Send+Sync (and carry a different
        // generic per stage), so they can't ride anyhow's `context`; flatten
        // each step to a string at the boundary.
        let build = |model: &[u8], what: &str| -> Result<Session> {
            let mut b = Session::builder().map_err(|e| anyhow::anyhow!("{what}: builder: {e}"))?;
            b = b
                .with_intra_threads(INTRA_THREADS)
                .map_err(|e| anyhow::anyhow!("{what}: threads: {e}"))?;
            b.commit_from_memory(model)
                .map_err(|e| anyhow::anyhow!("{what}: load: {e}"))
        };
        let det = build(DET_MODEL, "load PP-OCR det model")?;
        let rec = build(REC_MODEL, "load PP-OCR rec model")?;
        // Class 0 is the CTC blank; classes 1..=N map to dict lines; class
        // N+1 is the space (PaddleOCR `use_space_char`).
        let mut dict: Vec<String> = vec![String::new()];
        dict.extend(REC_DICT.lines().map(str::to_owned));
        dict.push(" ".to_owned());
        Ok(Mutex::new(Engine { det, rec, dict }))
    })
}

/// Write the embedded onnxruntime.dll where `LoadLibrary` can reach it —
/// the same per-user data dir `persist.rs` keeps state.json in, falling back
/// to the OS temp dir if the profile is unwritable. Skips the write when a
/// same-size copy already exists (it may be mapped by a running instance;
/// the versioned name keeps cross-version clobbering impossible).
fn extract_ort_dylib() -> Result<std::path::PathBuf> {
    let dir = directories::ProjectDirs::from("com", "etienneb", "ez-wishlist-overlay")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("onnxruntime-{ORT_DLL_VERSION}.dll"));
    let stale = match std::fs::metadata(&path) {
        Ok(m) => m.len() != ORT_DLL.len() as u64,
        Err(_) => true,
    };
    if stale {
        std::fs::write(&path, ORT_DLL)
            .with_context(|| format!("extract onnxruntime.dll to {}", path.display()))?;
    }
    Ok(path)
}

/// Run PP-OCR on an already-decoded image. Returns word-level boxes in source
/// pixel coordinates — the same contract the WinRT engine had.
pub fn recognize_image(img: &DynamicImage) -> Result<Vec<OcrWord>> {
    let rgb = img.to_rgb8();
    let (src_w, src_h) = rgb.dimensions();
    if src_w == 0 || src_h == 0 {
        anyhow::bail!("zero-sized image cannot be OCR'd");
    }

    let mut eng = engine()?.lock();

    // --- detection ---
    let (det_input, det_w, det_h, scale) = det_preprocess(&rgb);
    let prob = {
        let input = ort::value::Tensor::from_array((
            [1usize, 3, det_h as usize, det_w as usize],
            det_input,
        ))
        .context("det input tensor")?;
        let outputs = eng.det.run(ort::inputs!["x" => input]).context("det run")?;
        let (shape, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .context("det output")?;
        debug_assert_eq!(shape.len(), 4);
        data.to_vec()
    };

    let boxes = db_postprocess(&prob, det_w, det_h);

    // --- recognition, line by line, then word splitting ---
    let mut words: Vec<OcrWord> = Vec::new();
    for b in boxes {
        // Map the det-space box back to source pixels and clamp.
        let x0 = ((b.x as f32 / scale).floor().max(0.0) as u32).min(src_w - 1);
        let y0 = ((b.y as f32 / scale).floor().max(0.0) as u32).min(src_h - 1);
        let x1 = (((b.x + b.w) as f32 / scale).ceil() as u32).min(src_w);
        let y1 = (((b.y + b.h) as f32 / scale).ceil() as u32).min(src_h);
        if x1 <= x0 + 2 || y1 <= y0 + 2 {
            continue;
        }
        let crop = image::imageops::crop_imm(&rgb, x0, y0, x1 - x0, y1 - y0).to_image();
        let Some(line) = recognize_line(&mut eng, &crop)? else {
            continue;
        };
        words.extend(split_words(
            &line,
            x0 as f32,
            y0 as f32,
            (x1 - x0) as f32,
            (y1 - y0) as f32,
        ));
    }

    // Reading order: top-to-bottom, left-to-right — matches the order the
    // WinRT engine emitted, which `anchor`'s sliding token windows assume.
    // Rows are quantized to 10 px buckets rather than compared pairwise with
    // a tolerance: a pairwise "same row if |Δy| < ε" predicate is not a total
    // order (it's intransitive), which std's sort rejects at runtime.
    words.sort_by(|a, b| {
        let row_a = (a.rect.y / 10.0).round() as i64;
        let row_b = (b.rect.y / 10.0).round() as i64;
        row_a
            .cmp(&row_b)
            .then(a.rect.x.total_cmp(&b.rect.x))
            .then(a.rect.y.total_cmp(&b.rect.y))
    });

    tracing::debug!(
        width = src_w,
        height = src_h,
        words = words.len(),
        "PP-OCR finished"
    );
    Ok(words)
}

/// Det resize + normalize: short side ≥ [`DET_LIMIT_SIDE`], long side ≤
/// [`DET_MAX_SIDE`], both dimensions /32-aligned, `(x/127.5 - 1)` in **BGR**
/// channel order (PaddleOCR models are trained on cv2's BGR). Returns the CHW
/// buffer, its dimensions, and the applied scale (det px per source px).
fn det_preprocess(rgb: &RgbImage) -> (Vec<f32>, u32, u32, f32) {
    let (w, h) = rgb.dimensions();
    let mut scale = 1.0f32;
    let long = w.max(h) as f32;
    let short = w.min(h) as f32;
    if long * scale > DET_MAX_SIDE as f32 {
        scale = DET_MAX_SIDE as f32 / long;
    }
    if short * scale < DET_LIMIT_SIDE as f32 {
        scale = DET_LIMIT_SIDE as f32 / short;
    }

    // /32 alignment (round, min 32) — DBNet's FPN needs it.
    let align = |v: f32| -> u32 { (((v / 32.0).round() as u32).max(1)) * 32 };
    let det_w = align(w as f32 * scale);
    let det_h = align(h as f32 * scale);

    let resized = if (det_w, det_h) == (w, h) {
        rgb.clone()
    } else {
        image::imageops::resize(rgb, det_w, det_h, FilterType::Triangle)
    };

    // The alignment makes the effective scale slightly anisotropic; track the
    // dominant axis ratio for the inverse mapping (the residual error is sub-
    // pixel at our sizes and the unclip margin absorbs it).
    let eff_scale = det_w as f32 / w as f32;

    let n = (det_w * det_h) as usize;
    let mut chw = vec![0f32; 3 * n];
    for (i, px) in resized.pixels().enumerate() {
        let [r, g, b] = px.0;
        // BGR plane order.
        chw[i] = b as f32 / 127.5 - 1.0;
        chw[n + i] = g as f32 / 127.5 - 1.0;
        chw[2 * n + i] = r as f32 / 127.5 - 1.0;
    }
    (chw, det_w, det_h, eff_scale)
}

/// An axis-aligned text-line box in det-map pixel space.
struct DetBox {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// DB postprocess: binarize → dilate → connected components → score filter →
/// unclip. Component shapes are approximated by their bounding boxes — the
/// game UI's text is horizontal, so the bbox of a DB kernel *is* its
/// min-area rect for our purposes.
fn db_postprocess(prob: &[f32], w: u32, h: u32) -> Vec<DetBox> {
    let (wi, hi) = (w as usize, h as usize);
    debug_assert!(prob.len() >= wi * hi);

    // Binarize + 2×2 dilate (RapidOCR `use_dilation: true`).
    let bin: Vec<bool> = (0..wi * hi).map(|i| prob[i] > DB_THRESH).collect();
    let mut dil = vec![false; wi * hi];
    for y in 0..hi {
        for x in 0..wi {
            let mut on = bin[y * wi + x];
            if !on && x > 0 {
                on = bin[y * wi + x - 1];
            }
            if !on && y > 0 {
                on = bin[(y - 1) * wi + x];
            }
            if !on && x > 0 && y > 0 {
                on = bin[(y - 1) * wi + x - 1];
            }
            dil[y * wi + x] = on;
        }
    }

    // 8-connected components via BFS.
    let mut seen = vec![false; wi * hi];
    let mut boxes = Vec::new();
    let mut queue: Vec<usize> = Vec::new();
    for start in 0..wi * hi {
        if !dil[start] || seen[start] {
            continue;
        }
        seen[start] = true;
        queue.clear();
        queue.push(start);
        let (mut min_x, mut max_x) = (start % wi, start % wi);
        let (mut min_y, mut max_y) = (start / wi, start / wi);
        let mut head = 0;
        while head < queue.len() {
            let p = queue[head];
            head += 1;
            let (px, py) = (p % wi, p / wi);
            min_x = min_x.min(px);
            max_x = max_x.max(px);
            min_y = min_y.min(py);
            max_y = max_y.max(py);
            let x_lo = px.saturating_sub(1);
            let x_hi = (px + 1).min(wi - 1);
            let y_lo = py.saturating_sub(1);
            let y_hi = (py + 1).min(hi - 1);
            for ny in y_lo..=y_hi {
                for nx in x_lo..=x_hi {
                    let q = ny * wi + nx;
                    if dil[q] && !seen[q] {
                        seen[q] = true;
                        queue.push(q);
                    }
                }
            }
        }

        let bw = (max_x - min_x + 1) as u32;
        let bh = (max_y - min_y + 1) as u32;
        if bw < DB_MIN_SIZE || bh < DB_MIN_SIZE {
            continue;
        }

        // Box score: mean probability over the component's bbox ("fast" mode).
        let mut sum = 0f32;
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                sum += prob[y * wi + x];
            }
        }
        if (sum / (bw * bh) as f32) < DB_BOX_THRESH {
            continue;
        }

        // Unclip: DB predicts a shrunken text kernel; grow it back.
        let area = (bw * bh) as f32;
        let perim = 2.0 * (bw + bh) as f32;
        let dist = (area * DB_UNCLIP_RATIO / perim).ceil() as i64;
        let x0 = (min_x as i64 - dist).max(0) as u32;
        let y0 = (min_y as i64 - dist).max(0) as u32;
        let x1 = ((max_x as i64 + 1 + dist) as u32).min(w);
        let y1 = ((max_y as i64 + 1 + dist) as u32).min(h);
        boxes.push(DetBox {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        });
    }
    boxes
}

/// One recognized text line plus the CTC column geometry needed to split it
/// into words: `chars[i]` was decoded at column `cols[i]` of `total_cols`
/// over a crop resized to `resized_w` px wide.
struct RecLine {
    chars: Vec<String>,
    cols: Vec<usize>,
    total_cols: usize,
    resized_w: u32,
}

/// Recognize one cropped line. Returns `None` when the mean confidence falls
/// below [`TEXT_SCORE`] or nothing decodes.
fn recognize_line(eng: &mut Engine, crop: &RgbImage) -> Result<Option<RecLine>> {
    let (cw, ch) = crop.dimensions();
    let ratio = cw as f32 / ch as f32;
    let resized_w = ((REC_H as f32 * ratio).ceil() as u32).max(8);
    let padded_w = resized_w.max(REC_MIN_W);
    let resized = image::imageops::resize(crop, resized_w, REC_H, FilterType::Triangle);

    let n = (padded_w * REC_H) as usize;
    // Pad value 0.0 == the normalized 127.5 grey, matching RapidOCR's zero pad
    // after normalization.
    let mut chw = vec![0f32; 3 * n];
    for (x, y, px) in resized.enumerate_pixels() {
        let i = (y * padded_w + x) as usize;
        let [r, g, b] = px.0;
        chw[i] = b as f32 / 127.5 - 1.0;
        chw[n + i] = g as f32 / 127.5 - 1.0;
        chw[2 * n + i] = r as f32 / 127.5 - 1.0;
    }

    let input =
        ort::value::Tensor::from_array(([1usize, 3, REC_H as usize, padded_w as usize], chw))
            .context("rec input tensor")?;
    let outputs = eng.rec.run(ort::inputs!["x" => input]).context("rec run")?;
    let (shape, data) = outputs[0]
        .try_extract_tensor::<f32>()
        .context("rec output")?;
    // [1, T, C]
    let t_len = shape[1] as usize;
    let classes = shape[2] as usize;

    let mut chars = Vec::new();
    let mut cols = Vec::new();
    let mut confs = Vec::new();
    let mut prev = 0usize;
    for t in 0..t_len {
        let row = &data[t * classes..(t + 1) * classes];
        let (mut best, mut best_p) = (0usize, f32::MIN);
        for (c, &p) in row.iter().enumerate() {
            if p > best_p {
                best = c;
                best_p = p;
            }
        }
        if best != 0 && best != prev {
            if let Some(glyph) = eng.dict.get(best) {
                chars.push(glyph.clone());
                cols.push(t);
                confs.push(best_p);
            }
        }
        prev = best;
    }
    if chars.is_empty() {
        return Ok(None);
    }
    let mean = confs.iter().sum::<f32>() / confs.len() as f32;
    if mean < TEXT_SCORE {
        return Ok(None);
    }
    Ok(Some(RecLine {
        chars,
        cols,
        total_cols: t_len,
        resized_w,
    }))
}

/// Split a recognized line into word boxes. Each CTC column maps to an x slice
/// of the (padded) recognition input; glyphs only occupy the unpadded
/// `resized_w`, so a column's fraction across that span scales linearly back
/// onto the source crop `x..x+w`. The y/height are the line's — downstream
/// clustering only needs horizontal precision.
fn split_words(line: &RecLine, x: f32, y: f32, w: f32, h: f32) -> Vec<OcrWord> {
    let padded_w = line.resized_w.max(REC_MIN_W) as f32;
    let col_px = padded_w / line.total_cols.max(1) as f32;
    let to_src = |col: usize, end: bool| -> f32 {
        let cx = (col as f32 + if end { 1.0 } else { 0.0 }) * col_px;
        let frac = (cx / line.resized_w as f32).clamp(0.0, 1.0);
        x + frac * w
    };

    let mut out = Vec::new();
    let mut word = String::new();
    let (mut start_col, mut end_col) = (0usize, 0usize);
    let flush = |word: &mut String, start: usize, end: usize, out: &mut Vec<OcrWord>| {
        if word.is_empty() {
            return;
        }
        let x0 = to_src(start, false);
        let x1 = to_src(end, true);
        out.push(OcrWord {
            text: std::mem::take(word),
            rect: OcrRect {
                x: x0,
                y,
                width: (x1 - x0).max(1.0),
                height: h,
            },
        });
    };
    for (i, glyph) in line.chars.iter().enumerate() {
        if glyph == " " {
            flush(&mut word, start_col, end_col, &mut out);
            continue;
        }
        if word.is_empty() {
            start_col = line.cols[i];
        }
        end_col = line.cols[i];
        word.push_str(glyph);
    }
    flush(&mut word, start_col, end_col, &mut out);
    out
}
