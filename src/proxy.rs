use std::sync::OnceLock;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExA, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows_sys::Win32::UI::Input::XboxController::{
    XINPUT_BATTERY_INFORMATION, XINPUT_CAPABILITIES, XINPUT_STATE, XINPUT_VIBRATION,
};

pub type FnGetState = unsafe extern "system" fn(u32, *mut XINPUT_STATE) -> u32;
pub type FnSetState = unsafe extern "system" fn(u32, *mut XINPUT_VIBRATION) -> u32;
pub type FnGetCaps  = unsafe extern "system" fn(u32, u32, *mut XINPUT_CAPABILITIES) -> u32;
pub type FnEnable   = unsafe extern "system" fn(i32);
pub type FnGetBatt  = unsafe extern "system" fn(u32, u8, *mut XINPUT_BATTERY_INFORMATION) -> u32;

pub struct RealXInput {
    pub get_state: FnGetState,
    pub set_state: FnSetState,
    pub get_caps:  FnGetCaps,
    pub enable:    FnEnable,
    pub get_batt:  FnGetBatt,
}

static REAL: OnceLock<RealXInput> = OnceLock::new();

pub fn real() -> &'static RealXInput {
    REAL.get_or_init(|| unsafe {
        // LOAD_LIBRARY_SEARCH_SYSTEM32 prevents us from loading ourselves again.
        let lib = LoadLibraryExA(
            b"xinput1_4.dll\0".as_ptr(),
            0, // hFile: NULL
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        );
        assert!(lib != 0, "nobd-desktop: failed to load system xinput1_4.dll");

        macro_rules! sym {
            ($name:expr, $ty:ty) => {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(
                    GetProcAddress(lib, $name.as_ptr())
                        .expect(concat!("nobd-desktop: missing ", stringify!($name)))
                )
            };
        }

        RealXInput {
            get_state: sym!(b"XInputGetState\0",              FnGetState),
            set_state: sym!(b"XInputSetState\0",              FnSetState),
            get_caps:  sym!(b"XInputGetCapabilities\0",       FnGetCaps),
            enable:    sym!(b"XInputEnable\0",                FnEnable),
            get_batt:  sym!(b"XInputGetBatteryInformation\0", FnGetBatt),
        }
    })
}
