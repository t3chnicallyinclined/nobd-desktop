//! Tiny settings persistence. The shared-memory NOBD state is RAM-only and resets
//! to defaults every launch, so we mirror the user's config to a small file in
//! %APPDATA%\nobd-desktop\config.txt and restore it on startup.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use nobd_shared::state;

/// The persisted settings (everything the user can change in the panel/tray).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cfg {
    pub enabled: u32,
    pub window_ms: u32,
    pub directions: u32,
    pub mode: u32,
    pub settle: u32,
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var("APPDATA").ok()?;
    let dir = PathBuf::from(base).join("nobd-desktop");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("config.txt"))
}

/// Snapshot the current config out of shared memory.
pub fn current() -> Cfg {
    let s = state();
    Cfg {
        enabled: s.enabled.load(Ordering::Relaxed),
        window_ms: s.window_ms.load(Ordering::Relaxed),
        directions: s.directions_windowed.load(Ordering::Relaxed),
        mode: s.mode.load(Ordering::Relaxed),
        settle: s.settle_ms.load(Ordering::Relaxed),
    }
}

/// Load saved settings (if any) into shared memory. Values are clamped to valid
/// ranges so an old/edited file can't push the window past the 16 ms cap. Call
/// once at startup, after the shared mapping exists.
pub fn load() -> Cfg {
    let s = state(); // ensures the mapping + defaults exist
    if let Some(path) = config_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let mut it = line.splitn(2, '=');
                let k = it.next().unwrap_or("").trim();
                let Some(v) = it.next() else { continue };
                let Ok(n) = v.trim().parse::<u32>() else { continue };
                match k {
                    "enabled" => s.enabled.store(n.min(1), Ordering::Relaxed),
                    "window_ms" => s.window_ms.store(n.clamp(1, 16), Ordering::Relaxed),
                    "directions" => s.directions_windowed.store(n.min(1), Ordering::Relaxed),
                    "mode" => s.mode.store(n.min(2), Ordering::Relaxed),
                    "settle" => s.settle_ms.store(n.min(3), Ordering::Relaxed),
                    _ => {}
                }
            }
        }
    }
    current()
}

/// Write the current config to disk (called when it changes).
pub fn save(cfg: &Cfg) {
    if let Some(path) = config_path() {
        let body = format!(
            "enabled={}\nwindow_ms={}\ndirections={}\nmode={}\nsettle={}\n",
            cfg.enabled, cfg.window_ms, cfg.directions, cfg.mode, cfg.settle,
        );
        if let Ok(mut f) = std::fs::File::create(&path) {
            let _ = f.write_all(body.as_bytes());
        }
    }
}
