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

/// One row of `<UpgradeId>.label.txt` ground truth. Parsed from the
/// hand-labelled sibling file the user maintains for every fixture in
/// `hideout_screenshots_native/`. Each label line is
/// `<item_id>  <owned>/<needed>` (whitespace-separated; `#` comments
/// and blank lines ignored).
#[derive(Debug, Clone)]
pub struct LabelEntry {
    pub item_id: String,
    pub owned: u32,
    pub needed: u32,
}

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
    /// Optional hand-labelled ground truth from
    /// `<source_stem>.label.txt`. When present, the SUMMARY block
    /// prints per-cell expected vs read and an aggregate accuracy.
    pub labels: Option<Vec<LabelEntry>>,
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
/// `…/20260527203320_194572500.ocr-debug.220347.txt`
///
/// The trailing `HHMMSS` is when **this dump** was written, separate
/// from any timestamp the source filename might already carry. Useful
/// for the fixture tests where the source name is just `BookcaseLv1.webp`
/// — without it, regenerated debug files would silently overwrite the
/// previous run and you couldn't tell if a file on disk reflects the
/// latest pipeline behaviour or a stale build's.
/// Sibling-file path for the per-cell binarised strip PNG that the
/// template matcher actually consumes. e.g.
/// `…/<screenshot>.cell<idx>.<HHMMSS>.png`
///
/// Returns `None` if the source path has no file stem (shouldn't
/// happen in normal pipelines but the helper stays defensive). The
/// `HHMMSS` suffix tracks dump freshness the same way
/// [`debug_path_for`] does — successive captures with the same idx
/// stay distinguishable on disk.
pub fn cell_strip_path_for(source: &Path, idx: usize) -> Option<PathBuf> {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let hhmmss = now
        .format(time::macros::format_description!("[hour][minute][second]"))
        .unwrap_or_else(|_| "000000".into());
    let mut path = source.to_path_buf();
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    path.set_file_name(format!("{stem}.cell{idx}.{hhmmss}.png"));
    Some(path)
}

pub fn debug_path_for(source: &Path) -> PathBuf {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let hhmmss = now
        .format(time::macros::format_description!("[hour][minute][second]"))
        .unwrap_or_else(|_| "000000".into());
    let mut path = source.to_path_buf();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "screenshot".into());
    path.set_file_name(format!("{stem}.ocr-debug.{hhmmss}.txt"));
    path
}

/// Load `<source_stem>.label.txt` next to the source PNG if it
/// exists. The format (per the hand-curated files under
/// `hideout_screenshots_native/`) is:
///
/// ```text
/// # ModuleNameLvN — ground truth (owned / needed)
/// item_id_a          0/5
/// item_id_b          3/6
/// ```
///
/// Lines starting with `#` and blank lines are ignored. Malformed
/// rows are skipped (logged via `tracing::warn`) — we'd rather drop a
/// row than poison the whole dump's accuracy count.
///
/// Returns `None` when the file is missing so debug dumps for
/// ad-hoc captures (no paired label) still render cleanly without
/// the "ground truth" section.
pub fn load_labels(source: &Path) -> Option<Vec<LabelEntry>> {
    let mut p = source.to_path_buf();
    let stem = p.file_stem()?.to_string_lossy().into_owned();
    p.set_file_name(format!("{stem}.label.txt"));
    let text = std::fs::read_to_string(&p).ok()?;
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut tokens = trimmed.split_whitespace();
        let item_id = match tokens.next() {
            Some(s) => s.to_string(),
            None => continue,
        };
        let xy = match tokens.next() {
            Some(s) => s,
            None => {
                tracing::warn!(file = %p.display(), line = %trimmed, "label line missing X/Y");
                continue;
            }
        };
        let Some((owned_s, needed_s)) = xy.split_once('/') else {
            tracing::warn!(file = %p.display(), line = %trimmed, "label line missing slash");
            continue;
        };
        let owned: u32 = match owned_s.parse() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(file = %p.display(), line = %trimmed, "label owned not numeric");
                continue;
            }
        };
        let needed: u32 = match needed_s.parse() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(file = %p.display(), line = %trimmed, "label needed not numeric");
                continue;
            }
        };
        out.push(LabelEntry {
            item_id,
            owned,
            needed,
        });
    }
    Some(out)
}

/// Remove any prior `<stem>.ocr-debug.*.txt` files next to the source
/// PNG. Each pipeline run produces a freshly-timestamped sibling
/// dump; without this sweep, fixture-test reruns would leave a trail
/// of files that quickly clutters the directory (the user can't tell
/// at a glance which one reflects the current build).
pub fn purge_prior_dumps(source: &Path) {
    let Some(dir) = source.parent() else {
        return;
    };
    let stem = match source.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return,
    };
    let txt_prefix = format!("{stem}.ocr-debug.");
    let cell_prefix = format!("{stem}.cell");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let is_debug_dump = name_str.starts_with(&txt_prefix) && name_str.ends_with(".txt");
        let is_cell_strip = name_str.starts_with(&cell_prefix) && name_str.ends_with(".png");
        if is_debug_dump || is_cell_strip {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Look up a label entry by `item_id`. Item-IDs are unique within an
/// upgrade's requirement list (each item appears at most once per
/// panel), so an exact-match lookup is enough.
fn find_label<'a>(labels: &'a [LabelEntry], item_id: &str) -> Option<&'a LabelEntry> {
    labels.iter().find(|l| l.item_id == item_id)
}

pub fn write_text(dump: &OcrDebugDump<'_>, path: &Path) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "=== OCR DEBUG DUMP ===")?;
    writeln!(f, "Source: {}", dump.source_path.display())?;
    writeln!(f, "Image:  {}×{}", dump.img_w, dump.img_h)?;
    writeln!(f)?;

    // Headline summary up front so the user sees the verdict without
    // scrolling. The detailed sections below stay as before.
    writeln!(f, "=== SUMMARY ===")?;
    match &dump.resolution {
        Resolution::Resolved {
            upgrade_id,
            module_name,
            upgrade_level,
        } => {
            writeln!(
                f,
                "  upgrade:  {module_name} Lv {upgrade_level}  (id={upgrade_id})"
            )?;
        }
        Resolution::Unresolved => {
            writeln!(
                f,
                "  upgrade:  UNRESOLVED (no module.name passed strict-match)"
            )?;
        }
    }
    let total = dump.cells.len();
    let applied = dump
        .cells
        .iter()
        .filter(|c| c.parsed_owned.is_some())
        .count();
    writeln!(
        f,
        "  cells:    {applied}/{total} read, {} unread (existing counts preserved)",
        total - applied,
    )?;
    // OCR-vs-ground-truth accuracy. We compare against the hand
    // labels for both `owned` AND `needed` (a wrong slash position
    // shows up as both being off, so it's worth checking both). A
    // cell is correct only when both numbers match the label.
    if let Some(labels) = dump.labels.as_deref() {
        let mut correct = 0usize;
        let mut labelled = 0usize;
        for cell in &dump.cells {
            if let Some(label) = find_label(labels, cell.item_id) {
                labelled += 1;
                if cell.parsed_owned == Some(label.owned) && cell.needed == label.needed {
                    correct += 1;
                }
            }
        }
        writeln!(
            f,
            "  accuracy: {correct}/{labelled} cells match ground truth",
        )?;
    }
    for cell in &dump.cells {
        let label_str = match dump.labels.as_deref() {
            Some(labels) => find_label(labels, cell.item_id)
                .map(|l| format!("{}/{}", l.owned, l.needed))
                .unwrap_or_else(|| "—".into()),
            None => "—".into(),
        };
        let read_str = match cell.parsed_owned {
            Some(owned) => format!("{owned}/{}", cell.needed),
            None => format!("UNREAD/{}", cell.needed),
        };
        let mark = match (cell.parsed_owned, dump.labels.as_deref()) {
            (Some(owned), Some(labels)) => match find_label(labels, cell.item_id) {
                Some(l) if l.owned == owned && l.needed == cell.needed => "✓",
                Some(_) => "✗",
                None => " ",
            },
            _ => " ",
        };
        if cell.parsed_owned.is_none() {
            writeln!(
                f,
                "    {mark} [{}] {} ({})  read={read_str}  label={label_str}  recognised={:?}",
                cell.index, cell.item_name, cell.item_id, cell.recognised,
            )?;
        } else {
            writeln!(
                f,
                "    {mark} [{}] {} ({})  read={read_str}  label={label_str}",
                cell.index, cell.item_name, cell.item_id,
            )?;
        }
    }
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
