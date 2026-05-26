//! Top-level egui application.

mod about_dialog;
mod debug_dialog;
mod hideout_pane;
mod icon_cache;
mod overrides_export;
mod preview_pane;
mod settings_dialog;
mod tasks_pane;
pub mod theme;

use crate::data::GameData;
use crate::persist::PersistPaths;
use crate::state::AppState;
use crate::updater::CheckStatus;
use crossbeam_channel::{Receiver, Sender};
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
    show_settings: bool,
    show_debug: bool,
    settings_dirty: bool,
    confirm_reset: bool,
    tasks_filter: String,
    status_banner: Option<String>,
    vr: Arc<crate::vr::Runtime>,
    settings: Arc<RwLock<crate::settings::Settings>>,
    applied_theme: Option<crate::settings::Theme>,
    log_buf: crate::log_buffer::LogBuffer,
    /// One-slot channel from the background update-check thread. `None`
    /// when the user disabled the check in settings or the thread failed
    /// to spawn. Drained on the first frame that sees a value.
    update_rx: Option<Receiver<CheckStatus>>,
    /// Latest update-check status. Starts at `Checking` (or `Disabled` if
    /// the user turned off the check), gets replaced once the worker
    /// thread reports.
    check_status: CheckStatus,
    /// Markdown body of the "Export corrections" modal — `Some` while the
    /// dialog is open. Editable so the user can prepend their own context
    /// before copying to the GitHub issue body.
    export_body: Option<String>,
    /// Pre-computed proposed issue title (shown in the dialog) and the new-
    /// issue URL with `?title=…` pre-filled. Built when the dialog opens so
    /// the labels in the title match the overrides at that moment, even if
    /// the user subsequently edits more recipes.
    export_title: Option<String>,
    export_url: Option<String>,
    /// One-shot confirmation ("Copied. …") that appears under the Copy
    /// button after a successful copy; cleared when the dialog closes.
    export_copy_feedback: Option<String>,
}

impl App {
    // Already a wide constructor before the update_rx addition; folding the
    // args into a builder/config struct is its own refactor.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        state: Arc<RwLock<AppState>>,
        paths: Arc<PersistPaths>,
        save_tx: Sender<SaveTick>,
        vr: Arc<crate::vr::Runtime>,
        settings: Arc<RwLock<crate::settings::Settings>>,
        log_buf: crate::log_buffer::LogBuffer,
        update_rx: Option<Receiver<CheckStatus>>,
    ) -> Self {
        let icons = IconCache::new();
        // Pull any initial warning surfaced by persist::load.
        let banner = state.read().load_warning.clone();
        let check_status = if update_rx.is_some() {
            CheckStatus::Checking
        } else {
            CheckStatus::Disabled
        };
        Self {
            state,
            paths,
            save_tx,
            icons,
            tab: LeftTab::Hideout,
            show_about: false,
            show_settings: false,
            show_debug: false,
            settings_dirty: false,
            confirm_reset: false,
            tasks_filter: String::new(),
            status_banner: banner,
            vr,
            settings,
            applied_theme: None,
            log_buf,
            update_rx,
            check_status,
            export_body: None,
            export_title: None,
            export_url: None,
            export_copy_feedback: None,
        }
    }

    fn data(&self) -> Arc<GameData> {
        self.state.read().data.clone()
    }

    fn notify_save(&self) {
        let v = self.state.read().version;
        let _ = self.save_tx.try_send(SaveTick { version: v });
    }

    fn persist_settings(&self) {
        let snapshot = self.settings.read().clone();
        if let Err(e) = crate::settings::save(&self.paths.settings_file, &snapshot) {
            tracing::warn!(error = %e, "failed to persist settings.json");
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Re-paint at 1Hz so VR status transitions (connect / disconnect)
        // surface even without user input. egui otherwise sleeps until the
        // next event.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));

        let desired_theme = self.settings.read().theme;
        if self.applied_theme != Some(desired_theme) {
            theme::apply(ctx, desired_theme);
            self.applied_theme = Some(desired_theme);
        }

        // Header strip.
        egui::TopBottomPanel::top("header").show(ctx, |ui| self.header(ui));

        self.poll_update_check();
        self.update_banner(ctx);

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
                LeftTab::Hideout => {
                    hideout_pane::ui(ui, &self.state, &mut self.icons, &self.save_tx)
                }
                LeftTab::Tasks => {
                    tasks_pane::ui(ui, &self.state, &mut self.tasks_filter, &self.save_tx)
                }
            });
        });

        // Modal dialogs.
        if self.show_about {
            let data = self.data();
            about_dialog::show(ctx, &mut self.show_about, &data, &self.check_status);
        }
        if self.show_debug {
            debug_dialog::show(ctx, &mut self.show_debug, &self.log_buf);
        }
        if self.confirm_reset {
            self.confirm_reset_dialog(ctx);
        }
        if self.show_settings {
            let outcome = settings_dialog::show(
                ctx,
                &mut self.show_settings,
                &self.settings,
                &self.paths.data_dir,
            );
            if outcome.changed {
                self.settings_dirty = true;
            }
            if outcome.closed && self.settings_dirty {
                self.persist_settings();
                self.settings_dirty = false;
            }
        }
        if let (Some(body), Some(title), Some(url)) = (
            self.export_body.as_mut(),
            self.export_title.as_deref(),
            self.export_url.as_deref(),
        ) {
            let still_open = overrides_export::show_dialog(
                ctx,
                title,
                body,
                url,
                &mut self.export_copy_feedback,
            );
            if !still_open {
                self.export_body = None;
                self.export_title = None;
                self.export_url = None;
                self.export_copy_feedback = None;
            }
        }
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let data_version = self.data().data_version.clone();
            ui.label(
                egui::RichText::new(concat!("EZ Wishlist Overlay v", env!("CARGO_PKG_VERSION")))
                    .strong(),
            );
            self.update_status_indicator(ui);
            ui.separator();
            ui.label(format!("Data: {data_version}"));
            ui.separator();
            let status = self.vr.status();
            ui.colored_label(status.color(), status.label());
            ui.separator();

            if ui.button("Reset progress").clicked() {
                self.confirm_reset = true;
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Settings").clicked() {
                    self.show_settings = true;
                }
                if ui.button("Debug").clicked() {
                    self.show_debug = true;
                }
                if ui.button("About").clicked() {
                    self.show_about = true;
                }
                self.export_corrections_button(ui);
            });
        });
    }

    /// "Export corrections" pops up a modal with the user's per-recipe edits
    /// rendered as a markdown body, ready to be copied into a new GitHub
    /// issue. Disabled when no overrides exist.
    fn export_corrections_button(&mut self, ui: &mut egui::Ui) {
        let count = self.state.read().overrides.len();
        let tooltip = if count == 0 {
            "Edit a recipe first (click an upgrade's \"Edit\" button) to enable this.".to_string()
        } else {
            format!(
                "Show a copy-able markdown summary of your {count} recipe correction(s) \
                 so you can paste them into a new GitHub issue."
            )
        };
        let resp = ui
            .add_enabled(count > 0, egui::Button::new("Export corrections ↗"))
            .on_hover_text(&tooltip)
            .on_disabled_hover_text(&tooltip);
        if resp.clicked() && count > 0 {
            let snapshot = self.state.read();
            self.export_body = Some(overrides_export::build_issue_body(&snapshot));
            self.export_title = Some(overrides_export::build_issue_title(&snapshot));
            self.export_url = Some(overrides_export::build_issue_url(&snapshot));
            self.export_copy_feedback = None;
        }
    }

    /// Compact status chip next to the app name: spinner while the check
    /// thread is in flight, colored glyph + tooltip afterwards. Clickable
    /// — opens the About dialog where the full version comparison lives.
    fn update_status_indicator(&mut self, ui: &mut egui::Ui) {
        let (glyph, color, tooltip): (&str, egui::Color32, String) = match &self.check_status {
            CheckStatus::Disabled => (
                "—",
                ui.visuals().weak_text_color(),
                "Update check disabled in Settings".into(),
            ),
            CheckStatus::Checking => {
                let resp = ui.add(egui::Spinner::new().size(14.0));
                resp.on_hover_text("Checking for updates…");
                return;
            }
            CheckStatus::UpToDate { latest_version } => (
                "✓",
                egui::Color32::from_rgb(80, 180, 100),
                format!("Up to date (latest release: v{latest_version})"),
            ),
            CheckStatus::Ahead { latest_version } => (
                "✓",
                egui::Color32::from_rgb(100, 160, 220),
                format!("Dev build — ahead of latest release v{latest_version}"),
            ),
            CheckStatus::UpdateAvailable(info) => (
                "↑",
                egui::Color32::from_rgb(220, 180, 60),
                format!("Update available: v{}", info.latest_version),
            ),
            CheckStatus::Failed { reason } => (
                "!",
                egui::Color32::from_rgb(220, 100, 90),
                format!("Update check failed: {reason}"),
            ),
        };
        let resp = ui.add(
            egui::Label::new(egui::RichText::new(glyph).color(color).strong())
                .sense(egui::Sense::click()),
        );
        if resp.on_hover_text(tooltip).clicked() {
            self.show_about = true;
        }
    }

    /// Drain the update-check channel. The producer only ever sends once,
    /// so after the first hit we drop the receiver to stop polling. Cheap
    /// regardless — `try_recv` returns Empty immediately.
    fn poll_update_check(&mut self) {
        let Some(rx) = &self.update_rx else { return };
        match rx.try_recv() {
            Ok(status) => {
                self.check_status = status;
                self.update_rx = None;
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                // Sender dropped without producing a value — treat as a
                // failure so the user sees *something* in the header.
                if matches!(self.check_status, CheckStatus::Checking) {
                    self.check_status = CheckStatus::Failed {
                        reason: "check thread exited without reporting".into(),
                    };
                }
                self.update_rx = None;
            }
        }
    }

    fn update_banner(&mut self, ctx: &egui::Context) {
        let CheckStatus::UpdateAvailable(info) = self.check_status.clone() else {
            return;
        };

        // If the user already dismissed this exact version, stay quiet — but
        // a newer release that comes out later will have a different
        // `latest_version` string and will re-show the banner.
        let dismissed = self.settings.read().dismissed_update_version.as_deref()
            == Some(info.latest_version.as_str());
        if dismissed {
            return;
        }

        let current = env!("CARGO_PKG_VERSION");
        let mut clear = false;
        let mut dismiss = false;
        egui::TopBottomPanel::top("update_banner")
            .frame(
                egui::Frame::default()
                    .fill(egui::Color32::from_rgb(30, 90, 50))
                    .inner_margin(8.0),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "Update available: v{} (you have v{}).",
                            info.latest_version, current
                        ))
                        .color(egui::Color32::WHITE)
                        .strong(),
                    );
                    if ui.button("Download ↗").clicked() {
                        let _ = crate::platform::open(&info.release_url);
                    }
                    if ui.small_button("Dismiss").clicked() {
                        dismiss = true;
                        clear = true;
                    }
                    if ui.small_button("Later").clicked() {
                        clear = true;
                    }
                });
            });

        if dismiss {
            self.settings.write().dismissed_update_version = Some(info.latest_version.clone());
            self.persist_settings();
        }
        if clear {
            // User dismissed/snoozed the banner — collapse to an "up to date"-
            // style status so the header indicator doesn't keep advertising
            // an upgrade until the next app start.
            self.check_status = CheckStatus::UpToDate {
                latest_version: info.latest_version.clone(),
            };
        }
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
