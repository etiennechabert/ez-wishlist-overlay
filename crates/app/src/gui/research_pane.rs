//! Research tab — the merchant blueprint trees (issue #168, Neumann's
//! RESEARCH pad). Renders each category as a lane/depth DAG of node chips
//! with elbow connectors, plus a detail card for the selected node: unlock
//! item, parent gating, sample list with owned/needed, and a Track / Pin /
//! Done control cluster that mirrors the hideout's. Track feeds the node's
//! samples to the overlay wishlist ([`AppState::active_items`]); Pin
//! prioritizes a tracked node so its samples lead the overlay with the
//! highlight accent; Done marks the blueprint developed with the same
//! consume-or-keep choice as hideout completion. There is deliberately no
//! "start research" step — the in-game intermediate added a click without
//! touching the wishlist, so the card jumps straight from ready to done.

use crate::data::{ResearchCategory, ResearchNode, ResearchNodeId};
use crate::gui::{theme, IconCache, SaveTick};
use crate::state::{AppState, ResearchStatus};
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

/// Side of the full-size unlocked-item icon shown on the right of the detail
/// card. Big enough to use the wide horizontal space the card spans.
const DETAIL_ICON: f32 = 168.0;

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

/// Chip background. Mirrors the hideout grid's tracked/ready/done legend
/// rather than the raw research lifecycle: Developed → green, a tracked (or
/// legacy in-progress) node → wishlist blue, an available node → the warm
/// "ready" fill, a locked one → inert gray. Tracking a node therefore turns
/// its chip blue at a glance, the same signal as a tracked hideout cell.
fn chip_fill(status: ResearchStatus, tracked: bool, dark: bool) -> egui::Color32 {
    match status {
        ResearchStatus::Developed => theme::done_fill(dark),
        ResearchStatus::Locked if !tracked => theme::unknown_fill(dark),
        ResearchStatus::Available if !tracked => theme::ready_fill(dark),
        // Tracked (any non-developed status) or a legacy in-progress node.
        _ => theme::tracked_fill(dark),
    }
}

/// Always-visible key for the chip colors, mirroring the hideout's
/// `legend_row`. The chips no longer paint strictly to the in-game lifecycle
/// words printed on them (a tracked "Ready For Research" node goes blue), so
/// the legend is what reconciles the two — same swatch helper, same palette
/// tracking, so the two tabs read identically.
fn research_legend(ui: &mut egui::Ui) {
    let dark = ui.visuals().dark_mode;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(
            egui::RichText::new("Legend:")
                .small()
                .strong()
                .color(ui.visuals().weak_text_color()),
        );
        theme::legend_swatch(
            ui,
            theme::ready_fill(dark),
            "Ready",
            "Ready For Research — every parent developed, available now.",
        );
        theme::legend_swatch(
            ui,
            theme::tracked_fill(dark),
            "Tracked",
            "Its samples are on your wishlist — you're collecting them.",
        );
        theme::legend_swatch(
            ui,
            theme::pinned_accent(dark),
            "Pinned",
            "Prioritized (shown as a corner dot) — its samples lead the overlay.",
        );
        theme::legend_swatch(ui, theme::done_fill(dark), "Developed", "Researched.");
        theme::legend_swatch(
            ui,
            theme::unknown_fill(dark),
            "Locked",
            "Unknown Blueprint — a parent isn't developed yet. Samples can still \
             be collected ahead of time.",
        );
    });
}

/// The tree view: title + legend, then the blueprint tree scrolling below. The
/// selected node's detail card is rendered by [`detail_footer`], which the app
/// docks as a fixed (non-resizable) ctx-level bottom panel — pinned flush to the
/// window bottom, spanning the central width (left of the Active-items panel),
/// while the tree scrolls above it.
pub fn ui(ui: &mut egui::Ui, state: &Arc<RwLock<AppState>>, ui_state: &mut ResearchUi) {
    let data = state.read().data.clone();
    if data.research.is_empty() {
        ui.add_space(4.0);
        ui.label(egui::RichText::new("No research data in this build.").weak());
        return;
    }

    // Drop a selection that no longer resolves (data-version change).
    if let Some(sel) = &ui_state.selected {
        if !state.read().index.research_nodes_by_id.contains_key(sel) {
            ui_state.selected = None;
        }
    }

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
    research_legend(ui);
    ui.add_space(6.0);

    egui::ScrollArea::vertical()
        .id_salt("research-tree-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
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
        });
}

/// Whether a node is selected — the app uses this to decide whether to dock the
/// [`detail_footer`] bottom panel this frame.
pub fn detail_is_open(ui_state: &ResearchUi) -> bool {
    ui_state.selected.is_some()
}

/// Fixed height for the docked card panel — sized to the card's rows, but at
/// least tall enough for the side icon, so the panel hugs the card (flush at the
/// bottom, no dead space) and never clips it. Mirrors the rows `detail_card`
/// draws; the card's interactive widgets render fine in a fixed-height panel.
pub fn card_height(state: &Arc<RwLock<AppState>>, ui_state: &ResearchUi) -> f32 {
    let Some(id) = &ui_state.selected else {
        return 40.0;
    };
    let s = state.read();
    let Some(node) = s.index.research_nodes_by_id.get(id) else {
        return 40.0;
    };
    let developed = s.research_status(id) == ResearchStatus::Developed;
    let mut body = 46.0; // title + meta
    if !node.parents.is_empty() {
        body += 22.0; // "Requires developed: …"
    }
    body += 26.0 + node.samples.len() as f32 * 24.0; // header + sample rows
    body += if developed { 34.0 } else { 70.0 }; // Undo, vs Track/Pin/Focus + Developed
    body.max(DETAIL_ICON) + 22.0 // fit the side icon; + frame padding
}

/// The selected node's detail card, rendered by the app into the docked bottom
/// panel (sized by [`card_height`]).
pub fn detail_footer(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
    ui_state: &mut ResearchUi,
) {
    let Some(selected) = ui_state.selected.clone() else {
        return;
    };
    if !state
        .read()
        .index
        .research_nodes_by_id
        .contains_key(&selected)
    {
        ui_state.selected = None;
        return;
    }
    detail_card(ui, state, icons, save_tx, &selected);
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
                let (status, tracked, pinned) = {
                    let s = state.read();
                    (
                        s.research_status(&node.id),
                        s.tracked_research.contains(&node.id),
                        s.is_research_pinned(&node.id),
                    )
                };
                let selected = ui_state.selected.as_deref() == Some(node.id.as_str());
                // Two-tone label: the blueprint NAME always in the strong text
                // color so even locked ("Unknown Blueprint", gray fill) chips
                // stay legible — the previous all-weak text was unreadable on the
                // dark fill. The status line stays in the weak color as a quiet
                // subtitle. The concatenated text ("name\nstatus") is unchanged,
                // so accessibility labels (and the kittest queries) still match.
                let mut job = egui::text::LayoutJob::default();
                job.wrap.max_width = CHIP.x - 10.0;
                job.append(
                    &node.name,
                    0.0,
                    egui::TextFormat {
                        font_id: egui::FontId::proportional(11.5),
                        color: ui.visuals().strong_text_color(),
                        ..Default::default()
                    },
                );
                job.append(
                    &format!("\n{}", status_label(status)),
                    0.0,
                    egui::TextFormat {
                        font_id: egui::FontId::proportional(11.5),
                        color: ui.visuals().weak_text_color(),
                        ..Default::default()
                    },
                );
                let mut button = egui::Button::new(job).fill(chip_fill(status, tracked, dark));
                if selected {
                    button = button.stroke(egui::Stroke::new(2.0, theme::selected_outline(dark)));
                }
                let resp = ui.put(chip_rect(node), button);
                if resp.clicked() {
                    ui_state.selected = Some(node.id.clone());
                }
                // Pinned badge: an accent dot in the chip corner so a
                // prioritized node is visible without opening the card. Gated on
                // tracked — an inert pin (recorded but not on the wishlist)
                // shows nothing, matching what it contributes to the overlay.
                if tracked && pinned {
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
            let unlock = state
                .read()
                .index
                .items_by_id
                .get(&node.unlocks_item_id)
                .cloned();

            // Body (title, gating, samples, controls) on the left; a full-size
            // icon of the unlocked item on the right, using the wide horizontal
            // space the card spans. The card lives in a FIXED-height bottom panel,
            // so this side-by-side split renders fine — its height is the panel's,
            // not measured from the (otherwise zero-measuring) content.
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width((ui.available_width() - DETAIL_ICON - 16.0).max(240.0));

                    // Header: name + what it unlocks.
                    if let Some(item) = &unlock {
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
                            // The game's own pane phrasing ("// price 4486"); no
                            // currency glyph — ₽/€ are tofu in the bundled fonts
                            // (see hideout_pane's ●/○ note).
                            meta.push(format!("price {p}"));
                        }
                        if let Some(w) = item.weight {
                            meta.push(format!("{w} kg"));
                        }
                        meta.push(status_label(status).to_string());
                        ui.label(egui::RichText::new(meta.join("  ·  ")).weak());
                    } else {
                        ui.label(egui::RichText::new(&node.name).strong().size(15.0));
                    }

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
                                // ●/○ (Geometric Shapes, covered by Hack) — ✓/✗ render
                                // as tofu in the bundled fonts, same trap hideout_pane
                                // documents for its toggle glyphs.
                                let mark = if s.research_status(p) == ResearchStatus::Developed {
                                    "●"
                                } else {
                                    "○"
                                };
                                format!("{name} {mark}")
                            })
                            .collect();
                        ui.label(
                            egui::RichText::new(format!(
                                "Requires developed: {}",
                                gates.join(", ")
                            ))
                            .weak(),
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
                                ui.add(
                                    egui::Image::new(tex).fit_to_exact_size(egui::vec2(20.0, 20.0)),
                                );
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

                    // Wishlist controls — Track + Pin, the research counterparts of the
                    // hideout cluster. Hidden once developed: a developed node feeds
                    // nothing to the wishlist, so tracking/pinning would be a no-op.
                    // Mirrors `upgrade_controls`: both boxes read into locals, Pin
                    // enables off the *post-toggle* Track so track+pin in one frame
                    // behaves, and the diffs apply after the row.
                    if status != ResearchStatus::Developed {
                        let (mut tracked, mut pinned) = {
                            let s = state.read();
                            (
                                s.tracked_research.contains(node_id),
                                s.is_research_pinned(node_id),
                            )
                        };
                        let (orig_tracked, orig_pinned) = (tracked, pinned);
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut tracked, "Track samples").on_hover_text(
                                "Put this node's research samples on the overlay wishlist.",
                            );
                            ui.add_enabled(tracked, egui::Checkbox::new(&mut pinned, "Pin"))
                                .on_hover_text(
                                    "Prioritize: pull these samples to the front of the overlay, \
                             highlighted with a purple accent.",
                                )
                                .on_disabled_hover_text(
                                    "Track this node first, then you can pin it.",
                                );
                            // Focus: one click to grind toward a blueprint deeper in the
                            // tree — tracks + pins it and every prerequisite still needed
                            // to reach it. Only meaningful for nodes that *have* gates;
                            // a root is covered by Track + Pin alone.
                            if !node.parents.is_empty() {
                                ui.separator();
                                if ui
                            .button("Focus this blueprint")
                            .on_hover_text(
                                "Track and pin this blueprint and every prerequisite still \
                                 needed to reach it, so the whole route's samples lead the \
                                 overlay.",
                            )
                            .clicked()
                        {
                            state.write().focus_research(node_id);
                            notify(state, save_tx);
                        }
                            }
                        });
                        if tracked != orig_tracked {
                            state.write().set_tracked_research(node_id, tracked);
                            notify(state, save_tx);
                        }
                        if pinned != orig_pinned {
                            state.write().set_pinned_research(node_id, pinned);
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

                    // Done — mark the blueprint developed. There is no "start research"
                    // step: a Ready (or legacy in-progress) node completes straight from
                    // here, with the same consume-vs-keep choice hideout upgrades use.
                    // Locked nodes show nothing — you can't develop what the game still
                    // gates; a developed node offers only the undo.
                    ui.horizontal(|ui| match status {
                        ResearchStatus::Available | ResearchStatus::InProgress => {
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

                // Full-size icon of the unlocked item, right-aligned and
                // vertically centered, using the card's wide right space.
                if let Some(item) = &unlock {
                    if let Some(tex) = icons.get(ui.ctx(), &item.icon_path) {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add(
                                egui::Image::new(tex)
                                    .fit_to_exact_size(egui::vec2(DETAIL_ICON, DETAIL_ICON)),
                            );
                        });
                    }
                }
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
            scan_alias: None,
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
                // The app docks the detail card in a separate bottom panel, so the
                // test stacks the footer under the tree the same way — otherwise
                // the select-then-interact tests can't find the card's widgets.
                super::ui(ui, &ui_state, &mut pane);
                super::detail_footer(ui, &ui_state, &mut icons, &save_tx, &mut pane);
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
    fn legend_decodes_the_chip_colors() {
        // The chips adopt the hideout's color language, so the pane must ship
        // the key that decodes it (the swatch labels are distinct from the
        // chip/checkbox labels, so these matches are unambiguous).
        let state = fixture();
        let mut h = harness(&state);
        h.run();
        let _ = h.get_by_label("Legend:");
        let _ = h.get_by_label("Tracked");
        let _ = h.get_by_label("Pinned");
    }

    #[test]
    fn develop_unlocks_child_without_auto_tracking_it() {
        let state = fixture();
        let mut h = harness(&state);
        h.run();

        // No "start research" step — a Ready node develops straight from the
        // card via the consume/keep choice.
        h.get_by_label("Root Node\nReady For Research").click();
        h.run();
        h.get_by_label("Developed — keep items").click();
        h.run();
        assert_eq!(
            state
                .read()
                .research_status(&"task.research.a1".to_string()),
            ResearchStatus::Developed
        );
        // The child chip now reads available, but is NOT auto-tracked — pursuing
        // it is an explicit choice (Track / Focus), not a silent side effect.
        let _ = h.get_by_label("Child Node\nReady For Research");
        assert!(!state.read().tracked_research.contains("task.research.a2"));
    }

    #[test]
    fn focus_button_tracks_and_pins_the_whole_chain() {
        let state = fixture();
        let mut h = harness(&state);
        h.run();

        // Focus the child (which has a parent): both it and its prerequisite
        // root get tracked AND pinned in one click.
        h.get_by_label("Child Node\nUnknown Blueprint").click();
        h.run();
        h.get_by_label("Focus this blueprint").click();
        h.run();
        let s = state.read();
        for id in ["task.research.a1", "task.research.a2"] {
            assert!(s.tracked_research.contains(id), "{id} tracked by focus");
            assert!(
                s.is_research_pinned(&id.to_string()),
                "{id} pinned by focus"
            );
        }
    }

    #[test]
    fn track_checkbox_feeds_the_wishlist() {
        let state = fixture();
        let mut h = harness(&state);
        h.run();

        h.get_by_label("Root Node\nReady For Research").click();
        h.run();
        h.get_by_label("Track samples").click();
        h.run();
        assert!(state.read().tracked_research.contains("task.research.a1"));
        let items = state.read().active_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, "oil");
        assert_eq!(items[0].needed, 2);
    }

    #[test]
    fn pin_requires_track_then_prioritizes_samples() {
        // Untracked: the Pin box is disabled — the "track it first" cue.
        let state = fixture();
        let mut h = harness(&state);
        h.run();
        h.get_by_label("Root Node\nReady For Research").click();
        h.run();
        assert!(
            h.get_by_label("Pin").is_disabled(),
            "Pin is disabled until the node is tracked"
        );

        // Track it (set directly so Pin is deterministically enabled next
        // frame), then pinning flags its samples in the overlay aggregation.
        state
            .write()
            .set_tracked_research(&"task.research.a1".to_string(), true);
        h.run();
        h.get_by_label("Pin").click();
        h.run();
        assert!(state
            .read()
            .is_research_pinned(&"task.research.a1".to_string()));
        let items = state.read().active_items();
        let oil = items.iter().find(|i| i.item_id == "oil").expect("oil");
        assert!(oil.pinned, "pinning a tracked node flags its samples");
    }

    #[test]
    fn consume_button_gates_on_owned_samples() {
        let state = fixture();
        let mut h = harness(&state);
        h.run();
        // A Ready node offers the consume path directly (no start step),
        // disabled until every sample is owned.
        h.get_by_label("Root Node\nReady For Research").click();
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
