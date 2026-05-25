//! About / Credits modal.

use crate::data::GameData;

pub fn show(ctx: &egui::Context, open: &mut bool, data: &GameData) {
    let version = env!("CARGO_PKG_VERSION");
    let commit_short: String = data.source_commit.chars().take(7).collect();

    let mut close_now = false;
    egui::Window::new("About EZ Wishlist Overlay")
        .open(open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.heading(format!("EZ Wishlist Overlay v{version}"));
            ui.label(format!(
                "Data version: {} (synced from upstream {})",
                data.data_version, commit_short
            ));
            ui.add_space(8.0);
            ui.label("A free, open-source companion for Contractors Showdown: ExfilZone.");
            ui.label("Tracks hideout upgrades and quest tasks across desktop and VR.");

            ui.add_space(12.0);
            ui.heading("Credits");
            ui.label(
                "Hideout, task, and item data are sourced from ExfilZone Assistant by pogapwnz, \
                 used under the MIT license. ExfilZone Assistant is an excellent web companion \
                 covering combat simulators, weapon databases, guides, and more. If you find \
                 this app useful, check theirs too.",
            );
            ui.horizontal(|ui| {
                if ui.button("Open ExfilZone Assistant ↗").clicked() {
                    let _ = crate::platform::open("https://www.exfil-zone-assistant.app/");
                }
                if ui.button("Support pogapwnz on Ko-fi ↗").clicked() {
                    let _ = crate::platform::open("https://ko-fi.com/J3J41GATK0");
                }
            });
            ui.label("Game by Caveman Studio.");

            ui.add_space(12.0);
            ui.heading("License");
            ui.label(
                "EZ Wishlist Overlay is open source. ExfilZone Assistant data and icons are used \
                 under MIT — see LICENSES/exfil-zone-assistant-MIT.txt for the full text.\n\
                 Game content © Caveman Studio.",
            );

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("GitHub ↗").clicked() {
                    let _ = crate::platform::open(
                        "https://github.com/etiennechabert/ez-wishlist-overlay",
                    );
                }
                if ui.button("Report an issue ↗").clicked() {
                    let _ = crate::platform::open(
                        "https://github.com/etiennechabert/ez-wishlist-overlay/issues",
                    );
                }
                if ui.button("Close").clicked() {
                    close_now = true;
                }
            });
        });
    if close_now {
        *open = false;
    }
}
