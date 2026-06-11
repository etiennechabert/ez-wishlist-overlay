//! Research tab — the merchant blueprint trees (issue #168, Neumann's
//! RESEARCH pad). Renders each category as a lane/depth DAG of node chips
//! with elbow connectors, plus a detail card for the selected node: unlock
//! item, parent gating, sample list with owned/needed, and the state
//! transitions (start → developed, with the same consume-or-keep choice as
//! hideout completion). Tracking a node feeds its samples to the overlay
//! wishlist via [`AppState::active_items`].

use crate::data::{ResearchCategory, ResearchNode, ResearchNodeId};
use crate::gui::{theme, IconCache, SaveTick};
use crate::state::{AppState, ResearchNodeState, ResearchStatus};
use crossbeam_channel::Sender;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Pane-local UI state, owned by [`crate::gui::App`].
#[derive(Default)]
pub struct ResearchUi {
    /// Node whose detail card is open. Survives tab switches; cleared only
    /// when the id stops resolving (data update).
    pub selected: Option<ResearchNodeId>,
}

/// Chip geometry. Lanes are letter-groups of the game's node ids (`a1`…`d3`),
/// depth is the longest parent chain — together they reproduce the in-game
/// column layout without hand-placed coordinates.
const CHIP: egui::Vec2 = egui::vec2(168.0, 54.0);
const H_GAP: f32 = 26.0;
const V_GAP: f32 = 30.0;
const MARGIN: f32 = 8.0;

/// The game's own state vocabulary (`Gunsmith_StringTable` `Research_state_*`),
/// reused verbatim so the tab reads like the in-game pad.
fn status_label(status: ResearchStatus) -> &'static str {
    match status {
        ResearchStatus::Locked => "Unknown Blueprint",
        ResearchStatus::Available => "Ready For Research",
        ResearchStatus::InProgress => "Need Research Samples",
        ResearchStatus::Developed => "Developed",
    }
}

fn status_fill(status: ResearchStatus, dark: bool) -> egui::Color32 {
    match status {
        ResearchStatus::Locked => theme::unknown_fill(dark),
        ResearchStatus::Available => theme::ready_fill(dark),
        ResearchStatus::InProgress => theme::tracked_fill(dark),
        ResearchStatus::Developed => theme::done_fill(dark),
    }
}

pub fn ui(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
    ui_state: &mut ResearchUi,
) {
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("Research")
            .heading()
            .strong()
            .size(26.0)
            .color(ui.visuals().strong_text_color()),
    );
    ui.label(
        egui::RichText::new(
            "Merchant blueprint trees. Track a node to put its research samples on \
             the wishlist — every sample must be found in raid, so knowing what to \
             keep is the whole game.",
        )
        .weak(),
    );
    ui.add_space(8.0);

    let data = state.read().data.clone();
    if data.research.is_empty() {
        ui.label(egui::RichText::new("No research data in this build.").weak());
        return;
    }

    // Drop a selection that no longer resolves (data-version change).
    if let Some(sel) = &ui_state.selected {
        if !state.read().index.research_nodes_by_id.contains_key(sel) {
            ui_state.selected = None;
        }
    }

    for category in &data.research {
        ui.label(
            egui::RichText::new(format!("{} — {}", category.name, category.merchant))
                .strong()
                .size(18.0),
        );
        ui.add_space(4.0);
        tree_canvas(ui, state, category, ui_state);
        ui.add_space(8.0);
    }

    if let Some(selected) = ui_state.selected.clone() {
        detail_card(ui, state, icons, save_tx, &selected);
    } else {
        ui.label(egui::RichText::new("Select a node to see its samples.").weak());
    }
}

/// Longest-parent-chain depth per node. The data layer guarantees acyclicity
/// (`research_section_is_internally_consistent`), so the recursion terminates;
/// a malformed cycle would still bottom out via the visiting guard rather
/// than hang the UI.
fn depths(category: &ResearchCategory) -> HashMap<&str, usize> {
    fn walk<'a>(
        id: &'a str,
        by_id: &HashMap<&'a str, &'a ResearchNode>,
        memo: &mut HashMap<&'a str, usize>,
        visiting: &mut Vec<&'a str>,
    ) -> usize {
        if let Some(&d) = memo.get(id) {
            return d;
        }
        if visiting.contains(&id) {
            return 0; // cycle guard — unreachable with valid data
        }
        visiting.push(id);
        let d = by_id
            .get(id)
            .map(|n| {
                n.parents
                    .iter()
                    .map(|p| walk(p.as_str(), by_id, memo, visiting) + 1)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        visiting.pop();
        memo.insert(id, d);
        d
    }
    let by_id: HashMap<&str, &ResearchNode> =
        category.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut memo = HashMap::new();
    for node in &category.nodes {
        walk(node.id.as_str(), &by_id, &mut memo, &mut Vec::new());
    }
    memo
}

/// Lane key: the letter group of the node id's last segment
/// (`task.research.a7` → `"a"`). Ids outside that shape collapse into one
/// shared lane — layout degrades to a single column instead of breaking.
fn lane_key(id: &str) -> String {
    let tail = id.rsplit('.').next().unwrap_or(id);
    let letters: String = tail
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if letters.is_empty() {
        "?".into()
    } else {
        letters
    }
}

fn tree_canvas(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    category: &ResearchCategory,
    ui_state: &mut ResearchUi,
) {
    let depth_of = depths(category);
    let mut lanes: Vec<String> = category.nodes.iter().map(|n| lane_key(&n.id)).collect();
    lanes.sort();
    lanes.dedup();
    let lane_index: HashMap<&str, usize> = lanes
        .iter()
        .enumerate()
        .map(|(i, l)| (l.as_str(), i))
        .collect();

    let max_depth = depth_of.values().copied().max().unwrap_or(0);
    let canvas = egui::vec2(
        MARGIN * 2.0 + lanes.len() as f32 * CHIP.x + (lanes.len().saturating_sub(1)) as f32 * H_GAP,
        MARGIN * 2.0 + (max_depth + 1) as f32 * CHIP.y + max_depth as f32 * V_GAP,
    );

    egui::ScrollArea::horizontal()
        .id_salt(&category.id)
        .show(ui, |ui| {
            let (rect, _) = ui.allocate_exact_size(canvas, egui::Sense::hover());
            let origin = rect.min;
            let chip_rect = |node: &ResearchNode| -> egui::Rect {
                let lane = lane_index
                    .get(lane_key(&node.id).as_str())
                    .copied()
                    .unwrap_or(0);
                let depth = depth_of.get(node.id.as_str()).copied().unwrap_or(0);
                egui::Rect::from_min_size(
                    origin
                        + egui::vec2(
                            MARGIN + lane as f32 * (CHIP.x + H_GAP),
                            MARGIN + depth as f32 * (CHIP.y + V_GAP),
                        ),
                    CHIP,
                )
            };

            // Connectors first so chips paint over them.
            let painter = ui.painter_at(rect);
            let stroke =
                egui::Stroke::new(1.5, ui.visuals().widgets.noninteractive.bg_stroke.color);
            let by_id: HashMap<&str, &ResearchNode> =
                category.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
            for node in &category.nodes {
                let child = chip_rect(node);
                for parent_id in &node.parents {
                    let Some(parent) = by_id.get(parent_id.as_str()) else {
                        continue;
                    };
                    let parent = chip_rect(parent);
                    let start = parent.center_bottom();
                    let end = child.center_top();
                    let mid_y = end.y - V_GAP * 0.45;
                    painter.line_segment([start, egui::pos2(start.x, mid_y)], stroke);
                    painter.line_segment(
                        [egui::pos2(start.x, mid_y), egui::pos2(end.x, mid_y)],
                        stroke,
                    );
                    painter.line_segment([egui::pos2(end.x, mid_y), end], stroke);
                }
            }

            let dark = ui.visuals().dark_mode;
            for node in &category.nodes {
                let status = state.read().research_status(&node.id);
                let selected = ui_state.selected.as_deref() == Some(node.id.as_str());
                let name_color = if status == ResearchStatus::Locked {
                    ui.visuals().weak_text_color()
                } else {
                    ui.visuals().strong_text_color()
                };
                let text = egui::RichText::new(format!("{}\n{}", node.name, status_label(status)))
                    .size(11.5)
                    .color(name_color);
                let mut button = egui::Button::new(text).fill(status_fill(status, dark));
                if selected {
                    button = button.stroke(egui::Stroke::new(2.0, theme::selected_outline(dark)));
                }
                let resp = ui.put(chip_rect(node), button);
                if resp.clicked() {
                    ui_state.selected = Some(node.id.clone());
                }
                // Tracked badge: a small dot in the chip corner so wishlist
                // membership is visible without opening the card.
                if state.read().tracked_research.contains(&node.id) {
                    let painter = ui.painter_at(rect);
                    painter.circle_filled(
                        chip_rect(node).right_top() + egui::vec2(-8.0, 8.0),
                        3.5,
                        theme::pinned_accent(dark),
                    );
                }
            }
        });
}

fn detail_card(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
    node_id: &ResearchNodeId,
) {
    let Some(node) = state
        .read()
        .index
        .research_nodes_by_id
        .get(node_id)
        .cloned()
    else {
        return;
    };
    let status = state.read().research_status(node_id);
    let dark = ui.visuals().dark_mode;

    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            // Header: unlocked item identity.
            let unlock = state
                .read()
                .index
                .items_by_id
                .get(&node.unlocks_item_id)
                .cloned();
            ui.horizontal(|ui| {
                if let Some(item) = &unlock {
                    if let Some(tex) = icons.get(ui.ctx(), &item.icon_path) {
                        ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(40.0, 40.0)));
                    }
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{} — unlocks {}", node.name, item.name))
                                .strong()
                                .size(15.0),
                        );
                        let mut meta: Vec<String> = Vec::new();
                        if let Some(r) = &item.rarity {
                            meta.push(r.clone());
                        }
                        if let Some(p) = item.price {
                            meta.push(format!("{p} ₽"));
                        }
                        if let Some(w) = item.weight {
                            meta.push(format!("{w} kg"));
                        }
                        meta.push(status_label(status).to_string());
                        ui.label(egui::RichText::new(meta.join("  ·  ")).weak());
                    });
                } else {
                    ui.label(egui::RichText::new(&node.name).strong().size(15.0));
                }
            });

            // Parent gating line.
            if !node.parents.is_empty() {
                let gates: Vec<String> = node
                    .parents
                    .iter()
                    .map(|p| {
                        let s = state.read();
                        let name = s
                            .index
                            .research_nodes_by_id
                            .get(p)
                            .map(|n| n.name.clone())
                            .unwrap_or_else(|| p.clone());
                        let mark = if s.research_status(p) == ResearchStatus::Developed {
                            "✓"
                        } else {
                            "✗"
                        };
                        format!("{name} {mark}")
                    })
                    .collect();
                ui.label(
                    egui::RichText::new(format!("Requires developed: {}", gates.join(", "))).weak(),
                );
            }
            ui.add_space(6.0);

            // Samples with owned/needed.
            let progress = state.read().research_progress(node_id);
            ui.label(
                egui::RichText::new(format!(
                    "Research samples (found in raid) — {}/{}:",
                    progress.collected, progress.needed
                ))
                .strong(),
            );
            for req in &node.samples {
                let (name, icon_path, owned) = {
                    let s = state.read();
                    let item = s.index.items_by_id.get(&req.item_id);
                    (
                        item.map(|i| i.name.clone())
                            .unwrap_or_else(|| req.item_id.clone()),
                        item.map(|i| i.icon_path.clone()).unwrap_or_default(),
                        s.owned_total(&req.item_id),
                    )
                };
                ui.horizontal(|ui| {
                    if let Some(tex) = icons.get(ui.ctx(), &icon_path) {
                        ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(20.0, 20.0)));
                    }
                    let met = owned >= req.quantity;
                    let count = egui::RichText::new(format!("{owned}/{}", req.quantity));
                    ui.label(if met {
                        count.color(theme::done_text(dark))
                    } else {
                        count
                    });
                    ui.label(name);
                });
            }
            ui.add_space(6.0);

            // Wishlist tracking — meaningless once developed (contributes
            // nothing), so the toggle hides rather than tempting a no-op.
            if status != ResearchStatus::Developed {
                let mut tracked = state.read().tracked_research.contains(node_id);
                if ui
                    .checkbox(&mut tracked, "Track samples on wishlist")
                    .changed()
                {
                    state.write().set_tracked_research(node_id, tracked);
                    notify(state, save_tx);
                }
                if status == ResearchStatus::Locked {
                    ui.label(
                        egui::RichText::new(
                            "Locked in-game until every parent is developed — samples can \
                             still be collected (and tracked) ahead of time.",
                        )
                        .weak()
                        .italics(),
                    );
                }
            }
            ui.add_space(4.0);

            // State transitions.
            ui.horizontal(|ui| match status {
                ResearchStatus::Available => {
                    if ui.button("Start research").clicked() {
                        state
                            .write()
                            .set_research_state(node_id, Some(ResearchNodeState::InProgress));
                        notify(state, save_tx);
                    }
                }
                ResearchStatus::InProgress => {
                    let can_consume = state.read().can_consume_research_samples(node_id);
                    if ui
                        .add_enabled(
                            can_consume,
                            egui::Button::new("Developed — consume samples"),
                        )
                        .on_disabled_hover_text("You don't own every sample yet.")
                        .clicked()
                    {
                        state.write().complete_research(node_id, true);
                        notify(state, save_tx);
                    }
                    if ui.button("Developed — keep items").clicked() {
                        state.write().complete_research(node_id, false);
                        notify(state, save_tx);
                    }
                    if ui.button("Reset to not started").clicked() {
                        state.write().set_research_state(node_id, None);
                        notify(state, save_tx);
                    }
                }
                ResearchStatus::Developed => {
                    if ui.button("Undo developed").clicked() {
                        state.write().set_research_state(node_id, None);
                        notify(state, save_tx);
                    }
                }
                ResearchStatus::Locked => {}
            });
        });
}

fn notify(state: &Arc<RwLock<AppState>>, save_tx: &Sender<SaveTick>) {
    let v = state.read().version;
    let _ = save_tx.try_send(SaveTick { version: v });
}

#[cfg(test)]
mod tests {
    //! Headless GUI tests, same `egui_kittest` pattern as the Hideout and
    //! Containers panes. Chip labels are two-line ("name\nstatus"), so the
    //! queries construct the exact same strings the pane renders.

    use super::*;
    use crate::data::{GameData, Item, Requirement, ResearchCategory, ResearchNode};
    use crate::settings::ColorScheme;
    use egui_kittest::kittest::Queryable;
    use egui_kittest::Harness;

    fn item(id: &str, name: &str) -> Item {
        Item {
            id: id.into(),
            name: name.into(),
            icon_path: String::new(),
            category: Some("gunsmith".into()),
            subcategory: None,
            weight: None,
            price: None,
            rarity: None,
        }
    }

    fn node(id: &str, name: &str, parents: &[&str], samples: &[(&str, u32)]) -> ResearchNode {
        ResearchNode {
            id: id.into(),
            name: name.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            unlocks_item_id: "part".into(),
            samples: samples
                .iter()
                .map(|&(i, q)| Requirement {
                    item_id: i.into(),
                    quantity: q,
                })
                .collect(),
        }
    }

    /// Two-node chain: `task.research.a1` (root) → `task.research.a2`.
    fn fixture() -> Arc<RwLock<AppState>> {
        let data = Arc::new(GameData {
            data_version: "test".into(),
            scraped_at: "now".into(),
            source_repo: "test".into(),
            source_commit: "deadbeef".into(),
            modules: Vec::new(),
            items: vec![item("part", "Part"), item("oil", "Gun oil")],
            research: vec![ResearchCategory {
                id: "basic".into(),
                name: "Basic Research".into(),
                merchant: "Neumann".into(),
                nodes: vec![
                    node("task.research.a1", "Root Node", &[], &[("oil", 2)]),
                    node(
                        "task.research.a2",
                        "Child Node",
                        &["task.research.a1"],
                        &[("oil", 1)],
                    ),
                ],
            }],
        });
        Arc::new(RwLock::new(AppState::new(data)))
    }

    fn harness(state: &Arc<RwLock<AppState>>) -> Harness<'static> {
        let ui_state = Arc::clone(state);
        let (save_tx, _save_rx) = crossbeam_channel::unbounded::<SaveTick>();
        let mut icons = IconCache::new();
        let mut pane = ResearchUi::default();
        Harness::builder()
            .with_size(egui::vec2(1200.0, 900.0))
            .build_ui(move |ui| {
                theme::set_scheme(ColorScheme::OkabeIto);
                super::ui(ui, &ui_state, &mut icons, &save_tx, &mut pane);
            })
    }

    #[test]
    fn chips_show_game_vocabulary_states() {
        let state = fixture();
        let mut h = harness(&state);
        h.run();
        // Root is available, child gated behind it — the game's own words.
        let _ = h.get_by_label("Root Node\nReady For Research");
        let _ = h.get_by_label("Child Node\nUnknown Blueprint");
    }

    #[test]
    fn start_then_develop_unlocks_the_child() {
        let state = fixture();
        let mut h = harness(&state);
        h.run();

        h.get_by_label("Root Node\nReady For Research").click();
        h.run();
        h.get_by_label("Start research").click();
        h.run();
        assert_eq!(
            state
                .read()
                .research_status(&"task.research.a1".to_string()),
            ResearchStatus::InProgress
        );

        h.get_by_label("Developed — keep items").click();
        h.run();
        assert_eq!(
            state
                .read()
                .research_status(&"task.research.a1".to_string()),
            ResearchStatus::Developed
        );
        // The child chip now reads available, and the develop cascade put it
        // on the wishlist.
        let _ = h.get_by_label("Child Node\nReady For Research");
        assert!(state.read().tracked_research.contains("task.research.a2"));
    }

    #[test]
    fn track_checkbox_feeds_the_wishlist() {
        let state = fixture();
        let mut h = harness(&state);
        h.run();

        h.get_by_label("Root Node\nReady For Research").click();
        h.run();
        h.get_by_label("Track samples on wishlist").click();
        h.run();
        assert!(state.read().tracked_research.contains("task.research.a1"));
        let items = state.read().active_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, "oil");
        assert_eq!(items[0].needed, 2);
    }

    #[test]
    fn consume_button_gates_on_owned_samples() {
        let state = fixture();
        state.write().set_research_state(
            &"task.research.a1".to_string(),
            Some(ResearchNodeState::InProgress),
        );
        let mut h = harness(&state);
        h.run();
        h.get_by_label("Root Node\nNeed Research Samples").click();
        h.run();
        assert!(
            h.get_by_label("Developed — consume samples").is_disabled(),
            "no samples owned — consume path must be disabled"
        );

        state.write().set_collected(&"oil".to_string(), 2);
        h.run();
        h.get_by_label("Developed — consume samples").click();
        h.run();
        assert_eq!(
            state
                .read()
                .research_status(&"task.research.a1".to_string()),
            ResearchStatus::Developed
        );
        assert_eq!(
            state.read().owned_total(&"oil".to_string()),
            0,
            "consume path burned the samples"
        );
    }
}
