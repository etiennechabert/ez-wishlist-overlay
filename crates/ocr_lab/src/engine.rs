//! OCR engine abstraction. Default impl shells out to the `tesseract` CLI.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use image::{DynamicImage, ImageFormat};

/// One word recognized by the OCR engine, with pixel-space bounding box and confidence.
#[derive(Debug, Clone)]
pub struct Word {
    pub text: String,
    pub bbox: BBox,
    pub confidence: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct BBox {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// PSM (page segmentation mode) hints we expose to the pipeline.
/// Maps to Tesseract `--psm` integers internally.
#[derive(Debug, Clone, Copy)]
pub enum Psm {
    /// Default: assume a single uniform block of text.
    Block,
    /// Single text line. Good for cost line, item-name strips.
    Line,
    /// Single word. Good for the X/Y progress fragment.
    Word,
    /// Sparse text. Good for full-screen passes where text is scattered.
    Sparse,
}

impl Psm {
    fn as_int(self) -> u8 {
        match self {
            Psm::Block => 6,
            Psm::Line => 7,
            Psm::Word => 8,
            Psm::Sparse => 11,
        }
    }
}

/// Optional character whitelist for digit-only / structured passes.
#[derive(Debug, Clone, Default)]
pub struct OcrOptions {
    pub psm: Option<Psm>,
    pub whitelist: Option<&'static str>,
}

pub trait OcrEngine: Send + Sync {
    fn name(&self) -> &'static str;
    fn recognize(&self, image: &DynamicImage, opts: &OcrOptions) -> Result<Vec<Word>>;
}

/// Picks the best engine the host has available. Tesseract CLI for now.
pub fn build_default_engine() -> Result<Box<dyn OcrEngine>> {
    let tess = TesseractCli::detect()
        .context("could not find a working `tesseract` CLI in PATH. \
                 Install via `winget install --id UB-Mannheim.TesseractOCR` on Windows, \
                 or `brew install tesseract` / `apt install tesseract-ocr` elsewhere.")?;
    Ok(Box::new(tess))
}

pub struct TesseractCli {
    exe: String,
}

impl TesseractCli {
    pub fn detect() -> Result<Self> {
        let mut candidates: Vec<String> = vec!["tesseract".to_string()];
        if cfg!(target_os = "windows") {
            candidates.push(r"C:\Program Files\Tesseract-OCR\tesseract.exe".to_string());
            candidates.push(r"C:\Program Files (x86)\Tesseract-OCR\tesseract.exe".to_string());
            if let Ok(local) = std::env::var("LOCALAPPDATA") {
                candidates.push(format!(r"{local}\Programs\Tesseract-OCR\tesseract.exe"));
            }
        }
        let mut last_err: Option<anyhow::Error> = None;
        for cand in candidates {
            match Command::new(&cand).arg("--version").output() {
                Ok(out) if out.status.success() || !out.stderr.is_empty() => {
                    let v = String::from_utf8_lossy(&out.stderr)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_string();
                    tracing::info!("found tesseract at {cand}: {v}");
                    return Ok(Self { exe: cand });
                }
                Ok(out) => {
                    last_err = Some(anyhow::anyhow!(
                        "{cand} --version returned status {}",
                        out.status
                    ));
                }
                Err(e) => {
                    last_err = Some(anyhow::Error::from(e).context(format!("exec `{cand}`")));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no tesseract candidates tried")))
    }
}

impl OcrEngine for TesseractCli {
    fn name(&self) -> &'static str {
        "tesseract-cli"
    }

    fn recognize(&self, image: &DynamicImage, opts: &OcrOptions) -> Result<Vec<Word>> {
        let tmp_dir = std::env::temp_dir();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let in_path = tmp_dir.join(format!("ocr_lab_in_{stamp}.png"));
        image
            .save_with_format(&in_path, ImageFormat::Png)
            .with_context(|| format!("saving temp image {}", in_path.display()))?;

        let psm = opts.psm.unwrap_or(Psm::Block).as_int();
        let out_stem = tmp_dir.join(format!("ocr_lab_out_{stamp}"));

        let mut cmd = Command::new(&self.exe);
        cmd.arg(&in_path)
            .arg(&out_stem)
            .args(["-l", "eng"])
            .args(["--psm", &psm.to_string()])
            .args(["--oem", "1"])
            .arg("tsv");
        if let Some(wl) = opts.whitelist {
            cmd.args(["-c", &format!("tessedit_char_whitelist={wl}")]);
        }

        let output = cmd.output().context("running tesseract")?;
        let _ = std::fs::remove_file(&in_path);
        if !output.status.success() {
            anyhow::bail!(
                "tesseract failed status={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let tsv_path = with_ext(&out_stem, "tsv");
        let tsv = std::fs::read_to_string(&tsv_path)
            .with_context(|| format!("reading {}", tsv_path.display()))?;
        let _ = std::fs::remove_file(&tsv_path);

        Ok(parse_tesseract_tsv(&tsv))
    }
}

fn with_ext(stem: &Path, ext: &str) -> std::path::PathBuf {
    let mut p = stem.to_path_buf();
    p.set_extension(ext);
    p
}

/// Tesseract TSV columns:
///   level page_num block_num par_num line_num word_num left top width height conf text
fn parse_tesseract_tsv(tsv: &str) -> Vec<Word> {
    let mut words = Vec::new();
    for line in tsv.lines().skip(1) {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 12 {
            continue;
        }
        // level 5 == individual word in Tesseract's hierarchy
        if cols[0] != "5" {
            continue;
        }
        let text = cols[11].trim();
        if text.is_empty() {
            continue;
        }
        let (Ok(x), Ok(y), Ok(w), Ok(h), Ok(conf)) = (
            cols[6].parse::<i32>(),
            cols[7].parse::<i32>(),
            cols[8].parse::<i32>(),
            cols[9].parse::<i32>(),
            cols[10].parse::<f32>(),
        ) else {
            continue;
        };
        if w <= 0 || h <= 0 {
            continue;
        }
        words.push(Word {
            text: text.to_string(),
            bbox: BBox {
                x: x.max(0) as u32,
                y: y.max(0) as u32,
                w: w as u32,
                h: h as u32,
            },
            confidence: conf,
        });
    }
    words
}
