//! Shared-memory NOBD state. Both DINPUT8.dll (in the game) and nobd.exe map the
//! same named region, so the app can drive the latch live and read real in-game
//! stats. Config fields are written by either side; stats are written by the DLL
//! and read by the app.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};

const MAGIC: u32 = 0x4E4F4244; // "NOBD"

// Fixed mapping size, decoupled from the struct size, so adding fields later
// never changes the region size (which would break a running old DLL vs new
// app). The struct must stay under this — it's a few hundred bytes.
const MAP_SIZE: usize = 4096;

/// Fixed-layout shared state. repr(C) so the DLL (edition 2024) and app
/// (edition 2021) agree on offsets. Atomics are plain memory + atomic ops,
/// which work across processes on a shared mapping.
#[repr(C)]
pub struct SharedState {
    pub magic: AtomicU32,

    // --- config (either side writes) ---
    pub enabled: AtomicU32,             // bool
    pub window_ms: AtomicU32,
    pub block_in_frame: AtomicU32,      // bool
    pub directions_windowed: AtomicU32, // bool
    pub settle_ms: AtomicU32,

    // --- stats (DLL writes, app reads) ---
    pub groups: AtomicU64,
    pub singles: AtomicU64,
    pub lat_last_us: AtomicU64,
    pub lat_max_us: AtomicU64,
    pub lat_sum_us: AtomicU64,
    pub lat_count: AtomicU64,
    pub gap_sum_us: AtomicU64,
    pub gap_max_us: AtomicU64,
    pub gap_count: AtomicU64,

    // "misses prevented": deliveries where NOBD added an attack the game's raw
    // read didn't have (a stray jab / split dash that would have happened).
    pub saves: AtomicU64,

    // game frame time (µs) derived from poll cadence; 0 until measured.
    pub frame_us: AtomicU64,

    // heartbeat the DLL bumps each XInput poll so the app can show "connected"
    pub dll_heartbeat: AtomicU64,

    // --- appended fields (keep at the END so existing offsets never move; an
    //     older DLL just ignores these, a newer DLL reads 0 from an older app) ---

    // Latch mode: 0 = Defer, 1 = Block, 2 = Continuous. Supersedes
    // block_in_frame; the UI keeps block_in_frame in sync so an older DLL that
    // only knows block/defer still behaves sanely.
    pub mode: AtomicU32,

    // Measured continuous-poll-thread rate (Hz). 0 unless Continuous is running.
    pub poll_hz: AtomicU32,

    // Game-perceived input latency (µs): physical press → the first game read
    // that actually sees it (includes frame quantization). Measured only in
    // Continuous mode, where the poll thread can timestamp the physical press.
    pub gp_lat_sum_us: AtomicU64,
    pub gp_lat_count: AtomicU64,
    pub gp_lat_max_us: AtomicU64,

    // Of the deliveries counted in gp_lat_count, how many had to wait at least
    // one game frame (the press was physically held but withheld at a prior
    // read). In Continuous this should be low; in Defer it's ~every lone press.
    pub frame_waits: AtomicU64,

    // Passive monitoring while sync is OFF: gapped two-button attempts observed,
    // and how many the game actually split across a frame (a missed dash). Only
    // recorded when sync is disabled (when on, the same events become `saves`).
    pub attempts: AtomicU64,
    pub misses: AtomicU64,
}

impl SharedState {
    fn init_defaults(&self) {
        self.enabled.store(1, Ordering::Relaxed);
        self.window_ms.store(5, Ordering::Relaxed);
        // Default to DEFER: it never stalls the game thread, so it's safe to run
        // alongside online rollback netcode. Block is opt-in for offline only.
        self.block_in_frame.store(0, Ordering::Relaxed);
        self.directions_windowed.store(0, Ordering::Relaxed);
        self.settle_ms.store(1, Ordering::Relaxed);
        self.mode.store(2, Ordering::Relaxed); // Continuous (best latency, online-safe)
        // stats start at 0 (mapping is zero-initialized)
        self.magic.store(MAGIC, Ordering::Release);
    }

    pub fn reset_stats(&self) {
        for a in [&self.groups, &self.singles, &self.lat_last_us, &self.lat_max_us,
                  &self.lat_sum_us, &self.lat_count, &self.gap_sum_us, &self.gap_max_us,
                  &self.gap_count, &self.saves,
                  &self.gp_lat_sum_us, &self.gp_lat_count, &self.gp_lat_max_us,
                  &self.frame_waits, &self.attempts, &self.misses] {
            a.store(0, Ordering::Relaxed);
        }
    }

    /// (avg_ms, max_ms) of game-perceived input latency (Continuous mode only).
    pub fn game_perceived_ms(&self) -> (f64, f64) {
        let n = self.gp_lat_count.load(Ordering::Relaxed);
        if n == 0 { return (0.0, 0.0); }
        let avg = self.gp_lat_sum_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0;
        (avg, self.gp_lat_max_us.load(Ordering::Relaxed) as f64 / 1000.0)
    }

    /// (avg_ms, max_ms) of total added latency.
    pub fn latency_ms(&self) -> (f64, f64) {
        let n = self.lat_count.load(Ordering::Relaxed);
        let avg = if n > 0 {
            self.lat_sum_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0
        } else { 0.0 };
        (avg, self.lat_max_us.load(Ordering::Relaxed) as f64 / 1000.0)
    }

    /// (avg_ms, max_ms) of measured finger gap (block-mode early-exit).
    pub fn finger_gap_ms(&self) -> (f64, f64) {
        let n = self.gap_count.load(Ordering::Relaxed);
        if n == 0 { return (0.0, 0.0); }
        let avg = self.gap_sum_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0;
        (avg, self.gap_max_us.load(Ordering::Relaxed) as f64 / 1000.0)
    }

    /// Smallest window that still catches your dashes: ceil(max gap)+1, floor 3ms.
    pub fn recommended_window_ms(&self) -> u32 {
        let (_avg, max) = self.finger_gap_ms();
        if max <= 0.0 { return 0; }
        (max.ceil() as u32 + 1).max(3)
    }
}

static PTR: OnceLock<usize> = OnceLock::new();

/// Map (creating if needed) the shared state. Safe to call from both processes.
pub fn state() -> &'static SharedState {
    let p = *PTR.get_or_init(|| unsafe { map_or_create() });
    unsafe { &*(p as *const SharedState) }
}

unsafe fn map_or_create() -> usize {
    assert!(std::mem::size_of::<SharedState>() <= MAP_SIZE, "SharedState exceeds MAP_SIZE");
    let name: Vec<u16> = "Local\\NobdSyncState\0".encode_utf16().collect();

    let h = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            MAP_SIZE as u32,
            name.as_ptr(),
        )
    };
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;

    let view = unsafe { MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, MAP_SIZE) };
    let ptr = view.Value as *mut SharedState;

    let s = unsafe { &*ptr };
    if !existed {
        s.init_defaults();
    } else {
        // Creator may still be initializing — wait briefly for the magic.
        for _ in 0..1000 {
            if s.magic.load(Ordering::Acquire) == MAGIC { break; }
            std::hint::spin_loop();
        }
    }
    ptr as usize
}
