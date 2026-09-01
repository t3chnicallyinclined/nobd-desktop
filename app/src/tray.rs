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
/// Take the window out of (or back into) the taskbar.
///
/// A tray app should vanish from the taskbar when you close it - but making the
/// window INVISIBLE is exactly what sends eframe's event loop spinning at 100%
/// of a core, because there is no surface to present and nothing to block on.
///
/// So we minimise and strip the taskbar button instead. Windows reports a
/// minimised window as VISIBLE (`IsWindowVisible` is true while `IsIconic` is
/// also true), so the loop keeps blocking normally - measured at ~1% - while
/// `WS_EX_TOOLWINDOW` removes the button. Restoring puts `WS_EX_APPWINDOW` back
/// so the window behaves like a normal one again.
pub fn set_taskbar_button(show: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_APPWINDOW,
        WS_EX_TOOLWINDOW,
    };
    let title: Vec<u16> = "NOBD Desktop".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd == 0 {
            return;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let ex = if show {
            (ex & !WS_EX_TOOLWINDOW) | WS_EX_APPWINDOW
        } else {
            (ex | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW
        };
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex as isize);
    }
}

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Kept alive for the life of the app. Holds the checkable items so the main
/// thread can sync their check marks to the live shared config.
pub struct Tray {
    _icon: TrayIcon,
    enabled: CheckMenuItem,
    w3: CheckMenuItem,
    w5: CheckMenuItem,
    w8: CheckMenuItem,
}

impl Tray {
    /// Sync menu check marks to the current shared config. Call from update().
    pub fn refresh_checks(&self) {
        let s = state();
        self.enabled.set_checked(s.enabled.load(Ordering::Relaxed) != 0);
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

    let open = MenuItem::new("Open NOBD", true, None);
    // Same wording as the main screen's switch. It used to say "Sync enabled"
    // while the panel said "NOBD" and the button said "On", for one flag.
    // A BYPASS, not the master switch: it leaves the controller in place and
    // passes presses through, so a running game does not lose its pad.
    let enabled = CheckMenuItem::new("Syncing (uncheck to bypass)", true, s0.enabled.load(Ordering::Relaxed) != 0, None);
    let w3 = CheckMenuItem::new("Window: 3 ms", true, cur_w == 3, None);
    let w5 = CheckMenuItem::new("Window: 5 ms", true, cur_w == 5, None);
    let w8 = CheckMenuItem::new("Window: 8 ms", true, cur_w == 8, None);
    let quit = MenuItem::new("Quit NOBD", true, None);

    let (id_open, id_enabled) = (open.id().clone(), enabled.id().clone());
    let (id_w3, id_w5, id_w8, id_quit) =
        (w3.id().clone(), w5.id().clone(), w8.id().clone(), quit.id().clone());

    let menu = Menu::new();
    menu.append(&open).ok()?;
    menu.append(&PredefinedMenuItem::separator()).ok()?;
    menu.append(&enabled).ok()?;
    // No "Mode" item: nothing reads `state().mode` any more, so it was a checked
    // menu entry that did nothing at all.
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
                } else if ev.id == id_w3 {
                    for w in &s.window_ms { w.store(3, Ordering::Relaxed); }
                } else if ev.id == id_w5 {
                    for w in &s.window_ms { w.store(5, Ordering::Relaxed); }
                } else if ev.id == id_w8 {
                    for w in &s.window_ms { w.store(8, Ordering::Relaxed); }
                } else if ev.id == id_quit {
                    // Un-cloak any hidden stick (best-effort) so it isn't left
                    // hidden, then exit right here — the window may be hidden in
                    // the tray, where the UI loop's update() doesn't run, so we
                    // must NOT defer the exit to it.
                    let _ = crate::hidhide::cloak(false);
                    // process::exit skips every Drop, so the sync loop never
                    // gets to hand the pad back. Release it here or a button
                    // held at quit time stays held in every game.
                    crate::sync_service::release_pad();
                    // Unless the user asked us to leave it, take the controller
                    // out of Windows on the way out. Leaving it behind meant a
                    // phantom Xbox pad in Steam with NOBD closed and the stick
                    // unplugged - a second identical controller nothing explains.
                    if crate::persist::load_ui().keep_controller == 0 {
                        let _ = crate::nobd_setup::eject();
                    }
                    std::process::exit(0);
                }
                // Wake the UI so check marks re-sync to the new state.
                ctx.request_repaint();
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    });

    Some(Tray { _icon: tray, enabled, w3, w5, w8 })
}
