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

/// Is the DLL in the game folder byte-identical to the one we ship?
///
/// Presence alone is not enough. A stale build sat in this user's game folder
/// for two months: it was compiled against an older shared-memory layout, so
/// every launch it re-initialised the section - resetting the window, re-enabling
/// sync and zeroing the stats - and then applied its own second sync window on
/// top. "A DLL is there" must never be mistaken for "the right DLL is there".
pub fn is_current(game_dir: &Path) -> bool {
    let Some(src) = dll_source() else { return false };
    let dst = game_dir.join(DLL);
    match (std::fs::metadata(&src), std::fs::metadata(&dst)) {
        (Ok(a), Ok(b)) if a.len() == b.len() => {
            matches!((std::fs::read(&src), std::fs::read(&dst)), (Ok(x), Ok(y)) if x == y)
        }
        _ => false,
    }
}

/// Is the game running right now? Used to explain why an update cannot be
/// applied yet - a loaded DLL cannot be replaced.
pub fn game_running() -> bool {
    use std::os::windows::process::CommandExt;
    // NOT `tasklist /FI "IMAGENAME eq ..."`: that filter does not match this
    // game's 36-character executable name, so it reported "not running" while
    // the game was demonstrably running - and the caller then retried a copy
    // that can never succeed against a loaded DLL, forever.
    //
    // Take the raw list and match a prefix ourselves, since tasklist truncates
    // the image name it prints.
    let stem: String = GAME_EXE.trim_end_matches(".exe").chars().take(20).collect();
    let stem = stem.to_ascii_lowercase();
    std::process::Command::new("tasklist")
        .args(["/NH", "/FO", "CSV"])
        .creation_flags(0x0800_0000)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase().contains(&stem))
        .unwrap_or(false)
}

/// Put the current DLL in place if it is missing or stale. Returns Ok(true) when
/// something was written. Needs no elevation: Steam grants BUILTIN\Users
/// FullControl on its game folders.
pub fn ensure_installed(game_dir: &Path) -> Result<bool, String> {
    if is_current(game_dir) {
        return Ok(false);
    }
    if game_running() {
        return Err("Marvel is running - close it so NOBD can update itself".into());
    }
    // Keep whatever was there before, once, so a bad update is recoverable.
    let dst = game_dir.join(DLL);
    if dst.exists() {
        let bak = game_dir.join(format!("{DLL}.replaced-by-nobd"));
        if !bak.exists() {
            let _ = std::fs::copy(&dst, &bak);
        }
    }
    install(game_dir).map(|_| true)
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
