//! App-facing API.
//!
//! The NOBD Controller comes in two modes, both pure-Rust HIDMaestro (no ViGEm):
//!   - `Hid`    — a branded plain-HID gamepad ("NOBD Controller" in joy.cpl /
//!                Steam / DirectInput games).
//!   - `Xinput` — an XInput device (Xbox 360 wired) for raw-XInput games (MvC2).
//!
//! `setup_*()` are the one-time ELEVATED setup steps; `NobdController` is the
//! per-session runtime (open once, submit each ~1 kHz iteration).

use std::io;

use crate::report::REPORT_LEN;
use crate::shared::InputChannel;
use crate::{device, gip, install, oem, report, swdevice};

/// The branded plain-HID NOBD Controller identity.
pub const NOBD_VID: u16 = 0x1209; // pid.codes
pub const NOBD_PID: u16 = 0x4E43;
/// The old shared PID that collided with the ViGEm backend — cleaned up on setup.
const LEGACY_PID: u16 = 0x4E42;
pub const NOBD_LABEL: &str = "NOBD Controller";
/// Controller index N ↔ `Global\HIDMaestroInput<N>` ↔ `Device Parameters\ControllerIndex`.
pub const NOBD_INDEX: u32 = 0;

/// The XInput (XUSB companion) identity — Xbox 360 wired, the proven XInput id
/// (a custom VID risks not enumerating as XInput). Distinct devnode from the HID pad.
pub const XUSB_VID: u16 = 0x045E;
pub const XUSB_PID: u16 = 0x028E;
/// Hardware-id fragment used to dedup XUSB companion devnodes (System class).
const XUSB_HWID_NEEDLE: &str = "root\\hidmaestroxusb";

const INPUT_REPORT_LEN: u32 = REPORT_LEN as u32;

/// Which virtual pad the NOBD Controller presents as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PadMode {
    /// Branded plain-HID gamepad — "NOBD Controller" in joy.cpl / Steam / DInput.
    Hid,
    /// XInput device (Xbox 360 wired) — for raw-XInput games like MvC2.
    Xinput,
}

/// One-time elevated setup for HID mode: install the vendored HID driver, create
/// + brand the device. `cert_path`/`inf_path` point at the vendored bundle.
pub fn setup_hid(cert_path: &str, inf_path: &str) -> io::Result<String> {
    install::install_driver(cert_path, inf_path)?;
    // Migrate off the old shared 1209:4E42 identity (see PID history above).
    let _ = device::remove_devices(NOBD_VID, LEGACY_PID);
    oem::clear_oem_name(NOBD_VID, LEGACY_PID);

    device::write_instance_config(NOBD_INDEX, NOBD_VID, NOBD_PID, NOBD_LABEL, INPUT_REPORT_LEN)?;
    let instance_id =
        device::create_device(NOBD_INDEX, NOBD_VID, NOBD_PID, NOBD_LABEL, inf_path)?;
    oem::set_oem_name(NOBD_VID, NOBD_PID, NOBD_LABEL)?;
    let _ = device::set_friendly_name(&instance_id, NOBD_LABEL);
    Ok(instance_id)
}

/// One-time elevated setup for XInput mode: install the vendored XUSB companion
/// driver, write its config, and create the companion devnode (Xbox 360 id).
pub fn setup_xinput(cert_path: &str, xusb_inf_path: &str) -> io::Result<String> {
    install::install_driver(cert_path, xusb_inf_path)?;
    // The companion reads only VID/PID from Controller<N>; ReportDescriptor and
    // report length are unused by it, so pass 0.
    device::write_instance_config(NOBD_INDEX, XUSB_VID, XUSB_PID, NOBD_LABEL, 0)?;
    // Dedup any prior companion (across all device classes), then create fresh.
    let _ = device::remove_devices_by_hwid(XUSB_HWID_NEEDLE);
    let suffix = swdevice::unique_suffix(NOBD_INDEX);
    let instance_id =
        swdevice::create_companion(NOBD_INDEX, XUSB_VID, XUSB_PID, &suffix, NOBD_LABEL)?;
    // Per-instance FriendlyName so Device Manager reads "NOBD Controller" (not
    // the Xbox default), without touching real Xbox pads on the same VID/PID.
    let _ = device::set_friendly_name(&instance_id, NOBD_LABEL);
    Ok(instance_id)
}

/// Remove the HID-mode OEM branding.
pub fn clear_branding() {
    oem::clear_oem_name(NOBD_VID, NOBD_PID);
}

/// Whether the driver package for `mode` is installed.
pub fn is_installed(mode: PadMode) -> bool {
    match mode {
        PadMode::Hid => install::package_installed("hidmaestro.inf"),
        PadMode::Xinput => install::package_installed("hidmaestro_xusb.inf"),
    }
}

/// Per-session runtime handle. Open once, `submit` each loop iteration.
pub struct NobdController {
    channel: InputChannel,
    mode: PadMode,
}

impl NobdController {
    /// Open the shared channel for the NOBD device (must already be set up).
    pub fn open(mode: PadMode) -> io::Result<Self> {
        Ok(Self { channel: InputChannel::create(NOBD_INDEX)?, mode })
    }

    /// True if the signal event is live (else the driver polls at ~500 ms).
    pub fn has_event(&self) -> bool {
        self.channel.has_event()
    }

    /// Submit a grouped XInput-style frame. Buttons should already be run through
    /// the NOBD sync window by the caller. HID mode uses the left stick only;
    /// XInput mode uses both sticks.
    pub fn submit(&mut self, buttons: u16, lt: u8, rt: u8, lx: i16, ly: i16, rx: i16, ry: i16) {
        match self.mode {
            PadMode::Hid => {
                let r = report::pack(buttons, lt, rt, lx, ly);
                self.channel.submit(&r);
            }
            PadMode::Xinput => {
                let g = gip::pack(buttons, lt, rt, lx, ly, rx, ry);
                self.channel.submit_gip(&g);
            }
        }
    }
}
