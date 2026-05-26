//! Build a markdown diff of the user's recipe overrides against the bundled
//! dataset, and show it in a modal so the user can copy-paste it into a new
//! GitHub issue manually. A pre-stuffed `?body=…` URL was tempting but
//! GitHub silently truncates long querystrings, so we keep the text in the
//! app and let the user paste it themselves.

use crate::data::{base_slots, RecipeOverride, Requirement};
use crate::state::AppState;

const NEW_ISSUE_URL: &str = "https://github.com/etiennechabert/ez-wishlist-overlay/issues/new";

/// A short, descriptive title for the GitHub issue. Lists up to two affected
/// upgrades by name; collapses to "+N more" once it would otherwise sprawl.
pub fn build_issue_title(state: &AppState) -> String {
    let mut labels: Vec<String> = state
        .overrides
        .keys()
        .filter_map(|id| {
            let uref = state.index.upgrades_by_id.get(id)?;
            Some(format!("{} Lv{}", uref.module_name, uref.upgrade.level))
        })
        .collect();
    labels.sort();

    match labels.len() {
        0 => "Recipe corrections from in-app editor".to_string(),
        1 => format!("Recipe correction: {}", labels[0]),
        2 => format!("Recipe corrections: {}, {}", labels[0], labels[1]),
        n => format!(
            "Recipe corrections: {}, {} (+{} more)",
            labels[0],
            labels[1],
            n - 2
        ),
    }
}

/// `https://github.com/.../issues/new?title=…` — only the title rides in the
/// URL (it's short and safe); the body still goes via the clipboard since
/// GitHub silently truncates long querystrings.
pub fn build_issue_url(state: &AppState) -> String {
    format!(
        "{NEW_ISSUE_URL}?title={}",
        encode_query_component(&build_issue_title(state))
    )
}

fn encode_query_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

pub fn build_issue_body(state: &AppState) -> String {
    let mut out = String::new();
    out.push_str(
        "These corrections were exported from the in-app recipe editor. They are \
         based on what I observed in-game, not on the wiki — please verify before \
         merging.\n\n",
    );
    out.push_str(&format!(
        "Data version: `{}`\nApp version: `{}`\n\n",
        state.data.data_version,
        env!("CARGO_PKG_VERSION"),
    ));

    // Stable ordering: by module name, then upgrade level — easier to diff
    // across multiple users' reports.
    let mut entries: Vec<(String, String, u32, &RecipeOverride)> = state
        .overrides
        .iter()
        .filter_map(|(id, ov)| {
            let uref = state.index.upgrades_by_id.get(id)?;
            Some((uref.module_name.clone(), id.clone(), uref.upgrade.level, ov))
        })
        .collect();
    entries.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.1.cmp(&b.1))
    });

    for (module_name, upgrade_id, level, ov) in entries {
        let uref = state.index.upgrades_by_id.get(&upgrade_id).unwrap();
        let base = base_slots(&uref.upgrade);
        out.push_str(&format!("## {module_name} Lv{level} — `{upgrade_id}`\n\n"));
        // ```diff``` fence: GitHub renders `-` red and `+` green, and the
        // monospace also makes the in-app preview scannable.
        out.push_str("```diff\n");
        for (slot_idx, (base_slot, mine_slot)) in base.iter().zip(ov.slots.iter()).enumerate() {
            let official = render_slot(base_slot, state);
            let mine = render_slot(mine_slot, state);
            let label = slot_idx + 1;
            if official == mine {
                out.push_str(&format!("  Slot {label}: {official}\n"));
            } else {
                out.push_str(&format!("- Slot {label}: {official}\n"));
                out.push_str(&format!("+ Slot {label}: {mine}\n"));
            }
        }
        out.push_str("```\n\n");
    }

    out
}

fn render_slot(slot: &Option<Requirement>, state: &AppState) -> String {
    match slot {
        None => "(empty)".to_string(),
        Some(req) => {
            let name = state
                .index
                .items_by_id
                .get(&req.item_id)
                .map(|i| i.name.clone())
                .unwrap_or_else(|| req.item_id.clone());
            format!("{name} × {} (`{}`)", req.quantity, req.item_id)
        }
    }
}

/// Centered modal showing the proposed issue title and a copy-able markdown
/// body. "Copy to clipboard" copies the body; "Open GitHub Issues ↗" opens
/// the new-issue page with the title pre-filled (paste the body in from the
/// clipboard). Returns `true` while the dialog should stay open.
pub fn show_dialog(
    ctx: &egui::Context,
    title: &str,
    body: &mut String,
    issue_url: &str,
    copy_feedback: &mut Option<String>,
) -> bool {
    let mut open = true;
    let mut close = false;
    egui::Window::new("Export corrections")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([760.0, 540.0])
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(
                "Open GitHub Issues to file a new report with the title \
                 pre-filled, then paste the body from your clipboard.",
            );
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Title:").strong());
                ui.label(egui::RichText::new(title).monospace());
            });
            ui.add_space(4.0);

            ui.label(egui::RichText::new("Body:").strong());
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(body)
                            .desired_width(f32::INFINITY)
                            .desired_rows(16)
                            .font(egui::TextStyle::Monospace),
                    );
                });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Copy body").clicked() {
                    ui.ctx().copy_text(body.clone());
                    *copy_feedback = Some("Copied. Paste into the GitHub issue body.".into());
                }
                if ui.button("Open GitHub Issues ↗").clicked() {
                    if let Err(e) = crate::platform::open(issue_url) {
                        tracing::warn!(error = %e, "failed to open new-issue URL");
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });

            if let Some(msg) = copy_feedback.as_deref() {
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(msg)
                        .small()
                        .color(egui::Color32::from_rgb(80, 180, 100)),
                );
            }
        });

    open && !close
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{GameData, HideoutModule, Item, Upgrade, RECIPE_SLOTS};
    use std::sync::Arc;

    fn fixture() -> Arc<GameData> {
        Arc::new(GameData {
            data_version: "test-1".into(),
            scraped_at: "now".into(),
            source_repo: "test".into(),
            source_commit: "deadbeef".into(),
            modules: vec![HideoutModule {
                id: "kitchen".into(),
                name: "Kitchen".into(),
                upgrades: vec![Upgrade {
                    id: "kitchen_lv1".into(),
                    name: "Kitchen".into(),
                    level: 1,
                    description: String::new(),
                    requirements: vec![Requirement {
                        item_id: "nut".into(),
                        quantity: 5,
                    }],
                }],
            }],
            vendors: vec![],
            items: vec![Item {
                id: "nut".into(),
                name: "Nut".into(),
                icon_path: String::new(),
            }],
        })
    }

    #[test]
    fn body_lists_each_overridden_slot_difference() {
        let mut state = AppState::new(fixture());
        let mut ov = RecipeOverride {
            slots: std::array::from_fn(|_| None),
        };
        ov.slots[0] = Some(Requirement {
            item_id: "nut".into(),
            quantity: 3,
        });
        state.set_recipe_override(&"kitchen_lv1".to_string(), ov);

        let body = build_issue_body(&state);
        assert!(body.contains("Kitchen Lv1"));
        assert!(body.contains("`kitchen_lv1`"));
        // Diff fence so GitHub renders it with red/green highlighting.
        assert!(body.contains("```diff"), "diff fence missing");
        // The official slot 0 should show on a `-` line, the user's on `+`.
        assert!(
            body.contains("- Slot 1: Nut × 5"),
            "official slot 0 should be a removed line"
        );
        assert!(
            body.contains("+ Slot 1: Nut × 3"),
            "user's slot 0 should be an added line"
        );
    }

    #[test]
    fn title_picks_module_names_for_small_counts() {
        let mut data = (*fixture()).clone();
        data.modules.push(HideoutModule {
            id: "armory".into(),
            name: "Armory".into(),
            upgrades: vec![Upgrade {
                id: "armory_lv2".into(),
                name: "Armory".into(),
                level: 2,
                description: String::new(),
                requirements: vec![],
            }],
        });
        let mut state = AppState::new(Arc::new(data));
        let one_nut: [Option<Requirement>; RECIPE_SLOTS] = {
            let mut s: [Option<Requirement>; RECIPE_SLOTS] = std::array::from_fn(|_| None);
            s[0] = Some(Requirement {
                item_id: "nut".into(),
                quantity: 9,
            });
            s
        };

        // 1 override → single-name title.
        state.set_recipe_override(
            &"kitchen_lv1".to_string(),
            RecipeOverride {
                slots: one_nut.clone(),
            },
        );
        assert_eq!(build_issue_title(&state), "Recipe correction: Kitchen Lv1");

        // 2 overrides → both named, alphabetical order.
        state.set_recipe_override(
            &"armory_lv2".to_string(),
            RecipeOverride {
                slots: one_nut.clone(),
            },
        );
        assert_eq!(
            build_issue_title(&state),
            "Recipe corrections: Armory Lv2, Kitchen Lv1"
        );
    }

    #[test]
    fn title_collapses_when_three_or_more() {
        let mut data = (*fixture()).clone();
        for (id, name) in [("a_lv1", "Aaa"), ("b_lv1", "Bbb"), ("c_lv1", "Ccc")] {
            data.modules.push(HideoutModule {
                id: id.into(),
                name: name.into(),
                upgrades: vec![Upgrade {
                    id: id.into(),
                    name: name.into(),
                    level: 1,
                    description: String::new(),
                    requirements: vec![],
                }],
            });
        }
        let mut state = AppState::new(Arc::new(data));
        let stub_slots: [Option<Requirement>; RECIPE_SLOTS] = {
            let mut s: [Option<Requirement>; RECIPE_SLOTS] = std::array::from_fn(|_| None);
            s[0] = Some(Requirement {
                item_id: "nut".into(),
                quantity: 1,
            });
            s
        };
        for id in ["a_lv1", "b_lv1", "c_lv1"] {
            state.set_recipe_override(
                &id.to_string(),
                RecipeOverride {
                    slots: stub_slots.clone(),
                },
            );
        }
        assert_eq!(
            build_issue_title(&state),
            "Recipe corrections: Aaa Lv1, Bbb Lv1 (+1 more)"
        );
    }

    #[test]
    fn url_carries_encoded_title_only() {
        let mut state = AppState::new(fixture());
        state.set_recipe_override(
            &"kitchen_lv1".to_string(),
            RecipeOverride {
                slots: {
                    let mut s: [Option<Requirement>; RECIPE_SLOTS] = std::array::from_fn(|_| None);
                    s[0] = Some(Requirement {
                        item_id: "nut".into(),
                        quantity: 3,
                    });
                    s
                },
            },
        );
        let url = build_issue_url(&state);
        assert!(url.starts_with(NEW_ISSUE_URL));
        assert!(url.contains("?title="));
        // Body must not be smuggled into the URL — long querystrings get
        // silently truncated by GitHub, which is exactly the regression we
        // moved away from.
        assert!(!url.contains("&body="));
        // Spaces and colons in the title must be percent-encoded.
        assert!(url.contains("%20") || url.contains("Recipe"));
    }

    #[test]
    fn body_groups_modules_with_stable_ordering() {
        // Two modules, two overrides each — the body should sort by module
        // then by level so users diffing each other's reports see the same
        // line ordering.
        let mut data = (*fixture()).clone();
        data.modules.push(HideoutModule {
            id: "armory".into(),
            name: "Armory".into(),
            upgrades: vec![Upgrade {
                id: "armory_lv1".into(),
                name: "Armory".into(),
                level: 1,
                description: String::new(),
                requirements: vec![Requirement {
                    item_id: "nut".into(),
                    quantity: 1,
                }],
            }],
        });
        let mut state = AppState::new(Arc::new(data));

        let one_nut: [Option<Requirement>; RECIPE_SLOTS] = {
            let mut s: [Option<Requirement>; RECIPE_SLOTS] = std::array::from_fn(|_| None);
            s[0] = Some(Requirement {
                item_id: "nut".into(),
                quantity: 9,
            });
            s
        };
        state.set_recipe_override(
            &"kitchen_lv1".to_string(),
            RecipeOverride {
                slots: one_nut.clone(),
            },
        );
        state.set_recipe_override(&"armory_lv1".to_string(), RecipeOverride { slots: one_nut });

        let body = build_issue_body(&state);
        let armory_idx = body.find("Armory Lv1").unwrap();
        let kitchen_idx = body.find("Kitchen Lv1").unwrap();
        assert!(
            armory_idx < kitchen_idx,
            "Armory should sort before Kitchen alphabetically"
        );
    }
}
