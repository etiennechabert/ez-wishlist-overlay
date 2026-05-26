//! OCR debug + bulk test harness.
//!
//! Two modes:
//! - **Pick image…** — run OCR + the two-pass extract on a single image
//!   from anywhere on disk (handy for ad-hoc files outside the watch dir).
//! - **Re-OCR watch dir** — iterate every non-stereo screenshot in the
//!   currently-watched Steam screenshots dir and process them through
//!   the same pipeline the live watcher uses. Lets us iterate on the
//!   extract pipeline against the existing corpus of screenshots
//!   without taking new ones in-game.
//!
//! Results show a per-file summary (key, items, items-with-progress,
//! cost) plus a click-to-expand row with raw OCR text + word boxes
//! for the most recently inspected file. No persistence here — bulk
//! re-OCR is read-only; the live watcher remains the canonical write
//! path to `wishlist.json`.

use crate::ocr;
use std::path::PathBuf;

pub struct OcrDialogState {
    /// Per-file row from the most recent bulk run, or single-image run.
    pub rows: Vec<BulkRow>,
    /// Which row is "active" — its raw OCR text shows in the detail
    /// pane. None when nothing's been processed yet.
    pub active_idx: Option<usize>,
    pub last_error: Option<String>,
}

pub struct BulkRow {
    pub path: PathBuf,
    pub outcome: BulkOutcome,
}

pub enum BulkOutcome {
    Ok {
        key: String,
        items: usize,
        with_progress: usize,
        cost: Option<u64>,
        /// First-pass raw text; expanded in the detail pane.
        raw_text: String,
        /// Word boxes from the first pass; expanded in the detail pane.
        word_boxes: Vec<String>,
        /// Per-item line for the detail pane.
        items_detail: Vec<String>,
    },
    Err(String),
}

impl Default for OcrDialogState {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            active_idx: None,
            last_error: None,
        }
    }
}

pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    state: &mut OcrDialogState,
    settings: &parking_lot::RwLock<crate::settings::Settings>,
) {
    egui::Window::new("OCR test harness")
        .open(open)
        .default_width(900.0)
        .default_height(640.0)
        .vscroll(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Pick image…").clicked() {
                    pick_and_extract(state);
                }
                let watch_dir = resolve_watch_dir(settings);
                let btn_label = match &watch_dir {
                    Some(d) => format!("Re-OCR all in: {}", d.display()),
                    None => "Re-OCR watch dir (none configured)".to_string(),
                };
                if ui
                    .add_enabled(watch_dir.is_some(), egui::Button::new(btn_label))
                    .clicked()
                {
                    if let Some(d) = watch_dir {
                        bulk_extract(state, &d);
                    }
                }
                if ui
                    .add_enabled(!state.rows.is_empty(), egui::Button::new("Copy report"))
                    .on_hover_text(
                        "Copy a paste-able summary of every row + the active row's \
                         raw OCR text + word boxes to the clipboard.",
                    )
                    .clicked()
                {
                    let report = build_report(state);
                    ctx.copy_text(report);
                }
            });

            if let Some(err) = &state.last_error {
                ui.add_space(4.0);
                ui.colored_label(egui::Color32::from_rgb(220, 100, 90), err);
            }

            ui.separator();

            if state.rows.is_empty() {
                ui.add_space(8.0);
                ui.label(
                    "Pick a single image, or hit \"Re-OCR all\" to bulk-process every \
                     screenshot in the configured Steam dir. Stereo `_vr.jpg` files are \
                     filtered out automatically.",
                );
                return;
            }

            // Results table.
            egui::ScrollArea::vertical()
                .id_salt("ocr-rows")
                .max_height(220.0)
                .show(ui, |ui| {
                    let active = state.active_idx;
                    let mut new_active: Option<usize> = None;
                    for (i, row) in state.rows.iter().enumerate() {
                        let filename = row
                            .path
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let summary = match &row.outcome {
                            BulkOutcome::Ok {
                                key,
                                items,
                                with_progress,
                                cost,
                                ..
                            } => {
                                let cost_str =
                                    cost.map(|c| format!(" · {c}¤")).unwrap_or_default();
                                format!(
                                    "✓ {filename}  →  {key}  ·  {items} items  ·  \
                                     {with_progress}/{items} progress{cost_str}"
                                )
                            }
                            BulkOutcome::Err(e) => format!("× {filename}: {e}"),
                        };
                        let selected = active == Some(i);
                        let response = ui.selectable_label(selected, summary);
                        if response.clicked() {
                            new_active = Some(i);
                        }
                    }
                    if let Some(idx) = new_active {
                        state.active_idx = Some(idx);
                    }
                });

            ui.separator();

            // Detail pane for the active row.
            let Some(idx) = state.active_idx else {
                ui.label("Click a row above to see its raw OCR output.");
                return;
            };
            let Some(row) = state.rows.get(idx) else {
                return;
            };

            match &row.outcome {
                BulkOutcome::Err(e) => {
                    ui.colored_label(egui::Color32::from_rgb(220, 100, 90), e);
                }
                BulkOutcome::Ok {
                    raw_text,
                    word_boxes,
                    items_detail,
                    ..
                } => {
                    egui::CollapsingHeader::new("Items")
                        .default_open(true)
                        .show(ui, |ui| {
                            for line in items_detail {
                                ui.label(egui::RichText::new(line).monospace());
                            }
                        });
                    egui::CollapsingHeader::new("First-pass raw text")
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("ocr-raw-text")
                                .max_height(160.0)
                                .show(ui, |ui| {
                                    let mut text = raw_text.as_str();
                                    ui.add(
                                        egui::TextEdit::multiline(&mut text)
                                            .desired_width(f32::INFINITY)
                                            .desired_rows(8)
                                            .font(egui::TextStyle::Monospace),
                                    );
                                });
                        });
                    egui::CollapsingHeader::new(format!("Word boxes ({})", word_boxes.len()))
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("ocr-word-boxes")
                                .max_height(160.0)
                                .show(ui, |ui| {
                                    for line in word_boxes {
                                        ui.label(egui::RichText::new(line).monospace().small());
                                    }
                                });
                        });
                }
            }
        });
}

/// Markdown-ish text dump of every row + the active row's detail pane.
/// Designed to be pasted straight into chat — keeps line lengths reasonable
/// and uses fenced sections so OCR text doesn't get mistaken for markup.
fn build_report(state: &OcrDialogState) -> String {
    let mut s = String::new();
    s.push_str("## OCR test harness — bulk results\n\n");
    s.push_str(&format!("Rows: {}\n\n", state.rows.len()));
    for row in &state.rows {
        let filename = row
            .path
            .file_name()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| row.path.display().to_string());
        match &row.outcome {
            BulkOutcome::Ok {
                key,
                items,
                with_progress,
                cost,
                items_detail,
                ..
            } => {
                let cost_str = cost.map(|c| format!(" · {c}¤")).unwrap_or_default();
                s.push_str(&format!(
                    "- ✓ `{filename}` → **{key}** · {items} items · \
                     {with_progress}/{items} progress{cost_str}\n"
                ));
                for line in items_detail {
                    s.push_str(&format!("  {}\n", line.trim_start()));
                }
            }
            BulkOutcome::Err(e) => {
                s.push_str(&format!("- × `{filename}` — {e}\n"));
            }
        }
    }

    // Append the active row's raw OCR text + word boxes so the recipient
    // has enough context to debug parser misbehavior without asking for
    // a second round-trip.
    if let Some(idx) = state.active_idx {
        if let Some(row) = state.rows.get(idx) {
            if let BulkOutcome::Ok {
                raw_text,
                word_boxes,
                ..
            } = &row.outcome
            {
                let filename = row
                    .path
                    .file_name()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                s.push_str(&format!("\n### Detail: `{filename}`\n\n"));
                s.push_str("**First-pass raw text:**\n\n```\n");
                s.push_str(raw_text);
                s.push_str("\n```\n\n");
                s.push_str(&format!("**Word boxes ({}):**\n\n```\n", word_boxes.len()));
                for line in word_boxes {
                    s.push_str(line);
                    s.push('\n');
                }
                s.push_str("```\n");
            }
        }
    }
    s
}

fn resolve_watch_dir(settings: &parking_lot::RwLock<crate::settings::Settings>) -> Option<PathBuf> {
    settings
        .read()
        .ocr
        .watch_dir_override
        .clone()
        .or_else(ocr::watcher::auto_detect_exfilzone_screenshots_dir)
}

#[cfg(target_os = "windows")]
fn pick_and_extract(state: &mut OcrDialogState) {
    let Some(path) = rfd::FileDialog::new()
        .add_filter("Image", &["png", "jpg", "jpeg", "webp", "bmp"])
        .pick_file()
    else {
        return;
    };
    let row = process_one(&path);
    state.rows.clear();
    state.rows.push(row);
    state.active_idx = Some(0);
    state.last_error = None;
}

#[cfg(not(target_os = "windows"))]
fn pick_and_extract(state: &mut OcrDialogState) {
    state.last_error = Some("OCR is Windows-only in this build".into());
}

#[cfg(target_os = "windows")]
fn bulk_extract(state: &mut OcrDialogState, dir: &std::path::Path) {
    state.rows.clear();
    state.active_idx = None;
    state.last_error = None;

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            state.last_error = Some(format!("reading {}: {e}", dir.display()));
            return;
        }
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            if !p.is_file() {
                return false;
            }
            // Same filters the watcher uses.
            let ext_ok = matches!(
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase()),
                Some(ref e) if matches!(e.as_str(), "png" | "jpg" | "jpeg" | "webp" | "bmp")
            );
            let stereo = p
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase().ends_with("_vr"))
                .unwrap_or(false);
            ext_ok && !stereo
        })
        .collect();
    paths.sort();

    if paths.is_empty() {
        state.last_error = Some(format!("no non-stereo images found in {}", dir.display()));
        return;
    }

    for path in paths {
        state.rows.push(process_one(&path));
    }
    if !state.rows.is_empty() {
        state.active_idx = Some(0);
    }
}

#[cfg(not(target_os = "windows"))]
fn bulk_extract(state: &mut OcrDialogState, _dir: &std::path::Path) {
    state.last_error = Some("OCR is Windows-only in this build".into());
}

#[cfg(target_os = "windows")]
fn process_one(path: &std::path::Path) -> BulkRow {
    // We run the full two-pass extract here (same as the watcher's
    // live path) so the test harness reflects production behavior.
    // Additionally we re-run the first-pass OCR to capture raw text +
    // word boxes for the detail pane — the extractor only returns
    // the structured result, not the intermediate diagnostics.
    let first_pass = match ocr::recognize_file(path) {
        Ok(r) => r,
        Err(e) => return BulkRow { path: path.to_path_buf(), outcome: BulkOutcome::Err(format!("{e:#}")) },
    };
    let extract_result = ocr::extract::extract_upgrade(path);

    match extract_result {
        Ok((upgrade, _raw)) => {
            let with_progress = upgrade
                .items
                .iter()
                .filter(|i| i.collected.is_some())
                .count();
            let items_detail = upgrade
                .items
                .iter()
                .enumerate()
                .map(|(i, it)| {
                    let progress = match (it.collected, it.needed) {
                        (Some(c), Some(n)) => format!("{c}/{n}"),
                        _ => "?/?".to_string(),
                    };
                    format!("  [{i}] {:32}  {progress}", it.name)
                })
                .collect();
            let word_boxes = first_pass
                .words
                .iter()
                .map(|w| {
                    format!(
                        "[{:>4.0},{:>4.0}  {:>4.0}×{:>4.0}]  {}",
                        w.rect.x, w.rect.y, w.rect.width, w.rect.height, w.text
                    )
                })
                .collect();
            BulkRow {
                path: path.to_path_buf(),
                outcome: BulkOutcome::Ok {
                    key: upgrade.key,
                    items: upgrade.items.len(),
                    with_progress,
                    cost: upgrade.cost,
                    raw_text: first_pass.text,
                    word_boxes,
                    items_detail,
                },
            }
        }
        Err(e) => BulkRow {
            path: path.to_path_buf(),
            outcome: BulkOutcome::Err(format!("{e:#}")),
        },
    }
}

#[cfg(not(target_os = "windows"))]
fn process_one(path: &std::path::Path) -> BulkRow {
    BulkRow {
        path: path.to_path_buf(),
        outcome: BulkOutcome::Err("OCR is Windows-only in this build".into()),
    }
}
