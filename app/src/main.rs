// No console window — GUI app, lives in the system tray.
#![windows_subsystem = "windows"]

mod app;
mod hid;
mod input;
mod install;
mod logo;
mod monitor;
mod persist;
mod stats;
mod tray;

use egui::Color32;

fn configure_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(18, 18, 24);
    visuals.window_fill = Color32::from_rgb(18, 18, 24);
    visuals.selection.bg_fill = Color32::from_rgb(0, 180, 216);
    visuals.hyperlink_color = Color32::from_rgb(0, 180, 216);
    ctx.set_visuals(visuals);
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([820.0, 640.0])
            .with_min_inner_size([640.0, 480.0])
            .with_title("NOBD Desktop")
            .with_icon(std::sync::Arc::new(egui::IconData {
                rgba: logo::rgba(256, true),
                width: 256,
                height: 256,
            }))
            // Start hidden — the app lives in the tray; left-click the icon to open.
            .with_visible(false),
        ..Default::default()
    };

    eframe::run_native(
        "NOBD Desktop",
        options,
        Box::new(|cc| {
            configure_style(&cc.egui_ctx);
            Ok(Box::new(app::FingerGapApp::new(&cc.egui_ctx)))
        }),
    )
}
