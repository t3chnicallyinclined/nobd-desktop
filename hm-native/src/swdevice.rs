//! `SwDeviceCreate` FFI for the XUSB companion devnode (the XInput NOBD pad).
//!
//! Ported from HIDMaestro's `driver/hmswd/hmswd.c`. Creates a software-enumerated
//! device under the `HIDMAESTRO` enumerator that binds the prebuilt `HMXInput.dll`
//! companion driver, which publishes the XUSB (XInput) interface raw-XInput games
//! read. HIDMaestro used a separate `hmswd.exe` only because .NET P/Invoke to
//! `SwDeviceCreate` fails on Win11 26200 — Rust FFI calls it directly.
//!
//! Footguns (all handled here): a UNIQUE instance-id suffix per create (else
//! Windows reuses a stale empty shell), a NON-sentinel ContainerId (else XInput
//! slotting breaks), and writing `Device Parameters\ControllerIndex` so the
//! companion finds its `Global\HIDMaestroInput<N>` section.

use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows_sys::core::GUID;

use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_WRITE};
use winreg::RegKey;

type HRESULT = i32;
type HSWDEVICE = isize;

// SW_DEVICE_CAPABILITIES flags.
const SILENT_INSTALL: u32 = 0x0000_0002;
const NO_DISPLAY_IN_UI: u32 = 0x0000_0004;
const DRIVER_REQUIRED: u32 = 0x0000_0008;

// SW_DEVICE_LIFETIME.
const LIFETIME_HANDLE: i32 = 0;
const LIFETIME_PARENT_PRESENT: i32 = 1;

const ENUMERATOR: &str = "HIDMAESTRO";
const PARENT: &str = "HTREE\\ROOT\\0";

#[repr(C)]
struct SwDeviceCreateInfo {
    cb_size: u32,
    psz_instance_id: *const u16,
    pszz_hardware_ids: *const u16,   // MULTI_SZ
    pszz_compatible_ids: *const u16, // MULTI_SZ
    p_container_id: *const GUID,
    capability_flags: u32,
    psz_device_description: *const u16,
    psz_device_location: *const u16,
    p_security_descriptor: *const c_void,
}

type SwDeviceCreateCallback =
    Option<unsafe extern "system" fn(HSWDEVICE, HRESULT, *const c_void, *const u16)>;

#[link(name = "cfgmgr32")]
extern "system" {
    fn SwDeviceCreate(
        enumerator: *const u16,
        parent: *const u16,
        create_info: *const SwDeviceCreateInfo,
        property_count: u32,
        properties: *const c_void,
        callback: SwDeviceCreateCallback,
        context: *const c_void,
        h_sw_device: *mut HSWDEVICE,
    ) -> HRESULT;
    fn SwDeviceClose(h_sw_device: HSWDEVICE);
    fn SwDeviceSetLifetime(h_sw_device: HSWDEVICE, lifetime: i32) -> HRESULT;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A unique-per-call SWD instance-id suffix. MUST differ each create, else
/// Windows reuses a stale empty devnode shell (DeviceOrchestrator.cs:29-49).
pub fn unique_suffix(index: u32) -> String {
    use std::sync::atomic::AtomicU32;
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let sess = (std::process::id() as u64) ^ t;
    format!("{:08X}{:04X}_{:04}", (sess & 0xFFFF_FFFF) as u32, seq & 0xFFFF, index)
}

/// Build a double-null-terminated MULTI_SZ from a list of strings.
fn multi_sz(items: &[&str]) -> Vec<u16> {
    let mut v = Vec::new();
    for s in items {
        v.extend(s.encode_utf16());
        v.push(0);
    }
    v.push(0);
    v
}

/// Deterministic non-sentinel ContainerId per controller index:
/// ASCII "HIDMAESTRO"-derived, {48494430-4D41-4553-5452-4F0000<idx>}.
fn container_id(index: u32) -> GUID {
    GUID {
        data1: 0x4849_4430,
        data2: 0x4D41,
        data3: 0x4553,
        data4: [0x54, 0x52, 0x4F, 0x00, 0x00, ((index >> 8) & 0xFF) as u8, (index & 0xFF) as u8, 0x00],
    }
}

struct CreateCtx {
    done: AtomicBool,
    out: Mutex<(HRESULT, Option<String>)>,
}

unsafe extern "system" fn on_created(
    _dev: HSWDEVICE,
    result: HRESULT,
    context: *const c_void,
    instance_id: *const u16,
) {
    if context.is_null() {
        return;
    }
    let ctx = &*(context as *const CreateCtx);
    let id = if instance_id.is_null() {
        None
    } else {
        let mut len = 0usize;
        while *instance_id.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(instance_id, len);
        Some(String::from_utf16_lossy(slice))
    };
    *ctx.out.lock().unwrap() = (result, id);
    ctx.done.store(true, Ordering::SeqCst);
}

/// Create (idempotently) the XUSB companion devnode for `index`, bound to
/// HMXInput.dll, tagged with `ControllerIndex`. `vid`/`pid` go into the hardware
/// id (use 0x045E/0x028E). `suffix` MUST be unique per call. Returns the device
/// instance id. Requires admin.
pub fn create_companion(
    index: u32,
    vid: u16,
    pid: u16,
    suffix: &str,
    description: &str,
) -> io::Result<String> {
    let instance_id = wide(suffix);
    let hw = multi_sz(&[
        &format!("root\\VID_{vid:04X}&PID_{pid:04X}&XI_00"),
        "root\\HIDMaestroXUSB",
    ]);
    let compat = multi_sz(&[
        "USB\\MS_COMP_XUSB10",
        "USB\\Class_FF&SubClass_5D&Prot_01",
        "USB\\Class_FF&SubClass_5D",
        "USB\\Class_FF",
    ]);
    let container = container_id(index);
    let desc = wide(description);

    let info = SwDeviceCreateInfo {
        cb_size: std::mem::size_of::<SwDeviceCreateInfo>() as u32,
        psz_instance_id: instance_id.as_ptr(),
        pszz_hardware_ids: hw.as_ptr(),
        pszz_compatible_ids: compat.as_ptr(),
        p_container_id: &container,
        capability_flags: SILENT_INSTALL | NO_DISPLAY_IN_UI | DRIVER_REQUIRED,
        psz_device_description: desc.as_ptr(),
        psz_device_location: std::ptr::null(),
        p_security_descriptor: std::ptr::null(),
    };

    let ctx = Box::new(CreateCtx {
        done: AtomicBool::new(false),
        out: Mutex::new((0, None)),
    });
    let enumerator = wide(ENUMERATOR);
    let parent = wide(PARENT);

    let mut h_dev: HSWDEVICE = 0;
    let hr = unsafe {
        SwDeviceCreate(
            enumerator.as_ptr(),
            parent.as_ptr(),
            &info,
            0,
            std::ptr::null(),
            Some(on_created),
            &*ctx as *const CreateCtx as *const c_void,
            &mut h_dev,
        )
    };
    if hr < 0 {
        return Err(io::Error::from_raw_os_error(hr));
    }

    // Wait for the creation callback (up to 10s).
    let start = Instant::now();
    while !ctx.done.load(Ordering::Acquire) && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let (result, id) = {
        let guard = ctx.out.lock().unwrap();
        (guard.0, guard.1.clone())
    };
    if !ctx.done.load(Ordering::Acquire) || result < 0 {
        unsafe { SwDeviceClose(h_dev) };
        return Err(io::Error::other(format!("SwDeviceCreate callback failed (hr=0x{result:08X})")));
    }
    let iid = id.unwrap_or_default();

    // Persist the device past process exit, then release the handle.
    unsafe {
        SwDeviceSetLifetime(h_dev, LIFETIME_PARENT_PRESENT);
        SwDeviceClose(h_dev);
    }

    // Tell the companion which shared section to open.
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let params_path = format!(r"SYSTEM\CurrentControlSet\Enum\{iid}\Device Parameters");
    if let Ok((params, _)) = hklm.create_subkey_with_flags(&params_path, KEY_WRITE) {
        let _ = params.set_value("ControllerIndex", &index);
    }

    Ok(iid)
}

/// Remove the XUSB companion by reconnecting with the same identity and
/// downgrading its lifetime so it's deleted when the handle closes.
pub fn remove_companion(index: u32, vid: u16, pid: u16, suffix: &str, description: &str) {
    let instance_id = wide(suffix);
    let hw = multi_sz(&[
        &format!("root\\VID_{vid:04X}&PID_{pid:04X}&XI_00"),
        "root\\HIDMaestroXUSB",
    ]);
    let compat = multi_sz(&[
        "USB\\MS_COMP_XUSB10",
        "USB\\Class_FF&SubClass_5D&Prot_01",
        "USB\\Class_FF&SubClass_5D",
        "USB\\Class_FF",
    ]);
    let container = container_id(index);
    let desc = wide(description);
    let info = SwDeviceCreateInfo {
        cb_size: std::mem::size_of::<SwDeviceCreateInfo>() as u32,
        psz_instance_id: instance_id.as_ptr(),
        pszz_hardware_ids: hw.as_ptr(),
        pszz_compatible_ids: compat.as_ptr(),
        p_container_id: &container,
        // No DriverRequired on the reconnect-to-remove path.
        capability_flags: SILENT_INSTALL | NO_DISPLAY_IN_UI,
        psz_device_description: desc.as_ptr(),
        psz_device_location: std::ptr::null(),
        p_security_descriptor: std::ptr::null(),
    };
    let ctx = Box::new(CreateCtx {
        done: AtomicBool::new(false),
        out: Mutex::new((0, None)),
    });
    let enumerator = wide(ENUMERATOR);
    let parent = wide(PARENT);
    let mut h_dev: HSWDEVICE = 0;
    let hr = unsafe {
        SwDeviceCreate(
            enumerator.as_ptr(),
            parent.as_ptr(),
            &info,
            0,
            std::ptr::null(),
            Some(on_created),
            &*ctx as *const CreateCtx as *const c_void,
            &mut h_dev,
        )
    };
    if hr < 0 {
        return;
    }
    let start = Instant::now();
    while !ctx.done.load(Ordering::Acquire) && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        SwDeviceSetLifetime(h_dev, LIFETIME_HANDLE);
        SwDeviceClose(h_dev); // handle-lifetime → removed on close
    }
}
