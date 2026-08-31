use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;

static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);

pub fn init() {
    let path = std::env::temp_dir().join("nobd_desktop.log");
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(&path) {
        *LOG.lock().unwrap() = Some(f);
        log("nobd-desktop: DLL loaded, log open");
    }
}

pub fn log(msg: &str) {
    if let Ok(mut guard) = LOG.lock() {
        if let Some(f) = guard.as_mut() {
            let _ = writeln!(f, "{msg}");
            let _ = f.flush();
        }
    }
}
