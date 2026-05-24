//! Background thread that debounces save requests from the GUI.

use crate::gui::SaveTick;
use crate::persist::{self, PersistPaths};
use crate::state::AppState;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;

const DEBOUNCE: Duration = Duration::from_millis(500);

/// Spawn a thread that:
/// 1. Blocks on the channel.
/// 2. When a tick arrives, waits up to `DEBOUNCE` for more ticks (resets the
///    timer each time).
/// 3. Saves once the channel goes quiet.
pub fn spawn(
    state: Arc<RwLock<AppState>>,
    paths: Arc<PersistPaths>,
    rx: Receiver<SaveTick>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("ez-wishlist-save".into())
        .spawn(move || run(state, paths, rx))
        .expect("spawn save thread")
}

fn run(state: Arc<RwLock<AppState>>, paths: Arc<PersistPaths>, rx: Receiver<SaveTick>) {
    let mut pending: Option<u64> = None;
    let mut last_saved: u64 = 0;

    loop {
        let timeout = if pending.is_some() {
            DEBOUNCE
        } else {
            Duration::from_secs(60 * 60) // effectively block
        };

        match rx.recv_timeout(timeout) {
            Ok(tick) => {
                pending = Some(tick.version);
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(v) = pending.take() {
                    if v != last_saved {
                        match persist::save(&paths, &state.read()) {
                            Ok(()) => {
                                tracing::debug!(version = v, "state saved");
                                last_saved = v;
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "save failed");
                            }
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                // GUI is gone — do one last save if needed, then exit.
                if let Some(v) = pending.take() {
                    if v != last_saved {
                        let _ = persist::save(&paths, &state.read());
                    }
                }
                tracing::debug!("save thread shutting down");
                return;
            }
        }
    }
}
