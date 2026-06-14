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
//!    softmax over the embedded 6625-class dict. Lines are width-sorted and
//!    recognized in batches ([`REC_BATCH`]) in one ONNX call each, not one call
//!    per line — a box/stash frame has 40-80 lines and the per-line dispatch
//!    dominated latency.
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
/// How many line crops to recognize per ONNX rec call. Crops are width-sorted
/// then chunked, so a batch holds similar widths and wastes little compute on
/// padding (RapidOCR's `rec_batch_num`). Batching collapses the dozens of tiny
/// per-line rec dispatches a box/stash frame used to issue.
const REC_BATCH: usize = 8;
/// Minimum rec input width. Each batch pads to its widest crop but never below
/// this: very narrow names (e.g. "WD-40") decode more reliably with the canonical
/// ~320 px context the model was trained on — padding a 2-char crop to only its
/// own ~60 px flipped a borderline glyph (d→o). The dispatch + thread wins below
/// don't depend on the padding width, so keeping the floor costs ~nothing.
const REC_MIN_W: u32 = 320;
/// Drop a recognized line when its mean per-glyph confidence is below this
/// (RapidOCR's global `text_score`).
const TEXT_SCORE: f32 = 0.5;
/// Intra-op threads for the det/rec sessions. Detection (one large conv) is the
/// per-frame bottleneck and scales with threads, so we give it about half the
/// machine's cores — enough to cut latency materially while leaving the other
/// half for the VR compositor + game (OCR runs in bursts while the user pauses
/// to scan, not every frame). Clamped to [2, 8]: 8 is past DBNet's useful
/// scaling here, and 2 keeps a low-core machine from over-subscribing.
fn intra_threads() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores / 2).clamp(2, 8)
}

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
        let threads = intra_threads();
        let build = |model: &[u8], what: &str| -> Result<Session> {
            let mut b = Session::builder().map_err(|e| anyhow::anyhow!("{what}: builder: {e}"))?;
            b = b
                .with_intra_threads(threads)
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
    let t_det = std::time::Instant::now();
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
    let det_ms = t_det.elapsed().as_secs_f32() * 1000.0;

    // --- recognition: batch the line crops through rec ---
    // One ONNX rec call per detected line dominated latency (40-80 tiny runs per
    // box/stash frame, each paying full dispatch + thread fan-out overhead).
    // Batching mirrors RapidOCR's `rec_batch_num`: crop every line from the
    // FULL-RES source (glyph quality unchanged — det only locates), sort by
    // width so similar widths share a batch, and pad each batch to its own max
    // (no fixed 320 floor) so a batch wastes minimal compute on grey padding.
    let t_rec = std::time::Instant::now();
    let mut pend: Vec<(RgbImage, u32, f32, f32, f32, f32)> = Vec::new();
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
        let (resized, resized_w) = rec_resize(&crop);
        pend.push((
            resized,
            resized_w,
            x0 as f32,
            y0 as f32,
            (x1 - x0) as f32,
            (y1 - y0) as f32,
        ));
    }
    // Widest-similar crops together: sort by width, batch, pad to batch max.
    let mut order: Vec<usize> = (0..pend.len()).collect();
    order.sort_by_key(|&i| pend[i].1);
    let mut rec_runs = 0usize;
    let mut words: Vec<OcrWord> = Vec::new();
    for chunk in order.chunks(REC_BATCH) {
        let wmax = chunk
            .iter()
            .map(|&i| pend[i].1)
            .max()
            .unwrap_or(8)
            .max(REC_MIN_W);
        let crops = chunk.iter().map(|&i| (&pend[i].0, pend[i].1));
        let lines = recognize_batch(&mut eng, crops, wmax)?;
        rec_runs += 1;
        for (&i, line) in chunk.iter().zip(lines) {
            if let Some(line) = line {
                let (_, _, x, y, w, h) = pend[i];
                words.extend(split_words(&line, x, y, w, h));
            }
        }
    }
    let rec_ms = t_rec.elapsed().as_secs_f32() * 1000.0;

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
        det_ms,
        rec_ms,
        rec_runs,
        "PP-OCR finished"
    );
    // Opt-in measurement (env OCR_TIMING) so per-frame det/rec latency is visible
    // without enabling debug logging — used to size the rec-batching work (#182
    // follow-up). Cheap: one env read per frame.
    if std::env::var_os("OCR_TIMING").is_some() {
        eprintln!(
            "OCR_TIMING det={det_ms:.0}ms rec={rec_ms:.0}ms rec_runs={rec_runs} total={:.0}ms words={}",
            det_ms + rec_ms,
            words.len()
        );
    }
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
/// into words: `chars[i]` was decoded at column `cols[i]` of `total_cols` over a
/// rec input of `padded_w` px, of which the crop's real content occupies the
/// leftmost `resized_w` px (the rest is right-padding).
struct RecLine {
    chars: Vec<String>,
    cols: Vec<usize>,
    total_cols: usize,
    resized_w: u32,
    padded_w: u32,
}

/// Resize one crop to the rec input height ([`REC_H`]), width proportional
/// (min 8 px). Returns the resized image and its width.
fn rec_resize(crop: &RgbImage) -> (RgbImage, u32) {
    let (cw, ch) = crop.dimensions();
    let ratio = cw as f32 / ch.max(1) as f32;
    let resized_w = ((REC_H as f32 * ratio).ceil() as u32).max(8);
    (
        image::imageops::resize(crop, resized_w, REC_H, FilterType::Triangle),
        resized_w,
    )
}

/// Run rec on a batch of pre-resized crops in ONE ONNX call. Every crop is
/// right-padded to `wmax` (0.0 == the normalized 127.5 grey, matching RapidOCR's
/// zero pad). Returns one `Option<RecLine>` per input, in input order (`None` =
/// empty or below [`TEXT_SCORE`]).
fn recognize_batch<'a>(
    eng: &mut Engine,
    crops: impl Iterator<Item = (&'a RgbImage, u32)>,
    wmax: u32,
) -> Result<Vec<Option<RecLine>>> {
    let crops: Vec<(&RgbImage, u32)> = crops.collect();
    let b = crops.len();
    if b == 0 {
        return Ok(Vec::new());
    }
    let n = (wmax * REC_H) as usize;
    let mut chw = vec![0f32; b * 3 * n];
    for (bi, (crop, _)) in crops.iter().enumerate() {
        let base = bi * 3 * n;
        for (x, y, px) in crop.enumerate_pixels() {
            let i = (y * wmax + x) as usize;
            let [r, g, bl] = px.0;
            chw[base + i] = bl as f32 / 127.5 - 1.0;
            chw[base + n + i] = g as f32 / 127.5 - 1.0;
            chw[base + 2 * n + i] = r as f32 / 127.5 - 1.0;
        }
    }

    let input = ort::value::Tensor::from_array(([b, 3, REC_H as usize, wmax as usize], chw))
        .context("rec batch tensor")?;
    let outputs = eng.rec.run(ort::inputs!["x" => input]).context("rec run")?;
    let (shape, data) = outputs[0]
        .try_extract_tensor::<f32>()
        .context("rec output")?;
    // [B, T, C]
    let t_len = shape[1] as usize;
    let classes = shape[2] as usize;

    let mut out = Vec::with_capacity(b);
    for (bi, &(_, resized_w)) in crops.iter().enumerate() {
        let slice = &data[bi * t_len * classes..(bi + 1) * t_len * classes];
        out.push(ctc_decode(
            slice, t_len, classes, &eng.dict, resized_w, wmax,
        ));
    }
    Ok(out)
}

/// Greedy CTC-decode one `[T, C]` logit slice into a [`RecLine`]. Returns `None`
/// when nothing decodes or the mean per-glyph confidence is below [`TEXT_SCORE`].
fn ctc_decode(
    data: &[f32],
    t_len: usize,
    classes: usize,
    dict: &[String],
    resized_w: u32,
    padded_w: u32,
) -> Option<RecLine> {
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
            if let Some(glyph) = dict.get(best) {
                chars.push(glyph.clone());
                cols.push(t);
                confs.push(best_p);
            }
        }
        prev = best;
    }
    if chars.is_empty() {
        return None;
    }
    let mean = confs.iter().sum::<f32>() / confs.len() as f32;
    if mean < TEXT_SCORE {
        return None;
    }
    Some(RecLine {
        chars,
        cols,
        total_cols: t_len,
        resized_w,
        padded_w,
    })
}

/// Split a recognized line into word boxes. Each CTC column maps to an x slice
/// of the (padded) recognition input; glyphs only occupy the unpadded
/// `resized_w`, so a column's fraction across that span scales linearly back
/// onto the source crop `x..x+w`. The y/height are the line's — downstream
/// clustering only needs horizontal precision.
fn split_words(line: &RecLine, x: f32, y: f32, w: f32, h: f32) -> Vec<OcrWord> {
    let padded_w = line.padded_w as f32;
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
