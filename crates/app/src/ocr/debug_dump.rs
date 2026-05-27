//! Sibling-file debug dump for an OCR pipeline run.
//!
//! Writes a plain-text `<screenshot>.ocr-debug.txt` next to the source
//! PNG with every intermediate the pipeline produced — anchor box,
//! OCR'd words and their positions, resolved upgrade, per-cell strip
//! rect, raw + filtered connected components, per-template scores for
//! each kept component, the recognised string, and the parsed
//! owned-count (or the parse-failed marker).
//!
//! Gated to `cfg(debug_assertions)` callers. In release builds the
//! pipeline doesn't construct or write this, so production users
//! never pay the I/O.

use crate::ocr::anchor::BBox;
use crate::ocr::templates::KeptComponent;
use crate::ocr::OcrWord;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Snapshot of everything the pipeline saw on one run. Built up
/// incrementally as the pipeline walks the screenshot.
pub struct OcrDebugDump<'a> {
    pub source_path: &'a Path,
    pub img_w: u32,
    pub img_h: u32,
    pub anchor: BBox,
    pub words: &'a [OcrWord],
    pub current_level: u32,
    pub panel_text: &'a str,
    pub resolution: Resolution<'a>,
    pub cells: Vec<CellDebug<'a>>,
}

pub enum Resolution<'a> {
    /// Pipeline matched an upgrade in `data.json`.
    Resolved {
        upgrade_id: &'a str,
        module_name: &'a str,
        upgrade_level: u32,
    },
    /// Pipeline could not strict-match — no module name passed the
    /// fuzzy-windowed threshold against the OCR text. The current
    /// pipeline returns `Ok(None)` and bails before reaching the dump
    /// writer in this case, so this variant isn't constructed today;
    /// kept around for the path where we'd want a dump on unresolved
    /// runs too (e.g. wrong-panel diagnostics).
    #[allow(dead_code)]
    Unresolved,
}

pub struct CellDebug<'a> {
    pub index: usize,
    pub item_id: &'a str,
    pub item_name: &'a str,
    pub needed: u32,
    pub strip: BBox,
    pub raw_components: Vec<(u32, u32, u32, u32)>,
    pub kept_components: Vec<KeptComponent>,
    pub recognised: String,
    /// `Some(owned)` if `split_progress` succeeded; `None` if the
    /// pipeline kept the user's existing count intact.
    pub parsed_owned: Option<u32>,
}

/// Sibling-file path next to the source screenshot. e.g.
/// `…/20260527203320_194572500.png` →
/// `…/20260527203320_194572500.ocr-debug.txt`.
pub fn debug_path_for(source: &Path) -> PathBuf {
    let mut path = source.to_path_buf();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "screenshot".into());
    path.set_file_name(format!("{stem}.ocr-debug.txt"));
    path
}

pub fn write_text(dump: &OcrDebugDump<'_>, path: &Path) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "=== OCR DEBUG DUMP ===")?;
    writeln!(f, "Source: {}", dump.source_path.display())?;
    writeln!(f, "Image:  {}×{}", dump.img_w, dump.img_h)?;
    writeln!(f)?;

    writeln!(f, "=== ANCHOR (\"Need to submit items\") ===")?;
    writeln!(
        f,
        "  x={} y={} w={} h={}",
        dump.anchor.x, dump.anchor.y, dump.anchor.w, dump.anchor.h,
    )?;
    writeln!(f)?;

    writeln!(f, "=== RESOLUTION ===")?;
    match &dump.resolution {
        Resolution::Resolved {
            upgrade_id,
            module_name,
            upgrade_level,
        } => {
            writeln!(f, "  upgrade_id:     {upgrade_id}")?;
            writeln!(f, "  module name:    {module_name}")?;
            writeln!(f, "  upgrade level:  {upgrade_level}")?;
            writeln!(
                f,
                "  current level:  {} (parsed from LV<n>)",
                dump.current_level
            )?;
        }
        Resolution::Unresolved => {
            writeln!(
                f,
                "  UNRESOLVED — no module.name passed the strict-match threshold"
            )?;
        }
    }
    writeln!(f)?;

    writeln!(f, "=== OCR PANEL TEXT (whitespace-joined) ===")?;
    writeln!(f, "  {}", dump.panel_text)?;
    writeln!(f)?;

    writeln!(
        f,
        "=== OCR WORDS ({} total, sorted by Y then X) ===",
        dump.words.len()
    )?;
    let mut sorted: Vec<&OcrWord> = dump.words.iter().collect();
    sorted.sort_by(|a, b| {
        a.rect
            .y
            .partial_cmp(&b.rect.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.rect
                    .x
                    .partial_cmp(&b.rect.x)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    for w in &sorted {
        writeln!(
            f,
            "  y={:>5.0} x={:>5.0} w={:>4.0} h={:>3.0}  {:?}",
            w.rect.y, w.rect.x, w.rect.width, w.rect.height, w.text,
        )?;
    }
    writeln!(f)?;

    writeln!(f, "=== CELLS ({}) ===", dump.cells.len())?;
    for cell in &dump.cells {
        writeln!(f)?;
        writeln!(
            f,
            "-- Cell {}: {} ({}) needed={} --",
            cell.index, cell.item_name, cell.item_id, cell.needed,
        )?;
        writeln!(
            f,
            "  strip rect:  x={} y={} w={} h={}",
            cell.strip.x, cell.strip.y, cell.strip.w, cell.strip.h,
        )?;
        writeln!(f, "  raw components ({}):", cell.raw_components.len(),)?;
        for (x, y, w, h) in &cell.raw_components {
            writeln!(f, "    x={x:>3} y={y:>3} w={w:>3} h={h:>3}")?;
        }
        writeln!(
            f,
            "  kept after filter + row-cluster ({}):",
            cell.kept_components.len(),
        )?;
        for k in &cell.kept_components {
            // Top 3 scores so the picked winner + ties are visible.
            let top3: String = k
                .scores
                .iter()
                .take(3)
                .map(|(c, s)| format!("{c:?}={s:.3}"))
                .collect::<Vec<_>>()
                .join("  ");
            writeln!(
                f,
                "    x={:>3} y={:>3} w={:>3} h={:>3}   {}",
                k.x, k.y, k.w, k.h, top3,
            )?;
        }
        writeln!(f, "  recognised:   {:?}", cell.recognised)?;
        match cell.parsed_owned {
            Some(owned) => writeln!(f, "  parsed owned: {owned}  (applied to AppState)")?,
            None => writeln!(
                f,
                "  parsed owned: NONE  (split_progress failed — existing count preserved)"
            )?,
        }
    }
    Ok(())
}
