// On Windows, suppress the console window for release builds. Debug builds
// keep the console so tracing output is visible during development.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod assets;
mod data;
mod gui;
mod hierarchy;
mod log_buffer;
mod ocr;
mod persist;
mod platform;
mod presets;
mod save_loop;
mod settings;
mod state;
mod updater;
mod vr;

use anyhow::{Context, Result};
use parking_lot::RwLock;
use std::sync::Arc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    let log_buf = log_buffer::LogBuffer::new();
    init_logging(log_buf.clone());

    let paths = Arc::new(persist::PersistPaths::discover().context("resolve user data dir")?);
    paths.ensure_dirs().context("create user data dirs")?;
    tracing::info!(data_dir = %paths.data_dir.display(), "user data");

    let data = Arc::new(assets::load_game_data().context("load embedded data.json")?);
    tracing::info!(
        version = %data.data_version,
        modules = data.modules.len(),
        vendors = data.vendors.len(),
        items = data.items.len(),
        "game data loaded",
    );

    let mut app_state = state::AppState::new(data);

    match persist::load(&paths) {
        persist::LoadOutcome::Fresh => {
            tracing::info!("no saved state — starting fresh");
        }
        persist::LoadOutcome::Loaded(p) => {
            if let Some(warning) = p.merge_into(&mut app_state) {
                tracing::warn!(%warning, "state loaded with warnings");
                app_state.load_warning = Some(warning);
            } else {
                tracing::info!("state loaded");
            }
        }
        persist::LoadOutcome::Corrupt(boxed) => {
            let msg = format!(
                "state.json was corrupt and has been backed up to {}: {}",
                boxed.backup_path.display(),
                boxed.error
            );
            tracing::warn!(%msg);
            app_state.load_warning = Some(msg);
        }
    }

    match persist::load_overrides(&paths) {
        persist::OverridesLoadOutcome::Fresh => {
            tracing::info!("no recipe overrides — using bundled recipes");
        }
        persist::OverridesLoadOutcome::Loaded(p) => {
            if let Some(warning) = p.merge_into(&mut app_state) {
                tracing::warn!(%warning, "overrides loaded with warnings");
                // Don't clobber an existing state warning — concatenate.
                app_state.load_warning = Some(match app_state.load_warning.take() {
                    Some(prev) => format!("{prev} {warning}"),
                    None => warning,
                });
            } else {
                tracing::info!(count = app_state.overrides.len(), "overrides loaded");
            }
        }
        persist::OverridesLoadOutcome::Corrupt(boxed) => {
            let msg = format!(
                "overrides.json was corrupt and has been backed up to {}: {}",
                boxed.backup_path.display(),
                boxed.error
            );
            tracing::warn!(%msg);
            app_state.load_warning = Some(match app_state.load_warning.take() {
                Some(prev) => format!("{prev} {msg}"),
                None => msg,
            });
        }
    }

    let shared_state = Arc::new(RwLock::new(app_state));
    let (save_tx, save_rx) = crossbeam_channel::unbounded::<gui::SaveTick>();
    let _save_handle = save_loop::spawn(shared_state.clone(), paths.clone(), save_rx);

    let settings = Arc::new(RwLock::new(settings::load(&paths.settings_file)));
    tracing::info!(settings = ?&*settings.read(), "settings loaded");

    // OCR worker thread. The VR render thread pushes captured PNG paths
    // into `ocr_path_tx` after each successful screenshot; this worker
    // drains them, runs the OCR pipeline, and applies the results to
    // `AppState.collected` via `set_collected` + a `SaveTick`. Kept off
    // the VR thread because a full OCR pass can take 100-500 ms and the
    // 90 Hz render loop must not block.
    let (ocr_path_tx, ocr_path_rx) = crossbeam_channel::bounded::<std::path::PathBuf>(4);
    // The OCR feedback now lives entirely in VR (see vr::ocr_render +
    // OverlaySession's head-locked second overlay). The worker writes
    // OcrFeedback messages here; the VR render loop drains them and
    // manages show/hide + auto-fade on its own.
    let (ocr_feedback_tx, ocr_feedback_rx) = crossbeam_channel::bounded::<gui::OcrFeedback>(4);
    let _ocr_handle = spawn_ocr_worker(
        shared_state.clone(),
        settings.clone(),
        save_tx.clone(),
        ocr_feedback_tx,
        ocr_path_rx,
    );

    let vr_runtime = Arc::new(vr::Runtime::spawn(
        shared_state.clone(),
        settings.clone(),
        paths.clone(),
        ocr_path_tx.clone(),
        ocr_feedback_rx,
    ));

    let update_rx = if settings.read().check_for_updates {
        Some(updater::spawn_check())
    } else {
        tracing::info!("update check disabled by settings");
        None
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("EZ Wishlist Overlay")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 600.0])
            .with_icon(load_window_icon()),
        persist_window: true,
        ..Default::default()
    };

    eframe::run_native(
        "EZ Wishlist Overlay",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(gui::App::new(
                cc,
                shared_state,
                paths,
                save_tx,
                vr_runtime,
                settings,
                log_buf,
                update_rx,
                ocr_path_tx,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe failed: {e}"))?;

    Ok(())
}

/// Decode the embedded `assets/icon.png` for the egui window icon. Eframe
/// hands this to the OS for the title-bar, Alt-Tab, and dock representations.
/// Falls back to a 1×1 transparent pixel if decoding ever fails, so a bad
/// icon never crashes the app.
fn load_window_icon() -> egui::IconData {
    const BYTES: &[u8] = include_bytes!("../assets/icon.png");
    match image::load_from_memory(BYTES) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to decode embedded window icon");
            egui::IconData {
                rgba: vec![0; 4],
                width: 1,
                height: 1,
            }
        }
    }
}

fn init_logging(buf: log_buffer::LogBuffer) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("ez_wishlist_overlay=info,warn"));
    let fmt_layer = tracing_subscriber::fmt::layer().with_target(false);
    let buffer_layer = log_buffer::LogBufferLayer::new(buf);
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(buffer_layer)
        .init();
}

/// Spawn the OCR worker thread. Receives screenshot paths from the VR
/// thread, runs the OCR pipeline, applies the resulting per-item owned
/// counts to `AppState.collected` (snapshot is truth — overwrite, never
/// merge), and sends a `SaveTick` so the new state hits disk via the
/// debounced save loop. Errors are logged; nothing is retried — the user
/// just triggers another screenshot.
fn spawn_ocr_worker(
    state: Arc<RwLock<state::AppState>>,
    settings: Arc<RwLock<settings::Settings>>,
    save_tx: crossbeam_channel::Sender<gui::SaveTick>,
    ocr_feedback_tx: crossbeam_channel::Sender<gui::OcrFeedback>,
    ocr_path_rx: crossbeam_channel::Receiver<std::path::PathBuf>,
) -> std::thread::JoinHandle<()> {
    // Log + forward a feedback record to the VR overlay. The render loop
    // owns the lifecycle (fade timers, show/hide); the worker just
    // emits state transitions.
    let publish = move |tx: &crossbeam_channel::Sender<gui::OcrFeedback>,
                        feedback: gui::OcrFeedback| {
        feedback.log();
        let _ = tx.try_send(feedback);
    };

    std::thread::Builder::new()
        .name("ez-wishlist-ocr".into())
        .spawn(move || {
            while let Ok(first) = ocr_path_rx.recv() {
                // Drain any newer paths that arrived while we were
                // idle, keeping only the most recent. A full OCR pass
                // takes ~3-7 s (PNG decode + Windows.Media.Ocr on a 3K
                // image + per-cell template matching). If the user
                // tapped capture twice while the worker was idle on
                // recv(), FIFO order would have shown the older
                // shot's result first — manifesting as "OCR is reading
                // the previous screenshot, not the one I just took."
                let mut path = first;
                let mut skipped = 0usize;
                while let Ok(newer) = ocr_path_rx.try_recv() {
                    skipped += 1;
                    path = newer;
                }
                if skipped > 0 {
                    tracing::info!(
                        skipped,
                        latest = %path.display(),
                        "OCR worker: drained stale queued captures, processing latest only",
                    );
                }

                if !settings.read().ocr_enabled {
                    // Surface this at info — users only see the
                    // info+warn levels and we want them to know why no
                    // overlay popped up after a capture. Enable in
                    // Settings → "Auto-extract counts from VR
                    // screenshots".
                    tracing::info!(
                        path = %path.display(),
                        "OCR disabled in Settings → \
                         'Auto-extract counts from VR screenshots' — \
                         skipping. Toggle it on to see the in-headset \
                         feedback overlay.",
                    );
                    continue;
                }

                // Surface "we're working" within ~1 frame of capture so
                // the in-headset card pops up before the 100-500 ms
                // pipeline finishes.
                publish(&ocr_feedback_tx, gui::OcrFeedback::processing());

                // Process the path. If a NEWER path arrives during
                // processing, drop the stale terminal and re-process
                // with the newer path. Without this loop the user
                // would see the older shot's result flash up briefly
                // before the newer one finishes — same "previous-
                // screenshot" symptom, just on the trailing edge of
                // a long OCR pass instead of the leading edge.
                loop {
                    let data = state.read().data.clone();
                    let terminal = match ocr::process_screenshot(&path, &data) {
                        Ok(Some(outcome)) => {
                            let (version, feedback) = {
                                let mut w = state.write();
                                let mut feedback = gui::OcrFeedback::done(&outcome, &w);
                                // Only apply cells the pipeline could
                                // actually read. `None` means we saw
                                // the cell but couldn't parse an X/Y
                                // count — leave the user's existing
                                // collected value alone instead of
                                // resetting to 0.
                                for (item_id, owned) in &outcome.items {
                                    if let Some(value) = owned {
                                        w.set_collected(item_id, *value);
                                    }
                                }
                                let progression = w.apply_ocr_progression(&outcome.upgrade_id);
                                feedback.attach_progression(&w, progression);
                                (w.version, feedback)
                            };
                            let _ = save_tx.try_send(gui::SaveTick { version });
                            feedback
                        }
                        Ok(None) => gui::OcrFeedback::not_a_panel(),
                        Err(e) => gui::OcrFeedback::failed(format!("{e:#}")),
                    };

                    // Check: did a newer capture arrive while we were
                    // processing this one? If so, this terminal is
                    // already stale before we publish it — skip the
                    // publish and re-process with the newest path.
                    // Note: `set_collected` writes above still apply,
                    // since the underlying screenshot was real game
                    // state at capture time — only the user-visible
                    // feedback card is suppressed.
                    let mut newer_path = None;
                    while let Ok(np) = ocr_path_rx.try_recv() {
                        newer_path = Some(np);
                    }
                    if let Some(np) = newer_path {
                        tracing::info!(
                            stale = %path.display(),
                            latest = %np.display(),
                            "OCR worker: terminal stale (newer capture during processing) — \
                             reprocessing with latest path",
                        );
                        path = np;
                        continue;
                    }

                    publish(&ocr_feedback_tx, terminal);
                    break;
                }
            }
            tracing::info!("OCR worker: channel closed, thread exiting");
        })
        .expect("spawn OCR worker thread")
}
