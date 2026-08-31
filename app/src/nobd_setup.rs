//! config-1: one-time "Enable NOBD Controller" setup + auto-elevate login task.
//!
//! Installs the vendored driver and creates + brands the native NOBD device (all
//! via the pure-Rust `hm-native` client), then registers a Scheduled Task that
//! relaunches nobd.exe elevated at logon. After this one elevated setup there
//! are no more UAC prompts and no visible second process — the app just runs
//! elevated, which is what the NobdNative backend needs to create its section.

use std::io;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

/// Suppress the console window for the schtasks helper (windowless GUI app).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};

use crate::sync_service::PadType;

const TASK_NAME: &str = "NOBD Controller (elevated)";

fn inf_name(mode: PadType) -> &'static str {
    match mode {
        PadType::Hid => "hidmaestro.inf",
        PadType::Xinput => "hidmaestro_xusb.inf",
    }
}

/// Directory holding the vendored driver bundle: next to nobd.exe when shipped,
/// falling back to the dev checkout so `cargo run` works.
fn driver_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let d = dir.join("driver");
            if d.join("hidmaestro.inf").exists() {
                return d;
            }
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hm-native/driver")
}

/// Whether the current process token is elevated (admin).
pub fn is_elevated() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

/// Whether a NOBD virtual pad currently exists in Windows (independent of the
/// driver being installed) — the state the Enable/Disable toggle reflects.
pub fn device_present() -> bool {
    hm_native::is_present()
}

/// Whether the driver package for `mode` is already in the DriverStore. When it
/// is, re-adding the device is just a devnode create - no cert import, no
/// pnputil - so the UI must not call that an "install".
pub fn driver_installed(mode: PadType) -> bool {
    hm_native::install::package_installed(inf_name(mode))
}

/// True when the DriverStore holds a NOBD driver for `mode` at a DIFFERENT
/// version from the one we ship — i.e. this machine is running a previous
/// release's driver.
///
/// This exists because setup is only offered when no controller is present, so
/// a user who already has a working NOBD Controller could never reach
/// `ensure_driver` and kept the old driver forever while the app reported
/// everything was fine. Spawns pnputil: call it once and cache the answer.
pub fn driver_stale(mode: PadType) -> bool {
    let inf = driver_dir().join(inf_name(mode));
    let Some(want) = hm_native::install::inf_driver_ver(&inf.to_string_lossy()) else {
        return false; // cannot tell - never nag
    };
    let name = inf_name(mode);
    let pkgs = hm_native::install::enum_packages(name.trim_end_matches(".inf"));
    let mine: Vec<_> = pkgs
        .iter()
        .filter(|p| p.original.eq_ignore_ascii_case(name))
        .collect();
    !mine.is_empty() && !mine.iter().any(|p| p.version == want)
}

/// One-time setup (must be elevated): install the vendored driver for `mode`,
/// create + brand the NOBD device, then register the auto-elevate login task.
/// Logs its outcome to `TEMP\nobd-setup.log` (the GUI path swallowed errors, so
/// a failed Enable looked like "nothing happened").
pub fn run_setup(mode: PadType) -> io::Result<()> {
    let r = run_setup_inner(mode);
    let outcome = match &r {
        Ok(()) => "OK".to_string(),
        Err(e) => format!("FAILED: {e}"),
    };
    let log = std::env::temp_dir().join("nobd-setup.log");
    let _ = std::fs::write(&log, format!("run_setup ({}): {outcome}\n", inf_name(mode)));
    r
}

fn run_setup_inner(mode: PadType) -> io::Result<()> {
    let dir = driver_dir();
    let cer = dir.join("nobd-driver.cer");
    let inf = dir.join(inf_name(mode));
    if !inf.exists() || !cer.exists() {
        return Err(io::Error::other(format!(
            "driver bundle missing in {}",
            dir.display()
        )));
    }
    let cer = cer.to_string_lossy();
    let inf = inf.to_string_lossy();
    match mode {
        PadType::Hid => hm_native::setup_hid(&cer, &inf)?,
        PadType::Xinput => hm_native::setup_xinput(&cer, &inf)?,
    };
    // Deliberately does NOT register the logon task. Setup used to create an
    // elevated ONLOGON scheduled task as a silent side effect of the user
    // clicking one button; the "Start with Windows" checkbox then rendered
    // pre-ticked as if they had asked for it. The checkbox is now the only thing
    // that creates it.
    Ok(())
}

/// Eject (remove) the NOBD Controller device(s): the branded HID pad and the
/// XUSB companion. The driver package stays installed so re-Enabling is instant.
/// Requires elevation. Returns how many devnodes were removed.
pub fn eject() -> io::Result<u32> {
    if !is_elevated() {
        return Err(io::Error::other("eject requires elevation"));
    }
    Ok(hm_native::remove_all())
}

/// SHA-1 thumbprint of a DER certificate, via `certutil -hashfile`.
///
/// Shelled out rather than hashed in-process to avoid pulling a crypto crate in
/// for one uninstall path. The label line is localized; the hash line is not, so
/// we look for the 40 hex characters and ignore everything else.
fn cert_thumbprint(cer_path: &std::path::Path) -> Option<String> {
    let out = Command::new("certutil")
        .args(["-hashfile", &cer_path.to_string_lossy(), "SHA1"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .map(|l| l.replace(' ', ""))
        .find(|l| l.len() == 40 && l.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Is any of the OLD virtual-controller stack still on this machine?
///
/// The in-game hook needs none of it: no driver, no certificate, no devnode, no
/// scheduled task. Anyone upgrading from a virtual-controller build is still
/// carrying all four, including a self-signed certificate in the machine root
/// store and a controller that shows up in Steam as a second Xbox pad.
pub fn legacy_stack_present() -> bool {
    device_present()
        || hm_native::install::package_installed("hidmaestro.inf")
        || hm_native::install::package_installed("hidmaestro_xusb.inf")
        || login_task_present()
}

/// Remove EVERYTHING NOBD put on this machine. Elevated, headless — this is what
/// the uninstaller runs before deleting the program files.
///
/// Order matters. HidHide is un-cloaked FIRST: if the files were deleted while a
/// cloak was active, the user's stick would stay invisible in every game with
/// nothing left on the machine to explain why or undo it.
///
/// Best-effort throughout — a missing piece is not a reason to abandon the rest —
/// but every step is reported so the log says what actually happened.
pub fn uninstall_everything() -> String {
    let mut log = String::new();

    // 1. Un-cloak the stick. FIRST, for the reason above.
    let _ = crate::hidhide::cloak(false);
    let ui = crate::persist::load_ui();
    if !ui.hid_device.is_empty() {
        let _ = crate::hidhide::unhide_device(&ui.hid_device);
    }
    log.push_str("hidhide: cloak cleared\n");

    // 2. Hand the virtual pad back neutral before it disappears.
    crate::sync_service::release_pad();

    // 3. The devnodes.
    let devs = if is_elevated() { hm_native::remove_all() } else { 0 };
    log.push_str(&format!("devnodes removed: {devs}\n"));

    // 4. The logon task.
    match unregister_login_task() {
        Ok(()) => log.push_str("logon task: removed\n"),
        Err(e) => log.push_str(&format!("logon task: {e}\n")),
    }

    // 5. The driver packages — both of them. The needle is `hidmaestro`, because
    //    `hidmaestro_xusb.inf` does not contain the string `hidmaestro.inf`.
    let pkgs = hm_native::install::uninstall_hidmaestro();
    log.push_str(&format!("driver packages removed: {pkgs}\n"));

    // 6. The signing certificate, from both stores it was added to. Keyed on the
    //    THUMBPRINT, never the subject name, so a near-name collision cannot take
    //    out an unrelated certificate.
    let cer = driver_dir().join("nobd-driver.cer");
    match cert_thumbprint(&cer) {
        Some(tp) => {
            for store in ["Root", "TrustedPublisher"] {
                let ok = Command::new("certutil")
                    .args(["-delstore", store, &tp])
                    .creation_flags(CREATE_NO_WINDOW)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                log.push_str(&format!("cert {store}: {}\n", if ok { "removed" } else { "not found" }));
            }
        }
        None => log.push_str("cert: thumbprint unreadable, NOT removed\n"),
    }

    // 7. Settings.
    if let Ok(base) = std::env::var("APPDATA") {
        let dir = std::path::PathBuf::from(base).join("nobd-desktop");
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => log.push_str("settings: removed\n"),
            Err(_) => log.push_str("settings: none\n"),
        }
    }
    log
}

/// Relaunch nobd.exe elevated with `--setup-<mode>` — this is what fires the UAC
/// prompt. The non-elevated caller should exit afterward so the elevated instance
/// takes over. `ShellExecuteW` returns an HINSTANCE > 32 on success; <= 32 means
/// the prompt failed or the user declined.
pub fn relaunch_elevated_for_setup(mode: PadType) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let arg = match mode {
        PadType::Hid => "--setup-hid\0",
        PadType::Xinput => "--setup-xinput\0",
    };
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file: Vec<u16> = exe
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let args: Vec<u16> = arg.encode_utf16().collect();
    let r = unsafe {
        ShellExecuteW(0, verb.as_ptr(), file.as_ptr(), args.as_ptr(), std::ptr::null(), 1)
    };
    if (r as isize) <= 32 {
        return Err(io::Error::other(format!(
            "ShellExecute(runas) returned {}",
            r as isize
        )));
    }
    Ok(())
}

/// Relaunch elevated to remove the virtual controller, for the case where the
/// app itself is not elevated. Mirrors `relaunch_elevated_for_setup`.
pub fn relaunch_elevated_for_eject() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file: Vec<u16> = exe
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let args: Vec<u16> = "--eject\0".encode_utf16().collect();
    let r = unsafe {
        ShellExecuteW(0, verb.as_ptr(), file.as_ptr(), args.as_ptr(), std::ptr::null(), 0)
    };
    if (r as isize) <= 32 {
        return Err(io::Error::other("admin request cancelled"));
    }
    Ok(())
}

/// Relaunch elevated to tear down the old virtual-controller stack.
pub fn relaunch_elevated_for_legacy_removal() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file: Vec<u16> = exe
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let args: Vec<u16> = "--remove-legacy\0".encode_utf16().collect();
    let r = unsafe {
        ShellExecuteW(0, verb.as_ptr(), file.as_ptr(), args.as_ptr(), std::ptr::null(), 0)
    };
    if (r as isize) <= 32 {
        return Err(io::Error::other("admin request cancelled"));
    }
    Ok(())
}

/// Register a Scheduled Task that relaunches nobd.exe elevated at logon, so
/// after setup there is no per-launch UAC prompt (config-1). Re-registering also
/// repoints the task at the CURRENT exe, so calling it again repairs a stale path.
/// Requires elevation (RL HIGHEST). Public so the "Start with Windows" toggle can
/// re-arm it, not just first-run setup.
pub fn register_login_task() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let status = Command::new("schtasks")
        .args([
            "/Create",
            "/TN",
            TASK_NAME,
            "/SC",
            "ONLOGON",
            "/RL",
            "HIGHEST",
            "/TR",
            // `--tray` keeps the logon launch hidden; a manual launch shows the window.
            &format!("\"{}\" --tray", exe.display()),
            "/F",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("schtasks /Create failed"))
    }
}

/// Whether the auto-elevate logon task is currently registered (i.e. the app is
/// set to start with Windows). Cheap-ish (spawns schtasks) -- cache it, don't
/// poll it per frame.
pub fn login_task_present() -> bool {
    Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Remove the auto-elevate logon task (stop starting with Windows). Requires
/// elevation, since the task runs with HIGHEST privileges.
pub fn unregister_login_task() -> io::Result<()> {
    let status = Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("schtasks /Delete failed"))
    }
}
