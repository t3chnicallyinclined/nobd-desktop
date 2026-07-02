//! Stage 2 — one-time device setup (ELEVATED): registry config + PnP devnode.
//!
//! Ported from HIDMaestro `DeviceNodeCreator.cs` (plain-HID ROOT path) +
//! `DeviceOrchestrator.WriteInstanceConfig`. The driver reads its descriptor /
//! VID / PID / product string from `HKLM\SOFTWARE\HIDMaestro\Controller<N>` at
//! EvtDeviceAdd; the devnode's `Device Parameters\ControllerIndex` tells it
//! which `Global\HIDMaestroInput<N>` section to attach to.
//!
//! All of this requires admin (DIF_REGISTERDEVICE, HKLM writes) — it is the
//! one-time "Enable NOBD Controller" setup, not a per-launch step.

use std::io;

use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_WRITE, REG_BINARY};
use winreg::{RegKey, RegValue};

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW, SetupDiOpenDeviceInfoW,
    SetupDiSetDeviceRegistryPropertyW, UpdateDriverForPlugAndPlayDevicesW, HDEVINFO,
    SP_DEVINFO_DATA,
};

use crate::report::REPORT_DESCRIPTOR;

// HIDClass setup class GUID {745a17a0-74d3-11d0-b6fe-00a0c90f57da}.
const GUID_DEVCLASS_HIDCLASS: GUID = GUID {
    data1: 0x745a_17a0,
    data2: 0x74d3,
    data3: 0x11d0,
    data4: [0xb6, 0xfe, 0x00, 0xa0, 0xc9, 0x0f, 0x57, 0xda],
};

// Constants defined locally to avoid feature-gated import churn.
const DICD_GENERATE_ID: u32 = 0x0000_0001;
const SPDRP_HARDWAREID: u32 = 0x0000_0001;
const DIF_REGISTERDEVICE: u32 = 0x0000_0019;
const DIF_REMOVE: u32 = 0x0000_0005;
const DIGCF_ALLCLASSES: u32 = 0x0000_0004;
const SPDRP_FRIENDLYNAME: u32 = 0x0000_000C;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Write `HKLM\SOFTWARE\HIDMaestro\Controller<index>` — the config the driver
/// reads at bind time (descriptor, VID/PID, product string, report length).
pub fn write_instance_config(
    index: u32,
    vid: u16,
    pid: u16,
    product: &str,
    input_report_len: u32,
) -> io::Result<()> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let (key, _) =
        hklm.create_subkey(format!(r"SOFTWARE\HIDMaestro\Controller{index}"))?;

    key.set_raw_value(
        "ReportDescriptor",
        &RegValue { vtype: REG_BINARY, bytes: REPORT_DESCRIPTOR.to_vec() },
    )?;
    key.set_value("VendorId", &(vid as u32))?;
    key.set_value("ProductId", &(pid as u32))?;
    key.set_value("VersionNumber", &0x0100u32)?;
    key.set_value("ProductString", &product.to_string())?;
    key.set_value("InputReportByteLength", &input_report_len)?;
    key.set_value("FunctionMode", &0u32)?; // 0 = plain HID (1 would be XUSB main)
    Ok(())
}

/// Destroys the device-info list on scope exit.
struct DevInfoList(HDEVINFO);
impl Drop for DevInfoList {
    fn drop(&mut self) {
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// Remove every existing devnode whose hardware id matches ours. Idempotency:
/// setup / re-install converges to exactly one NOBD device instead of stacking
/// a new one each run. Returns how many were removed.
pub fn remove_devices(vid: u16, pid: u16) -> u32 {
    let hwid = format!("root\\vid_{vid:04x}&pid_{pid:04x}");
    let mut removed = 0u32;
    unsafe {
        // flags = 0 (NOT DIGCF_PRESENT) so we also enumerate GHOST (non-present)
        // devnodes and clean stale entries left by prior installs/switches.
        let dis = SetupDiGetClassDevsW(&GUID_DEVCLASS_HIDCLASS, std::ptr::null(), 0, 0);
        if dis as isize == -1 {
            return 0;
        }
        let _list = DevInfoList(dis);

        let mut idx = 0u32;
        loop {
            let mut dev: SP_DEVINFO_DATA = std::mem::zeroed();
            dev.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiEnumDeviceInfo(dis, idx, &mut dev) == 0 {
                break; // no more devices
            }
            idx += 1;

            let mut buf = [0u16; 512];
            let mut req = 0u32;
            let ok = SetupDiGetDeviceRegistryPropertyW(
                dis,
                &mut dev,
                SPDRP_HARDWAREID,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as *mut u8,
                (buf.len() * 2) as u32,
                &mut req,
            );
            if ok == 0 {
                continue;
            }
            // Multi-sz of hardware ids; a substring match on our id is enough.
            let ids = String::from_utf16_lossy(&buf).to_ascii_lowercase();
            if ids.contains(&hwid) && SetupDiCallClassInstaller(DIF_REMOVE, dis, &mut dev) != 0 {
                removed += 1;
            }
        }
    }
    removed
}

/// Set the per-instance Device Manager FriendlyName (e.g. "NOBD Controller").
/// Per-instance, so it never collides with other devices sharing the VID/PID
/// (unlike the OEM name). Best-effort.
pub fn set_friendly_name(instance_id: &str, name: &str) -> bool {
    unsafe {
        let dis = SetupDiCreateDeviceInfoList(std::ptr::null(), 0);
        if dis as isize == -1 {
            return false;
        }
        let _list = DevInfoList(dis);

        let iid = wide(instance_id);
        let mut dev: SP_DEVINFO_DATA = std::mem::zeroed();
        dev.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
        if SetupDiOpenDeviceInfoW(dis, iid.as_ptr(), 0, 0, &mut dev) == 0 {
            return false;
        }

        let name_w = wide(name);
        let bytes = (name_w.len() * 2) as u32;
        SetupDiSetDeviceRegistryPropertyW(
            dis,
            &mut dev,
            SPDRP_FRIENDLYNAME,
            name_w.as_ptr() as *const u8,
            bytes,
        ) != 0
    }
}

/// Remove every devnode (ANY class) whose hardware-id list contains `needle`
/// (lowercased substring). Used to dedup the SWD-enumerated XUSB companions
/// (System class, so the HIDClass-only `remove_devices` can't see them). Returns
/// how many were removed.
pub fn remove_devices_by_hwid(needle: &str) -> u32 {
    let needle = needle.to_ascii_lowercase();
    let mut removed = 0u32;
    unsafe {
        let dis = SetupDiGetClassDevsW(std::ptr::null(), std::ptr::null(), 0, DIGCF_ALLCLASSES);
        if dis as isize == -1 {
            return 0;
        }
        let _list = DevInfoList(dis);

        let mut idx = 0u32;
        loop {
            let mut dev: SP_DEVINFO_DATA = std::mem::zeroed();
            dev.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiEnumDeviceInfo(dis, idx, &mut dev) == 0 {
                break;
            }
            idx += 1;

            let mut buf = [0u16; 512];
            let mut req = 0u32;
            if SetupDiGetDeviceRegistryPropertyW(
                dis,
                &mut dev,
                SPDRP_HARDWAREID,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as *mut u8,
                (buf.len() * 2) as u32,
                &mut req,
            ) == 0
            {
                continue;
            }
            let ids = String::from_utf16_lossy(&buf).to_ascii_lowercase();
            if ids.contains(&needle) && SetupDiCallClassInstaller(DIF_REMOVE, dis, &mut dev) != 0 {
                removed += 1;
            }
        }
    }
    removed
}

/// Create the ROOT\HIDClass devnode for our NOBD gamepad, tag it with
/// `ControllerIndex`, and bind the vendored driver (`inf_path`). Returns the
/// device instance id (e.g. `ROOT\HIDCLASS\0000`). Requires admin.
pub fn create_device(
    index: u32,
    vid: u16,
    pid: u16,
    description: &str,
    inf_path: &str,
) -> io::Result<String> {
    // Idempotency: clear any prior NOBD devnodes so we never stack duplicates.
    let n = remove_devices(vid, pid);
    if n > 0 {
        eprintln!("removed {n} existing NOBD devnode(s) before creating a fresh one");
    }

    let hwid = format!("root\\VID_{vid:04X}&PID_{pid:04X}");

    // Multi-sz HardwareID: "<hwid>\0root\HIDMaestro\0\0"
    let mut hwid_multi: Vec<u16> = Vec::new();
    hwid_multi.extend(hwid.encode_utf16());
    hwid_multi.push(0);
    hwid_multi.extend("root\\HIDMaestro".encode_utf16());
    hwid_multi.push(0);
    hwid_multi.push(0);

    unsafe {
        let dis = SetupDiCreateDeviceInfoList(&GUID_DEVCLASS_HIDCLASS, 0);
        if dis as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        let _list = DevInfoList(dis);

        let mut dev: SP_DEVINFO_DATA = std::mem::zeroed();
        dev.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;

        let class_name = wide("HIDClass");
        let desc = wide(description);
        if SetupDiCreateDeviceInfoW(
            dis,
            class_name.as_ptr(),
            &GUID_DEVCLASS_HIDCLASS,
            desc.as_ptr(),
            0,
            DICD_GENERATE_ID,
            &mut dev,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }

        let byte_len = (hwid_multi.len() * 2) as u32;
        if SetupDiSetDeviceRegistryPropertyW(
            dis,
            &mut dev,
            SPDRP_HARDWAREID,
            hwid_multi.as_ptr() as *const u8,
            byte_len,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }

        // Register the PnP node (admin).
        if SetupDiCallClassInstaller(DIF_REGISTERDEVICE, dis, &mut dev) == 0 {
            return Err(io::Error::last_os_error());
        }

        // Retrieve the instance id so we can tag it with ControllerIndex.
        let mut buf = [0u16; 256];
        let mut required = 0u32;
        if SetupDiGetDeviceInstanceIdW(
            dis,
            &mut dev,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut required,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let instance_id = String::from_utf16_lossy(&buf[..end]);

        // The load-bearing coupling: match the shared-section index.
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        let params_path =
            format!(r"SYSTEM\CurrentControlSet\Enum\{instance_id}\Device Parameters");
        let (params, _) =
            hklm.create_subkey_with_flags(&params_path, KEY_WRITE)?;
        params.set_value("ControllerIndex", &index)?;

        // Bind + install the vendored driver against the hardware id.
        let hwid_w = wide(&hwid);
        let inf_w = wide(inf_path);
        let mut reboot: i32 = 0;
        if UpdateDriverForPlugAndPlayDevicesW(0, hwid_w.as_ptr(), inf_w.as_ptr(), 0, &mut reboot)
            == 0
        {
            return Err(io::Error::last_os_error());
        }

        Ok(instance_id)
    }
}
