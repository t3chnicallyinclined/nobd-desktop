use std::ffi::c_void;
use std::sync::OnceLock;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExA, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

type DirectInput8CreateFn = unsafe extern "system" fn(
    hinst: *mut c_void, dw_version: u32, riid: *const [u8; 16],
    ppv_out: *mut *mut c_void, punk_outer: *mut c_void,
) -> i32;

static REAL: OnceLock<DirectInput8CreateFn> = OnceLock::new();

fn real() -> DirectInput8CreateFn {
    *REAL.get_or_init(|| unsafe {
        let lib = LoadLibraryExA(
            b"DINPUT8.dll\0".as_ptr(), 0, LOAD_LIBRARY_SEARCH_SYSTEM32,
        );
        assert!(lib != 0, "nobd-desktop: failed to load system DINPUT8.dll");
        let fp = GetProcAddress(lib, b"DirectInput8Create\0".as_ptr())
            .expect("nobd-desktop: DirectInput8Create not in system DINPUT8.dll");
        std::mem::transmute(fp)
    })
}

#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub unsafe extern "system" fn DirectInput8Create(
    hinst: *mut c_void, dw_version: u32, riid: *const [u8; 16],
    ppv_out: *mut *mut c_void, punk_outer: *mut c_void,
) -> i32 {
    let hr = unsafe { real()(hinst, dw_version, riid, ppv_out, punk_outer) };
    if hr == 0 {
        let di8 = unsafe { *ppv_out };
        if !di8.is_null() {
            unsafe { crate::dinput_hook::hook_di8(di8) };
        }
    }
    hr
}
