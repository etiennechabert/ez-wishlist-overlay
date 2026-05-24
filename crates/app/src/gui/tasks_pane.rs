//! Left tab: quest tasks, grouped by vendor, with name-search filter.

use crate::gui::SaveTick;
use crate::state::AppState;
use crossbeam_channel::Sender;
use parking_lot::RwLock;
use std::sync::Arc;

pub fn ui(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    filter: &mut String,
    save_tx: &Sender<SaveTick>,
) {
    let data = state.read().data.clone();

    ui.horizontal(|ui| {
        ui.label("Filter:");
        ui.text_edit_singleline(filter);
        if ui.button("✕").clicked() {
            filter.clear();
        }
    });
    ui.separator();

    let filter_lc = filter.to_lowercase();
    let filter_active = !filter_lc.is_empty();

    for vendor in &data.vendors {
        let matching_tasks: Vec<_> = vendor
            .tasks
            .iter()
            .filter(|t| {
                if !filter_active {
                    return true;
                }
                t.name.to_lowercase().contains(&filter_lc)
                    || t.id.to_lowercase().contains(&filter_lc)
            })
            .collect();
        if matching_tasks.is_empty() {
            continue;
        }
        egui::CollapsingHeader::new(format!(
            "{}  ({} task{})",
            vendor.name,
            matching_tasks.len(),
            if matching_tasks.len() == 1 { "" } else { "s" }
        ))
        .id_salt(&vendor.id)
        .default_open(filter_active)
        .show(ui, |ui| {
            for task in matching_tasks {
                task_row(ui, state, save_tx, task);
            }
        });
    }
}

fn task_row(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    save_tx: &Sender<SaveTick>,
    task: &crate::data::Task,
) {
    let (mut tracked, mut done) = {
        let s = state.read();
        (
            s.tracked_tasks.contains(&task.id),
            s.completed_tasks.contains(&task.id),
        )
    };
    let original_tracked = tracked;
    let original_done = done;

    ui.horizontal(|ui| {
        ui.checkbox(&mut tracked, "Track");
        ui.checkbox(&mut done, "Done");
        ui.label(&task.name);
        ui.add_space(8.0);
        if ui.small_button("Open ↗").clicked() {
            let _ = webbrowser_open(&task.source_url);
        }
    });

    if !task.prerequisites.is_empty() {
        let s = state.read();
        let names: Vec<String> = task
            .prerequisites
            .iter()
            .map(|pid| {
                s.index
                    .tasks_by_id
                    .get(pid)
                    .map(|t| t.task.name.clone())
                    .unwrap_or_else(|| pid.clone())
            })
            .collect();
        ui.label(
            egui::RichText::new(format!("Requires: {}", names.join(", ")))
                .small()
                .color(egui::Color32::GRAY),
        );
    }

    egui::CollapsingHeader::new(format!("{} requirement(s)", task.requirements.len()))
        .id_salt(format!("{}-reqs", task.id))
        .default_open(false)
        .show(ui, |ui| {
            let s = state.read();
            for req in &task.requirements {
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
        state.write().set_tracked_task(&task.id, tracked);
        notify(state, save_tx);
    }
    if done != original_done {
        state.write().set_completed_task(&task.id, done);
        notify(state, save_tx);
    }
}

fn notify(state: &Arc<RwLock<AppState>>, save_tx: &Sender<SaveTick>) {
    let v = state.read().version;
    let _ = save_tx.try_send(SaveTick { version: v });
}

fn webbrowser_open(url: &str) -> std::io::Result<()> {
    // Use the Windows shell to open the user's default browser.
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map(|_| ())
}
