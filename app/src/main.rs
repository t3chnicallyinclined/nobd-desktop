// No console window — GUI app, lives in the system tray.
#![windows_subsystem = "windows"]

mod app;
mod bulk;
mod hid;
mod hidhide;
mod input;
mod logo;
mod nobd_setup;
mod palette;
mod persist;
mod stats;
mod sync_service;
mod tray;

fn configure_style(ctx: &egui::Context) {
    use palette::*;
    // Pin the theme. `theme_preference` defaults to System, so on a machine set
    // to light Windows egui kept swapping in its own light visuals and every
    // panel background came back rgb(248,248,248) — only the explicit card fills
    // stayed dark. The charter is a dark palette; it is not a system preference.
    ctx.options_mut(|o| o.theme_preference = egui::ThemePreference::Dark);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BASE;
    v.window_fill = BASE;
    v.extreme_bg_color = WELL;
    v.faint_bg_color = SURFACE;
    v.selection.bg_fill = ACTION;
    v.hyperlink_color = ACTION;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, HAIRLINE);
    // Make the DIM voice the default, so an uncoloured label lands in prose ink
    // and every promotion to full-strength INK is deliberate and greppable.
    v.widgets.noninteractive.fg_stroke.color = INK_DIM;
    ctx.set_visuals(v);
}

/// Already-running check. Without it the logon task plus a manual launch gives
/// two tray icons and two sync loops submitting to the same virtual pad.
/// Returns false if another instance owns the name (and was asked to show).
fn claim_single_instance() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    // Raw string + explicit NUL: the name needs a literal backslash.
    let name: Vec<u16> = r"Local\NobdDesktopSingleton"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if h == 0 {
            // ACCESS_DENIED means the name exists at an integrity level we
            // cannot write to — i.e. the logon task's elevated instance is
            // already running and we are the medium-IL double-click. That is
            // "already running", not "cannot tell"; treating it as the latter
            // let a second instance start and fight over the same virtual pad.
            let denied = GetLastError() == ERROR_ACCESS_DENIED;
            if !denied {
                return true; // genuinely cannot tell — don't block startup
            }
        }
        if h == 0 || GetLastError() == ERROR_ALREADY_EXISTS {
            // Hand the running instance the foreground instead of starting a second.
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
            };
            let title: Vec<u16> = "NOBD Desktop"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
            if hwnd != 0 {
                ShowWindow(hwnd, SW_SHOW);
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
            }
            return false;
        }
        // The handle is intentionally never closed: the mutex must stay held for
        // the life of the process, and the OS releases it when we exit.
        true
    }
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

    // The elevated setup run is exempt from the singleton. It is spawned by
    // `ShellExecuteW(runas)` from a parent that then exits, and nothing
    // sequences the two: if the child claimed the mutex before the parent's
    // handles closed it saw ERROR_ALREADY_EXISTS, returned early, and NOTHING
    // WAS EVER INSTALLED — with no error anywhere. Setup is short-lived and
    // idempotent; it does not need the guard.
    let is_setup_run = std::env::args().any(|a| a.starts_with("--setup-") || a == "--uninstall" || a == "--eject" || a == "--hid-probe");
    if !is_setup_run && !claim_single_instance() {
        return Ok(());
    }

    // `--hid-probe`: dump every HID gamepad's report layout to
    // %TEMP%\nobd-hidprobe.txt. Development aid for retargeting the HID filter.
    if std::env::args().any(|a| a == "--hid-probe") {
        let mut out = String::new();
        for d in hid::list_hid_gamepads() {
            if d.id.vid == 0x1209 {
                continue; // our own virtual pad
            }
            out.push_str(&hid::probe_report(&d.id(), 45));
            out.push_str("\n----------------------------------------\n\n");
        }
        if out.is_empty() {
            out.push_str("no HID gamepads found\n");
        }
        let _ = std::fs::write(std::env::temp_dir().join("nobd-hidprobe.txt"), out);
        return Ok(());
    }

    // `--eject`: headless removal of just the virtual controller, so the Remove
    // button can elevate itself the same way Install does. Without this a
    // non-elevated user could never remove the controller at all.
    if std::env::args().any(|a| a == "--eject") {
        let _ = nobd_setup::eject();
        return Ok(());
    }

    // `--uninstall`: headless full removal, run by the NSIS uninstaller before it
    // deletes the program files. Never opens a window.
    if std::env::args().any(|a| a == "--uninstall") {
        let report = nobd_setup::uninstall_everything();
        let log = std::env::temp_dir().join("nobd-uninstall.log");
        let _ = std::fs::write(&log, report);
        return Ok(());
    }

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
            // Visible by default. Hiding is OPT-IN via `--tray`, which is what the
            // logon task passes. It used to be the other way round: a first-timer
            // double-clicking nobd.exe got no window at all, and the UAC relaunch
            // (which exits and comes back through here) made the app vanish at the
            // exact moment they clicked the one button on screen.
            .with_visible(!std::env::args().any(|a| a == "--tray")),
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
