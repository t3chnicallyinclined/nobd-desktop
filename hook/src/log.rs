use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);
static WRITTEN: AtomicUsize = AtomicUsize::new(0);

/// Hard ceiling on the diagnostic log. The previous build opened in APPEND mode
/// and never truncated, so it accumulated across every launch - found at 13.8 MB
/// in a live game. A diagnostic that grows without bound in someone's TEMP
/// folder forever is a bug, not a diagnostic.
const MAX_LOG_BYTES: usize = 1 << 20; // 1 MiB per launch

pub fn init() {
    let path = std::env::temp_dir().join("nobd_desktop.log");
    // truncate: this launch's log, not every launch ever.
    if let Ok(f) = OpenOptions::new().create(true).write(true).truncate(true).open(&path) {
        *LOG.lock().unwrap() = Some(f);
        log("nobd: DLL loaded, log open");
    }
}

pub fn log(msg: &str) {
    let used = WRITTEN.fetch_add(msg.len() + 1, Ordering::Relaxed);
    if used > MAX_LOG_BYTES {
        return;
    }
    if let Ok(mut guard) = LOG.lock() {
        if let Some(f) = guard.as_mut() {
            let _ = writeln!(f, "{msg}");
            let _ = f.flush();
            if used + msg.len() > MAX_LOG_BYTES {
                let _ = writeln!(f, "-- log ceiling reached, further messages suppressed --");
            }
        }
    }
}
