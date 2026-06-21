use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use windows_sys::Win32::System::Memory::{VirtualProtect, PAGE_READWRITE};
use nobd_shared::NUM_PLAYERS;
use crate::sync_window::SyncWindow;
use crate::log::log;

const SLOT_CREATE_DEVICE:    usize = 3;
const SLOT_GET_DEVICE_STATE: usize = 9;
const SLOT_GET_DEVICE_DATA:  usize = 10;
// rgbButtons offset in both DIJOYSTATE (80b) and DIJOYSTATE2 (272b)
const BUTTONS_OFFSET:    usize = 48;
const ATTACK_BTN_COUNT:  usize = 8; // buttons 0-7 per config.ini

type CreateDeviceFn = unsafe extern "system" fn(
    this: *mut c_void, rguid: *const [u8; 16],
    out_device: *mut *mut c_void, punk_outer: *mut c_void,
) -> i32;

type GetDeviceStateFn = unsafe extern "system" fn(
    this: *mut c_void, cb_data: u32, lpv_data: *mut c_void,
) -> i32;

type GetDeviceDataFn = unsafe extern "system" fn(
    this: *mut c_void, cb_obj_data: u32, rgdod: *mut c_void,
    pdw_inout: *mut u32, dw_flags: u32,
) -> i32;

static REAL_CREATE_DEVICE:    OnceLock<CreateDeviceFn>   = OnceLock::new();
static REAL_GET_DEVICE_STATE: OnceLock<GetDeviceStateFn> = OnceLock::new();
static REAL_GET_DEVICE_DATA:  OnceLock<GetDeviceDataFn>  = OnceLock::new();

// Per-player sync window, keyed by device pointer → slot (first device hooked =
// P1, second = P2). MvC2 uses XInput, so this DInput path is a fallback.
static DI_SW: LazyLock<[Mutex<SyncWindow>; NUM_PLAYERS]> =
    LazyLock::new(|| [Mutex::new(SyncWindow::with_player(0)), Mutex::new(SyncWindow::with_player(1))]);
static DI_DEVICE_PTRS: [AtomicUsize; NUM_PLAYERS] = [const { AtomicUsize::new(0) }; NUM_PLAYERS];

/// Register a DInput device pointer into the next free player slot (idempotent).
fn assign_slot(device: usize) -> usize {
    for s in 0..NUM_PLAYERS {
        if DI_DEVICE_PTRS[s].load(Ordering::Relaxed) == device {
            return s;
        }
    }
    for s in 0..NUM_PLAYERS {
        if DI_DEVICE_PTRS[s]
            .compare_exchange(0, device, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return s;
        }
    }
    0 // more devices than slots — fold extras into P1
}

/// Slot for an already-registered device pointer (defaults to P1 if unknown).
fn slot_for(device: usize) -> usize {
    for s in 0..NUM_PLAYERS {
        if DI_DEVICE_PTRS[s].load(Ordering::Relaxed) == device {
            return s;
        }
    }
    0
}

// --- diagnostics ---
static GDS_CALLS: AtomicU64 = AtomicU64::new(0); // GetDeviceState call count
static GDD_CALLS: AtomicU64 = AtomicU64::new(0); // GetDeviceData call count
static EDGE_LOGS: AtomicU64 = AtomicU64::new(0); // capped button-change logs
static LAST_ALL:  AtomicU32 = AtomicU32::new(0); // last 32-button snapshot

unsafe fn patch_vtable_slot(obj: *mut c_void, slot: usize, new_fn: *const ()) -> Option<*const ()> {
    if obj.is_null() {
        log("patch_vtable_slot: obj is null");
        return None;
    }
    let vtable: *mut *const () = unsafe { *(obj as *const *mut *const ()) };
    if vtable.is_null() {
        log("patch_vtable_slot: vtable ptr is null");
        return None;
    }
    let slot_ptr = unsafe { vtable.add(slot) };
    let original = unsafe { *slot_ptr };
    let mut old_prot = 0u32;
    let mut dummy = 0u32;
    unsafe {
        VirtualProtect(slot_ptr as _, std::mem::size_of::<usize>(), PAGE_READWRITE, &mut old_prot);
        *slot_ptr = new_fn;
        VirtualProtect(slot_ptr as _, std::mem::size_of::<usize>(), old_prot, &mut dummy);
    }
    Some(original)
}

pub unsafe fn hook_di8(di8: *mut c_void) {
    log(&format!("hook_di8: patching slot {SLOT_CREATE_DEVICE} on {:p}", di8));
    match unsafe { patch_vtable_slot(di8, SLOT_CREATE_DEVICE, our_create_device as *const ()) } {
        Some(orig) => { REAL_CREATE_DEVICE.get_or_init(|| unsafe { std::mem::transmute(orig) }); }
        None => log("hook_di8: patch failed"),
    }
}

unsafe extern "system" fn our_create_device(
    this: *mut c_void, rguid: *const [u8; 16],
    out_device: *mut *mut c_void, punk_outer: *mut c_void,
) -> i32 {
    log("our_create_device: called");
    let real = match REAL_CREATE_DEVICE.get() {
        Some(f) => f,
        None => { log("our_create_device: REAL_CREATE_DEVICE not set — passing through"); return -1; }
    };
    let hr = unsafe { real(this, rguid, out_device, punk_outer) };
    log(&format!("our_create_device: real returned hr={hr:#010x}"));
    if hr == 0 && !out_device.is_null() {
        let device = unsafe { *out_device };
        if !device.is_null() {
            unsafe { hook_device(device) };
        }
    }
    hr
}

unsafe fn hook_device(device: *mut c_void) {
    let slot = assign_slot(device as usize);
    log(&format!("hook_device: P{} patching slots {SLOT_GET_DEVICE_STATE}+{SLOT_GET_DEVICE_DATA} on {:p}", slot + 1, device));
    match unsafe { patch_vtable_slot(device, SLOT_GET_DEVICE_STATE, our_get_device_state as *const ()) } {
        Some(orig) => { REAL_GET_DEVICE_STATE.get_or_init(|| unsafe { std::mem::transmute(orig) }); }
        None => log("hook_device: GetDeviceState patch failed"),
    }
    match unsafe { patch_vtable_slot(device, SLOT_GET_DEVICE_DATA, our_get_device_data as *const ()) } {
        Some(orig) => { REAL_GET_DEVICE_DATA.get_or_init(|| unsafe { std::mem::transmute(orig) }); }
        None => log("hook_device: GetDeviceData patch failed"),
    }
}

unsafe extern "system" fn our_get_device_state(
    this: *mut c_void, cb_data: u32, lpv_data: *mut c_void,
) -> i32 {
    let real = match REAL_GET_DEVICE_STATE.get() {
        Some(f) => f,
        None => {
            log("our_get_device_state: REAL_GET_DEVICE_STATE not set");
            return -1;
        }
    };
    let hr = unsafe { real(this, cb_data, lpv_data) };

    let n = GDS_CALLS.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        log(&format!("GetDeviceState FIRST CALL: cbData={cb_data} hr={hr:#010x} null={}", lpv_data.is_null()));
    }

    // Only process joystick reads. Keyboard=256b, mouse=16/24b — skip those.
    if hr == 0 && matches!(cb_data, 80 | 272) && !lpv_data.is_null() {
        unsafe {
            let btn_base = (lpv_data as *mut u8).add(BUTTONS_OFFSET);

            // Diagnostic: snapshot ALL 32 buttons (offset 48..80) and log on
            // change so we can see the game's real button layout / 0x80 format.
            let mut all: u32 = 0;
            for i in 0..32 {
                if *btn_base.add(i) & 0x80 != 0 { all |= 1u32 << i; }
            }
            if all != LAST_ALL.load(Ordering::Relaxed) {
                LAST_ALL.store(all, Ordering::Relaxed);
                if EDGE_LOGS.fetch_add(1, Ordering::Relaxed) < 400 {
                    log(&format!("btn change: 0x{all:08X}  (cbData={cb_data}, call #{n})"));
                }
            }

            // Sync window operates on attack buttons 0-7 (DInput layout). Route to
            // this device's player slot so two pads sync independently.
            let raw = (all & 0x00FF) as u16;
            let slot = slot_for(this as usize);
            if let Ok(mut sw) = DI_SW[slot].lock() {
                let filtered = sw.process(raw, crate::sync_window::ATTACK_MASK);
                for i in 0..ATTACK_BTN_COUNT {
                    *btn_base.add(i) = if filtered & (1u16 << i) != 0 { 0x80 } else { 0x00 };
                }
            }
        }
    } else if hr == 0 && n < 20 {
        log(&format!("GetDeviceState cbData={cb_data} (skipped — not joystick 80/272)"));
    }
    hr
}

// Passthrough probe: detect whether the game reads input via BUFFERED
// GetDeviceData (slot 10) instead of immediate GetDeviceState (slot 9).
// If this fires during gameplay but GetDeviceState does not, that's why the
// counters don't move — we'd need to filter the buffered event stream instead.
unsafe extern "system" fn our_get_device_data(
    this: *mut c_void, cb_obj_data: u32, rgdod: *mut c_void,
    pdw_inout: *mut u32, dw_flags: u32,
) -> i32 {
    let real = match REAL_GET_DEVICE_DATA.get() {
        Some(f) => f,
        None => return -1,
    };
    let hr = unsafe { real(this, cb_obj_data, rgdod, pdw_inout, dw_flags) };

    let n = GDD_CALLS.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        log(&format!("GetDeviceData FIRST CALL: cbObjData={cb_obj_data} flags={dw_flags:#x} hr={hr:#010x}"));
    }
    if hr == 0 && !pdw_inout.is_null() {
        let count = unsafe { *pdw_inout };
        // Log only when actual events come through (a real input), capped.
        if count > 0 && n < 4000 && EDGE_LOGS.fetch_add(1, Ordering::Relaxed) < 400 {
            log(&format!("GetDeviceData: {count} buffered events (call #{n})"));
        }
    }
    hr
}
