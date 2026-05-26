// On Windows, suppress the console window for release builds. Debug builds
// keep the console so tracing output is visible during development.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod assets;
mod data;
mod gui;
mod log_buffer;
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

    let shared_state = Arc::new(RwLock::new(app_state));
    let (save_tx, save_rx) = crossbeam_channel::unbounded::<gui::SaveTick>();
    let _save_handle = save_loop::spawn(shared_state.clone(), paths.clone(), save_rx);

    let settings = Arc::new(RwLock::new(settings::load(&paths.settings_file)));
    tracing::info!(settings = ?&*settings.read(), "settings loaded");
    let vr_runtime = Arc::new(vr::Runtime::spawn(
        shared_state.clone(),
        settings.clone(),
        paths.clone(),
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
