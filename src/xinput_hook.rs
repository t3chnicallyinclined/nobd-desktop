use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
use retour::RawDetour;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use crate::sync_window::SyncWindow;
use crate::log::log;

// XInput XINPUT_GAMEPAD.wButtons bit layout:
//   0x0001 DPAD_UP   0x0002 DPAD_DOWN  0x0004 DPAD_LEFT  0x0008 DPAD_RIGHT
//   0x0010 START     0x0020 BACK       0x0040 LTHUMB     0x0080 RTHUMB
//   0x0100 LB        0x0200 RB         0x1000 A  0x2000 B  0x4000 X  0x8000 Y
// MVC2 attacks land on the face buttons + shoulders. Triggers (LT/RT) are
// analog bytes, handled separately below.
const XINPUT_ATTACK_MASK: u16 = 0xF300; // A,B,X,Y,LB,RB

// wButtons sits at offset 4 in XINPUT_STATE (after dwPacketNumber:u32).
const WBUTTONS_OFFSET: usize = 4;

// Hard cap on how long block-in-frame may stall the game's input read, to
// protect the 16.67ms frame budget regardless of the configured window.
const MAX_BLOCK_MS: u64 = 8;

// Sanity ceiling for a game-perceived latency sample (µs). A real press→read is
// at most the window (≤16ms) plus a frame or two; anything larger is a pause /
// alt-tab / load screen and must not pollute the latency average.
const GP_LAT_SANE_MAX_US: u64 = 100_000; // 100 ms

// Settle window default lives in shared config (config::settle_ms); once we have
// 2+ attack buttons we wait that long for a 3rd straggler (e.g. assist call after
// a 2-button action) so multi-button inputs land on one frame instead of split.

// Last attack bits we delivered, for rising-edge detection in block_latch.
static LAST_DELIVERED: AtomicU32 = AtomicU32::new(0);

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
static SWX: LazyLock<Mutex<SyncWindow>> = LazyLock::new(|| Mutex::new(SyncWindow::default()));

// diagnostics
static XIGS_CALLS: AtomicU64 = AtomicU64::new(0);
static XEDGE_LOGS: AtomicU64 = AtomicU64::new(0);
static XLAST:      AtomicU32 = AtomicU32::new(0xFFFF_FFFF);

// frame-time measurement from poll cadence on the connected pad
static EPOCH:        OnceLock<Instant> = OnceLock::new();
static LAST_POLL_US: AtomicU64 = AtomicU64::new(0);

// --- continuous-poll mode (mode==2) ---
// committed (windowed) bits + the synced-mask used, published by the poll thread
// and sampled by the game's read.
static CONT_COMMITTED:   AtomicU32 = AtomicU32::new(0);
static CONT_SYNCED_MASK: AtomicU32 = AtomicU32::new(XINPUT_ATTACK_MASK as u32);
// physical-press timestamp (epoch µs) per button bit, set by the poll thread.
static CONT_PRESS_TS: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];
// attack bits last shown to the game, for game-perceived-latency edge detection.
static GAME_LAST_DELIVERED: AtomicU32 = AtomicU32::new(0);
// last controller index the game polled, so the poll thread reads the same pad.
static ACTIVE_IDX: AtomicU32 = AtomicU32::new(0);
// throttle counter for the poll-thread heartbeat log line.
static HEARTBEATS: AtomicU64 = AtomicU64::new(0);
// attack bits physically pressed but withheld from the game at a prior read,
// so a delivery that started withheld can be counted as "waited a frame".
static WITHHELD_SEEN: AtomicU32 = AtomicU32::new(0);

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

    // ERROR_SUCCESS == 0 → a controller is connected and pState is valid.
    if ret == 0 && !p_state.is_null() {
        // Frame-time from poll cadence (connected pad is read once per frame).
        let epoch = EPOCH.get_or_init(Instant::now);
        let now_us = epoch.elapsed().as_micros() as u64;
        let last = LAST_POLL_US.swap(now_us, Ordering::Relaxed);
        if last != 0 {
            crate::config::record_frame_us(now_us - last);
        }

        unsafe {
            let btn = (p_state as *mut u8).add(WBUTTONS_OFFSET) as *mut u16;
            let raw = *btn;

            if (raw as u32) != XLAST.swap(raw as u32, Ordering::Relaxed) {
                if XEDGE_LOGS.fetch_add(1, Ordering::Relaxed) < 400 {
                    log(&format!("XINPUT btn change: 0x{raw:04X}  (call #{n})"));
                }
            }

            ACTIVE_IDX.store(idx, Ordering::Relaxed);
            match crate::config::mode() {
                1 => {
                    // Block: hold the read open and group within THIS frame.
                    block_latch(*real, idx, p_state, btn, raw);
                }
                2 => {
                    // Continuous: the poll thread maintains committed state on its
                    // own ~1kHz clock; here we just sample it (directions stay raw).
                    continuous_apply(btn, raw);
                }
                _ => {
                    // Defer: per-read window (+1 frame on a lone press, 0 budget cost).
                    if let Ok(mut sw) = SWX.lock() {
                        let filtered = sw.process(raw, XINPUT_ATTACK_MASK);
                        *btn = filtered;
                    }
                }
            }
        }
    }
    ret
}

// Block-in-frame latch. On a fresh attack rising edge, spin (re-reading the real
// pad) until a partner attack arrives or the window expires, then let the
// naturally-grouped real state pass through. Delivers within the same frame.
unsafe fn block_latch(
    real: XInputGetStateFn, idx: u32, p_state: *mut c_void, btn: *mut u16, raw0: u16,
) {
    if !crate::config::enabled() {
        return; // raw passthrough
    }
    let synced: u16 = if crate::config::directions_windowed() { 0xFFFF } else { XINPUT_ATTACK_MASK };
    let atks0 = raw0 & synced;
    let prev = LAST_DELIVERED.load(Ordering::Relaxed) as u16;
    let fresh = atks0 & !prev;

    if fresh == 0 {
        // Holding or releasing — track current attacks so releases re-arm edges.
        LAST_DELIVERED.store(atks0 as u32, Ordering::Relaxed);
        return;
    }

    let window = (crate::config::window_ms() as u64).min(MAX_BLOCK_MS);
    let start = Instant::now();
    let mut gap_us: Option<u64> = None;

    // Phase 1 — lone fresh press: wait up to `window` for a partner button.
    // If the press already arrived grouped (2+ bits in one packet), skip the
    // wait entirely — the grouping is already done, so don't add latency.
    if atks0.count_ones() < 2 {
        loop {
            if (start.elapsed().as_millis() as u64) >= window {
                break;
            }
            let _ = unsafe { real(idx, p_state) };
            let now = unsafe { *btn } & synced;
            if now.count_ones() >= 2 {
                // A real partner landed → this wait IS the finger gap.
                gap_us = Some(start.elapsed().as_micros() as u64);
                break;
            }
            if now == 0 {
                // Lone press released before any partner — a sub-window tap.
                // Stop blocking immediately instead of stalling the whole window
                // on a press that's already gone (firmware drops these too).
                break;
            }
            std::hint::spin_loop();
        }
    }

    // Phase 2 — settle: once we have 2+ buttons, briefly wait for a 3rd
    // straggler (assist + 2-button action, etc.) so it lands on the same frame.
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

    crate::config::record_latency(start.elapsed().as_micros() as u64);
    crate::config::record_delivery(delivered_atks);

    // A provable frame-boundary save: the poll read a LONE attack (gap_us is set
    // only when atks0 had <2 bits and a partner then arrived before we returned).
    // That means the boundary fell between the two presses — without NOBD they'd
    // have been read on different frames. Already-grouped presses don't count.
    if let Some(g) = gap_us {
        crate::config::record_gap(g);
        crate::config::record_save();
    }
    LAST_DELIVERED.store(delivered as u32, Ordering::Relaxed);
}

// Continuous mode: sample the poll thread's committed state. Directions and
// already-held bits come straight from the fresh real read; attack bits are
// overwritten with the windowed-committed value. Records game-perceived latency
// (physical press → first game read that sees it) on each fresh delivery.
unsafe fn continuous_apply(btn: *mut u16, raw: u16) {
    if !crate::config::enabled() {
        return; // raw passthrough for live A/B
    }
    let mask = CONT_SYNCED_MASK.load(Ordering::Relaxed) as u16;
    let committed = CONT_COMMITTED.load(Ordering::Relaxed) as u16;
    let delivered = (raw & !mask) | (committed & mask);
    unsafe { *btn = delivered; }

    let committed_atks = committed & XINPUT_ATTACK_MASK;
    let raw_atks = raw & XINPUT_ATTACK_MASK;

    // Bits the game would have seen pressed (raw) but we're withholding this read.
    // Mark them sticky; `seen` includes prior reads + this one.
    let withheld_now = raw_atks & !committed_atks;
    let seen = WITHHELD_SEEN.fetch_or(withheld_now as u32, Ordering::Relaxed) as u16 | withheld_now;

    let prev = GAME_LAST_DELIVERED.load(Ordering::Relaxed) as u16;
    let newly = committed_atks & !prev;
    if newly != 0 {
        let now = EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64;
        for bit in 0..16 {
            if newly & (1 << bit) != 0 {
                let ts = CONT_PRESS_TS[bit].load(Ordering::Relaxed);
                if ts != 0 && now >= ts {
                    let d = now - ts;
                    // Ignore implausible samples (game pause / alt-tab / load screen:
                    // a press sat committed across a long no-read gap), which would
                    // otherwise inflate the latency average and max.
                    if d <= GP_LAT_SANE_MAX_US {
                        crate::config::record_gp_latency(d);
                    }
                }
                // A game read passed while this press was withheld → it cost a frame.
                if seen & (1 << bit) != 0 {
                    crate::config::record_frame_wait();
                }
            }
        }
        // True frame-boundary save: a group is delivered together AND at least one
        // member had waited a frame (so without NOBD it would have been read alone
        // on an earlier frame, splitting from its partner).
        if newly.count_ones() >= 2 && (newly & seen) != 0 {
            crate::config::record_save();
        }
        // Delivered bits are no longer withheld.
        WITHHELD_SEEN.fetch_and(!(newly as u32), Ordering::Relaxed);
    }
    // Drop sticky bits no longer physically pressed (released sub-window taps).
    WITHHELD_SEEN.fetch_and(raw_atks as u32, Ordering::Relaxed);
    GAME_LAST_DELIVERED.store(committed_atks as u32, Ordering::Relaxed);
}

// Background thread: poll the real pad ~1kHz, run the sync window on this fine
// clock, and publish the committed state for the game's read to sample. This is
// the firmware's continuous-poll model ported to the desktop — a lone press's
// window resolves off the game's cadence, so most presses land on the same frame
// they would have anyway (no unconditional +1 frame).
fn continuous_poll_loop() {
    let mut sw = SyncWindow::default();
    // Saves are counted accurately in the hook for Continuous (a group delivery
    // that actually crossed a game frame), not here at ~1kHz.
    sw.record_saves = false;
    let mut last_raw_atks: u16 = 0;
    // Passive monitor (sync OFF): timestamp of the lead press of a potential pair.
    let mut shadow_lead: Option<u64> = None;
    let mut iters: u64 = 0;
    let mut rate_start = Instant::now();

    loop {
        if crate::config::mode() != 2 {
            std::thread::sleep(Duration::from_millis(10));
            last_raw_atks = 0;
            continue;
        }
        let Some(real) = REAL_XIGS.get() else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        // XINPUT_STATE = dwPacketNumber(4) + XINPUT_GAMEPAD(12) = 16 bytes.
        let mut buf = [0u8; 16];
        let idx = ACTIVE_IDX.load(Ordering::Relaxed);
        let r = unsafe { real(idx, buf.as_mut_ptr() as *mut c_void) };
        if r == 0 {
            let raw = u16::from_le_bytes([buf[WBUTTONS_OFFSET], buf[WBUTTONS_OFFSET + 1]]);
            let now = EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64;

            // Timestamp physical attack rising edges for game-perceived latency.
            let raw_atks = raw & XINPUT_ATTACK_MASK;
            let rising = raw_atks & !last_raw_atks;
            if rising != 0 {
                for bit in 0..16 {
                    if rising & (1 << bit) != 0 {
                        CONT_PRESS_TS[bit].store(now, Ordering::Relaxed);
                    }
                }
            }
            last_raw_atks = raw_atks;

            // Passive monitor while sync is OFF: detect gapped two-button attempts
            // and whether the game split them across a frame (a missed dash). The
            // straddle test reuses LAST_POLL_US (the latest game read time, same
            // epoch). When sync is ON these are prevented and counted as saves.
            if !crate::config::enabled() {
                let window_us = (crate::config::window_ms() as u64) * 1000;
                if let Some(lead) = shadow_lead {
                    if now.saturating_sub(lead) > window_us {
                        shadow_lead = None; // lead had no partner — a single press
                    }
                }
                if rising != 0 {
                    match shadow_lead {
                        None => shadow_lead = Some(now),
                        Some(lead) => {
                            let gap = now.saturating_sub(lead);
                            if gap <= window_us {
                                crate::config::record_attempt();
                                crate::config::record_gap(gap);
                                // A game read fell between the two presses → the
                                // game saw the lead alone → the pair split.
                                if LAST_POLL_US.load(Ordering::Relaxed) > lead {
                                    crate::config::record_miss();
                                }
                            }
                            shadow_lead = None;
                        }
                    }
                }
                if raw_atks == 0 {
                    shadow_lead = None;
                }
            } else {
                shadow_lead = None;
            }

            let synced_mask: u16 =
                if crate::config::directions_windowed() { 0xFFFF } else { XINPUT_ATTACK_MASK };
            // process() records groups/singles/saves/finger-gap/window-hold on
            // this fine clock; we publish only the committed (windowed) bits.
            let filtered = sw.process(raw, XINPUT_ATTACK_MASK);
            CONT_COMMITTED.store((filtered & synced_mask) as u32, Ordering::Relaxed);
            CONT_SYNCED_MASK.store(synced_mask as u32, Ordering::Relaxed);
        }

        iters += 1;
        let el = rate_start.elapsed();
        if el.as_millis() >= 500 {
            let hz = (iters as f64 / el.as_secs_f64()) as u32;
            crate::config::set_poll_hz(hz);
            // Heartbeat ~every 2s (capped) so the log confirms the poll thread is
            // alive and at what rate, even with no input.
            let h = HEARTBEATS.fetch_add(1, Ordering::Relaxed);
            if h % 4 == 0 && h < 480 {
                crate::log::log(&format!(
                    "continuous: poll_hz={hz}  committed=0x{:04X}  waited_a_frame={}/{}",
                    CONT_COMMITTED.load(Ordering::Relaxed),
                    crate::config::frame_waits(), crate::config::gp_count(),
                ));
            }
            iters = 0;
            rate_start = Instant::now();
        }

        // ~1.4kHz target. Rust's std sleep uses high-resolution timers on Windows,
        // so a sub-ms sleep is honored without timeBeginPeriod.
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
