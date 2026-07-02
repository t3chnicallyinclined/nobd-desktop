//! App-facing API.
//!
//! `setup()` is the one-time ELEVATED "Enable NOBD Controller" step (install
//! the vendored driver, write its config, create the devnode, brand it).
//! `NobdController` is the per-session runtime: open the channel and submit
//! synced frames (~1 kHz). Under config-1 the whole app runs elevated (via a
//! one-time login task), so the runtime create-Global-section is covered too.

use std::io;

use crate::report::{pack, REPORT_DESCRIPTOR, REPORT_LEN};
use crate::shared::InputChannel;
use crate::{device, install, oem};

/// The NOBD virtual controller identity — matches the C# `NobdProfile`.
pub const NOBD_VID: u16 = 0x1209; // pid.codes
pub const NOBD_PID: u16 = 0x4E42; // "NB" — placeholder pending pid.codes registration
pub const NOBD_LABEL: &str = "NOBD Controller";
/// Controller index N ↔ `Global\HIDMaestroInput<N>` ↔ `Device Parameters\ControllerIndex`.
pub const NOBD_INDEX: u32 = 0;

// InputReportByteLength published to the driver. Our descriptor is REPORT_LEN
// data bytes with NO report ID. VERIFY AT FIRST RUNTIME TEST: if the device
// enumerates but shows no/garbled input, try REPORT_LEN + 1 (some HID stacks
// count a phantom report-ID byte in this length).
const INPUT_REPORT_LEN: u32 = REPORT_LEN as u32;

/// One-time elevated setup. `cert_path`/`inf_path` point at the pre-signed
/// vendored bundle shipped next to the app. Returns the device instance id.
pub fn setup(cert_path: &str, inf_path: &str) -> io::Result<String> {
    install::install_driver(cert_path, inf_path)?;
    device::write_instance_config(NOBD_INDEX, NOBD_VID, NOBD_PID, NOBD_LABEL, INPUT_REPORT_LEN)?;
    let instance_id =
        device::create_device(NOBD_INDEX, NOBD_VID, NOBD_PID, NOBD_LABEL, inf_path)?;
    oem::set_oem_name(NOBD_VID, NOBD_PID, NOBD_LABEL)?;
    Ok(instance_id)
}

/// Remove the OEM branding (the idle devnode itself is harmless; a full
/// uninstall path can remove it later).
pub fn clear_branding() {
    oem::clear_oem_name(NOBD_VID, NOBD_PID);
}

/// Whether the vendored driver package is present in the DriverStore.
pub fn is_installed() -> bool {
    install::driver_installed()
}

/// Per-session runtime handle. Open once, `submit` each loop iteration.
pub struct NobdController {
    channel: InputChannel,
}

impl NobdController {
    /// The HID report descriptor we author (also written to the driver's
    /// registry config at setup).
    pub const DESCRIPTOR: &'static [u8] = REPORT_DESCRIPTOR;

    /// Open the shared channel for the NOBD device (must already be `setup`).
    pub fn open() -> io::Result<Self> {
        Ok(Self { channel: InputChannel::create(NOBD_INDEX)? })
    }

    /// True if the signal event is live (else the driver polls at ~500 ms).
    pub fn has_event(&self) -> bool {
        self.channel.has_event()
    }

    /// Submit a grouped XInput-style frame. Buttons should already be run
    /// through the NOBD sync window by the caller; directions/sticks/triggers
    /// pass through.
    pub fn submit(&mut self, buttons: u16, lt: u8, rt: u8, lx: i16, ly: i16) {
        let report = pack(buttons, lt, rt, lx, ly);
        self.channel.submit(&report);
    }
}
