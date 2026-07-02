//! De-risk harness for the pure-Rust HIDMaestro client (`hm-native`).
//!
//! Prereq — install the HIDMaestro driver once, from an ELEVATED terminal:
//!   dotnet run --project ...\hidmaestro-sync -c Release -- --install-only
//!
//! Then run THIS elevated. It creates our NOBD devnode (OUR descriptor + OEM
//! name) bound to that driver and drives a visible pattern — proving the whole
//! port end to end: devnode creation, our HID descriptor, the shared-memory
//! seqlock submit, and the joy.cpl branding. If "NOBD Controller" shows up in
//! joy.cpl and its A button toggles, the ABI port is validated.
//!
//! NOTE: each run creates a fresh devnode (DICD_GENERATE_ID). If stale
//! "NOBD Controller" nodes pile up, remove them in Device Manager between runs.

use std::thread::sleep;
use std::time::Duration;

use hm_native::report::REPORT_LEN;
use hm_native::{device, install, oem, NobdController, NOBD_LABEL, NOBD_PID, NOBD_VID};

fn main() {
    let inf = match install::installed_inf_path() {
        Some(p) => p,
        None => {
            eprintln!("HIDMaestro driver not installed. First, from an elevated terminal:");
            eprintln!(r"  dotnet run --project C:\Users\trist\projects\nobd-desktop\hidmaestro-sync -c Release -- --install-only");
            std::process::exit(1);
        }
    };
    println!("DriverStore inf: {inf}");

    println!("Writing driver config (our descriptor)…");
    device::write_instance_config(0, NOBD_VID, NOBD_PID, NOBD_LABEL, REPORT_LEN as u32)
        .expect("write_instance_config (needs elevation)");

    println!("Creating NOBD devnode…");
    let iid = device::create_device(0, NOBD_VID, NOBD_PID, NOBD_LABEL, &inf)
        .expect("create_device (needs elevation)");
    println!("  instance: {iid}");

    println!("Branding joy.cpl name -> \"{NOBD_LABEL}\"…");
    oem::set_oem_name(NOBD_VID, NOBD_PID, NOBD_LABEL).expect("set_oem_name");

    let mut ctrl = NobdController::open().expect("open shared channel");
    println!("event signalling: {}", ctrl.has_event());
    println!();
    println!("-> Open joy.cpl — you should see \"NOBD Controller\".");
    println!("-> Its A button toggles ~1 Hz and the hat rotates. Ctrl+C to stop.");

    const A: u16 = 0x1000;
    const DPAD: [u16; 4] = [0x0001, 0x0008, 0x0002, 0x0004]; // Up, Right, Down, Left
    let mut tick: u32 = 0;
    loop {
        let a = if (tick / 30) % 2 == 0 { A } else { 0 }; // ~1s toggle at 33ms
        let hat = DPAD[((tick / 15) % 4) as usize];
        ctrl.submit(a | hat, 0, 0, 0, 0);
        sleep(Duration::from_millis(33));
        tick += 1;
    }
}
