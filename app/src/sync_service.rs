//! System-wide NOBD sync — runs in-process. Reads the real controller, runs the
//! NOBD sync window on its attack buttons, and presents the grouped result as a
//! ViGEm virtual Xbox pad. Every game reads the virtual pad, so the sync is
//! universal — no per-game DLL. Driven live by `nobd_shared` (enabled + window).
//!
//! HidHide (hiding the real pad so games see ONLY the synced pad) is layered on
//! separately; without it both pads are visible (fine for the Finger Gap Tester,
//! doubled inputs in real games).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nobd_shared::sync_window::SyncWindow;
use windows_sys::Win32::Media::timeBeginPeriod;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::UI::Input::XboxController::XINPUT_STATE;

/// XInput wButtons attack bits: A, B, X, Y, LB, RB.
const ATTACK_MASK: u16 = 0xF300;
const NO_SLOT: u32 = u32::MAX;

/// error codes for `SyncStatus::error`
pub const ERR_NONE: u8 = 0;
pub const ERR_NO_XINPUT: u8 = 1;
pub const ERR_NO_VIGEM: u8 = 2;

/// Which identity the virtual pad presents as. DualShock4 shows distinctly in
/// Steam ("Wireless Controller"), so it's tell-apart-able from a real Xbox stick;
/// Xbox360 is XInput-native (needed for raw-XInput games outside Steam).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PadType {
    Xbox360,
    DualShock4,
}

impl PadType {
    pub fn from_u32(n: u32) -> Self {
        if n == 1 { PadType::DualShock4 } else { PadType::Xbox360 }
    }
    pub fn as_u32(self) -> u32 {
        match self {
            PadType::Xbox360 => 0,
            PadType::DualShock4 => 1,
        }
    }
}

/// Either virtual-pad target, so the loop can drive whichever was plugged.
enum Target {
    X(vigem_client::Xbox360Wired<vigem_client::Client>),
    D(vigem_client::DualShock4Wired<vigem_client::Client>),
}

/// Translate a grouped XInput button mask (+ analog) into a DS4 HID report.
/// DS4 buttons: low nibble = dpad hat (8 = neutral), then Square/Cross/Circle/
/// Triangle, L1/R1, L2/R2, Share/Options, L3/R3.
fn xbox_to_ds4(buttons: u16, lt: u8, rt: u8, lx: i16, ly: i16, rx: i16, ry: i16) -> vigem_client::DS4Report {
    let up = buttons & 0x0001 != 0;
    let down = buttons & 0x0002 != 0;
    let left = buttons & 0x0004 != 0;
    let right = buttons & 0x0008 != 0;
    let hat: u16 = match (up, down, left, right) {
        (true, false, false, false) => 0,
        (true, false, false, true) => 1,
        (false, false, false, true) => 2,
        (false, true, false, true) => 3,
        (false, true, false, false) => 4,
        (false, true, true, false) => 5,
        (false, false, true, false) => 6,
        (true, false, true, false) => 7,
        _ => 8, // neutral
    };
    let mut b: u16 = hat;
    if buttons & 0x4000 != 0 { b |= 0x10; }   // X  -> Square
    if buttons & 0x1000 != 0 { b |= 0x20; }   // A  -> Cross
    if buttons & 0x2000 != 0 { b |= 0x40; }   // B  -> Circle
    if buttons & 0x8000 != 0 { b |= 0x80; }   // Y  -> Triangle
    if buttons & 0x0100 != 0 { b |= 0x100; }  // LB -> L1
    if buttons & 0x0200 != 0 { b |= 0x200; }  // RB -> R1
    if lt > 30 { b |= 0x400; }                // LT -> L2
    if rt > 30 { b |= 0x800; }                // RT -> R2
    if buttons & 0x0020 != 0 { b |= 0x1000; } // Back  -> Share
    if buttons & 0x0010 != 0 { b |= 0x2000; } // Start -> Options
    if buttons & 0x0040 != 0 { b |= 0x4000; } // LS -> L3
    if buttons & 0x0080 != 0 { b |= 0x8000; } // RS -> R3

    let axis = |v: i16| (((v as i32) + 32768) >> 8) as u8;
    vigem_client::DS4Report {
        thumb_lx: axis(lx),
        thumb_ly: 255u8.wrapping_sub(axis(ly)), // DS4 Y is inverted vs XInput
        thumb_rx: axis(rx),
        thumb_ry: 255u8.wrapping_sub(axis(ry)),
        buttons: b,
        special: 0,
        trigger_l: lt,
        trigger_r: rt,
    }
}

type XInputGetStateFn = unsafe extern "system" fn(u32, *mut XINPUT_STATE) -> u32;

/// Resolve XInputGetState from System32 (the static windows-sys symbol hangs on
/// some systems; the dynamic load matches the GUI's input backend).
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

fn xinput_state(f: XInputGetStateFn, slot: u32) -> Option<XINPUT_STATE> {
    let mut state: XINPUT_STATE = unsafe { std::mem::zeroed() };
    if unsafe { f(slot, &mut state) } == 0 {
        Some(state)
    } else {
        None
    }
}

pub struct SyncStatus {
    /// Virtual pad plugged and the loop is running.
    pub active: AtomicBool,
    /// The real pad is currently reporting.
    pub real_present: AtomicBool,
    /// XInput slot of the real pad (NO_SLOT until found).
    pub real_slot: AtomicU32,
    /// ERR_* code.
    pub error: AtomicU8,
}

impl SyncStatus {
    fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            real_present: AtomicBool::new(false),
            real_slot: AtomicU32::new(NO_SLOT),
            error: AtomicU8::new(ERR_NONE),
        }
    }
}

/// Background system-wide sync. Drop stops the thread and unplugs the virtual pad.
pub struct SyncService {
    stop: Arc<AtomicBool>,
    status: Arc<SyncStatus>,
    handle: Option<JoinHandle<()>>,
}

impl SyncService {
    pub fn start(pad: PadType) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(SyncStatus::new());
        let handle = {
            let stop = stop.clone();
            let status = status.clone();
            std::thread::spawn(move || run(stop, status, pad))
        };
        Self { stop, status, handle: Some(handle) }
    }

    pub fn is_active(&self) -> bool {
        self.status.active.load(Ordering::Relaxed)
    }

    pub fn real_present(&self) -> bool {
        self.status.real_present.load(Ordering::Relaxed)
    }

    pub fn real_slot(&self) -> Option<u32> {
        let s = self.status.real_slot.load(Ordering::Relaxed);
        if s == NO_SLOT {
            None
        } else {
            Some(s)
        }
    }

    pub fn error(&self) -> u8 {
        self.status.error.load(Ordering::Relaxed)
    }
}

impl Drop for SyncService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run(stop: Arc<AtomicBool>, status: Arc<SyncStatus>, pad: PadType) {
    let xi = match load_xinput() {
        Some(f) => f,
        None => {
            status.error.store(ERR_NO_XINPUT, Ordering::Relaxed);
            return;
        }
    };

    // Wait for the real pad BEFORE plugging the virtual one (avoid feedback).
    let real_slot = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if let Some(s) = (0..4).find(|&s| xinput_state(xi, s).is_some()) {
            status.real_slot.store(s, Ordering::Relaxed);
            status.real_present.store(true, Ordering::Relaxed);
            break s;
        }
        status.real_slot.store(NO_SLOT, Ordering::Relaxed);
        status.real_present.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(500));
    };

    let client = match vigem_client::Client::connect() {
        Ok(c) => c,
        Err(_) => {
            status.error.store(ERR_NO_VIGEM, Ordering::Relaxed);
            return;
        }
    };
    let mut target = match pad {
        PadType::Xbox360 => {
            let mut t = vigem_client::Xbox360Wired::new(client, vigem_client::TargetId::XBOX360_WIRED);
            if t.plugin().is_err() {
                status.error.store(ERR_NO_VIGEM, Ordering::Relaxed);
                return;
            }
            let _ = t.wait_ready();
            Target::X(t)
        }
        PadType::DualShock4 => {
            let mut t = vigem_client::DualShock4Wired::new(client, vigem_client::TargetId::DUALSHOCK4_WIRED);
            if t.plugin().is_err() {
                status.error.store(ERR_NO_VIGEM, Ordering::Relaxed);
                return;
            }
            let _ = t.wait_ready();
            Target::D(t)
        }
    };

    unsafe { timeBeginPeriod(1) };
    status.active.store(true, Ordering::Relaxed);
    status.error.store(ERR_NONE, Ordering::Relaxed);

    let epoch = Instant::now();
    let mut sync = SyncWindow::new();

    while !stop.load(Ordering::Relaxed) {
        let now_us = epoch.elapsed().as_micros() as u64;
        let s = nobd_shared::state();
        let enabled = s.enabled.load(Ordering::Relaxed) != 0;
        let window_us = s.window_ms[0].load(Ordering::Relaxed).clamp(1, 16) * 1000;

        if let Some(state) = xinput_state(xi, real_slot) {
            status.real_present.store(true, Ordering::Relaxed);
            let gp = state.Gamepad;
            let grouped =
                sync.process(gp.wButtons, ATTACK_MASK, ATTACK_MASK, now_us, window_us, enabled);
            match &mut target {
                Target::X(t) => {
                    let out = vigem_client::XGamepad {
                        buttons: vigem_client::XButtons { raw: grouped },
                        left_trigger: gp.bLeftTrigger,
                        right_trigger: gp.bRightTrigger,
                        thumb_lx: gp.sThumbLX,
                        thumb_ly: gp.sThumbLY,
                        thumb_rx: gp.sThumbRX,
                        thumb_ry: gp.sThumbRY,
                    };
                    let _ = t.update(&out);
                }
                Target::D(t) => {
                    let r = xbox_to_ds4(
                        grouped, gp.bLeftTrigger, gp.bRightTrigger,
                        gp.sThumbLX, gp.sThumbLY, gp.sThumbRX, gp.sThumbRY,
                    );
                    let _ = t.update(&r);
                }
            }
        } else {
            status.real_present.store(false, Ordering::Relaxed);
        }

        std::thread::sleep(Duration::from_micros(1000)); // ~1 kHz
    }

    status.active.store(false, Ordering::Relaxed);
    // `target` drops here → virtual pad unplugged.
}
