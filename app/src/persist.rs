//! Tiny settings persistence. The shared-memory NOBD state is RAM-only and resets
//! to defaults every launch, so we mirror the user's config to a small file in
//! %APPDATA%\nobd-desktop\config.txt and restore it on startup.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use nobd_shared::{state, NUM_PLAYERS};

/// The persisted settings (everything the user can change in the panel/tray).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cfg {
    pub enabled: u32,
    pub window: [u32; NUM_PLAYERS], // per-player sync window (ms)
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

fn ui_config_path() -> Option<PathBuf> {
    let base = std::env::var("APPDATA").ok()?;
    let dir = PathBuf::from(base).join("nobd-desktop");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("ui.txt"))
}

/// UI-only settings — the Finger Gap Tester input source. Kept in its OWN file,
/// completely separate from the shared-memory `Cfg` flow (which is re-derived
/// from shared memory every frame and has no field for a UI source choice).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct UiCfg {
    /// 0 = XInput, 1 = raw HID.
    pub input_source: u32,
    /// HID device interface path when `input_source == 1`; empty otherwise.
    pub hid_device: String,
}

/// Load the UI-only settings (input source + HID device).
pub fn load_ui() -> UiCfg {
    let mut ui = UiCfg::default();
    if let Some(path) = ui_config_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let mut it = line.splitn(2, '=');
                let k = it.next().unwrap_or("").trim();
                let Some(v) = it.next() else { continue };
                let v = v.trim();
                match k {
                    "input_source" => ui.input_source = v.parse::<u32>().unwrap_or(0).min(1),
                    "hid_device" => ui.hid_device = v.to_string(),
                    _ => {}
                }
            }
        }
    }
    ui
}

/// Persist the UI-only settings.
pub fn save_ui(ui: &UiCfg) {
    if let Some(path) = ui_config_path() {
        let body = format!(
            "input_source={}\nhid_device={}\n",
            ui.input_source, ui.hid_device,
        );
        if let Ok(mut f) = std::fs::File::create(&path) {
            let _ = f.write_all(body.as_bytes());
        }
    }
}

/// Snapshot the current config out of shared memory.
pub fn current() -> Cfg {
    let s = state();
    let mut window = [5u32; NUM_PLAYERS];
    for (i, w) in window.iter_mut().enumerate() {
        *w = s.window_ms[i].load(Ordering::Relaxed);
    }
    Cfg {
        enabled: s.enabled.load(Ordering::Relaxed),
        window,
        directions: s.directions_windowed.load(Ordering::Relaxed),
        mode: s.mode.load(Ordering::Relaxed),
        settle: s.settle_ms.load(Ordering::Relaxed),
    }
}

/// Load saved settings (if any) into shared memory. Values are clamped to valid
/// ranges so an old/edited file can't push a window past the 16 ms cap. Call
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
                    "directions" => s.directions_windowed.store(n.min(1), Ordering::Relaxed),
                    "mode" => s.mode.store(n.min(2), Ordering::Relaxed),
                    "settle" => s.settle_ms.store(n.min(3), Ordering::Relaxed),
                    // per-player windows: window0=..., window1=...
                    _ if k.starts_with("window") => {
                        if let Ok(i) = k["window".len()..].parse::<usize>() {
                            if i < NUM_PLAYERS {
                                s.window_ms[i].store(n.clamp(1, 16), Ordering::Relaxed);
                            }
                        }
                    }
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
        let mut body = format!(
            "enabled={}\ndirections={}\nmode={}\nsettle={}\n",
            cfg.enabled, cfg.directions, cfg.mode, cfg.settle,
        );
        for (i, w) in cfg.window.iter().enumerate() {
            body.push_str(&format!("window{i}={w}\n"));
        }
        if let Ok(mut f) = std::fs::File::create(&path) {
            let _ = f.write_all(body.as_bytes());
        }
    }
}
