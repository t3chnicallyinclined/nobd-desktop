//! Stage 4 — joy.cpl / DirectInput OEM display name (ELEVATED for the HKLM
//! tables). Ported from HIDMaestro `OemNameOverrideStore.cs`. The device name
//! Windows shows comes from three registry OEM tables, NOT the USB product
//! string, so we write all three to make it read "NOBD Controller".
//!
//! Our VID:PID is unique to NOBD (pid.codes), so there is no Windows-preloaded
//! clone label to preserve — a plain set/clear is correct. The full
//! capture/restore dance in the C# store only matters when cloning a *known*
//! VID:PID whose label Windows preloads.

use std::io;

use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_WRITE};
use winreg::RegKey;

const DINPUT: &str =
    r"SYSTEM\CurrentControlSet\Control\MediaProperties\PrivateProperties\DirectInput";
const JOYSTICK: &str =
    r"SYSTEM\CurrentControlSet\Control\MediaProperties\PrivateProperties\Joystick\OEM";

fn vid_pid(vid: u16, pid: u16) -> String {
    format!("VID_{vid:04X}&PID_{pid:04X}")
}

/// Set the joy.cpl / DirectInput label across all three OEM tables.
pub fn set_oem_name(vid: u16, pid: u16, label: &str) -> io::Result<()> {
    let vp = vid_pid(vid, pid);
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let label = label.to_string();

    // KEY_WRITE only — these protected MediaProperties keys deny the default
    // KEY_ALL_ACCESS (which asks for WRITE_DAC/ownership) even to admins.

    // 1. DirectInput OEM table — value name has a space: "OEM Name".
    let (k1, _) = hklm.create_subkey_with_flags(format!(r"{DINPUT}\{vp}\OEM"), KEY_WRITE)?;
    k1.set_value("OEM Name", &label)?;

    // 2. Joystick OEM table (HKLM) — value name "OEMName" (no space).
    let (k2, _) = hklm.create_subkey_with_flags(format!(r"{JOYSTICK}\{vp}"), KEY_WRITE)?;
    k2.set_value("OEMName", &label)?;

    // 3. Joystick OEM table (HKCU) — wins for joy.cpl display.
    let (k3, _) = hkcu.create_subkey_with_flags(format!(r"{JOYSTICK}\{vp}"), KEY_WRITE)?;
    k3.set_value("OEMName", &label)?;
    Ok(())
}

/// Remove the OEM-name overrides we wrote (best-effort; ignores missing keys).
pub fn clear_oem_name(vid: u16, pid: u16) {
    let vp = vid_pid(vid, pid);
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hklm.delete_subkey_all(format!(r"{DINPUT}\{vp}\OEM"));
    let _ = hklm.delete_subkey_all(format!(r"{JOYSTICK}\{vp}"));
    let _ = hkcu.delete_subkey_all(format!(r"{JOYSTICK}\{vp}"));
}
