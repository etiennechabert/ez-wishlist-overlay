//! Overlay click handling: hit-test mouse events against the last
//! rendered grid, cycle the underlying counter, fire haptic feedback.
//!
//! Kept OpenVR-agnostic for unit testability — the runtime feeds in the
//! `Mouse` event data and the cached hit table; this module decides
//! what (if anything) to do with it.

use super::render::CellHit;
use crate::data::ItemId;
use crate::state::AppState;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Reject a repeat click on the same cell within this window.
pub const DEBOUNCE_MS: u64 = 100;
/// Short pulse on a regular increment click.
pub const HAPTIC_INCREMENT_US: u16 = 1_500;
/// Longer, sustained pulse when the click cycled the counter back to 0.
/// SPEC §7.4 asks for a "distinct pattern"; the alternative of two
/// quick pulses needs cross-tick scheduling, this one tick suffices.
pub const HAPTIC_RESET_US: u16 = 3_500;

/// What the runtime should do as a result of a click. The runtime owns
/// the [`OverlaySession`] handle for haptics, so this layer just tells
/// it which device to buzz and for how long.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickOutcome {
    /// Click landed on `item_id`; counter went up to `new_value`.
    Incremented { item_id: ItemId, new_value: u32 },
    /// Click landed on `item_id` while it was already at target; counter
    /// reset to 0.
    Reset { item_id: ItemId },
    /// Click landed in an empty area, or the same cell was clicked again
    /// inside the debounce window.
    Ignored,
}

/// Per-loop debounce tracker. One entry per item_id whose last click is
/// still inside the debounce window; entries are reaped lazily on the
/// next click attempt for that cell.
#[derive(Default)]
pub struct Debouncer {
    last_click: HashMap<ItemId, Instant>,
}

impl Debouncer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if a click on `item_id` at `now` is within the
    /// debounce window of the previous click; updates the timestamp
    /// otherwise.
    fn should_drop(&mut self, item_id: &ItemId, now: Instant) -> bool {
        let window = Duration::from_millis(DEBOUNCE_MS);
        if let Some(&prev) = self.last_click.get(item_id) {
            if now.duration_since(prev) < window {
                return true;
            }
        }
        self.last_click.insert(item_id.clone(), now);
        false
    }
}

/// Convert the overlay's mouse position (texcoords, origin bottom-left)
/// into pixel space (origin top-left) using the render canvas dimensions.
/// Canvas width and height vary independently with `VrSettings::grid_cols`
/// and wishlist size, so the caller must pass both.
pub fn texcoord_to_pixel(tex_x: f32, tex_y: f32, canvas_w: u32, canvas_h: u32) -> (f32, f32) {
    (tex_x * canvas_w as f32, (1.0 - tex_y) * canvas_h as f32)
}

/// Find the [`CellHit`] (if any) that contains the pixel point.
pub fn hit_test(hits: &[CellHit], pixel_x: f32, pixel_y: f32) -> Option<&CellHit> {
    hits.iter().find(|h| {
        pixel_x >= h.rect.x()
            && pixel_x <= h.rect.right()
            && pixel_y >= h.rect.y()
            && pixel_y <= h.rect.bottom()
    })
}

/// Apply a click on `(pixel_x, pixel_y)` to the current state. Returns
/// the [`ClickOutcome`] so the runtime can decide haptics.
pub fn handle_click(
    state: &Arc<RwLock<AppState>>,
    hits: &[CellHit],
    pixel_x: f32,
    pixel_y: f32,
    debounce: &mut Debouncer,
    now: Instant,
) -> ClickOutcome {
    let Some(hit) = hit_test(hits, pixel_x, pixel_y) else {
        return ClickOutcome::Ignored;
    };
    if debounce.should_drop(&hit.item_id, now) {
        return ClickOutcome::Ignored;
    }
    let mut w = state.write();
    let (new_value, was_reset) = w.cycle_collected(&hit.item_id, hit.needed);
    if was_reset {
        ClickOutcome::Reset {
            item_id: hit.item_id.clone(),
        }
    } else {
        ClickOutcome::Incremented {
            item_id: hit.item_id.clone(),
            new_value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::Rect;

    fn hit(id: &str, x: f32, y: f32, w: f32, h: f32, needed: u32) -> CellHit {
        CellHit {
            item_id: id.to_string(),
            rect: Rect::from_xywh(x, y, w, h).unwrap(),
            needed,
        }
    }

    #[test]
    fn texcoord_flips_y_and_scales() {
        let (px, py) = texcoord_to_pixel(0.5, 0.5, 1024, 1024);
        assert!((px - 512.0).abs() < 0.01);
        assert!((py - 512.0).abs() < 0.01);
        // Bottom-left of texcoord (0,0) → top-left of pixel (0, canvas_h).
        let (_, py_origin) = texcoord_to_pixel(0.0, 0.0, 1024, 1024);
        assert!((py_origin - 1024.0).abs() < 0.01);
    }

    #[test]
    fn texcoord_uses_independent_w_and_h() {
        // Non-square canvas (3-col grid, 5 rows): width and height scale
        // their own axis.
        let (px, py) = texcoord_to_pixel(0.5, 0.5, 512, 800);
        assert!((px - 256.0).abs() < 0.01);
        assert!((py - 400.0).abs() < 0.01);
    }

    #[test]
    fn hit_test_picks_containing_cell() {
        let hits = vec![
            hit("a", 0.0, 0.0, 100.0, 100.0, 5),
            hit("b", 200.0, 200.0, 100.0, 100.0, 5),
        ];
        let h = hit_test(&hits, 50.0, 50.0);
        assert_eq!(h.map(|h| h.item_id.as_str()), Some("a"));
        let h = hit_test(&hits, 250.0, 250.0);
        assert_eq!(h.map(|h| h.item_id.as_str()), Some("b"));
        let h = hit_test(&hits, 150.0, 150.0);
        assert!(h.is_none());
    }

    #[test]
    fn debouncer_drops_repeat_within_window() {
        let mut d = Debouncer::new();
        let t0 = Instant::now();
        let id = "a".to_string();
        assert!(!d.should_drop(&id, t0));
        assert!(d.should_drop(&id, t0 + Duration::from_millis(50)));
        assert!(!d.should_drop(&id, t0 + Duration::from_millis(150)));
    }

    #[test]
    fn debouncer_independent_per_item() {
        let mut d = Debouncer::new();
        let t0 = Instant::now();
        assert!(!d.should_drop(&"a".to_string(), t0));
        assert!(!d.should_drop(&"b".to_string(), t0 + Duration::from_millis(10)));
    }

    /// Build a one-item AppState with the named item required at `qty`,
    /// already tracked. Returns the shared handle the click handler
    /// expects.
    fn one_item_state(item: &str, qty: u32) -> Arc<RwLock<AppState>> {
        use crate::data::{GameData, HideoutModule, Item, Requirement, Upgrade};
        let data = GameData {
            data_version: "test".into(),
            scraped_at: "now".into(),
            source_repo: "test".into(),
            source_commit: "test".into(),
            modules: vec![HideoutModule {
                id: "m".into(),
                name: "Mod".into(),
                upgrades: vec![Upgrade {
                    id: "u1".into(),
                    name: "U1".into(),
                    level: 1,
                    description: String::new(),
                    requirements: vec![Requirement {
                        item_id: item.into(),
                        quantity: qty,
                    }],
                }],
            }],
            vendors: vec![],
            items: vec![Item {
                id: item.into(),
                name: item.to_string(),
                icon_path: "i.png".into(),
            }],
        };
        let mut state = AppState::new(Arc::new(data));
        state.set_tracked_upgrade(&"u1".to_string(), true);
        Arc::new(RwLock::new(state))
    }

    #[test]
    fn handle_click_increments_below_target() {
        let state = one_item_state("bolts", 5);
        let hits = vec![hit("bolts", 0.0, 0.0, 100.0, 100.0, 5)];
        let mut deb = Debouncer::new();
        let t0 = Instant::now();

        let out = handle_click(&state, &hits, 50.0, 50.0, &mut deb, t0);
        assert!(matches!(
            out,
            ClickOutcome::Incremented { new_value: 1, .. }
        ));
        assert_eq!(state.read().collected.get("bolts").copied().unwrap_or(0), 1);

        // Repeat inside the debounce window: ignored.
        let out = handle_click(&state, &hits, 50.0, 50.0, &mut deb, t0);
        assert_eq!(out, ClickOutcome::Ignored);
        assert_eq!(state.read().collected.get("bolts").copied().unwrap_or(0), 1);
    }

    #[test]
    fn handle_click_cycles_at_target() {
        let state = one_item_state("wire", 2);
        state.write().set_collected(&"wire".to_string(), 2);

        let hits = vec![hit("wire", 0.0, 0.0, 100.0, 100.0, 2)];
        let mut deb = Debouncer::new();
        let t0 = Instant::now();

        let out = handle_click(&state, &hits, 50.0, 50.0, &mut deb, t0);
        assert!(matches!(out, ClickOutcome::Reset { .. }));
        assert_eq!(state.read().collected.get("wire").copied().unwrap_or(0), 0);
    }
}
