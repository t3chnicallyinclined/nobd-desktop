//! nobd-latency-probe -- measure submit -> XInputGetState latency through the XUSB companion.
//!
//! This is the Track-B Phase-1 make-or-break: does a value written to the companion's shared section
//! reach a game's XInputGetState fresher than the ~500us real-controller floor, or is there a 1 kHz
//! poll wall? We write a unique marker (the LX axis, as a fast counter), then tight-read
//! XInputGetState on the companion's slot until it appears, and time it.
//!
//! Run ELEVATED (writing Global\HIDMaestroInput0 is admin-only) with the nobd-desktop app CLOSED, so
//! this process is the SOLE writer to the shared section. XInput mode must be enabled (the companion
//! devnode present).  <100us here => the companion delivers fresh data and Track B's game half works.

use std::time::{Duration, Instant};

use hm_native::{NobdController, PadMode};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Gamepad {
    buttons: u16,
    lt: u8,
    rt: u8,
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct State {
    packet: u32,
    gp: Gamepad,
}
type XiGet = unsafe extern "system" fn(u32, *mut State) -> u32;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryA(name: *const u8) -> isize;
    fn GetProcAddress(module: isize, name: *const u8) -> *const core::ffi::c_void;
}

fn load_xinput() -> Option<XiGet> {
    let dlls: [&[u8]; 3] = [b"xinput1_4.dll\0", b"xinput1_3.dll\0", b"xinput9_1_0.dll\0"];
    for dll in dlls {
        let h = unsafe { LoadLibraryA(dll.as_ptr()) };
        if h != 0 {
            let p = unsafe { GetProcAddress(h, b"XInputGetState\0".as_ptr()) };
            if !p.is_null() {
                return Some(unsafe { std::mem::transmute::<*const core::ffi::c_void, XiGet>(p) });
            }
        }
    }
    None
}

fn read(xi: XiGet, slot: u32) -> Option<State> {
    let mut st = State::default();
    if unsafe { xi(slot, &mut st) } == 0 {
        Some(st)
    } else {
        None
    }
}

fn main() {
    let xi = match load_xinput() {
        Some(f) => f,
        None => {
            eprintln!("no XInput dll found");
            return;
        }
    };

    let mut ctrl = match NobdController::open(PadMode::Xinput) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("open companion failed ({e}).");
            eprintln!("-> run ELEVATED, with the nobd-desktop app CLOSED and XInput mode enabled.");
            return;
        }
    };

    // Fingerprint the companion's XInput slot (the same trick sync_service.rs uses): hold a unique
    // button marker and find the slot that reflects it. Allow ~2s for the UMDF driver to (re)attach.
    const MARKER: u16 = 0x0330; // LB|RB|Back|Start
    let mut slot: Option<u32> = None;
    let mut prev: Option<u32> = None;
    for _ in 0..200 {
        ctrl.submit(MARKER, 0, 0, 0, 0, 0, 0);
        std::thread::sleep(Duration::from_millis(10));
        let hit = (0..4).find(|&s| read(xi, s).map_or(false, |st| st.gp.buttons == MARKER));
        if hit.is_some() && hit == prev {
            slot = hit;
            break;
        }
        prev = hit;
    }
    ctrl.submit(0, 0, 0, 0, 0, 0, 0); // release
    let slot = match slot {
        Some(s) => s,
        None => {
            eprintln!("couldn't find the companion's XInput slot -- is XInput mode on + the app closed?");
            return;
        }
    };
    println!("companion at XInput slot {slot}. measuring submit -> XInputGetState latency...");

    let target = 3000usize;
    let mut lat: Vec<u64> = Vec::with_capacity(target);
    let mut timeouts = 0usize;
    let mut counter: i16 = 777;
    for _ in 0..target {
        counter = counter.wrapping_add(101);
        if counter == 0 {
            counter = 1;
        }
        let t0 = Instant::now();
        ctrl.submit(0, 0, 0, counter, 0, 0, 0); // LX = unique marker
        let mut seen = false;
        while t0.elapsed() < Duration::from_millis(20) {
            if let Some(st) = read(xi, slot) {
                if st.gp.lx == counter {
                    seen = true;
                    break;
                }
            }
        }
        if seen {
            lat.push(t0.elapsed().as_micros() as u64);
        } else {
            timeouts += 1;
        }
        std::thread::sleep(Duration::from_micros(200)); // let the readback settle before the next marker
    }

    if lat.is_empty() {
        eprintln!("no readbacks (timeouts={timeouts}) -- companion not reflecting submits.");
        return;
    }
    lat.sort_unstable();
    let n = lat.len();
    let sum: u64 = lat.iter().sum();
    println!("\n=== submit -> XInputGetState latency  ({n} samples, {timeouts} timeouts) ===");
    println!(
        "  min {} us    avg {} us    p50 {} us    p99 {} us    max {} us",
        lat[0],
        sum / n as u64,
        lat[n / 2],
        lat[n * 99 / 100],
        lat[n - 1]
    );
    println!("\n  real-controller floor ~500 us.  <100 us => the companion carries fresh data => Track B's game half WORKS.");
}
