//! config-1: one-time "Enable NOBD Controller" setup + auto-elevate login task.
//!
//! Installs the vendored driver and creates + brands the native NOBD device (all
//! via the pure-Rust `hm-native` client), then registers a Scheduled Task that
//! relaunches nobd.exe elevated at logon. After this one elevated setup there
//! are no more UAC prompts and no visible second process — the app just runs
//! elevated, which is what the NobdNative backend needs to create its section.

use std::io;
use std::path::PathBuf;
use std::process::Command;

use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};

const TASK_NAME: &str = "NOBD Controller (elevated)";

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

/// Whether the NOBD driver package is already installed.
pub fn is_installed() -> bool {
    hm_native::install::driver_installed()
}

/// Whether the current process token is elevated (admin).
pub fn is_elevated() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

/// The NobdNative backend is usable only when the driver is installed AND we're
/// elevated (to create the `Global\` section).
pub fn is_ready() -> bool {
    is_installed() && is_elevated()
}

/// One-time setup (must be elevated): install the vendored driver, create +
/// brand the NOBD device, then register the auto-elevate login task.
pub fn run_setup() -> io::Result<()> {
    let dir = driver_dir();
    let cer = dir.join("nobd-driver.cer");
    let inf = dir.join("hidmaestro.inf");
    if !inf.exists() || !cer.exists() {
        return Err(io::Error::other(format!(
            "driver bundle missing in {}",
            dir.display()
        )));
    }
    hm_native::setup(&cer.to_string_lossy(), &inf.to_string_lossy())?;
    let _ = register_login_task(); // best-effort — setup still succeeds without it
    Ok(())
}

/// Relaunch nobd.exe elevated with `--setup` (UAC prompt). The non-elevated
/// caller should exit afterward so the elevated instance takes over.
pub fn relaunch_elevated_for_setup() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file: Vec<u16> = exe
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let args: Vec<u16> = "--setup\0".encode_utf16().collect();
    let r = unsafe {
        ShellExecuteW(0, verb.as_ptr(), file.as_ptr(), args.as_ptr(), std::ptr::null(), 1)
    };
    if (r as isize) <= 32 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Register a Scheduled Task that relaunches nobd.exe elevated at logon, so
/// after setup there is no per-launch UAC prompt (config-1).
fn register_login_task() -> io::Result<()> {
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
            &format!("\"{}\"", exe.display()),
            "/F",
        ])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("schtasks /Create failed"))
    }
}
