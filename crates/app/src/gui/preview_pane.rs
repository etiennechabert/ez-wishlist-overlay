//! Right pane: aggregated active-items view, mirrors what the VR overlay shows.

use crate::gui::{icon_cache::IconCache, theme, SaveTick};
use crate::state::AppState;
use crossbeam_channel::Sender;
use parking_lot::RwLock;
use std::sync::Arc;

const ICON_SIZE: f32 = 32.0;

pub fn ui(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
) {
    let active = state.read().active_items();
    let weak = ui.visuals().weak_text_color();

    ui.heading("Active items");
    ui.label(
        egui::RichText::new("Aggregated across every tracked upgrade and task.")
            .small()
            .color(weak),
    );
    ui.separator();

    if active.is_empty() {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.label("Nothing tracked yet.");
            ui.label(
                egui::RichText::new("Open the Hideout or Tasks tab and check 'Track' on a row.")
                    .small()
                    .color(weak),
            );
        });
        return;
    }

    egui::ScrollArea::vertical().show(ui, |ui| {
        for item in &active {
            row(ui, state, icons, save_tx, item);
        }
    });
}

fn row(
    ui: &mut egui::Ui,
    state: &Arc<RwLock<AppState>>,
    icons: &mut IconCache,
    save_tx: &Sender<SaveTick>,
    item: &crate::state::ActiveItem,
) {
    let dark = ui.visuals().dark_mode;
    let strong_text = ui.visuals().strong_text_color();
    let done = item.collected >= item.needed && item.needed > 0;
    let frame_fill = if done {
        theme::done_frame_fill(dark)
    } else {
        egui::Color32::TRANSPARENT
    };

    egui::Frame::default()
        .fill(frame_fill)
        .inner_margin(egui::Margin::same(6.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let texture = icons.get(ui.ctx(), &item.icon_path);
                if let Some(tex) = texture {
                    ui.add(
                        egui::Image::new(tex).fit_to_exact_size(egui::vec2(ICON_SIZE, ICON_SIZE)),
                    );
                } else {
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(ICON_SIZE, ICON_SIZE),
                        egui::Sense::hover(),
                    );
                    ui.painter()
                        .rect_filled(rect, 4.0, theme::placeholder_icon(dark));
                }
                ui.vertical(|ui| {
                    let name_color = if done {
                        theme::done_text(dark)
                    } else {
                        strong_text
                    };
                    ui.label(egui::RichText::new(&item.name).color(name_color));

                    ui.horizontal(|ui| {
                        let progress = if item.needed == 0 {
                            0.0
                        } else {
                            (item.collected as f32 / item.needed as f32).clamp(0.0, 1.0)
                        };
                        ui.add(egui::ProgressBar::new(progress).desired_width(160.0).text(
                            format!("{} / {}", item.collected.min(item.needed), item.needed),
                        ));

                        if ui.small_button("-").clicked() {
                            state.write().adjust_collected(&item.item_id, -1);
                            notify(state, save_tx);
                        }
                        let mut value = item.collected;
                        if ui
                            .add(egui::DragValue::new(&mut value).range(0..=9999).speed(0.05))
                            .changed()
                        {
                            state.write().set_collected(&item.item_id, value);
                            notify(state, save_tx);
                        }
                        if ui.small_button("+").clicked() {
                            state.write().adjust_collected(&item.item_id, 1);
                            notify(state, save_tx);
                        }
                    });

                    if !item.sources.is_empty() {
                        ui.label(
                            egui::RichText::new(format!("↳ {}", item.sources.join(" • ")))
                                .small()
                                .color(theme::source_text(dark)),
                        );
                    }
                });
            });
        });
    ui.add_space(2.0);
}

fn notify(state: &Arc<RwLock<AppState>>, save_tx: &Sender<SaveTick>) {
    let v = state.read().version;
    let _ = save_tx.try_send(SaveTick { version: v });
}
