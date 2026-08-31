use std::ffi::c_void;
use std::sync::OnceLock;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryA;

type DirectInput8CreateFn = unsafe extern "system" fn(
    hinst: *mut c_void, dw_version: u32, riid: *const [u8; 16],
    ppv_out: *mut *mut c_void, punk_outer: *mut c_void,
) -> i32;

static REAL: OnceLock<DirectInput8CreateFn> = OnceLock::new();

fn real() -> DirectInput8CreateFn {
    *REAL.get_or_init(|| unsafe {
        // CRITICAL: load by ABSOLUTE System32 path. Loading by the bare name
        // "DINPUT8.dll" returns OUR already-loaded module (Windows keys the
        // already-loaded check on base name), so GetProcAddress would resolve
        // DirectInput8Create back to ourselves → infinite recursion → stack
        // overflow crash. A full path is a distinct module key → real DLL loads.
        let mut buf = [0u8; 260];
        let n = GetSystemDirectoryA(buf.as_mut_ptr(), buf.len() as u32) as usize;
        let mut path = Vec::with_capacity(n + 16);
        path.extend_from_slice(&buf[..n]);
        path.extend_from_slice(b"\\DINPUT8.dll\0");

        let lib = LoadLibraryA(path.as_ptr());
        if lib == 0 {
            crate::log::log("real(): LoadLibraryA(system32\\DINPUT8.dll) FAILED");
            std::process::abort();
        }
        let fp = GetProcAddress(lib, b"DirectInput8Create\0".as_ptr());
        match fp {
            Some(f) => {
                crate::log::log(&format!("real(): resolved system DirectInput8Create at {:p}", f as *const ()));
                std::mem::transmute(f)
            }
            None => {
                crate::log::log("real(): DirectInput8Create not found in system DINPUT8.dll");
                std::process::abort();
            }
        }
    })
}

#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub unsafe extern "system" fn DirectInput8Create(
    hinst: *mut c_void, dw_version: u32, riid: *const [u8; 16],
    ppv_out: *mut *mut c_void, punk_outer: *mut c_void,
) -> i32 {
    crate::log::log(&format!("DirectInput8Create: called, version={dw_version:#x}"));
    let hr = unsafe { real()(hinst, dw_version, riid, ppv_out, punk_outer) };
    crate::log::log(&format!("DirectInput8Create: real returned hr={hr:#010x}"));
    if hr == 0 && !ppv_out.is_null() {
        let di8 = unsafe { *ppv_out };
        if !di8.is_null() {
            unsafe { crate::dinput_hook::hook_di8(di8) };
        }
    }
    hr
}
