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

            if crate::config::block_in_frame() {
                // Hold the read open and group within THIS frame (sub-frame latency).
                block_latch(*real, idx, p_state, btn, raw);
            } else if let Ok(mut sw) = SWX.lock() {
                // Defer-to-next-frame sync window (+1 frame, zero budget cost).
                let filtered = sw.process(raw, XINPUT_ATTACK_MASK);
                *btn = filtered;
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
