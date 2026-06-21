use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
use retour::RawDetour;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use nobd_shared::NUM_PLAYERS;
use crate::sync_window::SyncWindow;
use crate::log::log;

// XInput XINPUT_GAMEPAD.wButtons bit layout:
//   0x0001 DPAD_UP   0x0002 DPAD_DOWN  0x0004 DPAD_LEFT  0x0008 DPAD_RIGHT
//   0x0010 START     0x0020 BACK       0x0040 LTHUMB     0x0080 RTHUMB
//   0x0100 LB        0x0200 RB         0x1000 A  0x2000 B  0x4000 X  0x8000 Y
const XINPUT_ATTACK_MASK: u16 = 0xF300; // A,B,X,Y,LB,RB

// wButtons sits at offset 4 in XINPUT_STATE (after dwPacketNumber:u32).
const WBUTTONS_OFFSET: usize = 4;

// Hard cap on how long block-in-frame may stall the game's input read.
const MAX_BLOCK_MS: u64 = 8;

// Sanity ceiling for a game-perceived latency sample (µs): anything larger is a
// pause / alt-tab / load screen and must not pollute the latency average.
const GP_LAT_SANE_MAX_US: u64 = 100_000; // 100 ms

/// A SyncWindow tagged with its player slot (for per-player stats).
fn make_sw(player: usize) -> SyncWindow {
    let mut sw = SyncWindow::default();
    sw.player = player;
    sw
}

// --- per-player state (index = controller slot 0..NUM_PLAYERS) ---
// Block mode: last attack bits delivered, for rising-edge detection.
static LAST_DELIVERED: [AtomicU32; NUM_PLAYERS] = [const { AtomicU32::new(0) }; NUM_PLAYERS];

// Candidate module names, newest first. The Fighting Collection loads 1_3.
const XINPUT_DLLS: [&[u8]; 5] = [
    b"xinput1_4.dll\0",
    b"xinput1_3.dll\0",
    b"xinput9_1_0.dll\0",
    b"xinput1_2.dll\0",
    b"xinput1_1.dll\0",
];

type XInputGetStateFn = unsafe extern "system" fn(u32, *mut c_void) -> u32;

static REAL_XIGS: OnceLock<XInputGetStateFn> = OnceLock::new();
static DETOUR:    OnceLock<RawDetour> = OnceLock::new();
// Defer mode: a per-player window.
static SWX: LazyLock<[Mutex<SyncWindow>; NUM_PLAYERS]> =
    LazyLock::new(|| [Mutex::new(make_sw(0)), Mutex::new(make_sw(1))]);

// diagnostics (shared across players)
static XIGS_CALLS: AtomicU64 = AtomicU64::new(0);
static XEDGE_LOGS: AtomicU64 = AtomicU64::new(0);
static XLAST:      AtomicU32 = AtomicU32::new(0xFFFF_FFFF);

static EPOCH: OnceLock<Instant> = OnceLock::new();
// per-player last game-read time (frame-time + straddle detection).
static LAST_POLL_US: [AtomicU64; NUM_PLAYERS] = [const { AtomicU64::new(0) }; NUM_PLAYERS];

// --- continuous-poll mode (mode==2), per player ---
static CONT_COMMITTED: [AtomicU32; NUM_PLAYERS] = [const { AtomicU32::new(0) }; NUM_PLAYERS];
static CONT_SYNCED_MASK: [AtomicU32; NUM_PLAYERS] =
    [const { AtomicU32::new(XINPUT_ATTACK_MASK as u32) }; NUM_PLAYERS];
static CONT_PRESS_TS: [[AtomicU64; 16]; NUM_PLAYERS] =
    [const { [const { AtomicU64::new(0) }; 16] }; NUM_PLAYERS];
static GAME_LAST_DELIVERED: [AtomicU32; NUM_PLAYERS] = [const { AtomicU32::new(0) }; NUM_PLAYERS];
static WITHHELD_SEEN: [AtomicU32; NUM_PLAYERS] = [const { AtomicU32::new(0) }; NUM_PLAYERS];
static HEARTBEATS: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn our_xinput_get_state(idx: u32, p_state: *mut c_void) -> u32 {
    let real = match REAL_XIGS.get() {
        Some(f) => f,
        None => return 1167, // ERROR_DEVICE_NOT_CONNECTED
    };
    let ret = unsafe { real(idx, p_state) };

    let n = XIGS_CALLS.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        log(&format!("XInputGetState FIRST CALL: idx={idx} ret={ret} null={}", p_state.is_null()));
    }
    crate::config::heartbeat(); // let nobd.exe know the in-game hook is live

    let p = idx as usize;
    // Sync only the player slots we track; a controller in a higher slot passes
    // through untouched.
    if ret == 0 && !p_state.is_null() && p < NUM_PLAYERS {
        // Frame-time from this controller's poll cadence.
        let epoch = EPOCH.get_or_init(Instant::now);
        let now_us = epoch.elapsed().as_micros() as u64;
        let last = LAST_POLL_US[p].swap(now_us, Ordering::Relaxed);
        if last != 0 {
            crate::config::record_frame_us(p, now_us - last);
        }

        unsafe {
            let btn = (p_state as *mut u8).add(WBUTTONS_OFFSET) as *mut u16;
            let raw = *btn;

            if (raw as u32) != XLAST.swap(raw as u32, Ordering::Relaxed) {
                if XEDGE_LOGS.fetch_add(1, Ordering::Relaxed) < 400 {
                    log(&format!("P{} btn change: 0x{raw:04X}  (call #{n})", p + 1));
                }
            }

            match crate::config::mode() {
                1 => block_latch(p, *real, idx, p_state, btn, raw),
                2 => continuous_apply(p, btn, raw),
                _ => {
                    if let Ok(mut sw) = SWX[p].lock() {
                        let filtered = sw.process(raw, XINPUT_ATTACK_MASK);
                        *btn = filtered;
                    }
                }
            }
        }
    }
    ret
}

// Block-in-frame latch (legacy). Spins re-reading the real pad until a partner
// arrives or the window expires, then lets the grouped state pass through.
unsafe fn block_latch(
    p: usize, real: XInputGetStateFn, idx: u32, p_state: *mut c_void, btn: *mut u16, raw0: u16,
) {
    if !crate::config::enabled() {
        return; // raw passthrough
    }
    let synced: u16 = if crate::config::directions_windowed() { 0xFFFF } else { XINPUT_ATTACK_MASK };
    let atks0 = raw0 & synced;
    let prev = LAST_DELIVERED[p].load(Ordering::Relaxed) as u16;
    let fresh = atks0 & !prev;

    if fresh == 0 {
        LAST_DELIVERED[p].store(atks0 as u32, Ordering::Relaxed);
        return;
    }

    let window = (crate::config::window_ms() as u64).min(MAX_BLOCK_MS);
    let start = Instant::now();
    let mut gap_us: Option<u64> = None;

    if atks0.count_ones() < 2 {
        loop {
            if (start.elapsed().as_millis() as u64) >= window {
                break;
            }
            let _ = unsafe { real(idx, p_state) };
            let now = unsafe { *btn } & synced;
            if now.count_ones() >= 2 {
                gap_us = Some(start.elapsed().as_micros() as u64);
                break;
            }
            if now == 0 {
                break;
            }
            std::hint::spin_loop();
        }
    }

    let settle = crate::config::settle_ms();
    if settle > 0 && (unsafe { *btn } & synced).count_ones() >= 2 {
        let s = Instant::now();
        while (s.elapsed().as_millis() as u64) < settle {
            let _ = unsafe { real(idx, p_state) };
            std::hint::spin_loop();
        }
    }

    let delivered = unsafe { *btn } & synced;
    let delivered_atks = delivered & XINPUT_ATTACK_MASK;

    crate::config::record_latency(p, start.elapsed().as_micros() as u64);
    crate::config::record_delivery(p, delivered_atks);

    if let Some(g) = gap_us {
        crate::config::record_gap(p, g);
        crate::config::record_save(p);
    }
    LAST_DELIVERED[p].store(delivered as u32, Ordering::Relaxed);
}

// Continuous mode: sample this player's committed state. Directions/held bits come
// from the fresh real read; attack bits are overwritten with the windowed value.
unsafe fn continuous_apply(p: usize, btn: *mut u16, raw: u16) {
    if !crate::config::enabled() {
        return; // raw passthrough
    }
    let mask = CONT_SYNCED_MASK[p].load(Ordering::Relaxed) as u16;
    let committed = CONT_COMMITTED[p].load(Ordering::Relaxed) as u16;
    let delivered = (raw & !mask) | (committed & mask);
    unsafe { *btn = delivered; }

    let committed_atks = committed & XINPUT_ATTACK_MASK;
    let raw_atks = raw & XINPUT_ATTACK_MASK;

    let withheld_now = raw_atks & !committed_atks;
    let seen = WITHHELD_SEEN[p].fetch_or(withheld_now as u32, Ordering::Relaxed) as u16 | withheld_now;

    let prev = GAME_LAST_DELIVERED[p].load(Ordering::Relaxed) as u16;
    let newly = committed_atks & !prev;
    if newly != 0 {
        let now = EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64;
        for bit in 0..16 {
            if newly & (1 << bit) != 0 {
                let ts = CONT_PRESS_TS[p][bit].load(Ordering::Relaxed);
                if ts != 0 && now >= ts {
                    let d = now - ts;
                    if d <= GP_LAT_SANE_MAX_US {
                        crate::config::record_gp_latency(p, d);
                    }
                }
                if seen & (1 << bit) != 0 {
                    crate::config::record_frame_wait(p);
                }
            }
        }
        if newly.count_ones() >= 2 && (newly & seen) != 0 {
            crate::config::record_save(p);
        }
        WITHHELD_SEEN[p].fetch_and(!(newly as u32), Ordering::Relaxed);
    }
    WITHHELD_SEEN[p].fetch_and(raw_atks as u32, Ordering::Relaxed);
    GAME_LAST_DELIVERED[p].store(committed_atks as u32, Ordering::Relaxed);
}

// Background thread: poll every connected pad ~1kHz, run each one's sync window on
// its own clock, and publish per-player committed state for the game's reads.
fn continuous_poll_loop() {
    let mut sw = [make_sw(0), make_sw(1)];
    for w in &mut sw {
        // Saves are counted accurately in the hook, not here at ~1kHz.
        w.record_saves = false;
    }
    let mut last_raw_atks = [0u16; NUM_PLAYERS];
    // Passive monitor (sync OFF): lead-press timestamp of a potential pair, per player.
    let mut shadow_lead = [None::<u64>; NUM_PLAYERS];
    let mut iters: u64 = 0;
    let mut rate_start = Instant::now();

    loop {
        if crate::config::mode() != 2 {
            std::thread::sleep(Duration::from_millis(10));
            last_raw_atks = [0; NUM_PLAYERS];
            continue;
        }
        let Some(real) = REAL_XIGS.get() else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };

        for p in 0..NUM_PLAYERS {
            // XINPUT_STATE = dwPacketNumber(4) + XINPUT_GAMEPAD(12) = 16 bytes.
            let mut buf = [0u8; 16];
            let r = unsafe { real(p as u32, buf.as_mut_ptr() as *mut c_void) };
            if r != 0 {
                continue; // controller not connected
            }
            let raw = u16::from_le_bytes([buf[WBUTTONS_OFFSET], buf[WBUTTONS_OFFSET + 1]]);
            let now = EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64;

            // Timestamp physical attack rising edges for game-perceived latency.
            let raw_atks = raw & XINPUT_ATTACK_MASK;
            let rising = raw_atks & !last_raw_atks[p];
            if rising != 0 {
                for bit in 0..16 {
                    if rising & (1 << bit) != 0 {
                        CONT_PRESS_TS[p][bit].store(now, Ordering::Relaxed);
                    }
                }
            }
            last_raw_atks[p] = raw_atks;

            // Passive monitor while sync is OFF.
            if !crate::config::enabled() {
                let window_us = (crate::config::window_ms() as u64) * 1000;
                if let Some(lead) = shadow_lead[p] {
                    if now.saturating_sub(lead) > window_us {
                        shadow_lead[p] = None;
                    }
                }
                if rising != 0 {
                    match shadow_lead[p] {
                        None => shadow_lead[p] = Some(now),
                        Some(lead) => {
                            let gap = now.saturating_sub(lead);
                            if gap <= window_us {
                                crate::config::record_attempt(p);
                                crate::config::record_gap(p, gap);
                                if LAST_POLL_US[p].load(Ordering::Relaxed) > lead {
                                    crate::config::record_miss(p);
                                }
                            }
                            shadow_lead[p] = None;
                        }
                    }
                }
                if raw_atks == 0 {
                    shadow_lead[p] = None;
                }
            } else {
                shadow_lead[p] = None;
            }

            let synced_mask: u16 =
                if crate::config::directions_windowed() { 0xFFFF } else { XINPUT_ATTACK_MASK };
            let filtered = sw[p].process(raw, XINPUT_ATTACK_MASK);
            CONT_COMMITTED[p].store((filtered & synced_mask) as u32, Ordering::Relaxed);
            CONT_SYNCED_MASK[p].store(synced_mask as u32, Ordering::Relaxed);
        }

        iters += 1;
        let el = rate_start.elapsed();
        if el.as_millis() >= 500 {
            let hz = (iters as f64 / el.as_secs_f64()) as u32;
            crate::config::set_poll_hz(hz);
            let h = HEARTBEATS.fetch_add(1, Ordering::Relaxed);
            if h % 4 == 0 && h < 480 {
                crate::log::log(&format!(
                    "continuous: poll_hz={hz}  P1=0x{:04X} P2=0x{:04X}  P1 waited={}/{}",
                    CONT_COMMITTED[0].load(Ordering::Relaxed),
                    CONT_COMMITTED[1].load(Ordering::Relaxed),
                    crate::config::frame_waits(0), crate::config::gp_count(0),
                ));
            }
            iters = 0;
            rate_start = Instant::now();
        }

        // ~1.4kHz target. Rust's std sleep uses high-resolution timers on Windows.
        std::thread::sleep(Duration::from_micros(700));
    }
}

unsafe fn try_install() -> bool {
    for name in XINPUT_DLLS {
        let h = unsafe { GetModuleHandleA(name.as_ptr()) };
        if h == 0 {
            continue;
        }
        let dll = String::from_utf8_lossy(&name[..name.len() - 1]).into_owned();
        let proc = unsafe { GetProcAddress(h, b"XInputGetState\0".as_ptr()) };
        let Some(target) = proc else {
            log(&format!("xinput: {dll} loaded but XInputGetState missing"));
            continue;
        };
        let detour = match unsafe {
            RawDetour::new(target as *const (), our_xinput_get_state as *const ())
        } {
            Ok(d) => d,
            Err(e) => { log(&format!("xinput: RawDetour::new failed on {dll}: {e}")); return false; }
        };
        if let Err(e) = unsafe { detour.enable() } {
            log(&format!("xinput: detour.enable failed on {dll}: {e}"));
            return false;
        }
        let tramp: XInputGetStateFn = unsafe { std::mem::transmute(detour.trampoline()) };
        REAL_XIGS.get_or_init(|| tramp);
        DETOUR.get_or_init(|| detour);
        log(&format!("xinput: hooked XInputGetState in {dll}"));
        return true;
    }
    false
}

pub fn spawn() {
    // Continuous-mode poll thread. Idles until mode==2 and the hook is installed.
    std::thread::spawn(continuous_poll_loop);

    std::thread::spawn(|| {
        // xinput is usually loaded lazily on first controller poll — poll for it.
        for _ in 0..600 {
            if unsafe { try_install() } {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        log("xinput: no xinput DLL loaded after 60s — not hooked");
    });
}
