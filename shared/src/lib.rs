//! Shared-memory NOBD state. Both DINPUT8.dll (in the game) and nobd.exe map the
//! same named region, so the app can drive the latch live and read real in-game
//! stats. Config fields are shared; stats are per-player (P1/P2), written by the
//! DLL and read by the app.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};

// Bumped from the single-player layout ("NOBD") — the per-player stats array
// changed the struct offsets, so a stale mapping from an older build is re-init'd.
const MAGIC: u32 = 0x4E_42_44_33; // "NBD3" — window_ms became per-player

/// Number of player slots with their own sync window + stats.
pub const NUM_PLAYERS: usize = 2;

// Fixed mapping size, decoupled from the struct size, so adding fields later
// never changes the region size. The struct must stay well under this.
const MAP_SIZE: usize = 4096;

/// Per-player live stats (DLL writes, app reads). One per controller slot.
#[repr(C)]
pub struct PlayerStats {
    pub groups: AtomicU64,
    pub singles: AtomicU64,
    pub lat_last_us: AtomicU64,
    pub lat_max_us: AtomicU64,
    pub lat_sum_us: AtomicU64,
    pub lat_count: AtomicU64,
    pub gap_sum_us: AtomicU64,
    pub gap_max_us: AtomicU64,
    pub gap_count: AtomicU64,
    // provable frame-boundary saves (a group that crossed a frame)
    pub saves: AtomicU64,
    // game frame time (µs) from this controller's read cadence
    pub frame_us: AtomicU64,
    // game-perceived input latency (µs): physical press → first game read
    pub gp_lat_sum_us: AtomicU64,
    pub gp_lat_count: AtomicU64,
    pub gp_lat_max_us: AtomicU64,
    // deliveries that actually waited a frame
    pub frame_waits: AtomicU64,
    // passive monitor (sync OFF): gapped attempts + splits the game made
    pub attempts: AtomicU64,
    pub misses: AtomicU64,
}

impl PlayerStats {
    fn reset(&self) {
        for a in [
            &self.groups, &self.singles, &self.lat_last_us, &self.lat_max_us,
            &self.lat_sum_us, &self.lat_count, &self.gap_sum_us, &self.gap_max_us,
            &self.gap_count, &self.saves, &self.frame_us, &self.gp_lat_sum_us,
            &self.gp_lat_count, &self.gp_lat_max_us, &self.frame_waits,
            &self.attempts, &self.misses,
        ] {
            a.store(0, Ordering::Relaxed);
        }
    }

    /// (avg_ms, max_ms) of game-perceived input latency (Continuous only).
    pub fn game_perceived_ms(&self) -> (f64, f64) {
        let n = self.gp_lat_count.load(Ordering::Relaxed);
        if n == 0 { return (0.0, 0.0); }
        let avg = self.gp_lat_sum_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0;
        (avg, self.gp_lat_max_us.load(Ordering::Relaxed) as f64 / 1000.0)
    }

    /// (avg_ms, max_ms) of the grouping hold (lead wait).
    pub fn latency_ms(&self) -> (f64, f64) {
        let n = self.lat_count.load(Ordering::Relaxed);
        let avg = if n > 0 {
            self.lat_sum_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0
        } else { 0.0 };
        (avg, self.lat_max_us.load(Ordering::Relaxed) as f64 / 1000.0)
    }

    /// (avg_ms, max_ms) of measured finger gap.
    pub fn finger_gap_ms(&self) -> (f64, f64) {
        let n = self.gap_count.load(Ordering::Relaxed);
        if n == 0 { return (0.0, 0.0); }
        let avg = self.gap_sum_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0;
        (avg, self.gap_max_us.load(Ordering::Relaxed) as f64 / 1000.0)
    }

    /// Smallest window that still catches dashes: ceil(max gap)+1, clamped 3..=16.
    pub fn recommended_window_ms(&self) -> u32 {
        let (_avg, max) = self.finger_gap_ms();
        if max <= 0.0 { return 0; }
        (max.ceil() as u32 + 1).clamp(3, 16)
    }

    /// Whether this slot has seen any activity (so the UI can flag it active).
    pub fn active(&self) -> bool {
        self.groups.load(Ordering::Relaxed) != 0
            || self.singles.load(Ordering::Relaxed) != 0
            || self.gp_lat_count.load(Ordering::Relaxed) != 0
            || self.attempts.load(Ordering::Relaxed) != 0
    }
}

/// Fixed-layout shared state. repr(C) so the DLL and app agree on offsets.
#[repr(C)]
pub struct SharedState {
    pub magic: AtomicU32,

    // --- config (either side writes) ---
    pub enabled: AtomicU32,             // bool (shared)
    pub window_ms: [AtomicU32; NUM_PLAYERS], // per-player sync window (ms)
    pub block_in_frame: AtomicU32,      // bool
    pub directions_windowed: AtomicU32, // bool
    pub settle_ms: AtomicU32,
    pub mode: AtomicU32,                // 0=Defer 1=Block 2=Continuous
    pub poll_hz: AtomicU32,             // poll-thread rate (shared)

    // heartbeat the DLL bumps each poll so the app can show "connected"
    pub dll_heartbeat: AtomicU64,

    // --- per-player stats ---
    pub players: [PlayerStats; NUM_PLAYERS],
}

impl SharedState {
    fn init_defaults(&self) {
        self.enabled.store(1, Ordering::Relaxed);
        for w in &self.window_ms {
            w.store(5, Ordering::Relaxed);
        }
        self.block_in_frame.store(0, Ordering::Relaxed);
        self.directions_windowed.store(0, Ordering::Relaxed);
        self.settle_ms.store(1, Ordering::Relaxed);
        self.mode.store(2, Ordering::Relaxed); // Continuous
        self.reset_stats();
        self.poll_hz.store(0, Ordering::Relaxed);
        self.dll_heartbeat.store(0, Ordering::Relaxed);
        self.magic.store(MAGIC, Ordering::Release);
    }

    pub fn reset_stats(&self) {
        for p in &self.players {
            p.reset();
        }
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
        // Creator may still be initializing — wait briefly for our magic. If a
        // stale mapping from an older layout is found, re-initialize it.
        let mut ok = false;
        for _ in 0..1000 {
            if s.magic.load(Ordering::Acquire) == MAGIC {
                ok = true;
                break;
            }
            std::hint::spin_loop();
        }
        if !ok {
            s.init_defaults();
        }
    }
    ptr as usize
}
