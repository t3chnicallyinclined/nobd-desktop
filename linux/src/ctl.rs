//! Control socket — a line-oriented Unix socket at `/run/nobd/nobdd.sock`.
//!
//! Runs on its **own thread** and only ever touches the shared atomics the
//! engine already re-reads each iteration. That is the whole point: the future
//! GUI, a Decky plugin, and `nobdd set window 6` all steer a running daemon
//! without adding a single instruction to the hot loop.
//!
//! Protocol (one command per line, one reply per line):
//! ```text
//!   get                 -> enabled=1 window_ms=5
//!   set window_ms 6     -> ok
//!   set enabled 0       -> ok
//!   stats               -> commits=… groups=… pipeline_avg_us=…
//!   ping                -> pong
//! ```
//! Deliberately text: `socat`, `nc` and a shell script are all valid clients on
//! a Deck with no tooling installed.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use crate::engine::Stats;

/// Snapshot the engine publishes for readers. A mutex is fine — it is touched
/// once per control request, never in the loop.
pub type StatsHandle = Arc<Mutex<Stats>>;

pub fn serve(path: &str, stats: StatsHandle) -> std::io::Result<()> {
    if let Some(dir) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // A stale socket from a killed daemon would make bind fail.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    // 0660 + the daemon's group: local users in that group can steer it, the
    // world cannot. The install script puts the desktop user in `nobd`.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660));

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let stats = stats.clone();
            std::thread::spawn(move || {
                let _ = handle(stream, stats);
            });
        }
    });
    Ok(())
}

fn handle(stream: UnixStream, stats: StatsHandle) -> std::io::Result<()> {
    let mut out = stream.try_clone()?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        let reply = dispatch(line.trim(), &stats);
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    Ok(())
}

fn dispatch(line: &str, stats: &StatsHandle) -> String {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("ping") => "pong".into(),
        Some("get") => {
            let s = nobd_shared::state();
            format!(
                "enabled={} window_ms={}",
                s.enabled.load(Ordering::Relaxed),
                s.window_ms[0].load(Ordering::Relaxed)
            )
        }
        Some("set") => {
            let (Some(key), Some(val)) = (parts.next(), parts.next()) else {
                return "error: usage: set <key> <value>".into();
            };
            let s = nobd_shared::state();
            match key {
                "window_ms" | "window" => match val.parse::<u32>() {
                    Ok(n) if n <= 16 => {
                        for w in &s.window_ms {
                            w.store(n, Ordering::Relaxed);
                        }
                        "ok".into()
                    }
                    Ok(_) => "error: window_ms must be 0..=16".into(),
                    Err(_) => format!("error: `{val}` is not a number"),
                },
                "enabled" => match val {
                    "0" | "off" | "false" => {
                        s.enabled.store(0, Ordering::Relaxed);
                        "ok".into()
                    }
                    "1" | "on" | "true" => {
                        s.enabled.store(1, Ordering::Relaxed);
                        "ok".into()
                    }
                    _ => format!("error: `{val}` is not a boolean"),
                },
                _ => format!("error: unknown key `{key}` (live keys: window_ms, enabled)"),
            }
        }
        Some("stats") => {
            let st = match stats.lock() {
                Ok(g) => *g,
                Err(p) => *p.into_inner(),
            };
            format!(
                "commits={} groups={} singles={} gap_avg_us={:.0} gap_max_us={} \
                 hold_avg_us={:.0} hold_max_us={} pipeline_avg_us={:.0} pipeline_max_us={} events={}",
                st.commits,
                st.groups,
                st.singles,
                st.gap_avg_us(),
                st.gap_max_us,
                st.hold_avg_us(),
                st.hold_max_us,
                st.pipeline_avg_us(),
                st.pipeline_max_us,
                st.source_events
            )
        }
        Some(other) => format!("error: unknown command `{other}`"),
        None => String::new(),
    }
}

/// Client side, for `nobdd set` / `nobdd stats` against a running daemon.
pub fn request(path: &str, cmd: &str) -> std::io::Result<String> {
    let stream = UnixStream::connect(path)?;
    let mut w = stream.try_clone()?;
    writeln!(w, "{cmd}")?;
    w.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line.trim_end().to_string())
}
