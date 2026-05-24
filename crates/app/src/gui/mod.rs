//! Top-level egui application.

mod about_dialog;
mod hideout_pane;
mod icon_cache;
mod preview_pane;
mod tasks_pane;

use crate::data::GameData;
use crate::persist::PersistPaths;
use crate::state::AppState;
use crossbeam_channel::Sender;
use parking_lot::RwLock;
use std::sync::Arc;

pub use icon_cache::IconCache;

/// Cross-thread message: "user changed state at this version, persist soon".
#[derive(Clone, Copy, Debug)]
pub struct SaveTick {
    pub version: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeftTab {
    Hideout,
    Tasks,
}

pub struct App {
    state: Arc<RwLock<AppState>>,
    paths: Arc<PersistPaths>,
    save_tx: Sender<SaveTick>,
    icons: IconCache,
    tab: LeftTab,
    show_about: bool,
    confirm_reset: bool,
    tasks_filter: String,
    status_banner: Option<String>,
}

impl App {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        state: Arc<RwLock<AppState>>,
        paths: Arc<PersistPaths>,
        save_tx: Sender<SaveTick>,
    ) -> Self {
        let icons = IconCache::new();
        // Pull any initial warning surfaced by persist::load.
        let banner = state.read().load_warning.clone();
        Self {
            state,
            paths,
            save_tx,
            icons,
            tab: LeftTab::Hideout,
            show_about: false,
            confirm_reset: false,
            tasks_filter: String::new(),
            status_banner: banner,
        }
    }

    fn data(&self) -> Arc<GameData> {
        self.state.read().data.clone()
    }

    fn notify_save(&self) {
        let v = self.state.read().version;
        let _ = self.save_tx.try_send(SaveTick { version: v });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Header strip.
        egui::TopBottomPanel::top("header").show(ctx, |ui| self.header(ui));

        if let Some(msg) = &self.status_banner.clone() {
            egui::TopBottomPanel::top("banner")
                .frame(
                    egui::Frame::default()
                        .fill(egui::Color32::from_rgb(120, 80, 0))
                        .inner_margin(8.0),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(msg).color(egui::Color32::WHITE));
                        if ui.small_button("Dismiss").clicked() {
                            self.status_banner = None;
                        }
                    });
                });
        }

        // Right preview pane.
        egui::SidePanel::right("preview")
            .resizable(true)
            .default_width(480.0)
            .min_width(320.0)
            .show(ctx, |ui| {
                preview_pane::ui(ui, &self.state, &mut self.icons, &self.save_tx);
            });

        // Left main panel: tab strip + content.
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, LeftTab::Hideout, "Hideout");
                ui.selectable_value(&mut self.tab, LeftTab::Tasks, "Tasks");
            });
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                LeftTab::Hideout => hideout_pane::ui(ui, &self.state, &self.save_tx),
                LeftTab::Tasks => {
                    tasks_pane::ui(ui, &self.state, &mut self.tasks_filter, &self.save_tx)
                }
            });
        });

        // Modal dialogs.
        if self.show_about {
            let data = self.data();
            about_dialog::show(ctx, &mut self.show_about, &data);
        }
        if self.confirm_reset {
            self.confirm_reset_dialog(ctx);
        }
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let data_version = self.data().data_version.clone();
            ui.label(egui::RichText::new("EZ Wishlist Overlay").strong());
            ui.separator();
            ui.label(format!("Data: {data_version}"));
            ui.separator();
            // VR status placeholder (Phase 3 will wire this up).
            ui.colored_label(egui::Color32::GRAY, "VR: not wired");
            ui.separator();

            if ui.button("Open data folder").clicked() {
                let _ = open_in_explorer(&self.paths.data_dir);
            }
            if ui.button("About").clicked() {
                self.show_about = true;
            }
            if ui.button("Reset progress").clicked() {
                self.confirm_reset = true;
            }
        });
    }

    fn confirm_reset_dialog(&mut self, ctx: &egui::Context) {
        egui::Window::new("Reset all progress?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("This clears every tracked upgrade, completed marker, and collected count. This cannot be undone.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.confirm_reset = false;
                    }
                    if ui
                        .add(egui::Button::new("Reset everything").fill(egui::Color32::DARK_RED))
                        .clicked()
                    {
                        self.state.write().reset_all();
                        self.notify_save();
                        self.confirm_reset = false;
                    }
                });
            });
    }
}

fn open_in_explorer(path: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
}
