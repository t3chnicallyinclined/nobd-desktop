// No console window — GUI app, lives in the system tray.
#![windows_subsystem = "windows"]

mod app;
mod hid;
mod input;
mod logo;
mod monitor;
mod nobd_setup;
mod persist;
mod stats;
mod sync_service;
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
    // Panic hook: a windows-subsystem app has no stderr, and abort-panics bypass
    // it anyway — write the panic (with backtrace) to a file so crashes are
    // diagnosable. TEMP\nobd-panic.txt.
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let msg = format!("{info}\n\nbacktrace:\n{bt}\n");
        let path = std::env::temp_dir().join("nobd-panic.txt");
        let _ = std::fs::write(&path, msg);
    }));

    // Relaunched elevated for one-time NOBD Controller setup: install the driver
    // + create the device before the GUI comes up, then continue as the (now
    // elevated) app. Best-effort; the UI reflects success/failure.
    {
        let mode = if std::env::args().any(|a| a == "--setup-xinput") {
            Some(sync_service::PadType::Xinput)
        } else if std::env::args().any(|a| a == "--setup-hid") {
            Some(sync_service::PadType::Hid)
        } else {
            None
        };
        if let Some(m) = mode {
            // Log the outcome — setup runs before the GUI and errors were being
            // swallowed, so a failed migrate looked like "nothing happened".
            let log = std::env::temp_dir().join("nobd-setup.log");
            let result = if !nobd_setup::is_elevated() {
                "requested but NOT elevated".to_string()
            } else {
                match nobd_setup::run_setup(m) {
                    Ok(()) => "OK".to_string(),
                    Err(e) => format!("FAILED: {e}"),
                }
            };
            let mode_name = match m {
                sync_service::PadType::Hid => "Hid",
                sync_service::PadType::Xinput => "Xinput",
            };
            let _ = std::fs::write(&log, format!("setup ({mode_name}): {result}\n"));
        }
    }

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
            .with_visible(std::env::var("NOBD_DEBUG_SHOW").is_ok()),
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
