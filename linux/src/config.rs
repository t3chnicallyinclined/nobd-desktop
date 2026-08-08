//! Config: a flat `key = value` file, deliberately the same shape as the
//! Windows build's `config.txt` rather than TOML — it keeps the daemon
//! dependency-free and the file hand-editable over SSH on a Deck.
//!
//! Load order (later wins): built-in defaults → system file → user file → CLI
//! flags. Live changes arrive over the control socket and land in the same
//! atomics the engine reads, so nothing here is on the hot path.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crate::engine::Settings;
use crate::pad::{self, ATTACK_MASK};
use crate::rt::TuningRequest;
use crate::uinput::Identity;

pub const SYSTEM_CONFIG: &str = "/etc/nobd/nobdd.conf";

#[derive(Clone)]
pub struct Config {
    /// Sync window in ms. 0 disables windowing without disabling the pad.
    pub window_ms: u32,
    pub enabled: bool,
    pub attack_mask: u16,
    /// Which bits the window applies to. Defaults to the attacks — directions
    /// are never delayed unless explicitly asked for (firmware-parity testing).
    pub synced_mask: u16,
    pub spin_us: u64,
    pub identity: Identity,
    /// Exact device path to use; otherwise the first gamepad that isn't ours.
    pub device: Option<String>,
    /// Prefer the NOBD Bulk stream when the device is present.
    pub prefer_bulk: bool,
    /// Take exclusive ownership of the physical device.
    pub grab: bool,
    pub tuning: TuningRequest,
    /// Force usbhid's joystick poll interval (ms). 0 leaves it alone.
    pub jspoll_ms: u32,
    pub control_socket: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            window_ms: 5, // the firmware default (DEFAULT_NOBD_SYNC_DELAY)
            enabled: true,
            attack_mask: ATTACK_MASK,
            synced_mask: ATTACK_MASK,
            spin_us: 200,
            identity: Identity::Xbox360,
            device: None,
            prefer_bulk: true,
            grab: true,
            tuning: TuningRequest::default(),
            jspoll_ms: 0,
            control_socket: "/run/nobd/nobdd.sock".into(),
        }
    }
}

fn user_config() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("nobd").join("nobdd.conf"))
}

impl Config {
    /// Read system then user config, ignoring files that don't exist.
    pub fn load() -> (Self, Vec<String>) {
        let mut cfg = Config::default();
        let mut notes = Vec::new();
        let mut paths = vec![PathBuf::from(SYSTEM_CONFIG)];
        if let Some(p) = user_config() {
            paths.push(p);
        }
        for p in paths {
            match std::fs::read_to_string(&p) {
                Ok(text) => {
                    notes.push(format!("config: {}", p.display()));
                    for (line_no, line) in text.lines().enumerate() {
                        if let Err(e) = cfg.apply_line(line) {
                            notes.push(format!("  {}:{}: {e}", p.display(), line_no + 1));
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => notes.push(format!("config: {} — {e}", p.display())),
            }
        }
        (cfg, notes)
    }

    /// Apply one `key = value` line. Public so the control socket and the CLI
    /// share exactly one parser — a setting can never mean two things.
    pub fn apply_line(&mut self, line: &str) -> Result<(), String> {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            return Ok(());
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| format!("expected `key = value`, got `{line}`"))?;
        self.set(k.trim(), v.trim())
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let parse_u32 = |v: &str| v.parse::<u32>().map_err(|_| format!("`{v}` is not a number"));
        let parse_u64 = |v: &str| v.parse::<u64>().map_err(|_| format!("`{v}` is not a number"));
        let parse_bool = |v: &str| match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(format!("`{v}` is not a boolean")),
        };

        match key {
            "window_ms" | "window" => {
                let n = parse_u32(value)?;
                if n > 16 {
                    return Err("window_ms must be 0..=16 (16 ms is one 60 Hz frame)".into());
                }
                self.window_ms = n;
            }
            "enabled" => self.enabled = parse_bool(value)?,
            "attack_buttons" => {
                self.attack_mask =
                    pad::parse_mask(value).ok_or_else(|| format!("unknown button in `{value}`"))?
            }
            "window_directions" => {
                self.synced_mask = if parse_bool(value)? {
                    // Firmware-exact behaviour. Documented as not recommended:
                    // it delays motion inputs too.
                    0xFFFF
                } else {
                    self.attack_mask
                }
            }
            "spin_us" => {
                let n = parse_u64(value)?;
                if n > 5_000 {
                    return Err("spin_us above 5000 burns a core for no benefit".into());
                }
                self.spin_us = n;
            }
            "identity" => {
                self.identity = match value.to_ascii_lowercase().as_str() {
                    "xbox360" | "xbox" | "x360" => Identity::Xbox360,
                    "nobd" | "branded" => Identity::Nobd,
                    _ => return Err(format!("identity must be xbox360 or nobd, got `{value}`")),
                }
            }
            "device" => {
                self.device = if value.is_empty() { None } else { Some(value.to_string()) }
            }
            "prefer_bulk" => self.prefer_bulk = parse_bool(value)?,
            "grab" => self.grab = parse_bool(value)?,
            "jspoll_ms" => self.jspoll_ms = parse_u32(value)?,
            "control_socket" => self.control_socket = value.to_string(),
            // rt.*
            "rt_timer_slack" => self.tuning.timer_slack = parse_bool(value)?,
            "rt_cpu_dma_latency" => self.tuning.cpu_dma_latency = parse_bool(value)?,
            "rt_sched_fifo" => {
                self.tuning.sched_fifo = if parse_bool(value).unwrap_or(true) {
                    Some(value.parse::<i32>().unwrap_or(20))
                } else {
                    None
                }
            }
            "rt_mlock" => self.tuning.mlock = parse_bool(value)?,
            "rt_cpu" => {
                self.tuning.cpu_affinity =
                    if value.is_empty() { None } else { Some(parse_u32(value)? as usize) }
            }
            _ => return Err(format!("unknown key `{key}`")),
        }
        Ok(())
    }

    /// Push the live-tunable values into the shared atomics the engine reads.
    pub fn publish(&self) {
        let s = nobd_shared::state();
        s.enabled.store(u32::from(self.enabled), Ordering::Relaxed);
        for w in &s.window_ms {
            w.store(self.window_ms, Ordering::Relaxed);
        }
    }

    pub fn settings(&self) -> Settings {
        Settings {
            attack_mask: self.attack_mask,
            synced_mask: self.synced_mask,
            spin_us: self.spin_us,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_validates() {
        let mut c = Config::default();
        assert!(c.apply_line("window_ms = 7").is_ok());
        assert_eq!(c.window_ms, 7);
        assert!(c.apply_line("  # a comment").is_ok());
        assert!(c.apply_line("window_ms = 9 # trailing comment").is_ok());
        assert_eq!(c.window_ms, 9);
        assert!(c.apply_line("window_ms = 99").is_err(), "must reject >16 ms");
        assert!(c.apply_line("nonsense = 1").is_err());
        assert!(c.apply_line("attack_buttons = a,b").is_ok());
        assert_eq!(c.attack_mask, pad::bit::A | pad::bit::B);
    }

    #[test]
    fn window_directions_follows_attack_mask() {
        let mut c = Config::default();
        c.apply_line("attack_buttons = a,b").unwrap();
        c.apply_line("window_directions = off").unwrap();
        assert_eq!(c.synced_mask, pad::bit::A | pad::bit::B);
    }
}
