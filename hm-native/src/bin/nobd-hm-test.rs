//! End-to-end test of the FULLY pure-Rust path: install our VENDORED, re-signed
//! driver bundle (no C#), create the NOBD devnode, brand it, and drive it.
//!
//! Prereq: package the bundle once (non-elevated):
//!   powershell -File hm-native\scripts\package-driver.ps1
//! Then run THIS elevated. To validate OUR bundle rather than a prior copy, it
//! first uninstalls any existing hidmaestro driver, then installs ours.
//!
//! If joy.cpl shows ONE "NOBD Controller" that toggles A / rotates the hat, the
//! entire stack — install, devnode, descriptor, seqlock submit, branding — is
//! pure Rust driving a vendored driver. Zero .NET anywhere.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;

use hm_native::{install, setup, NobdController};
use windows_sys::Win32::Foundation::BOOL;
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_ctrl_c(_ctrl_type: u32) -> BOOL {
    STOP.store(true, Ordering::SeqCst);
    1 // TRUE — handled; don't hard-kill, let main release inputs cleanly
}

fn main() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), r"\driver");
    let cer = format!(r"{dir}\nobd-driver.cer");
    let inf = format!(r"{dir}\hidmaestro.inf");
    if !Path::new(&inf).exists() || !Path::new(&cer).exists() {
        eprintln!("Vendored bundle missing. First run (non-elevated):");
        eprintln!(r"  powershell -File C:\Users\trist\projects\nobd-desktop\hm-native\scripts\package-driver.ps1");
        std::process::exit(1);
    }

    println!("Uninstalling any existing HIDMaestro driver (clean slate)…");
    let n = install::uninstall_hidmaestro();
    println!("  removed {n} package(s).");

    println!("Installing our VENDORED bundle + creating the NOBD device (pure Rust)…");
    println!("  cert: {cer}");
    println!("  inf:  {inf}");
    let iid = setup(&cer, &inf).expect("setup (needs elevation)");
    println!("Setup OK — device instance: {iid}");

    let mut ctrl = NobdController::open().expect("open shared channel");
    println!("event signalling: {}", ctrl.has_event());
    println!();
    println!("-> joy.cpl should show ONE \"NOBD Controller\".");
    println!("-> A toggles ~1 Hz, hat rotates. Ctrl+C to stop (releases all inputs).");
    unsafe { SetConsoleCtrlHandler(Some(on_ctrl_c), 1) };

    const A: u16 = 0x1000;
    const DPAD: [u16; 4] = [0x0001, 0x0008, 0x0002, 0x0004]; // Up, Right, Down, Left
    let mut tick: u32 = 0;
    while !STOP.load(Ordering::Relaxed) {
        let a = if (tick / 30) % 2 == 0 { A } else { 0 };
        let hat = DPAD[((tick / 15) % 4) as usize];
        ctrl.submit(a | hat, 0, 0, 0, 0);
        sleep(Duration::from_millis(33));
        tick += 1;
    }

    // Release everything (neutral frame) so we never leave a stuck input.
    ctrl.submit(0, 0, 0, 0, 0);
    sleep(Duration::from_millis(20));
    println!("\nreleased all inputs — exiting.");
}
