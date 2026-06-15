use std::sync::{LazyLock, Mutex};
use windows_sys::Win32::UI::Input::XboxController::{
    XINPUT_BATTERY_INFORMATION, XINPUT_CAPABILITIES, XINPUT_STATE, XINPUT_VIBRATION,
};

use crate::{proxy, sync_window::SyncWindow};

static SW: LazyLock<[Mutex<SyncWindow>; 4]> =
    LazyLock::new(|| std::array::from_fn(|_| Mutex::new(SyncWindow::default())));

#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub unsafe extern "system" fn XInputGetState(user: u32, state: *mut XINPUT_STATE) -> u32 {
    let ret = unsafe { (proxy::real().get_state)(user, state) };
    if ret == 0 && (user as usize) < 4 {
        let raw = unsafe { (*state).Gamepad.wButtons };
        let filtered = SW[user as usize].lock().unwrap().process(raw);
        unsafe { (*state).Gamepad.wButtons = filtered };
    }
    ret
}

#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub unsafe extern "system" fn XInputSetState(user: u32, vib: *mut XINPUT_VIBRATION) -> u32 {
    unsafe { (proxy::real().set_state)(user, vib) }
}

#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub unsafe extern "system" fn XInputGetCapabilities(
    user: u32, flags: u32, caps: *mut XINPUT_CAPABILITIES,
) -> u32 {
    unsafe { (proxy::real().get_caps)(user, flags, caps) }
}

#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub unsafe extern "system" fn XInputEnable(enable: i32) {
    unsafe { (proxy::real().enable)(enable) };
}

#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub unsafe extern "system" fn XInputGetBatteryInformation(
    user: u32, dev_type: u8, info: *mut XINPUT_BATTERY_INFORMATION,
) -> u32 {
    unsafe { (proxy::real().get_batt)(user, dev_type, info) }
}
