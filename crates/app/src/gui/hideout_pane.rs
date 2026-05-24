//! Left tab: hideout upgrades, grouped by module.

use crate::gui::SaveTick;
use crate::state::AppState;
use crossbeam_channel::Sender;
use parking_lot::RwLock;
use std::sync::Arc;

pub fn ui(ui: &mut egui::Ui, state: &Arc<RwLock<AppState>>, save_tx: &Sender<SaveTick>) {
    let data = state.read().data.clone();

    for module in &data.modules {
        egui::CollapsingHeader::new(egui::RichText::new(&module.name).strong())
            .id_salt(&module.id)
            .default_open(false)
            .show(ui, |ui| {
                for upgrade in &module.upgrades {
                    upgrade_row(ui, state, save_tx, &module.name, upgrade);
                }
            });
    }
}

fn upgrade_row(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
    module_name: &str,
    upgrade: &crate::data::Upgrade,
) {
    let (mut tracked, mut done) = {
        let s = state.read();
        (
            s.tracked_upgrades.contains(&upgrade.id),
            s.completed_upgrades.contains(&upgrade.id),
        )
    };
    let original_tracked = tracked;
    let original_done = done;

    ui.horizontal(|ui| {
        ui.checkbox(&mut tracked, "Track");
        ui.checkbox(&mut done, "Done");
        let label = if upgrade.name == module_name {
            format!("L{}", upgrade.level)
        } else {
            format!("L{} — {}", upgrade.level, upgrade.name)
        };
        ui.label(label);
    });

    egui::CollapsingHeader::new(format!("{} requirement(s)", upgrade.requirements.len()))
        .id_salt(format!("{}-reqs", upgrade.id))
        .default_open(false)
        .show(ui, |ui| {
            let s = state.read();
            for req in &upgrade.requirements {
                let name = s
                    .index
                    .items_by_id
                    .get(&req.item_id)
                    .map(|i| i.name.clone())
                    .unwrap_or_else(|| req.item_id.clone());
                ui.label(format!("{}× {}", req.quantity, name));
            }
        });
    ui.add_space(2.0);

    if tracked != original_tracked {
        state.write().set_tracked_upgrade(&upgrade.id, tracked);
        notify(state, save_tx);
    }
    if done != original_done {
        state.write().set_completed_upgrade(&upgrade.id, done);
        notify(state, save_tx);
    }
}

fn notify(state: &Arc<RwLock<AppState>>, save_tx: &Sender<SaveTick>) {
    let v = state.read().version;
    let _ = save_tx.try_send(SaveTick { version: v });
}
