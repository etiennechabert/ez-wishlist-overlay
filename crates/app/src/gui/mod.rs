//! Top-level egui application.

mod about_dialog;
mod containers_pane;
mod debug_dialog;
mod hideout_pane;
mod icon_cache;
mod items_db_pane;
pub mod ocr_feedback;
mod overrides_export;
mod preview_pane;
mod settings_dialog;
pub mod theme;

pub use ocr_feedback::{OcrFeedback, OcrFeedbackKind, OcrItemDelta};

use crate::data::GameData;
use crate::persist::PersistPaths;
use crate::state::AppState;
use crate::updater::CheckStatus;
use crate::vr::runtime::CaptureResult;
use crossbeam_channel::{Receiver, Sender};
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Instant;

pub use icon_cache::IconCache;

/// Cross-thread message: "user changed state at this version, persist soon".
#[derive(Clone, Copy, Debug)]
pub struct SaveTick {
    pub version: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeftTab {
    Hideout,
    Containers,
    ItemsDb,
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
    items_db: items_db_pane::ItemsDbState,
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
    /// Latest release version, set once the check reports a *newer* release
    /// (`CheckStatus::UpdateAvailable`) and never cleared thereafter. Kept
    /// separate from `check_status` because `update_banner` collapses that
    /// back to `UpToDate` when the user dismisses/snoozes the banner — and
    /// the "Export corrections" gate must stay firm for the whole session,
    /// not lift the moment an unrelated banner is dismissed.
    update_available: Option<String>,
    /// In-app MSI update lifecycle (download → verify → launch installer).
    /// `Idle` until the user clicks "Update now"; the download worker streams
    /// progress over [`Self::apply_rx`]. Separate from `check_status`, which
    /// only ever reflects the *check*, not the *apply*.
    update_apply: crate::updater::UpdateApplyStatus,
    /// Channel from the download worker — `Some` once "Update now" is clicked,
    /// cleared on the first terminal state. Drained each frame by
    /// `poll_update_apply`.
    apply_rx: Option<Receiver<crate::updater::UpdateApplyStatus>>,
    /// Whether this build was installed via the MSI (so it can self-apply
    /// updates) or is a portable exe (browser-link only). Computed once at
    /// startup; see [`crate::platform::install_kind`].
    install_kind: crate::platform::InstallKind,
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
    /// Most recent VR-screenshot result, drained from `vr` once per frame.
    /// Kept around (rather than re-drained) so the debug dialog can keep
    /// showing the last path/error even after the toast fades. Replaced on
    /// each new capture.
    last_capture: Option<CaptureResult>,
    /// When the centred capture-confirmation toast first appeared. `None`
    /// means no toast is showing right now. Reset to `Some(now)` every time
    /// `last_capture` is replaced.
    capture_toast_shown_at: Option<Instant>,
    /// Clone of the channel that feeds the OCR worker. Used by the
    /// debug-build "Run OCR on fixture" button so we can exercise the
    /// pipeline + in-headset overlay without a SteamVR session.
    ocr_job_tx: Sender<crate::ocr::OcrJob>,
    /// One-shot guard flag: has the startup window-geometry sanity check run?
    /// `eframe`'s `persist_window` can restore an unusable geometry (e.g. a
    /// window sized across two monitors, saved while maximized on a
    /// multi-monitor rig) that comes up blank on some GPU/driver combos and
    /// reads as "the app won't start". Checked once on the first frame that
    /// has viewport info; see [`App::guard_window_geometry`].
    window_geometry_checked: bool,
}

impl App {
    // Already a wide constructor before the update_rx addition; folding the
    // args into a builder/config struct is its own refactor.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        state: Arc<RwLock<AppState>>,
        paths: Arc<PersistPaths>,
        save_tx: Sender<SaveTick>,
        vr: Arc<crate::vr::Runtime>,
        settings: Arc<RwLock<crate::settings::Settings>>,
        log_buf: crate::log_buffer::LogBuffer,
        update_rx: Option<Receiver<CheckStatus>>,
        ocr_job_tx: Sender<crate::ocr::OcrJob>,
    ) -> Self {
        // Extend egui's default Proportional fallback chain with Hack.
        // Ubuntu-Light (the proportional primary) doesn't cover most of the
        // U+21xx arrows block — including ↳ used in the preview pane source
        // list — and neither do the NotoEmoji or emoji-icon-font fallbacks,
        // so those characters render as missing-glyph tofu. Hack DOES carry
        // the full arrows block (it's already the Monospace primary, just
        // not wired into Proportional by default); appending it last means
        // text still picks Ubuntu-Light first for the body characters and
        // only falls back to Hack for the few glyphs nothing else provides.
        let mut fonts = egui::FontDefinitions::default();
        if let Some(prop) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            prop.push("Hack".to_owned());
        }
        cc.egui_ctx.set_fonts(fonts);

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
            items_db: items_db_pane::ItemsDbState::default(),
            status_banner: banner,
            vr,
            settings,
            applied_theme: None,
            log_buf,
            update_rx,
            check_status,
            update_available: None,
            update_apply: crate::updater::UpdateApplyStatus::Idle,
            apply_rx: None,
            install_kind: crate::platform::install_kind(),
            export_body: None,
            export_title: None,
            export_url: None,
            export_copy_feedback: None,
            last_capture: None,
            capture_toast_shown_at: None,
            ocr_job_tx,
            window_geometry_checked: false,
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

    /// One-shot startup guard against an unusable restored window geometry.
    /// `eframe`'s `persist_window` faithfully restores whatever size/position
    /// was saved last — including a window sized across *two* monitors (saved
    /// while maximized on a multi-monitor rig). On some GPU/driver combos such
    /// a window comes up blank, so the app looks like it "won't start" when in
    /// fact only the desktop window is unusable. If the restored window is
    /// larger than the monitor it's on (i.e. it spans beyond one screen), snap
    /// it back to a sane default near the top-left so it's visible again.
    ///
    /// Returns `true` once it had viewport info to evaluate (so the caller
    /// stops re-checking); `false` on the first frame(s) before eframe has
    /// populated `outer_rect` / `monitor_size`.
    fn guard_window_geometry(&self, ctx: &egui::Context) -> bool {
        let (outer, monitor) = ctx.input(|i| {
            let vp = i.viewport();
            (vp.outer_rect, vp.monitor_size)
        });
        let (Some(outer), Some(monitor)) = (outer, monitor) else {
            return false;
        };
        // Slop covers a normal maximized window, whose borders sit a few px
        // past each monitor edge — that must not trip the guard.
        const SLOP: f32 = 16.0;
        let spans_beyond_monitor =
            outer.width() > monitor.x + SLOP || outer.height() > monitor.y + SLOP;
        if spans_beyond_monitor {
            let size = egui::vec2(
                1200.0_f32.min(monitor.x - 80.0),
                800.0_f32.min(monitor.y - 80.0),
            );
            tracing::warn!(
                restored_w = outer.width(),
                restored_h = outer.height(),
                monitor_w = monitor.x,
                monitor_h = monitor.y,
                "restored window spans beyond the current monitor; resetting to a visible default"
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(40.0, 40.0)));
        }
        true
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Re-paint at 1Hz so VR status transitions (connect / disconnect)
        // surface even without user input. egui otherwise sleeps until the
        // next event.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));

        // One-shot: rescue an unusable restored window geometry before it
        // reads as "the app won't start". See the field + method docs.
        if !self.window_geometry_checked && self.guard_window_geometry(ctx) {
            self.window_geometry_checked = true;
        }

        // Spacebar = "take a VR screenshot" while the desktop window has
        // focus. Clicking a button is impractical with the headset on, so
        // this hotkey lets the user trigger a capture from inside VR
        // (looking down at the desktop monitor — or, more usefully,
        // pressing space on the keyboard before putting the headset back
        // on). The `wants_keyboard_input` guard avoids stealing spaces
        // from any focused text input.
        if !ctx.wants_keyboard_input()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space))
        {
            self.vr.request_screenshot();
        }

        // Drain the VR worker's latest capture result once per frame and
        // stash it locally. Both the centred toast and the debug-dialog
        // status line read from `self.last_capture`, so the runtime's slot
        // is consumed exactly once per result.
        if let Some(result) = self.vr.take_last_capture() {
            self.last_capture = Some(result);
            self.capture_toast_shown_at = Some(Instant::now());
        }
        self.render_capture_toast(ctx);

        // OCR feedback now renders in the headset via the second
        // SteamVR overlay (see vr::ocr_render + vr::runtime). The
        // worker sends OcrFeedback messages to the VR thread; nothing
        // surfaces on the desktop except the tracing logs.

        let (desired_theme, desired_scheme) = {
            let s = self.settings.read();
            (s.theme, s.color_scheme)
        };
        // Status-color palette: cheap thread-local write, refreshed every frame
        // so toggling it in Settings recolors the panes immediately. The visual
        // (dark/light) is heavier and only re-applied when it actually changes.
        theme::set_scheme(desired_scheme);
        if self.applied_theme != Some(desired_theme) {
            theme::apply(ctx, desired_theme);
            self.applied_theme = Some(desired_theme);
        }

        // Header strip.
        egui::TopBottomPanel::top("header").show(ctx, |ui| self.header(ui));

        self.poll_update_check();
        self.poll_update_apply(ctx);
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

        // Right preview pane. Hidden on the Items DB and Containers tabs —
        // both are management/reference views, not tracked-progress views, so
        // the per-tracked-item aggregation that lives on the right would just
        // be noise next to them. The Items DB tab has its own "tracked only"
        // toggle for users who want to narrow to the active set.
        if !matches!(self.tab, LeftTab::ItemsDb | LeftTab::Containers) {
            egui::SidePanel::right("preview")
                .resizable(true)
                .default_width(480.0)
                .min_width(320.0)
                .show(ctx, |ui| {
                    let outcome = preview_pane::ui(
                        ui,
                        &self.state,
                        &self.settings,
                        &mut self.icons,
                        &self.save_tx,
                    );
                    if outcome.settings_changed {
                        self.persist_settings();
                    }
                });
        }

        // Left main panel: tab strip + content.
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, LeftTab::Hideout, "Hideout");
                ui.selectable_value(&mut self.tab, LeftTab::Containers, "Containers");
                ui.selectable_value(&mut self.tab, LeftTab::ItemsDb, "Items DB");
            });
            ui.separator();
            match self.tab {
                LeftTab::Hideout => {
                    let outcome = egui::ScrollArea::vertical()
                        .show(ui, |ui| {
                            hideout_pane::ui(
                                ui,
                                &self.state,
                                &self.settings,
                                &mut self.icons,
                                &self.save_tx,
                            )
                        })
                        .inner;
                    if outcome.settings_changed {
                        self.persist_settings();
                    }
                }
                LeftTab::Containers => {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        containers_pane::ui(ui, &self.state, &mut self.icons, &self.save_tx);
                    });
                }
                LeftTab::ItemsDb => {
                    // TableBuilder ships its own vertical scrolling; nesting
                    // a ScrollArea around it breaks sticky headers and the
                    // virtualized row layout.
                    items_db_pane::ui(ui, &self.state, &mut self.icons, &mut self.items_db);
                }
            }
        });

        // Modal dialogs.
        if self.show_about {
            let data = self.data();
            about_dialog::show(ctx, &mut self.show_about, &data, &self.check_status);
        }
        if self.show_debug {
            debug_dialog::show(
                ctx,
                &mut self.show_debug,
                &self.log_buf,
                &self.vr,
                self.last_capture.as_ref(),
                &self.ocr_job_tx,
            );
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

            self.auto_capture_toggle(ui);
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

    /// Prominent main-window toggle for the auto-capture loop. Loud when
    /// ON (and the in-headset OCR card stays up the whole time) so the
    /// mode can't be left running into a raid. Gated on OCR being enabled
    /// — the loop has nothing to do otherwise. The flag lives on the VR
    /// runtime and is never persisted, so it always starts OFF on launch.
    fn auto_capture_toggle(&mut self, ui: &mut egui::Ui) {
        let ocr_enabled = self.settings.read().ocr_enabled;
        let mut on = self.vr.auto_capture_enabled();
        let label = if on {
            egui::RichText::new("● Auto-capture ON")
                .strong()
                .color(egui::Color32::from_rgb(220, 99, 89))
        } else {
            egui::RichText::new("Auto-capture")
        };
        let tooltip = if ocr_enabled {
            "Loop OCR over whatever upgrade panel you look at, updating \
             counts hands-free. A constant card stays in the headset while \
             it runs — turn it OFF before playing. Interval is set in \
             Settings; the mode always starts OFF on launch."
        } else {
            "Enable \"Auto-extract counts from VR screenshots\" in Settings \
             to use auto-capture."
        };
        let resp = ui.add_enabled(ocr_enabled, egui::Checkbox::new(&mut on, label));
        if resp.on_hover_text(tooltip).changed() {
            self.vr.set_auto_capture(on);
            tracing::info!(on, "auto-capture toggled from desktop header");
        }
    }

    /// "Export corrections" pops up a modal with the user's per-recipe edits
    /// rendered as a markdown body, ready to be copied into a new GitHub
    /// issue. Disabled when no overrides exist, or when a newer release is
    /// available — an out-of-date build bundles stale data, so its
    /// "corrections" tend to just re-report recipes already fixed upstream.
    fn export_corrections_button(&mut self, ui: &mut egui::Ui) {
        let count = self.state.read().overrides.len();
        // `update_available` is latched in `poll_update_check` via
        // `CheckStatus::out_of_date_version` — `Some` only when the check
        // positively confirmed a newer release (every other state fails open;
        // see that method + its test). We gate only on confirmed staleness.
        let available_update = self.update_available.clone();
        let enabled = count > 0 && available_update.is_none();
        let tooltip = if let Some(latest) = &available_update {
            format!(
                "Update to v{latest} first — corrections from an out-of-date version \
                 may duplicate fixes already shipped upstream."
            )
        } else if count == 0 {
            "Edit a recipe first (click an upgrade's \"Edit\" button) to enable this.".to_string()
        } else {
            format!(
                "Show a copy-able markdown summary of your {count} recipe correction(s) \
                 so you can paste them into a new GitHub issue."
            )
        };
        let resp = ui
            .add_enabled(enabled, egui::Button::new("Export corrections ↗"))
            .on_hover_text(&tooltip)
            .on_disabled_hover_text(&tooltip);
        if resp.clicked() && enabled {
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
                // Latch "build is out of date" off the raw result, before
                // anything (e.g. the banner-dismiss path, which rewrites
                // `check_status` to `UpToDate`) can mutate it. This is the
                // authoritative signal the export gate reads.
                if let Some(latest) = status.out_of_date_version() {
                    self.update_available = Some(latest.to_string());
                }
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

    /// Drain the in-app update download channel. Mirrors `poll_update_check`:
    /// the worker sends progress then exactly one terminal state. On
    /// `ReadyToInstall` we hand the staged `.msi` to `msiexec` and close the
    /// app so the installer can swap the binary (it relaunches us from its
    /// Finish dialog). On failure we keep the banner up — it shows the error
    /// and the browser-download fallback.
    fn poll_update_apply(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.apply_rx else { return };
        // Drain to the most recent message, noting a disconnect. The worker
        // sends progress then one terminal state. Any mutation of
        // `self.apply_rx` is deferred until *after* this loop so it doesn't
        // clash with the borrow `rx` holds.
        let mut latest = None;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(s) => latest = Some(s),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        if let Some(status) = latest {
            match status {
                crate::updater::UpdateApplyStatus::ReadyToInstall { path } => {
                    match crate::platform::run_msi_installer(&path) {
                        Ok(()) => {
                            // Installer launched; close so it can replace our
                            // binary. Its Finish dialog offers to relaunch.
                            self.update_apply = crate::updater::UpdateApplyStatus::Launching;
                            self.apply_rx = None;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Err(e) => {
                            self.update_apply = crate::updater::UpdateApplyStatus::Failed {
                                reason: format!("could not launch installer: {e}"),
                            };
                            self.apply_rx = None;
                        }
                    }
                }
                terminal @ crate::updater::UpdateApplyStatus::Failed { .. } => {
                    self.update_apply = terminal;
                    self.apply_rx = None;
                }
                progress => self.update_apply = progress,
            }
        }

        // Channel closed without delivering a terminal message (worker panicked
        // mid-flight) — fail safe rather than spin on a dead receiver.
        if disconnected && self.apply_rx.is_some() {
            if matches!(
                self.update_apply,
                crate::updater::UpdateApplyStatus::Downloading { .. }
                    | crate::updater::UpdateApplyStatus::Verifying
            ) {
                self.update_apply = crate::updater::UpdateApplyStatus::Failed {
                    reason: "download thread stopped unexpectedly".into(),
                };
            }
            self.apply_rx = None;
        }

        // Keep repainting while a download is live so the progress bar moves —
        // egui otherwise sleeps until the next input event.
        if matches!(
            self.update_apply,
            crate::updater::UpdateApplyStatus::Downloading { .. }
                | crate::updater::UpdateApplyStatus::Verifying
        ) {
            ctx.request_repaint();
        }
    }

    fn update_banner(&mut self, ctx: &egui::Context) {
        let CheckStatus::UpdateAvailable(info) = self.check_status.clone() else {
            return;
        };

        use crate::updater::UpdateApplyStatus as Apply;

        // If the user already dismissed this exact version, stay quiet — but
        // a newer release that comes out later will have a different
        // `latest_version` string and will re-show the banner. A download in
        // flight (apply != Idle) overrides the dismissal so its progress is
        // never hidden.
        let dismissed = self.settings.read().dismissed_update_version.as_deref()
            == Some(info.latest_version.as_str());
        let idle = matches!(self.update_apply, Apply::Idle);
        if dismissed && idle {
            return;
        }

        let current = env!("CARGO_PKG_VERSION");
        let apply = self.update_apply.clone();
        // Offer in-app install only to MSI builds whose release actually ships
        // an .msi; everyone else gets the browser link (the graceful fallback).
        let can_self_update =
            self.install_kind == crate::platform::InstallKind::Msi && info.msi.is_some();
        let mut clear = false;
        let mut dismiss = false;
        let mut start_download: Option<crate::updater::ReleaseAsset> = None;
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
                    match &apply {
                        Apply::Idle => {
                            // MSI builds: one-click download + install. Portable
                            // builds skip straight to the manual link below.
                            if can_self_update {
                                if let Some(msi) = &info.msi {
                                    if ui.button("Update now").clicked() {
                                        start_download = Some(msi.clone());
                                    }
                                }
                            }
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
                        }
                        Apply::Downloading { received, total } => {
                            let frac = if *total > 0 {
                                (*received as f32 / *total as f32).clamp(0.0, 1.0)
                            } else {
                                0.0
                            };
                            let pct = (frac * 100.0).round() as u32;
                            ui.add(
                                egui::ProgressBar::new(frac)
                                    .desired_width(180.0)
                                    .text(format!("Downloading update… {pct}%")),
                            );
                        }
                        Apply::Verifying | Apply::ReadyToInstall { .. } | Apply::Launching => {
                            ui.add(egui::Spinner::new().size(14.0));
                            ui.label(
                                egui::RichText::new("Starting installer…")
                                    .color(egui::Color32::WHITE),
                            );
                        }
                        Apply::Failed { reason } => {
                            // Degrade gracefully: show the error and fall back
                            // to the browser link the user can always use.
                            ui.label(
                                egui::RichText::new(format!("Update failed: {reason}"))
                                    .color(egui::Color32::from_rgb(255, 210, 140))
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
                        }
                    }
                });
            });

        if let Some(msi) = start_download {
            self.update_apply = Apply::Downloading {
                received: 0,
                total: msi.size,
            };
            self.apply_rx = Some(crate::updater::spawn_msi_download(msi));
        }
        if dismiss {
            self.settings.write().dismissed_update_version = Some(info.latest_version.clone());
            self.persist_settings();
        }
        if clear {
            // User dismissed/snoozed the banner — collapse to an "up to date"-
            // style status so the header indicator doesn't keep advertising an
            // upgrade until the next app start, and reset the apply state so a
            // later re-show starts clean.
            self.update_apply = Apply::Idle;
            self.check_status = CheckStatus::UpToDate {
                latest_version: info.latest_version.clone(),
            };
        }
    }

    /// Centred transient toast confirming a VR-screenshot capture. Auto-
    /// dismisses after `TOAST_DURATION`; the last frame fades to zero
    /// alpha so it doesn't pop out abruptly. Will eventually carry the
    /// OCR'd panel summary instead of just the file path.
    fn render_capture_toast(&mut self, ctx: &egui::Context) {
        use std::time::Duration;
        const TOAST_DURATION: Duration = Duration::from_secs(3);
        const FADE_TAIL: Duration = Duration::from_millis(600);

        let Some(shown_at) = self.capture_toast_shown_at else {
            return;
        };
        let age = shown_at.elapsed();
        if age >= TOAST_DURATION {
            self.capture_toast_shown_at = None;
            return;
        }
        // Keep the UI repainting while the toast is on screen so it
        // disappears on time even if no other input arrives.
        ctx.request_repaint_after(Duration::from_millis(33));

        // Linear fade in the last `FADE_TAIL` of its lifetime.
        let alpha = if TOAST_DURATION.saturating_sub(age) < FADE_TAIL {
            let remaining = TOAST_DURATION.saturating_sub(age).as_secs_f32();
            (remaining / FADE_TAIL.as_secs_f32()).clamp(0.0, 1.0)
        } else {
            1.0
        };

        let (title, body, accent) = match &self.last_capture {
            Some(CaptureResult::Ok(path)) => (
                "Capture saved",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string()),
                egui::Color32::from_rgb(80, 180, 100),
            ),
            Some(CaptureResult::Ephemeral) => (
                "Captured",
                "Sent to OCR (no PNG saved).".to_string(),
                egui::Color32::from_rgb(80, 180, 100),
            ),
            Some(CaptureResult::Err(msg)) => (
                "Capture failed",
                msg.clone(),
                egui::Color32::from_rgb(220, 100, 90),
            ),
            None => return,
        };

        let scale = |c: egui::Color32, a: f32| {
            egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (c.a() as f32 * a) as u8)
        };

        egui::Area::new(egui::Id::new("vr_capture_toast"))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .interactable(false)
            .order(egui::Order::Tooltip)
            .show(ctx, |ui| {
                egui::Frame::default()
                    .fill(scale(egui::Color32::from_rgb(28, 28, 32), alpha))
                    .stroke(egui::Stroke::new(1.5, scale(accent, alpha)))
                    .rounding(egui::Rounding::same(10.0))
                    .inner_margin(egui::Margin::symmetric(24.0, 16.0))
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(title)
                                    .strong()
                                    .size(18.0)
                                    .color(scale(accent, alpha)),
                            );
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(body)
                                    .monospace()
                                    .size(13.0)
                                    .color(scale(egui::Color32::from_gray(230), alpha)),
                            );
                        });
                    });
            });
    }

    fn confirm_reset_dialog(&mut self, ctx: &egui::Context) {
        egui::Window::new("Reset all progress?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("This clears every tracked upgrade, completed marker, collected count, and secondary container. This cannot be undone.");
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
