mod config;
mod dinput_export;
mod dinput_hook;
mod log;
mod xinput_hook;

// The sync window itself is NOT vendored here: it is `nobd_shared::sync_window`,
// the same implementation the desktop app and the Linux daemon run, with the
// same tests. The old copy in this crate predated the delay-line rewrite and
// still swallowed any press shorter than the window.
// The in-game DLL is headless — all UI lives in nobd.exe (which shares config
// over named memory). No DLL tray icon.

use windows_sys::Win32::Foundation::{BOOL, HINSTANCE};

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(
    _hinst: HINSTANCE,
    reason: u32,
    _reserved: *mut std::ffi::c_void,
) -> BOOL {
    if reason == 1 /* DLL_PROCESS_ATTACH */ {
        log::init();
        xinput_hook::spawn();
    }
    1
}
