//! System-tray icon for nobd.exe. Left-click opens the window; right-click shows
//! a quick-settings menu with live check marks. Event handling runs on its own
//! thread (works while hidden); check-mark refresh happens on the main thread
//! (muda menu items are not Send), driven from the egui update loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use eframe::egui;
use nobd_shared::state;

/// Set by the tray event thread to ask the main thread (update loop) to re-sync
/// eframe's visible state once the OS window is back up.
pub static WANT_SHOW: AtomicBool = AtomicBool::new(false);

/// Un-hide / restore / foreground the window directly via Win32. eframe stops
/// running its update loop while the window is hidden, so we can't rely on a
/// viewport command from update() to bring it back — we poke the HWND ourselves.
fn show_window_win32() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };
    let title: Vec<u16> = "NOBD Desktop\0".encode_utf16().collect();
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd != 0 {
            ShowWindow(hwnd, SW_SHOW);
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
    }
}
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Kept alive for the life of the app. Holds the checkable items so the main
/// thread can sync their check marks to the live shared config.
pub struct Tray {
    _icon: TrayIcon,
    enabled: CheckMenuItem,
    mode_defer: CheckMenuItem,
    mode_block: CheckMenuItem,
    mode_continuous: CheckMenuItem,
    w3: CheckMenuItem,
    w5: CheckMenuItem,
    w8: CheckMenuItem,
}

impl Tray {
    /// Sync menu check marks to the current shared config. Call from update().
    pub fn refresh_checks(&self) {
        let s = state();
        self.enabled.set_checked(s.enabled.load(Ordering::Relaxed) != 0);
        let mode = s.mode.load(Ordering::Relaxed);
        self.mode_defer.set_checked(mode == 0);
        self.mode_block.set_checked(mode == 1);
        self.mode_continuous.set_checked(mode == 2);
        // Window is per-player now; the quick-set is checked only if all match.
        let w0 = s.window_ms[0].load(Ordering::Relaxed);
        let w = if s.window_ms.iter().all(|x| x.load(Ordering::Relaxed) == w0) { w0 } else { 0 };
        self.w3.set_checked(w == 3);
        self.w5.set_checked(w == 5);
        self.w8.set_checked(w == 8);
    }
}

fn make_icon() -> Icon {
    // The full branded app icon (dark rounded square + brackets + press-dots),
    // rendered at 32px for the tray. Same artwork as the window/taskbar icon.
    let size = 32u32;
    let rgba = crate::logo::rgba(size, true);
    Icon::from_rgba(rgba, size, size).expect("tray icon")
}

pub fn spawn(ctx: egui::Context) -> Option<Tray> {
    let s0 = state();
    let cur_w = s0.window_ms[0].load(Ordering::Relaxed);

    let mode0 = s0.mode.load(Ordering::Relaxed);
    let open = MenuItem::new("Open NOBD", true, None);
    let enabled = CheckMenuItem::new("Sync enabled", true, s0.enabled.load(Ordering::Relaxed) != 0, None);
    let mode_defer = CheckMenuItem::new("Mode: Defer (online-safe)", true, mode0 == 0, None);
    let mode_block = CheckMenuItem::new("Mode: Block (offline)", true, mode0 == 1, None);
    let mode_continuous = CheckMenuItem::new("Mode: Continuous (1kHz)", true, mode0 == 2, None);
    let w3 = CheckMenuItem::new("Window: 3 ms", true, cur_w == 3, None);
    let w5 = CheckMenuItem::new("Window: 5 ms", true, cur_w == 5, None);
    let w8 = CheckMenuItem::new("Window: 8 ms", true, cur_w == 8, None);
    let quit = MenuItem::new("Quit NOBD", true, None);

    let (id_open, id_enabled) = (open.id().clone(), enabled.id().clone());
    let (id_defer, id_block, id_continuous) =
        (mode_defer.id().clone(), mode_block.id().clone(), mode_continuous.id().clone());
    let (id_w3, id_w5, id_w8, id_quit) =
        (w3.id().clone(), w5.id().clone(), w8.id().clone(), quit.id().clone());

    let menu = Menu::new();
    menu.append(&open).ok()?;
    menu.append(&PredefinedMenuItem::separator()).ok()?;
    menu.append(&enabled).ok()?;
    // Continuous-only: Defer/Block items still exist (and their handlers below)
    // but are not shown. Re-append these to restore the multi-mode menu.
    // menu.append(&mode_defer).ok()?;
    // menu.append(&mode_block).ok()?;
    menu.append(&mode_continuous).ok()?;
    menu.append(&PredefinedMenuItem::separator()).ok()?;
    menu.append(&w3).ok()?;
    menu.append(&w5).ok()?;
    menu.append(&w8).ok()?;
    menu.append(&PredefinedMenuItem::separator()).ok()?;
    menu.append(&quit).ok()?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("NOBD Desktop")
        .with_icon(make_icon())
        .build()
        .ok()?;

    // Event thread — only needs the (Send) MenuIds, never the menu items.
    std::thread::spawn(move || {
        let tray_rx = TrayIconEvent::receiver();
        let menu_rx = MenuEvent::receiver();
        let show = |ctx: &egui::Context| {
            // Bring the OS window back directly (eframe's loop is asleep while
            // hidden), then nudge eframe to resume rendering + re-sync state.
            show_window_win32();
            WANT_SHOW.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        };
        loop {
            while let Ok(ev) = tray_rx.try_recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = ev
                {
                    show(&ctx);
                }
            }
            while let Ok(ev) = menu_rx.try_recv() {
                let s = state();
                if ev.id == id_open {
                    show(&ctx);
                } else if ev.id == id_enabled {
                    let v = s.enabled.load(Ordering::Relaxed) == 0;
                    s.enabled.store(v as u32, Ordering::Relaxed);
                } else if ev.id == id_defer {
                    s.mode.store(0, Ordering::Relaxed);
                    s.block_in_frame.store(0, Ordering::Relaxed);
                } else if ev.id == id_block {
                    s.mode.store(1, Ordering::Relaxed);
                    s.block_in_frame.store(1, Ordering::Relaxed);
                } else if ev.id == id_continuous {
                    s.mode.store(2, Ordering::Relaxed);
                    s.block_in_frame.store(0, Ordering::Relaxed);
                } else if ev.id == id_w3 {
                    for w in &s.window_ms { w.store(3, Ordering::Relaxed); }
                } else if ev.id == id_w5 {
                    for w in &s.window_ms { w.store(5, Ordering::Relaxed); }
                } else if ev.id == id_w8 {
                    for w in &s.window_ms { w.store(8, Ordering::Relaxed); }
                } else if ev.id == id_quit {
                    std::process::exit(0);
                }
                // Wake the UI so check marks re-sync to the new state.
                ctx.request_repaint();
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    });

    Some(Tray { _icon: tray, enabled, mode_defer, mode_block, mode_continuous, w3, w5, w8 })
}
