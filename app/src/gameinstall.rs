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
    let (Ok(a), Ok(b)) = (std::fs::metadata(&src), std::fs::metadata(&dst)) else {
        return false;
    };
    if a.len() != b.len() {
        return false;
    }
    // Same size AND same mtime is our own copy, which is the overwhelmingly
    // common case. Only fall through to reading 742 KB when that is in doubt -
    // this used to hash both files unconditionally, four times a second.
    if let (Ok(ta), Ok(tb)) = (a.modified(), b.modified()) {
        if ta == tb {
            return true;
        }
    }
    matches!((std::fs::read(&src), std::fs::read(&dst)), (Ok(x), Ok(y)) if x == y)
}

/// Is the game running right now? Used to explain why an update cannot be
/// applied yet - a loaded DLL cannot be replaced.
pub fn game_running() -> bool {
    // Snapshot the process list in-process. This used to shell out to
    // `tasklist`, which spawns a process that walks every process on the
    // machine - called four times a second from the UI thread, it stalled the
    // whole app and made input logging visibly lag.
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let want = GAME_EXE.to_ascii_lowercase();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return false;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        let mut found = false;
        while ok {
            let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
            let name = String::from_utf16_lossy(&e.szExeFile[..n]).to_ascii_lowercase();
            if name == want {
                found = true;
                break;
            }
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
        found
    }
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
    let dst = game_dir.join(DLL);
    std::fs::copy(&src, &dst)
        .map_err(|e| format!("Copy failed (is the game running?): {e}"))?;
    // Carry the source mtime across so `is_current` can answer from metadata
    // instead of re-reading both files.
    if let Ok(md) = std::fs::metadata(&src) {
        if let Ok(t) = md.modified() {
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&dst) {
                let _ = f.set_modified(t);
            }
        }
    }
    Ok(())
}

pub fn uninstall(game_dir: &Path) -> Result<(), String> {
    let dll = game_dir.join(DLL);
    if dll.exists() {
        std::fs::remove_file(&dll).map_err(|e| format!("Remove failed (is the game running?): {e}"))?;
    }
    Ok(())
}
