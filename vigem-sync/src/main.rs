// NOBD universal sync — PROTOTYPE.
//
// Read the real controller, run the NOBD sync window on its attack buttons, and
// present the RESULT as a virtual Xbox pad (ViGEm). Every game and the Finger Gap
// Tester read the virtual pad, so the grouping is universal — not tied to any
// game's DLL. ~1ms added latency (the virtual-pad hop); the zero-latency answer
// is the in-path HID filter.
//
// Controlled live by the existing GUI: it reads `enabled` + `window_ms[0]` from
// the shared `Local\NobdSyncState` block, so the "NOBD sync window" checkbox and
// the per-player window slider drive this prototype.

use std::time::{Duration, Instant};
use windows_sys::Win32::Media::timeBeginPeriod;
use windows_sys::Win32::UI::Input::XboxController::{XInputGetState, XINPUT_STATE};

use nobd_shared::sync_window::SyncWindow;

/// XInput wButtons attack bits: A, B, X, Y, LB, RB. (DPad/Start/Back/thumbs pass
/// through ungrouped for zero motion-input lag.)
const ATTACK_MASK: u16 = 0xF300;

fn xinput_connected(slot: u32) -> Option<XINPUT_STATE> {
    let mut state: XINPUT_STATE = unsafe { std::mem::zeroed() };
    if unsafe { XInputGetState(slot, &mut state) } == 0 {
        Some(state)
    } else {
        None
    }
}

fn main() {
    println!("NOBD universal sync (ViGEm prototype)");

    // Find the REAL pad's slot BEFORE plugging the virtual one, so we never read
    // our own virtual output back (feedback loop).
    let real_slot = (0..4).find(|&s| xinput_connected(s).is_some());
    let real_slot = match real_slot {
        Some(s) => {
            println!("Real controller on XInput slot {s}.");
            s
        }
        None => {
            println!("No controller found. Connect one and re-run.");
            return;
        }
    };

    // Connect ViGEm + plug a virtual Xbox 360 pad.
    let client = match vigem_client::Client::connect() {
        Ok(c) => c,
        Err(e) => {
            println!("ViGEmBus connect failed ({e:?}). Is the ViGEmBus driver installed?");
            return;
        }
    };
    let id = vigem_client::TargetId::XBOX360_WIRED;
    let mut target = vigem_client::Xbox360Wired::new(client, id);
    if let Err(e) = target.plugin() {
        println!("Virtual pad plugin failed: {e:?}");
        return;
    }
    let _ = target.wait_ready();
    println!("Virtual Xbox pad plugged. In the NOBD GUI, toggle 'NOBD sync window'");
    println!("and run the Finger Gap Tester — the virtual pad shows GROUPING, the real one doesn't.");

    unsafe { timeBeginPeriod(1) };
    let epoch = Instant::now();
    let mut sync = SyncWindow::new();
    let mut last_enabled = false;
    let mut last_log = Instant::now();

    loop {
        let now_us = epoch.elapsed().as_micros() as u64;

        // Live config from the shared block the GUI writes.
        let s = nobd_shared::state();
        let enabled = s.enabled.load(std::sync::atomic::Ordering::Relaxed) != 0;
        let window_ms = s.window_ms[0].load(std::sync::atomic::Ordering::Relaxed).clamp(1, 16);
        let window_us = window_ms * 1000;

        if enabled != last_enabled {
            println!("sync {} (window {window_ms}ms)", if enabled { "ON" } else { "OFF" });
            last_enabled = enabled;
        }

        if let Some(state) = xinput_connected(real_slot) {
            let gp = state.Gamepad;
            let raw = gp.wButtons;
            let grouped = sync.process(raw, ATTACK_MASK, ATTACK_MASK, now_us, window_us, enabled);

            let out = vigem_client::XGamepad {
                buttons: vigem_client::XButtons { raw: grouped },
                left_trigger: gp.bLeftTrigger,
                right_trigger: gp.bRightTrigger,
                thumb_lx: gp.sThumbLX,
                thumb_ly: gp.sThumbLY,
                thumb_rx: gp.sThumbRX,
                thumb_ry: gp.sThumbRY,
            };
            let _ = target.update(&out);
        }

        if last_log.elapsed() >= Duration::from_secs(5) {
            println!("running… sync={} window={window_ms}ms", if last_enabled { "ON" } else { "OFF" });
            last_log = Instant::now();
        }

        std::thread::sleep(Duration::from_micros(1000)); // ~1 kHz
    }
}
