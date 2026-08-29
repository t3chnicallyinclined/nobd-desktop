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
