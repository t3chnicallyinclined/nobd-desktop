//! One-click install: drop DINPUT8.dll into the MvC2 game folder. Auto-detects
//! the Steam library (registry + libraryfolders.vdf), with a manual path fallback.

use std::path::{Path, PathBuf};

const GAME_FOLDER: &str = "MARVEL vs. CAPCOM Fighting Collection";
const GAME_EXE: &str = "MarvelVsCapcomFightingCollection.exe";
const DLL: &str = "DINPUT8.dll";

/// Auto-detect the MvC2 install folder across all Steam libraries.
pub fn find_game_dir() -> Option<PathBuf> {
    for lib in steam_libraries() {
        let dir = lib.join("steamapps").join("common").join(GAME_FOLDER);
        if dir.join(GAME_EXE).exists() {
            return Some(dir);
        }
    }
    None
}

fn steam_libraries() -> Vec<PathBuf> {
    let mut libs = Vec::new();
    let Some(steam) = steam_path() else { return libs };
    libs.push(steam.clone());
    // Additional libraries listed in libraryfolders.vdf (any drive).
    for vdf in [
        steam.join("steamapps").join("libraryfolders.vdf"),
        steam.join("config").join("libraryfolders.vdf"),
    ] {
        if let Ok(text) = std::fs::read_to_string(&vdf) {
            for line in text.lines() {
                let t = line.trim();
                if t.starts_with("\"path\"") {
                    if let Some(raw) = t.rsplit('"').nth(1) {
                        libs.push(PathBuf::from(raw.replace("\\\\", "\\")));
                    }
                }
            }
        }
    }
    libs
}

fn steam_path() -> Option<PathBuf> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Software\\Valve\\Steam") {
        if let Ok(p) = key.get_value::<String, _>("SteamPath") {
            let pb = PathBuf::from(p);
            if pb.exists() {
                return Some(pb);
            }
        }
    }
    for p in ["C:\\Program Files (x86)\\Steam", "C:\\Program Files\\Steam"] {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    None
}

/// The DINPUT8.dll that ships next to nobd.exe.
pub fn dll_source() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let src = exe.parent()?.join(DLL);
    src.exists().then_some(src)
}

pub fn is_installed(game_dir: &Path) -> bool {
    game_dir.join(DLL).exists()
}

pub fn has_game(game_dir: &Path) -> bool {
    game_dir.join(GAME_EXE).exists()
}

pub fn install(game_dir: &Path) -> Result<(), String> {
    if !has_game(game_dir) {
        return Err(format!("{GAME_EXE} not found in that folder."));
    }
    let src = dll_source().ok_or_else(|| format!("{DLL} not found next to nobd.exe."))?;
    std::fs::copy(&src, game_dir.join(DLL))
        .map_err(|e| format!("Copy failed (is the game running?): {e}"))?;
    Ok(())
}

pub fn uninstall(game_dir: &Path) -> Result<(), String> {
    let dll = game_dir.join(DLL);
    if dll.exists() {
        std::fs::remove_file(&dll).map_err(|e| format!("Remove failed (is the game running?): {e}"))?;
    }
    Ok(())
}

/// Create a desktop shortcut to nobd.exe via the WScript.Shell COM object.
pub fn create_desktop_shortcut() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let workdir = exe.parent().ok_or("no parent dir")?.to_path_buf();
    let profile = std::env::var("USERPROFILE").map_err(|_| "USERPROFILE not set")?;
    let lnk = PathBuf::from(profile).join("Desktop").join("NOBD Desktop.lnk");

    let ps = format!(
        "$s=(New-Object -COM WScript.Shell).CreateShortcut('{}'); \
         $s.TargetPath='{}'; $s.WorkingDirectory='{}'; $s.Save()",
        lnk.display(),
        exe.display(),
        workdir.display()
    );
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("Shortcut creation failed.".into())
    }
}
