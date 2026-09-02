use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use retour::RawDetour;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use nobd_shared::NUM_PLAYERS;
use nobd_shared::sync_window::SyncWindow;
use crate::log::log;

// XInput XINPUT_GAMEPAD.wButtons bit layout:
//   0x0001 DPAD_UP   0x0002 DPAD_DOWN  0x0004 DPAD_LEFT  0x0008 DPAD_RIGHT
//   0x0010 START     0x0020 BACK       0x0040 LTHUMB     0x0080 RTHUMB
//   0x0100 LB        0x0200 RB         0x1000 A  0x2000 B  0x4000 X  0x8000 Y
const XINPUT_ATTACK_MASK: u16 = 0xF300; // A,B,X,Y,LB,RB

// wButtons sits at offset 4 in XINPUT_STATE (after dwPacketNumber:u32).
const WBUTTONS_OFFSET: usize = 4;

// Sanity ceiling for a game-perceived latency sample (µs): anything larger is a
// pause / alt-tab / load screen and must not pollute the latency average.
const GP_LAT_SANE_MAX_US: u64 = 100_000; // 100 ms

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

// diagnostics (shared across players)
static XIGS_CALLS: AtomicU64 = AtomicU64::new(0);
static XEDGE_LOGS: AtomicU64 = AtomicU64::new(0);
static XLAST:      AtomicU32 = AtomicU32::new(0xFFFF_FFFF);

/// XInput user index -> NOBD player slot, claimed in order of first successful read.
///
/// We used to use the user index AS the slot. The game does not: `sAppPad` walks user
/// indices 0..4 and claims the first that answers ERROR_SUCCESS, so a single stick very
/// commonly lands on index 1 with index 0 returning 1167 ERROR_DEVICE_NOT_CONNECTED --
/// which is exactly what this machine does. The effect was that a solo player's telemetry
/// (finger gap, frame time, the recommendation, the whole event tape) was written to
/// NOBD's player-2 record while every screen in the app reads player 1, so the app showed
/// an empty measurement for a stick that was working perfectly. A pad on index >= 2 was
/// dropped entirely by the `p < NUM_PLAYERS` gate.
///
/// Stored index+1 so that 0 means "unclaimed" and slot 0 is representable.
static XI_SLOT_OF_IDX: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];

/// Map an XInput user index to a NOBD slot, first-come-first-served.
#[inline]
fn slot_for_idx(idx: u32) -> Option<usize> {
    let i = idx as usize;
    if i >= XI_SLOT_OF_IDX.len() {
        return None;
    }
    let claimed = XI_SLOT_OF_IDX[i].load(Ordering::Relaxed);
    if claimed != 0 {
        return Some((claimed - 1) as usize);
    }
    // Unclaimed: take the lowest free slot. Racy only between two indices calling for the
    // first time simultaneously, and the CAS makes that safe -- the loser retries the next
    // slot rather than sharing one.
    for s in 0..NUM_PLAYERS {
        if !(0..XI_SLOT_OF_IDX.len()).any(|j| XI_SLOT_OF_IDX[j].load(Ordering::Relaxed) == s as u32 + 1)
            && XI_SLOT_OF_IDX[i]
                .compare_exchange(0, s as u32 + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            crate::log::log(&format!("xinput: user index {idx} -> NOBD player {}", s + 1));
            return Some(s);
        }
    }
    None // more pads than slots: pass through untouched rather than fold onto someone else
}

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

    // The user index is NOT the player slot -- see XI_SLOT_OF_IDX. Claim a slot on the
    // first successful read so the first stick to answer is player 1, whatever index the
    // game happened to find it on.
    let slot = (ret == 0).then(|| slot_for_idx(idx)).flatten();
    if let (Some(p), false) = (slot, p_state.is_null()) {
        // Frame-time from this controller's poll cadence.
        let epoch = EPOCH.get_or_init(Instant::now);
        let now_us = epoch.elapsed().as_micros() as u64;
        let last = LAST_POLL_US[p].swap(now_us, Ordering::Relaxed);
        if last != 0 {
            let delta = now_us - last;
            crate::config::record_frame_us(p, delta);
            // Two atomics, no filtering, stops when full. See pollprobe.rs for why the
            // sample must be RAW: record_frame_us's 4..40ms band would discard exactly
            // the cadence we are trying to detect.
            crate::pollprobe::sample(p, delta);
        }

        unsafe {
            let btn = (p_state as *mut u8).add(WBUTTONS_OFFSET) as *mut u16;
            let raw = *btn;

            if (raw as u32) != XLAST.swap(raw as u32, Ordering::Relaxed) {
                if XEDGE_LOGS.fetch_add(1, Ordering::Relaxed) < 400 {
                    log(&format!("P{} btn change: 0x{raw:04X}  (call #{n})", p + 1));
                }
            }

            // Continuous is the ONLY mode. The window runs on its own clock in
            // the background thread; this read just samples the already
            // committed result and returns. It never blocks the game thread,
            // which is what keeps rollback netcode untouched.
            continuous_apply(p, btn, raw);
        }
    }
    ret
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
    let mut sw = [SyncWindow::new(), SyncWindow::new()];
    let mut last_raw_atks = [0u16; NUM_PLAYERS];
    // Raw chord tracking, identical in rule to the desktop app's SyncTelemetry:
    // a group opens on the first attack press, closes when every attack is
    // released OR one frame has passed. The frame cap is load-bearing - without
    // it, holding one attack and pressing another a second later reports a
    // one-second "finger gap".
    let mut grp_first = [0u64; NUM_PLAYERS];
    let mut grp_last = [0u64; NUM_PLAYERS];
    let mut grp_mask = [0u16; NUM_PLAYERS];
    let mut grp_open = [false; NUM_PLAYERS];
    // Passive monitor (sync OFF): lead-press timestamp of a potential pair, per player.
    let mut shadow_lead = [None::<u64>; NUM_PLAYERS];
    let mut iters: u64 = 0;
    let mut rate_start = Instant::now();

    loop {
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
            // Close a finished raw group and publish what the fingers did.
            if grp_open[p]
                && (raw_atks == 0
                    || now.saturating_sub(grp_first[p]) >= crate::config::FRAME_US)
            {
                if grp_mask[p].count_ones() >= 2 {
                    let gap = grp_last[p].saturating_sub(grp_first[p]);
                    crate::config::record_raw_gap(p, gap);
                    crate::config::record_risk(p, gap);
                }
                grp_open[p] = false;
                grp_mask[p] = 0;
            }
            if rising != 0 {
                if !grp_open[p] {
                    grp_first[p] = now;
                    grp_mask[p] = 0;
                    grp_open[p] = true;
                }
                grp_last[p] = now;
                grp_mask[p] |= rising;
            }

            last_raw_atks[p] = raw_atks;

            // Passive monitor while sync is OFF.
            if !crate::config::enabled() {
                let window_us = (crate::config::window_ms(p) as u64) * 1000;
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
            // The shared window is pure: it takes the clock and the config
            // rather than reading them itself, so the identical code runs here,
            // in the desktop app and in the Linux daemon.
            let window_us = crate::config::window_ms_u32(p).clamp(1, 16) * 1000;
            let filtered = sw[p].process(
                raw,
                XINPUT_ATTACK_MASK,
                synced_mask,
                now,
                window_us,
                crate::config::enabled(),
            );
            // Count a commit and report the delay the window actually applied.
            // `last_commit_delay_us` is the window's own measure of what it
            // cost; deriving it from wall-clock here would include the time the
            // player simply held the button.
            let prev = CONT_COMMITTED[p].load(Ordering::Relaxed) as u16;
            let newly = filtered & !prev & XINPUT_ATTACK_MASK;
            if newly != 0 {
                crate::config::record_delivery(p, newly);
                crate::config::record_latency(p, sw[p].last_commit_delay_us() as u64);
            }
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
            // Poll-cadence probe: sorting and formatting happen HERE, never on the
            // game thread. Reports once, then costs nothing.
            if let Some(pp) = crate::pollprobe::ready() {
                if let Some(r) = crate::pollprobe::report(pp) {
                    crate::log::log(&r);
                }
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
