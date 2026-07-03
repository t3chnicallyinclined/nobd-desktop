//! System-wide NOBD sync — runs in-process. Reads the real controller, runs the
//! NOBD sync window on its attack buttons, and presents the grouped result as the
//! native NOBD Controller via the pure-Rust `hm-native` client (no ViGEm).
//!
//! Two modes: `Hid` = the branded "NOBD Controller" (DirectInput/Steam/joy.cpl),
//! `Xinput` = an Xbox 360 wired pad for raw-XInput games (MvC2). Both require the
//! one-time elevated setup and an elevated process (to create the shared section).

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
/// The NOBD device couldn't be opened (not set up, or not elevated).
pub const ERR_NO_NOBD: u8 = 3;

/// Which virtual pad the NOBD Controller presents as. Persisted as u32.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PadType {
    /// Branded plain-HID "NOBD Controller" (DirectInput / Steam / joy.cpl).
    Hid,
    /// XInput (Xbox 360 wired) — for raw-XInput games like MvC2.
    Xinput,
}

impl PadType {
    pub fn from_u32(n: u32) -> Self {
        // Branded HID is the default. Only the explicit XInput marker (100)
        // selects XInput — old ViGEm pad_type values (0/1/2) all fall through
        // to Hid, so a migrating user isn't stuck on the wrong mode.
        if n == 100 {
            PadType::Xinput
        } else {
            PadType::Hid
        }
    }
    pub fn as_u32(self) -> u32 {
        match self {
            PadType::Hid => 0,
            PadType::Xinput => 100,
        }
    }
    pub fn mode(self) -> hm_native::PadMode {
        match self {
            PadType::Hid => hm_native::PadMode::Hid,
            PadType::Xinput => hm_native::PadMode::Xinput,
        }
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
    /// Virtual pad live and the loop is running.
    pub active: AtomicBool,
    /// The real pad is currently reporting.
    pub real_present: AtomicBool,
    /// XInput slot of the real pad (NO_SLOT until found).
    pub real_slot: AtomicU32,
    /// XInput slot the virtual pad landed on (only for XInput mode; the branded
    /// HID pad isn't an XInput device so this stays NO_SLOT).
    pub virtual_slot: AtomicU32,
    /// ERR_* code.
    pub error: AtomicU8,
}

impl SyncStatus {
    fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            real_present: AtomicBool::new(false),
            real_slot: AtomicU32::new(NO_SLOT),
            virtual_slot: AtomicU32::new(NO_SLOT),
            error: AtomicU8::new(ERR_NONE),
        }
    }
}

/// Background system-wide sync. Drop stops the thread.
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

    /// A non-running service (no thread) — the state after Eject, when the NOBD
    /// device is gone. Reports not-active with ERR_NO_NOBD so the UI shows the
    /// "Enable" path again.
    pub fn stopped() -> Self {
        let status = Arc::new(SyncStatus::new());
        status.error.store(ERR_NO_NOBD, Ordering::Relaxed);
        Self { stop: Arc::new(AtomicBool::new(true)), status, handle: None }
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

    /// XInput slot of the virtual NOBD pad (only known in XInput mode).
    pub fn virtual_slot(&self) -> Option<u32> {
        let s = self.status.virtual_slot.load(Ordering::Relaxed);
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

    // Wait for the real pad before opening the virtual one (avoid feedback).
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

    // Snapshot connected XInput slots (to identify the virtual pad's slot in
    // XInput mode).
    let before: Vec<u32> = (0..4).filter(|&s| xinput_state(xi, s).is_some()).collect();

    let mut ctrl = match hm_native::NobdController::open(pad.mode()) {
        Ok(c) => c,
        Err(_) => {
            status.error.store(ERR_NO_NOBD, Ordering::Relaxed);
            return;
        }
    };

    if pad == PadType::Xinput {
        std::thread::sleep(Duration::from_millis(300)); // let XInput enumerate it
        if let Some(vs) = (0..4).find(|&s| !before.contains(&s) && xinput_state(xi, s).is_some()) {
            status.virtual_slot.store(vs, Ordering::Relaxed);
        }
    }

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
            ctrl.submit(
                grouped,
                gp.bLeftTrigger,
                gp.bRightTrigger,
                gp.sThumbLX,
                gp.sThumbLY,
                gp.sThumbRX,
                gp.sThumbRY,
            );
        } else {
            status.real_present.store(false, Ordering::Relaxed);
        }

        std::thread::sleep(Duration::from_micros(1000)); // ~1 kHz
    }

    status.active.store(false, Ordering::Relaxed);
}
