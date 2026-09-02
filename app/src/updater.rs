//! Self-updater. Modelled on the Retro Receipts agent's updater, which has been
//! self-applying in the field for months — same shape: a signed manifest, a minisign
//! signature over the payload, `self_replace` for the exe swap, and a safety gate that
//! defers rather than interrupting.
//!
//! ONE STRUCTURAL DIFFERENCE, and it drives the whole design: NOBD ships **two**
//! artifacts, not one.
//!
//!   nobd.exe      the app
//!   DINPUT8.dll   the in-game hook, shipped NEXT TO the exe and copied into the game
//!                 folder by gameinstall::ensure_installed
//!
//! They share `nobd_shared`'s repr(C) layout and its magic, so they must move together.
//! An app that updated its own exe and left the old DLL beside it would push a stale hook
//! into the game on the next install pass — exactly the failure v0.7.0 was written to
//! stop ("a stale build had sat in a game folder for two months"). So both are downloaded
//! and verified before either is written, and the DLL is rolled back if the exe swap
//! fails.
//!
//! We do NOT touch the copy inside the game folder. Updating the one beside the exe is
//! enough: `ensure_installed` byte-compares and pushes it across on its next pass.

use std::io::Read;

use std::sync::Mutex;

/// The version this build advertises, straight from Cargo.toml. One source of truth.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the release manifest lives.
pub const MANIFEST_URL: &str = "https://nobd.net/desktop/latest.json";

/// Minisign PUBLIC key for release payloads.
///
/// ⚠️ PLACEHOLDER — releases will not verify until this is replaced with the real one.
/// Generate the pair OFF this machine and keep the private half out of the repo:
///
///     minisign -G -p nobd-desktop.pub -s nobd-desktop.key
///
/// then sign each artifact at release time:
///
///     minisign -S -s nobd-desktop.key -m NOBD-Desktop-Setup-x.y.z.exe
///
/// Paste the `RW...` line from the .pub file here. A wrong or absent key means every
/// update is REFUSED, which is the correct direction to fail.
const MINISIGN_PUBKEY: &str = "RWQPLACEHOLDERREPLACEBEFORESHIPPINGxxxxxxxxxxxxxxxxxxxxx";

/// A newer release the manifest advertised, with a signature URL per artifact.
#[derive(Debug, Clone)]
pub struct Update {
    pub version: String,
    pub exe_url: String,
    pub exe_sig_url: String,
    pub dll_url: String,
    pub dll_sig_url: String,
    pub notes: Option<String>,
}

/// A newer version exists but could not be applied yet (Marvel is open). The UI reads
/// this to say "update ready — installs when you close Marvel" rather than staying quiet.
/// Cleared implicitly when an update applies: the process restarts and this re-inits.
pub static PENDING_UPDATE: Mutex<Option<String>> = Mutex::new(None);

#[derive(Debug)]
pub enum UpdateError {
    Http(String),
    Parse(String),
    Verify(String),
    Io(String),
    NotSafe,
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::Http(e) => write!(f, "download failed: {e}"),
            UpdateError::Parse(e) => write!(f, "bad update manifest: {e}"),
            UpdateError::Verify(e) => write!(f, "signature check failed: {e}"),
            UpdateError::Io(e) => write!(f, "could not write the update: {e}"),
            UpdateError::NotSafe => write!(f, "Marvel is running"),
        }
    }
}

/// Is it safe to swap the binaries right now?
///
/// The gate is "Marvel is closed", and unlike Retro Receipts' version this is a MECHANICAL
/// requirement rather than a judgement call: the hook DLL is mapped into the running game,
/// so the app cannot push a new one across while it is open, and shipping a new exe beside
/// a hook the game still has loaded is precisely the app/DLL version skew that has already
/// produced two separate bugs in this codebase.
pub fn safe_to_apply() -> bool {
    !crate::gameinstall::game_running()
}

/// Fetch the manifest; return Some(Update) only if it advertises a strictly newer version.
pub fn check_for_update(current: &str) -> Option<Update> {
    let body = http_get_string(MANIFEST_URL).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let version = v.get("version")?.as_str()?.to_string();
    if !is_newer(&version, current) {
        return None;
    }
    let exe_url = v.get("exe_url")?.as_str()?.to_string();
    let dll_url = v.get("dll_url")?.as_str()?.to_string();
    // Signature URLs default to `<url>.minisig`, the convention the signing step produces.
    let sig_of = |key: &str, url: &str| {
        v.get(key)
            .and_then(|s| s.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("{url}.minisig"))
    };
    Some(Update {
        exe_sig_url: sig_of("exe_sig_url", &exe_url),
        dll_sig_url: sig_of("dll_sig_url", &dll_url),
        version,
        exe_url,
        dll_url,
        notes: v.get("notes").and_then(|s| s.as_str()).map(String::from),
    })
}

/// Strict "is a newer than b", numeric per dotted component.
///
/// String comparison is wrong here and the failure is silent: "0.10.0" < "0.9.0"
/// lexicographically, so a fleet would stop updating at the first double-digit minor and
/// nobody would see an error. Missing components read as 0, so "0.8" == "0.8.0".
fn is_newer(a: &str, b: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split(['.', '-'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (x, y) = (parts(a), parts(b));
    for i in 0..x.len().max(y.len()) {
        let (l, r) = (x.get(i).copied().unwrap_or(0), y.get(i).copied().unwrap_or(0));
        if l != r {
            return l > r;
        }
    }
    false
}

/// Download, verify and install both artifacts, or change nothing at all.
pub fn apply_update(u: &Update) -> Result<(), UpdateError> {
    if !safe_to_apply() {
        return Err(UpdateError::NotSafe);
    }

    // Fetch and verify BOTH before writing EITHER. A half-applied update leaves an app and
    // a hook that disagree about the shared-memory layout, which is worse than no update.
    let exe = http_get_bytes(&u.exe_url)?;
    verify(&exe, &http_get_string(&u.exe_sig_url)?)?;
    let dll = http_get_bytes(&u.dll_url)?;
    verify(&dll, &http_get_string(&u.dll_sig_url)?)?;

    let live_exe = std::env::current_exe().map_err(|e| UpdateError::Io(e.to_string()))?;
    let live_dll = live_exe
        .parent()
        .ok_or_else(|| UpdateError::Io("no install directory".into()))?
        .join("DINPUT8.dll");

    let mut tmp = std::env::temp_dir();
    tmp.push(format!("nobd-{}.exe.new", u.version));
    std::fs::write(&tmp, &exe).map_err(|e| UpdateError::Io(e.to_string()))?;

    // DLL first: it is a plain file nothing has mapped (only the COPY in the game folder is
    // ever loaded), so it is the half we can undo. The exe swap is not reversible once
    // self_replace has run.
    let dll_backup = live_dll.with_extension("dll.bak");
    let had_dll = live_dll.exists();
    if had_dll {
        let _ = std::fs::remove_file(&dll_backup);
        std::fs::rename(&live_dll, &dll_backup).map_err(|e| UpdateError::Io(e.to_string()))?;
    }
    if let Err(e) = std::fs::write(&live_dll, &dll) {
        if had_dll {
            let _ = std::fs::rename(&dll_backup, &live_dll);
        }
        return Err(UpdateError::Io(e.to_string()));
    }

    // Now the exe. If this fails, put the old DLL back so the install stays self-consistent.
    if let Err(e) = self_replace::self_replace(&tmp) {
        let _ = std::fs::remove_file(&live_dll);
        if had_dll {
            let _ = std::fs::rename(&dll_backup, &live_dll);
        }
        let _ = std::fs::remove_file(&tmp);
        return Err(UpdateError::Io(e.to_string()));
    }

    let _ = std::fs::remove_file(&dll_backup);
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}

/// Re-exec the freshly written binary and leave. Anything the old image still holds (the
/// shared mapping, the tray icon) goes with the process.
pub fn restart() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::process::exit(0);
}

fn verify(payload: &[u8], sig_str: &str) -> Result<(), UpdateError> {
    use base64::Engine;
    let pk = minisign_verify::PublicKey::from_base64(MINISIGN_PUBKEY)
        .map_err(|e| UpdateError::Verify(format!("bad public key: {e}")))?;
    // Signing tools differ: some emit the raw minisign text, some base64 it (the Tauri
    // convention). Accept either by decoding only when the result still looks like a
    // minisign file.
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(sig_str.trim())
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .filter(|s| s.contains("untrusted comment"));
    let raw = decoded.as_deref().unwrap_or(sig_str);
    let sig = minisign_verify::Signature::decode(raw)
        .map_err(|e| UpdateError::Verify(format!("bad signature: {e}")))?;
    pk.verify(payload, &sig, false)
        .map_err(|e| UpdateError::Verify(e.to_string()))
}

fn http_get_bytes(url: &str) -> Result<Vec<u8>, UpdateError> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(60))
        .call()
        .map_err(|e| UpdateError::Http(e.to_string()))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(64 * 1024 * 1024) // a NOBD artifact is a few MB; refuse anything absurd
        .read_to_end(&mut buf)
        .map_err(|e| UpdateError::Http(e.to_string()))?;
    Ok(buf)
}

fn http_get_string(url: &str) -> Result<String, UpdateError> {
    ureq::get(url)
        .timeout(std::time::Duration::from_secs(30))
        .call()
        .map_err(|e| UpdateError::Http(e.to_string()))?
        .into_string()
        .map_err(|e| UpdateError::Http(e.to_string()))
}

/// Background updater: check at startup, then periodically. Applies when Marvel is closed;
/// otherwise records the pending version for the UI and tries again later.
/// True once a real signing key has been pasted in. Until then the updater stays
/// completely inert: no manifest fetch, no thread, nothing on the wire. Shipping it
/// "armed" against a placeholder key would poll a URL every hour and refuse every result,
/// which is noise that looks like a bug.
pub fn configured() -> bool {
    !MINISIGN_PUBKEY.contains("PLACEHOLDER")
}

pub fn spawn() {
    if !configured() {
        eprintln!("[updater] no signing key configured -- self-update disabled");
        return;
    }
    std::thread::Builder::new()
        .name("nobd-updater".into())
        .spawn(|| {
            const RECHECK: std::time::Duration = std::time::Duration::from_secs(60 * 60);
            // Let the window and the hook worker come up first; an update check racing
            // startup buys nothing and competes with the install pass.
            std::thread::sleep(std::time::Duration::from_secs(10));
            loop {
                match check_for_update(VERSION) {
                    Some(u) if safe_to_apply() => match apply_update(&u) {
                        Ok(()) => restart(),
                        Err(e) => eprintln!("[updater] apply failed: {e}"),
                    },
                    // Newer version, but Marvel is open. Never swap a hook out from under a
                    // running game; record it and let a later pass apply it.
                    Some(u) => {
                        *PENDING_UPDATE.lock().unwrap() = Some(u.version.clone());
                        eprintln!("[updater] {} ready, waiting for Marvel to close", u.version);
                    }
                    None => {}
                }
                std::thread::sleep(RECHECK);
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn newer_versions_are_detected() {
        assert!(is_newer("0.7.1", "0.7.0"));
        assert!(is_newer("0.8.0", "0.7.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn same_or_older_is_not_an_update() {
        assert!(!is_newer("0.7.0", "0.7.0"));
        assert!(!is_newer("0.6.9", "0.7.0"));
    }

    /// The bug a string compare would introduce, and it would be silent: lexicographically
    /// "0.10.0" < "0.9.0", so the whole fleet would quietly stop updating at the first
    /// double-digit minor with no error anywhere.
    #[test]
    fn double_digit_components_compare_numerically() {
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("0.7.10", "0.7.9"));
        assert!(!is_newer("0.9.0", "0.10.0"));
    }

    #[test]
    fn a_v_prefix_and_missing_components_are_tolerated() {
        assert!(is_newer("v0.8.0", "0.7.0"));
        assert!(!is_newer("0.8", "0.8.0")); // "0.8" == "0.8.0", not newer
        assert!(is_newer("0.8.1", "0.8"));
    }
}
