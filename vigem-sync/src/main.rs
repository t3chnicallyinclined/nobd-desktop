// NOBD universal sync — PROTOTYPE.
//
// Read the real controller, run the NOBD sync window on its attack buttons, and
// present the RESULT as a virtual Xbox pad (ViGEm). Every game and the Finger Gap
// Tester read the virtual pad, so the grouping is universal — not tied to any
// game's DLL. ~1ms added latency (the virtual-pad hop); the zero-latency answer
// is the in-path HID filter.
//
// Controlled live by the existing GUI via the shared `Local\NobdSyncState` block:
// the "NOBD sync window" checkbox + per-player window slider set enabled/window.
//
// Writes a flushed log to %TEMP%\vigem-sync.log so progress is visible even when
// console buffering hides it.

use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant};
use windows_sys::Win32::Media::timeBeginPeriod;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::UI::Input::XboxController::XINPUT_STATE;

use nobd_shared::sync_window::SyncWindow;

/// XInput wButtons attack bits: A, B, X, Y, LB, RB. (DPad/Start/Back/thumbs pass
/// through ungrouped for zero motion-input lag.)
const ATTACK_MASK: u16 = 0xF300;

/// `XInputGetState` resolved at runtime from System32's xinput1_4.dll — the
/// statically-linked windows-sys symbol hangs on some systems; this matches what
/// the GUI does and works reliably.
type XInputGetStateFn = unsafe extern "system" fn(u32, *mut XINPUT_STATE) -> u32;

fn load_xinput() -> Option<XInputGetStateFn> {
    unsafe {
        let mut dir = [0u16; 260];
        let n = GetSystemDirectoryW(dir.as_mut_ptr(), dir.len() as u32);
        if n == 0 || n as usize >= dir.len() {
            return None;
        }
        let mut path: Vec<u16> = dir[..n as usize].to_vec();
        path.extend("\\xinput1_4.dll".encode_utf16());
        path.push(0);
        let lib = LoadLibraryW(path.as_ptr());
        if lib == 0 {
            return None;
        }
        let proc = GetProcAddress(lib, b"XInputGetState\0".as_ptr());
        proc.map(|p| std::mem::transmute::<unsafe extern "system" fn() -> isize, XInputGetStateFn>(p))
    }
}

struct Log(Option<File>);
impl Log {
    fn new() -> Self {
        let path = format!(
            "{}\\vigem-sync.log",
            std::env::var("TEMP").unwrap_or_else(|_| ".".into())
        );
        Log(File::create(path).ok())
    }
    fn line(&mut self, s: &str) {
        println!("{s}");
        let _ = std::io::stdout().flush();
        if let Some(f) = self.0.as_mut() {
            let _ = writeln!(f, "{s}");
            let _ = f.flush();
        }
    }
}

fn xinput_connected(f: XInputGetStateFn, slot: u32) -> Option<XINPUT_STATE> {
    let mut state: XINPUT_STATE = unsafe { std::mem::zeroed() };
    if unsafe { f(slot, &mut state) } == 0 {
        Some(state)
    } else {
        None
    }
}

/// Measure the ViGEm write->read round trip: how long after we update the virtual
/// pad before an XInput reader (a game / the tester) actually sees the change.
/// This is the ADDED latency of the virtual-pad path. Does NOT include the real
/// controller's own USB latency, the game's frame sampling, or display latency.
fn run_latency(xi: XInputGetStateFn, log: &mut Log) {
    unsafe { timeBeginPeriod(1) };
    log.line("=== ViGEm round-trip latency ===");
    let before: Vec<u32> = (0..4).filter(|&s| xinput_connected(xi, s).is_some()).collect();

    let client = match vigem_client::Client::connect() {
        Ok(c) => c,
        Err(e) => { log.line(&format!("ViGEm connect failed: {e:?}")); return; }
    };
    let mut target = vigem_client::Xbox360Wired::new(client, vigem_client::TargetId::XBOX360_WIRED);
    if target.plugin().is_err() { log.line("plugin failed"); return; }
    let _ = target.wait_ready();
    std::thread::sleep(Duration::from_millis(300)); // let XInput enumerate it

    let vslot = match (0..4).find(|&s| xinput_connected(xi, s).is_some() && !before.contains(&s)) {
        Some(s) => s,
        None => { log.line("couldn't locate the virtual pad's XInput slot"); return; }
    };
    log.line(&format!("virtual pad on slot {vslot}; sampling…"));

    const A: u16 = 0x1000;
    let mut samples: Vec<f64> = Vec::new();
    for i in 0..400u32 {
        let want = i % 2 == 0;
        let raw = if want { A } else { 0 };
        let gp = vigem_client::XGamepad { buttons: vigem_client::XButtons { raw }, ..Default::default() };
        let t0 = Instant::now();
        let _ = target.update(&gp);
        let mut hit = false;
        while t0.elapsed() < Duration::from_millis(100) {
            if let Some(st) = xinput_connected(xi, vslot) {
                if (st.Gamepad.wButtons & A != 0) == want { hit = true; break; }
            }
        }
        if hit && i >= 20 {
            samples.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        std::thread::sleep(Duration::from_millis(3));
    }

    if samples.is_empty() {
        log.line("no samples captured");
        return;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples.len();
    let avg = samples.iter().sum::<f64>() / n as f64;
    let pct = |p: f64| samples[(((n - 1) as f64) * p).round() as usize];
    log.line(&format!(
        "n={n}  min={:.2}ms  median={:.2}ms  avg={:.2}ms  p95={:.2}ms  max={:.2}ms",
        samples[0], pct(0.5), avg, pct(0.95), samples[n - 1]
    ));
}

fn main() {
    let mut log = Log::new();
    log.line("NOBD universal sync (ViGEm prototype) — starting");

    let xi = match load_xinput() {
        Some(f) => f,
        None => {
            log.line("Could not load XInput (xinput1_4.dll). Aborting.");
            return;
        }
    };

    if std::env::args().any(|a| a == "--latency") {
        run_latency(xi, &mut log);
        return;
    }

    // Wait for the REAL pad and capture its slot BEFORE plugging the virtual one,
    // so we never read our own virtual output back (feedback loop).
    let mut waits = 0;
    let real_slot = loop {
        if let Some(s) = (0..4).find(|&s| xinput_connected(xi, s).is_some()) {
            log.line(&format!("Real controller found on XInput slot {s}."));
            break s;
        }
        if waits % 5 == 0 {
            log.line("Waiting for a controller… (plug it in / set it to XInput mode)");
        }
        waits += 1;
        std::thread::sleep(Duration::from_secs(1));
    };

    // Connect ViGEm + plug a virtual Xbox 360 pad.
    log.line("Connecting to ViGEmBus…");
    let client = match vigem_client::Client::connect() {
        Ok(c) => c,
        Err(e) => {
            log.line(&format!("ViGEmBus connect FAILED ({e:?}). Is the ViGEmBus driver installed?"));
            return;
        }
    };
    let id = vigem_client::TargetId::XBOX360_WIRED;
    let mut target = vigem_client::Xbox360Wired::new(client, id);
    if let Err(e) = target.plugin() {
        log.line(&format!("Virtual pad plugin FAILED: {e:?}"));
        return;
    }
    let _ = target.wait_ready();
    log.line("Virtual Xbox pad plugged. Reading real pad -> syncing -> virtual pad.");

    unsafe { timeBeginPeriod(1) };
    let epoch = Instant::now();
    let mut sync = SyncWindow::new();
    let mut last_enabled = false;
    let mut last_raw = 0u16;
    let mut last_log = Instant::now();

    loop {
        let now_us = epoch.elapsed().as_micros() as u64;

        let s = nobd_shared::state();
        let enabled = s.enabled.load(std::sync::atomic::Ordering::Relaxed) != 0;
        let window_ms = s.window_ms[0].load(std::sync::atomic::Ordering::Relaxed).clamp(1, 16);
        let window_us = window_ms * 1000;

        if enabled != last_enabled {
            log.line(&format!("sync {} (window {window_ms}ms)", if enabled { "ON" } else { "OFF" }));
            last_enabled = enabled;
        }

        if let Some(state) = xinput_connected(xi, real_slot) {
            let gp = state.Gamepad;
            let raw = gp.wButtons;
            let grouped = sync.process(raw, ATTACK_MASK, ATTACK_MASK, now_us, window_us, enabled);

            if raw != last_raw {
                log.line(&format!(
                    "in 0x{raw:04X} -> out 0x{grouped:04X}  (sync {})",
                    if enabled { "ON" } else { "off" }
                ));
                last_raw = raw;
            }

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
        } else if last_log.elapsed() >= Duration::from_secs(3) {
            log.line(&format!("(real pad on slot {real_slot} not reporting — still XInput?)"));
            last_log = Instant::now();
        }

        if last_log.elapsed() >= Duration::from_secs(5) {
            log.line(&format!("heartbeat: sync={} window={window_ms}ms", if last_enabled { "ON" } else { "off" }));
            last_log = Instant::now();
        }

        std::thread::sleep(Duration::from_micros(1000)); // ~1 kHz
    }
}
