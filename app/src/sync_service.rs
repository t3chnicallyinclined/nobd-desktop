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

/// One 60 Hz frame. A raw press group that has been open longer than this is not
/// a chord attempt any more - it is a held button - so the telemetry closes it.
const GROUP_MAX_US: u64 = 16_667;

/// The pad mode of the most recently started sync loop, so an exit path that
/// does not own the controller (the tray's Quit) can still release it.
/// `u32::MAX` = no loop has run this session.
static LAST_PAD: AtomicU32 = AtomicU32::new(u32::MAX);

/// Drive the virtual pad back to neutral: all buttons up, sticks centred.
///
/// The devnode outlives the app (LIFETIME_PARENT_PRESENT) and the driver keeps
/// publishing whatever is in the shared section, so quitting mid-press left the
/// NOBD Controller holding that button - or a direction - in every game, forever,
/// until the app was run again. Best-effort by design: if we cannot open it there
/// is nothing to release.
pub fn release_pad() {
    let raw = LAST_PAD.load(Ordering::Relaxed);
    if raw == u32::MAX {
        return;
    }
    if let Ok(mut c) = hm_native::NobdController::open(PadType::from_u32(raw).mode()) {
        c.submit(0, 0, 0, 0, 0, 0, 0);
    }
}

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
        // XInput is the default output — it works in the most games (incl.
        // XInput-only ones like MvC2). Only the explicit HID marker (1) selects
        // the branded HID pad; every other/legacy value falls through to XInput.
        if n == 1 {
            PadType::Hid
        } else {
            PadType::Xinput
        }
    }
    pub fn as_u32(self) -> u32 {
        match self {
            PadType::Hid => 1,
            PadType::Xinput => 0,
        }
    }
    pub fn mode(self) -> hm_native::PadMode {
        match self {
            PadType::Hid => hm_native::PadMode::Hid,
            PadType::Xinput => hm_native::PadMode::Xinput,
        }
    }
}

/// Where the sync loop reads the real controller from.
#[derive(Clone)]
pub enum SyncSource {
    /// XInput (Xbox pads) — scans slots 0-3. The default.
    XInput,
    /// A raw HID (DirectInput) stick read directly. Required to feed a
    /// non-XInput stick into an XInput-only game (the companion becomes the only
    /// XInput pad the game sees).
    Hid(crate::hid::HidDeviceId),
    /// NOBD Bulk (Extreme Low Latency): the stick's WinUSB bulk stream (~10 kHz / ~90 us) instead of
    /// its XInput poll (~500 us). The companion is still the game-facing XInput pad.
    Bulk,
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
    /// Companion submit->readable latency in us (min/avg/max), measured once when XInput sync
    /// starts. 0 = not measured. This is how fresh a submit reaches a game's XInputGetState.
    pub lat_min: AtomicU32,
    pub lat_avg: AtomicU32,
    pub lat_max: AtomicU32,
    /// NOBD Bulk stream rate (payloads/sec) in Extreme Low Latency mode; 0 = not in bulk mode. This is
    /// the stick->app freshness: at this rate the input is at most ~1/rate old when the app reads it.
    pub bulk_rate_hz: AtomicU32,
    /// A sync window is open RIGHT NOW (a press is being held back). Drives the
    /// live pulse in the UI — the one signal that proves the window is running
    /// without needing a second controller to read our own output back.
    pub window_open: AtomicBool,
}

impl SyncStatus {
    fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            real_present: AtomicBool::new(false),
            real_slot: AtomicU32::new(NO_SLOT),
            virtual_slot: AtomicU32::new(NO_SLOT),
            error: AtomicU8::new(ERR_NONE),
            lat_min: AtomicU32::new(0),
            lat_avg: AtomicU32::new(0),
            lat_max: AtomicU32::new(0),
            bulk_rate_hz: AtomicU32::new(0),
            window_open: AtomicBool::new(false),
        }
    }
}

/// Live proof that the window is doing something, measured INSIDE the sync loop
/// and published to the shared state.
///
/// This exists because the alternative — reading our own virtual pad back
/// through XInput and fingerprinting its slot — only ever worked on the XInput
/// source, so on a DirectInput stick or the NOBD Bulk stream the UI had no way
/// to tell whether sync was doing anything at all. Measuring here works on every
/// source, needs no second controller, and reports what the game actually got.
struct SyncTelemetry {
    prev_raw: u16,
    prev_out: u16,
    /// Raw press time of the first attack of the group currently forming.
    first_press_us: u64,
    /// Raw press time of the most recent attack press.
    last_press_us: u64,
    /// Attack bits pressed raw during the group currently forming. Lets us count
    /// chord ATTEMPTS, not just successes - the difference between "nothing is
    /// happening" and "your window is tighter than your finger gap".
    raw_group: u16,
    tracking: bool,
}

impl SyncTelemetry {
    fn new() -> Self {
        nobd_shared::state().reset_stats(); // per-session counters
        Self {
            prev_raw: 0,
            prev_out: 0,
            first_press_us: 0,
            last_press_us: 0,
            raw_group: 0,
            tracking: false,
        }
    }

    /// Feed one tick: the raw buttons in, the grouped buttons out, the delay the
    /// window actually applied, and whether a window is open. Call after
    /// `SyncWindow::process`.
    fn observe(
        &mut self,
        raw: u16,
        out: u16,
        now_us: u64,
        window_open: bool,
        delay_us: u32,
        status: &SyncStatus,
    ) {
        let p = &nobd_shared::state().players[0];

        // Close a finished raw group: every attack released, OR it has been open
        // longer than one frame. The frame cap is load-bearing - without it a
        // HELD button anchors `first_press_us` indefinitely, so holding LP and
        // pressing HP a second later reported a 1000 ms finger gap and pinned
        // the session maximum there via fetch_max.
        if self.tracking
            && (raw & ATTACK_MASK == 0
                || now_us.saturating_sub(self.first_press_us) >= GROUP_MAX_US)
        {
            if self.raw_group.count_ones() >= 2 {
                // A chord the player attempted, whether or not the window
                // grouped it - so the UI can tell "nothing pressed yet" from
                // "your window is too tight and every chord is splitting".
                p.attempts.fetch_add(1, Ordering::Relaxed);
                let g = self.last_press_us.saturating_sub(self.first_press_us);
                p.raw_gap_sum_us.fetch_add(g, Ordering::Relaxed);
                p.raw_gap_count.fetch_add(1, Ordering::Relaxed);
                p.raw_gap_max_us.fetch_max(g, Ordering::Relaxed);
            }
            self.tracking = false;
            self.raw_group = 0;
        }

        let pressed = raw & !self.prev_raw & ATTACK_MASK;
        if pressed != 0 {
            if !self.tracking {
                self.first_press_us = now_us;
                self.raw_group = 0;
                self.tracking = true;
            }
            self.last_press_us = now_us;
            self.raw_group |= pressed;
        }

        // Output attack press edges are commits the game actually sees.
        let committed = out & !self.prev_out & ATTACK_MASK;
        if committed != 0 {
            if committed.count_ones() >= 2 {
                p.groups.fetch_add(1, Ordering::Relaxed);
                // Spread of the presses that were grouped. Bounded by the window
                // by construction, so this is NOT the finger gap - raw_gap_* is.
                let gap_us = self.last_press_us.saturating_sub(self.first_press_us);
                p.gap_sum_us.fetch_add(gap_us, Ordering::Relaxed);
                p.gap_count.fetch_add(1, Ordering::Relaxed);
                p.gap_max_us.fetch_max(gap_us, Ordering::Relaxed);
                // Accumulate the PROBABILITY a free-running 60 Hz poll would have
                // split this pair, not a simulated coin flip: our clock has no
                // relationship to the game's phase, so a per-chord verdict is a
                // draw while the running sum is an unbiased estimate.
                let risk = (gap_us as f64 / 1000.0 / crate::stats::FRAME_MS).min(1.0);
                p.risk_sum_ppm
                    .fetch_add((risk * 1_000_000.0) as u64, Ordering::Relaxed);
            } else {
                p.singles.fetch_add(1, Ordering::Relaxed);
            }
            // The delay the window really applied, straight from SyncWindow.
            // Measuring it as `now - first_press` instead reported the time since
            // the player first touched ANY attack, which is unbounded.
            let d = delay_us as u64;
            p.lat_last_us.store(d, Ordering::Relaxed);
            p.lat_sum_us.fetch_add(d, Ordering::Relaxed);
            p.lat_count.fetch_add(1, Ordering::Relaxed);
            p.lat_max_us.fetch_max(d, Ordering::Relaxed);
        }

        status.window_open.store(window_open, Ordering::Relaxed);
        self.prev_raw = raw;
        self.prev_out = out;
    }
}

/// Background system-wide sync. Drop stops the thread.
pub struct SyncService {
    stop: Arc<AtomicBool>,
    status: Arc<SyncStatus>,
    handle: Option<JoinHandle<()>>,
}

impl SyncService {
    pub fn start(pad: PadType, source: SyncSource) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(SyncStatus::new());
        let handle = {
            let stop = stop.clone();
            let status = status.clone();
            std::thread::spawn(move || run(stop, status, pad, source))
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

    /// (min, avg, max) us of the companion submit->readable latency, if measured (XInput mode).
    pub fn latency(&self) -> Option<(u32, u32, u32)> {
        let avg = self.status.lat_avg.load(Ordering::Relaxed);
        if avg == 0 {
            None
        } else {
            Some((
                self.status.lat_min.load(Ordering::Relaxed),
                avg,
                self.status.lat_max.load(Ordering::Relaxed),
            ))
        }
    }

    /// NOBD Bulk stream rate (payloads/sec) in Extreme Low Latency mode; 0 when not in bulk mode.
    pub fn bulk_rate(&self) -> u32 {
        self.status.bulk_rate_hz.load(Ordering::Relaxed)
    }

    /// A sync window is open right now — a press is being held back this instant.
    pub fn window_open(&self) -> bool {
        self.status.window_open.load(Ordering::Relaxed)
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

/// Identify OUR companion's XInput slot by FINGERPRINTING it. XInput exposes no
/// device identity, so we drive a unique button marker onto the companion and
/// find the slot reporting exactly that — definitively our own output. Without
/// it we can't tell the companion from the real pad, which causes feedback (sync
/// reading its own output) and a phantom pad in the tester.
///
/// Runs on EVERY source, not just the XInput one. It used to be inline in
/// `run_xinput`, so on a DirectInput stick or the NOBD Bulk stream the tester
/// never learned which slot was ours — it then labelled our own synced output
/// "your stick" and could never show the synced side at all.
fn fingerprint_companion(
    xi: XInputGetStateFn,
    ctrl: &mut hm_native::NobdController,
    stop: &AtomicBool,
    status: &SyncStatus,
) -> Option<u32> {
    const MARKER: u16 = 0x0330; // LB|RB|Back|Start — an unlikely-to-be-held combo
    // Hold the marker and watch for the slot that reflects it. The companion's
    // UMDF driver can take up to ~500ms to (re)attach to the freshly-created
    // shared section, so give it up to ~2s. Require TWO consecutive matches on
    // the same slot so a real pad transiently holding those buttons can't be
    // mistaken for our output.
    let mut prev: Option<u32> = None;
    let mut found: Option<u32> = None;
    for _ in 0..200 {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        ctrl.submit(MARKER, 0, 0, 0, 0, 0, 0);
        std::thread::sleep(Duration::from_millis(10));
        let hit =
            (0..4).find(|&s| xinput_state(xi, s).map_or(false, |st| st.Gamepad.wButtons == MARKER));
        if hit.is_some() && hit == prev {
            found = hit;
            status.virtual_slot.store(hit.unwrap(), Ordering::Relaxed);
            break;
        }
        prev = hit;
    }
    ctrl.submit(0, 0, 0, 0, 0, 0, 0); // release the marker
    found
}

fn run(stop: Arc<AtomicBool>, status: Arc<SyncStatus>, pad: PadType, source: SyncSource) {
    LAST_PAD.store(pad.as_u32(), Ordering::Relaxed);
    match source {
        SyncSource::XInput => run_xinput(stop, status, pad),
        SyncSource::Hid(id) => run_hid(stop, status, pad, id),
        SyncSource::Bulk => run_bulk(stop, status, pad),
    }
}

/// Extreme Low Latency: read the stick's WinUSB bulk stream (~10 kHz) and present the grouped result
/// as the XUSB companion. Mirrors run_hid, but the source is the ~90 us bulk hop, not the ~500 us
/// XInput poll -- so the whole button->game path collapses toward the companion's ~54 us floor.
fn run_bulk(stop: Arc<AtomicBool>, status: Arc<SyncStatus>, pad: PadType) {
    let snap = crate::bulk::BulkSnapshot::new();
    {
        let snap = snap.clone();
        std::thread::spawn(move || crate::bulk::run_reader(snap));
    }
    let mut ctrl = match hm_native::NobdController::open(pad.mode()) {
        Ok(c) => c,
        Err(_) => {
            status.error.store(ERR_NO_NOBD, Ordering::Relaxed);
            return;
        }
    };
    // Tell the tester which XInput slot is our own output (see fingerprint_companion).
    if pad == PadType::Xinput {
        if let Some(xi) = load_xinput() {
            fingerprint_companion(xi, &mut ctrl, &stop, &status);
        }
    }

    unsafe { timeBeginPeriod(1) };
    status.active.store(true, Ordering::Relaxed);
    status.error.store(ERR_NONE, Ordering::Relaxed);

    let epoch = Instant::now();
    let mut sync = SyncWindow::new();
    let mut tel = SyncTelemetry::new();
    while !stop.load(Ordering::Relaxed) {
        let now_us = epoch.elapsed().as_micros() as u64;
        let s = nobd_shared::state();
        let enabled = s.enabled.load(Ordering::Relaxed) != 0;
        let window_us = s.window_ms[0].load(Ordering::Relaxed).clamp(1, 16) * 1000;

        if snap.present() {
            status.real_present.store(true, Ordering::Relaxed);
            let (buttons, lt, rt, lx, ly, rx, ry) = snap.get();
            let grouped = sync.process(buttons, ATTACK_MASK, ATTACK_MASK, now_us, window_us, enabled);
            ctrl.submit(grouped, lt, rt, lx, ly, rx, ry);
            tel.observe(buttons, grouped, now_us, sync.pending_since().is_some(), sync.last_commit_delay_us(), &status);
        } else {
            status.real_present.store(false, Ordering::Relaxed);
        }
        status.bulk_rate_hz.store(snap.rate_hz(), Ordering::Relaxed);
        std::thread::sleep(Duration::from_micros(250)); // ~4 kHz submit; the source is already ~10 kHz fresh
    }
    ctrl.submit(0, 0, 0, 0, 0, 0, 0); // never hand the game a stuck button
    status.active.store(false, Ordering::Relaxed);
    status.bulk_rate_hz.store(0, Ordering::Relaxed);
    snap.stop(); // signal the reader thread to exit
}

/// Read a raw HID (DirectInput) stick and present the grouped result as the NOBD
/// pad. This is the path that lets a non-XInput stick drive sync into an
/// XInput-only game: the stick feeds here, the XUSB companion is the only XInput
/// device the game sees.
fn run_hid(
    stop: Arc<AtomicBool>,
    status: Arc<SyncStatus>,
    pad: PadType,
    id: crate::hid::HidDeviceId,
) {
    // Full-state reader publishes into `snap` at the stick's report rate.
    let snap = crate::hid::HidSnapshot::new();
    {
        let snap = snap.clone();
        std::thread::spawn(move || crate::hid::run_state_reader(id, snap));
    }

    let mut ctrl = match hm_native::NobdController::open(pad.mode()) {
        Ok(c) => c,
        Err(_) => {
            status.error.store(ERR_NO_NOBD, Ordering::Relaxed);
            return;
        }
    };

    // Tell the tester which XInput slot is our own output (see fingerprint_companion).
    if pad == PadType::Xinput {
        if let Some(xi) = load_xinput() {
            fingerprint_companion(xi, &mut ctrl, &stop, &status);
        }
    }

    unsafe { timeBeginPeriod(1) };
    status.active.store(true, Ordering::Relaxed);
    status.error.store(ERR_NONE, Ordering::Relaxed);

    let epoch = Instant::now();
    let mut sync = SyncWindow::new();
    let mut tel = SyncTelemetry::new();

    while !stop.load(Ordering::Relaxed) {
        let now_us = epoch.elapsed().as_micros() as u64;
        let s = nobd_shared::state();
        let enabled = s.enabled.load(Ordering::Relaxed) != 0;
        let window_us = s.window_ms[0].load(Ordering::Relaxed).clamp(1, 16) * 1000;

        let (buttons, lt, rt, lx, ly) = snap.read();
        status.real_present.store(snap.is_present(), Ordering::Relaxed);
        let grouped = sync.process(buttons, ATTACK_MASK, ATTACK_MASK, now_us, window_us, enabled);
        // Right stick stays centered — a stick's directions come from the d-pad
        // (mirrored to the left stick by the reader).
        ctrl.submit(grouped, lt, rt, lx, ly, 0, 0);
        tel.observe(buttons, grouped, now_us, sync.pending_since().is_some(), sync.last_commit_delay_us(), &status);

        std::thread::sleep(Duration::from_micros(1000)); // ~1 kHz
    }

    snap.stop();
    ctrl.submit(0, 0, 0, 0, 0, 0, 0); // never hand the game a stuck button
    status.active.store(false, Ordering::Relaxed);
}

fn run_xinput(stop: Arc<AtomicBool>, status: Arc<SyncStatus>, pad: PadType) {
    let xi = match load_xinput() {
        Some(f) => f,
        None => {
            status.error.store(ERR_NO_XINPUT, Ordering::Relaxed);
            return;
        }
    };

    let mut ctrl = match hm_native::NobdController::open(pad.mode()) {
        Ok(c) => c,
        Err(_) => {
            status.error.store(ERR_NO_NOBD, Ordering::Relaxed);
            return;
        }
    };

    let mut companion_slot: Option<u32> = None;
    if pad == PadType::Xinput {
        companion_slot = fingerprint_companion(xi, &mut ctrl, &stop, &status);
    }

    // Track-B probe: measure the companion submit->readable latency ONCE. Write a unique LX marker,
    // tight-read the companion slot until it appears, time it -- how fresh a submit reaches a game's
    // XInputGetState. Real-controller floor ~500us; <100us means the companion carries fresh data,
    // so feeding it from a faster stick source (the bulk stream) is worth building.
    if let Some(cslot) = companion_slot {
        let mut samples: Vec<u64> = Vec::with_capacity(500);
        let mut counter: i16 = 777;
        for _ in 0..500 {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            counter = counter.wrapping_add(101);
            if counter == 0 {
                counter = 1;
            }
            let t0 = Instant::now();
            ctrl.submit(0, 0, 0, counter, 0, 0, 0);
            while t0.elapsed() < Duration::from_millis(10) {
                if xinput_state(xi, cslot).map_or(false, |st| st.Gamepad.sThumbLX == counter) {
                    break;
                }
            }
            samples.push(t0.elapsed().as_micros() as u64);
        }
        ctrl.submit(0, 0, 0, 0, 0, 0, 0); // release
        if !samples.is_empty() {
            samples.sort_unstable();
            let sum: u64 = samples.iter().sum();
            status.lat_min.store(samples[0] as u32, Ordering::Relaxed);
            status
                .lat_avg
                .store((sum / samples.len() as u64) as u32, Ordering::Relaxed);
            status
                .lat_max
                .store(samples[samples.len() - 1] as u32, Ordering::Relaxed);
        }
    }

    // Wait for the real pad = the first present slot that is NOT our companion.
    let real_slot = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if let Some(s) =
            (0..4).find(|&s| Some(s) != companion_slot && xinput_state(xi, s).is_some())
        {
            status.real_slot.store(s, Ordering::Relaxed);
            status.real_present.store(true, Ordering::Relaxed);
            break s;
        }
        status.real_slot.store(NO_SLOT, Ordering::Relaxed);
        status.real_present.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(500));
    };

    unsafe { timeBeginPeriod(1) };
    status.active.store(true, Ordering::Relaxed);
    status.error.store(ERR_NONE, Ordering::Relaxed);

    let epoch = Instant::now();
    let mut sync = SyncWindow::new();
    let mut tel = SyncTelemetry::new();

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
            tel.observe(gp.wButtons, grouped, now_us, sync.pending_since().is_some(), sync.last_commit_delay_us(), &status);
        } else {
            status.real_present.store(false, Ordering::Relaxed);
        }

        std::thread::sleep(Duration::from_micros(1000)); // ~1 kHz
    }

    ctrl.submit(0, 0, 0, 0, 0, 0, 0); // never hand the game a stuck button
    status.active.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod telemetry_tests {
    use super::*;

    const A: u16 = 0x1000; // XInput A
    const B: u16 = 0x2000; // XInput B

    /// One test, not several: `observe` writes to the process-global shared
    /// state, so parallel tests would race each other.
    #[test]
    fn raw_gap_survives_a_held_button_and_measures_a_real_chord() {
        let status = SyncStatus::new();
        let p = &nobd_shared::state().players[0];

        // --- 1. hold A for a full second, then press B ---------------------
        // The bug: `tracking` only reset when EVERY attack released, so
        // first_press_us stayed anchored at the A press and this reported a
        // 1000 ms finger gap - pinned for the session by fetch_max.
        let mut tel = SyncTelemetry::new(); // resets the counters
        for ms in 0..1100u64 {
            let raw = match ms {
                0..=999 => A,
                1000..=1049 => A | B,
                _ => 0, // release, so the group actually closes and records
            };
            tel.observe(raw, 0, ms * 1000, false, 0, &status);
        }
        let (_, max) = p.raw_finger_gap_ms();
        assert!(
            max < 17.0,
            "a held button inflated the finger gap to {max:.1} ms"
        );

        // --- 2. a real 3 ms chord, pressed and released --------------------
        let mut tel = SyncTelemetry::new();
        for ms in 0..40u64 {
            let raw = match ms {
                0..=2 => A,
                3..=20 => A | B,
                _ => 0,
            };
            tel.observe(raw, 0, ms * 1000, false, 0, &status);
        }
        let (avg, max) = p.raw_finger_gap_ms();
        let attempts = p.attempts.load(Ordering::Relaxed);
        assert_eq!(attempts, 1, "one chord attempt should have been counted");
        assert!(
            (avg - 3.0).abs() < 0.5 && (max - 3.0).abs() < 0.5,
            "expected a ~3 ms finger gap, got avg {avg:.1} max {max:.1}"
        );

        // --- 3. a chord the window FAILS to group still counts as an attempt
        // This is the signal that drives the "TOO TIGHT" diagnosis, so it must
        // survive the window splitting the chord into two singles.
        let mut tel = SyncTelemetry::new();
        for ms in 0..40u64 {
            let raw = match ms {
                0..=7 => A,
                8..=20 => A | B,
                _ => 0,
            };
            // window too tight: A commits alone at 1 ms, B alone at 9 ms
            let out = match ms {
                0 => 0,
                1..=8 => A,
                _ => A | B,
            };
            tel.observe(raw, out, ms * 1000, false, 1_000, &status);
        }
        assert_eq!(
            p.attempts.load(Ordering::Relaxed),
            1,
            "a split chord is still an attempt"
        );
        assert_eq!(p.groups.load(Ordering::Relaxed), 0, "nothing was grouped");
        assert_eq!(p.singles.load(Ordering::Relaxed), 2, "two separate singles");
        let (_, hold_max) = p.latency_ms();
        assert!(
            hold_max < 2.0,
            "hold must be the window's own delay, got {hold_max:.1} ms"
        );
    }
}
