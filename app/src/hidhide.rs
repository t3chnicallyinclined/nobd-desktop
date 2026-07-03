//! HidHide integration — hide the physical stick from games/Steam while the app
//! still reads it, so only the virtual NOBD Controller shows up.
//!
//! We drive Nefarius's signed HidHide filter driver via its bundled CLI
//! (`HidHideCLI.exe`). The flow when syncing from a HID stick:
//!   1. `--app-reg <nobd.exe>`  — whitelist ourselves (so WE still see the stick)
//!   2. `--dev-hide <instance>` — add the stick to the block list
//!   3. `--cloak-on`            — hide all blocked devices from everyone else
//! On stop we `--cloak-off` (and unhide our device) so the stick is normal again.
//!
//! HidHide is a boot-start filter driver — installing it needs a one-time reboot.
//! We ship the official signed installer and run it on request.

use std::io;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The bundled installer filename (see `tools/hidhide/`).
const INSTALLER_NAME: &str = "HidHide_1.5.230_x64.exe";

/// Locate `HidHideCLI.exe` under Program Files (the installer's fixed layout).
/// The company folder name has varied ("e.U" suffix), so try both.
pub fn cli_path() -> Option<PathBuf> {
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    let candidates = [
        format!(r"{pf}\Nefarius Software Solutions\HidHide\x64\HidHideCLI.exe"),
        format!(r"{pf}\Nefarius Software Solutions e.U\HidHide\x64\HidHideCLI.exe"),
        format!(r"{pf}\Nefarius Software Solutions e.U.\HidHide\x64\HidHideCLI.exe"),
    ];
    candidates.into_iter().map(PathBuf::from).find(|p| p.exists())
}

/// Whether HidHide is installed (its CLI is present).
pub fn is_installed() -> bool {
    cli_path().is_some()
}

/// The vendored installer: next to nobd.exe when shipped (`tools/hidhide/`),
/// else the dev checkout.
pub fn installer_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("hidhide").join(INSTALLER_NAME);
            if p.exists() {
                return p;
            }
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tools/hidhide")
        .join(INSTALLER_NAME)
}

/// Launch the official signed HidHide installer. It's interactive and requires a
/// reboot — we don't force silent so the user sees Nefarius's driver prompts.
/// Returns once the installer process has been launched.
pub fn run_installer() -> io::Result<()> {
    let installer = installer_path();
    if !installer.exists() {
        return Err(io::Error::other(format!(
            "HidHide installer missing at {}",
            installer.display()
        )));
    }
    Command::new(&installer).spawn()?;
    Ok(())
}

fn cli(args: &[&str]) -> io::Result<()> {
    let path = cli_path().ok_or_else(|| io::Error::other("HidHide not installed"))?;
    let status = Command::new(path)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("HidHideCLI {args:?} exited {status}")))
    }
}

/// Whitelist nobd.exe so our reader keeps seeing the stick once it's cloaked.
pub fn whitelist_self() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    cli(&["--app-reg", &exe.to_string_lossy()])
}

/// Convert a HID device-interface path to the device instance path HidHide wants.
/// `\\?\HID#VID_x&PID_y#7&ab&0&0000#{guid}` → `HID\VID_x&PID_y\7&ab&0&0000`.
pub fn interface_to_instance(path: &str) -> Option<String> {
    let s = path.trim_start_matches(r"\\?\").trim_start_matches(r"\\.\");
    let core = match s.find("#{") {
        Some(i) => &s[..i],
        None => s,
    };
    if core.is_empty() {
        return None;
    }
    Some(core.replace('#', "\\"))
}

/// Hide the given HID device (by its interface path) from non-whitelisted apps.
pub fn hide_device(interface_path: &str) -> io::Result<()> {
    let inst = interface_to_instance(interface_path)
        .ok_or_else(|| io::Error::other("bad device path"))?;
    cli(&["--dev-hide", &inst])
}

/// Stop hiding the given HID device.
pub fn unhide_device(interface_path: &str) -> io::Result<()> {
    let inst = interface_to_instance(interface_path)
        .ok_or_else(|| io::Error::other("bad device path"))?;
    cli(&["--dev-unhide", &inst])
}

/// Global cloak: when on, all hidden devices vanish for non-whitelisted apps.
pub fn cloak(on: bool) -> io::Result<()> {
    cli(&[if on { "--cloak-on" } else { "--cloak-off" }])
}

/// Full teardown: cloak off + unhide the given device. Best-effort — used when
/// sync stops or the app quits so the stick is never left hidden.
pub fn release(interface_path: &str) {
    let _ = cloak(false);
    let _ = unhide_device(interface_path);
}
