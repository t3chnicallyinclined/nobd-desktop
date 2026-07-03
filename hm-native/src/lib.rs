//! Pure-Rust client for the vendored HIDMaestro UMDF2 driver.
//!
//! Reimplements the parts of HIDMaestro's C# SDK we need to drive its prebuilt
//! (vendored, version-pinned) driver from Rust — no .NET runtime in-process.
//! Scope is a PLAIN-HID gamepad (VID != 0x045E, no report ID), which uses the
//! simple SetupDi/ROOT path and skips the entire Xbox/hmswd/XUSB/FFB machinery.
//!
//! Stages:
//!   1. `shared` — the shared-memory input ABI: create the section+event and
//!      seqlock-write reports. `report` — our HID descriptor + report packer.
//!   2. `device` (TODO) — SetupDi ROOT devnode create + `ControllerIndex`.
//!   3. `install` (TODO) — cert import + `pnputil` against the vendored bundle.
//!   4. `oem` (TODO) — the three joy.cpl OEM-name registry tables.

pub mod controller;
pub mod device;
pub mod gip;
pub mod install;
pub mod oem;
pub mod report;
pub mod shared;
pub mod swdevice;

pub use controller::{
    is_present, remove_all, setup_hid, setup_xinput, NobdController, PadMode, NOBD_LABEL, NOBD_PID,
    NOBD_VID,
};
