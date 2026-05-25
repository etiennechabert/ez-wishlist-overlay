//! Modal that surfaces the in-memory log buffer (see `log_buffer.rs`).

use crate::log_buffer::{LogBuffer, LogLine};
use egui::Color32;
use tracing::Level;

pub fn show(ctx: &egui::Context, open: &mut bool, log_buf: &LogBuffer) {
    let mut close_now = false;
    egui::Window::new("Debug logs")
        .open(open)
        .collapsible(false)
        .resizable(true)
        .default_width(720.0)
        .default_height(460.0)
        .show(ctx, |ui| {
            let lines = log_buf.snapshot();
            let dark = ui.visuals().dark_mode;
            let weak = ui.visuals().weak_text_color();

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{} line{} captured this session (oldest first, newest at bottom)",
                        lines.len(),
                        if lines.len() == 1 { "" } else { "s" }
                    ))
                    .small()
                    .color(weak),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        close_now = true;
                    }
                    if ui.button("Clear").clicked() {
                        log_buf.clear();
                    }
                    if ui.button("Copy to clipboard").clicked() {
                        let joined = lines.iter().map(format_line).collect::<Vec<_>>().join("\n");
                        ui.ctx().copy_text(joined);
                    }
                });
            });
            ui.separator();

            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if lines.is_empty() {
                        ui.add_space(20.0);
                        ui.vertical_centered(|ui| {
                            ui.label(egui::RichText::new("No log lines yet.").small().color(weak));
                        });
                        return;
                    }
                    for line in &lines {
                        let color = level_color(line.level, dark);
                        ui.label(
                            egui::RichText::new(format_line(line))
                                .monospace()
                                .small()
                                .color(color),
                        );
                    }
                });
        });

    if close_now {
        *open = false;
    }
}

fn format_line(line: &LogLine) -> String {
    format!(
        "{:02}:{:02}:{:02} {:>5} {}",
        line.timestamp.hour(),
        line.timestamp.minute(),
        line.timestamp.second(),
        line.level,
        line.message,
    )
}

fn level_color(level: Level, dark: bool) -> Color32 {
    match level {
        Level::ERROR if dark => Color32::from_rgb(255, 130, 130),
        Level::ERROR => Color32::from_rgb(165, 30, 30),
        Level::WARN if dark => Color32::from_rgb(240, 200, 110),
        Level::WARN => Color32::from_rgb(150, 110, 0),
        Level::INFO if dark => Color32::from_rgb(220, 220, 220),
        Level::INFO => Color32::from_rgb(40, 40, 40),
        Level::DEBUG | Level::TRACE if dark => Color32::from_gray(160),
        Level::DEBUG | Level::TRACE => Color32::from_gray(110),
    }
}
