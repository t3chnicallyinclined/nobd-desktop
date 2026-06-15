use std::ffi::c_void;
use std::sync::{LazyLock, Mutex, OnceLock};
use windows_sys::Win32::System::Memory::{VirtualProtect, PAGE_READWRITE};
use crate::sync_window::SyncWindow;

const SLOT_CREATE_DEVICE:    usize = 3;
const SLOT_GET_DEVICE_STATE: usize = 9;
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

static REAL_CREATE_DEVICE:    OnceLock<CreateDeviceFn>   = OnceLock::new();
static REAL_GET_DEVICE_STATE: OnceLock<GetDeviceStateFn> = OnceLock::new();
static SW: LazyLock<Mutex<SyncWindow>> =
    LazyLock::new(|| Mutex::new(SyncWindow::default()));

unsafe fn patch_vtable_slot(obj: *mut c_void, slot: usize, new_fn: *const ()) -> *const () {
    let vtable: *mut *const () = unsafe { *(obj as *const *mut *const ()) };
    let slot_ptr = unsafe { vtable.add(slot) };
    let original = unsafe { *slot_ptr };
    let mut old_prot = 0u32;
    unsafe {
        VirtualProtect(slot_ptr as _, std::mem::size_of::<usize>(), PAGE_READWRITE, &mut old_prot);
        *slot_ptr = new_fn;
        VirtualProtect(slot_ptr as _, std::mem::size_of::<usize>(), old_prot, &mut old_prot);
    }
    original
}

pub unsafe fn hook_di8(di8: *mut c_void) {
    let original = unsafe { patch_vtable_slot(di8, SLOT_CREATE_DEVICE, our_create_device as *const ()) };
    REAL_CREATE_DEVICE.get_or_init(|| unsafe { std::mem::transmute(original) });
}

unsafe extern "system" fn our_create_device(
    this: *mut c_void, rguid: *const [u8; 16],
    out_device: *mut *mut c_void, punk_outer: *mut c_void,
) -> i32 {
    let real = REAL_CREATE_DEVICE.get().expect("nobd: REAL_CREATE_DEVICE not set");
    let hr = unsafe { real(this, rguid, out_device, punk_outer) };
    if hr == 0 {
        let device = unsafe { *out_device };
        if !device.is_null() {
            unsafe { hook_device(device) };
        }
    }
    hr
}

unsafe fn hook_device(device: *mut c_void) {
    let original = unsafe { patch_vtable_slot(device, SLOT_GET_DEVICE_STATE, our_get_device_state as *const ()) };
    REAL_GET_DEVICE_STATE.get_or_init(|| unsafe { std::mem::transmute(original) });
}

unsafe extern "system" fn our_get_device_state(
    this: *mut c_void, cb_data: u32, lpv_data: *mut c_void,
) -> i32 {
    let real = REAL_GET_DEVICE_STATE.get().expect("nobd: REAL_GET_DEVICE_STATE not set");
    let hr = unsafe { real(this, cb_data, lpv_data) };

    // Only process joystick reads. Keyboard=256b, mouse=16/24b — skip those.
    if hr == 0 && matches!(cb_data, 80 | 272) && !lpv_data.is_null() {
        unsafe {
            let btn_base = (lpv_data as *mut u8).add(BUTTONS_OFFSET);
            let mut raw: u16 = 0;
            for i in 0..ATTACK_BTN_COUNT {
                if *btn_base.add(i) & 0x80 != 0 { raw |= 1u16 << i; }
            }
            let filtered = SW.lock().unwrap().process(raw);
            for i in 0..ATTACK_BTN_COUNT {
                *btn_base.add(i) = if filtered & (1u16 << i) != 0 { 0x80 } else { 0x00 };
            }
        }
    }
    hr
}
