//! nobdd — the NOBD sync window as a Linux input daemon.
//!
//! Reads the real stick, groups near-simultaneous attack presses on a fine
//! clock, and presents the result as a virtual Xbox pad. Same window, same
//! `SyncWindow`, same `ATTACK_MASK` as the GP2040-CE NOBD firmware and the
//! Windows build — the platform layer around it is what changes.
//!
//! Usage:
//! ```text
//!   nobdd run                 start the daemon (default)
//!   nobdd list                list candidate sticks
//!   nobdd probe               measure the sink hop (uinput -> readable)
//!   nobdd tune                report which RT tunings this machine allows
//!   nobdd get|stats           query a running daemon
//!   nobdd set <key> <value>   steer a running daemon live
//! ```

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "nobdd is the Linux build of NOBD Desktop.\n\
         On Windows, build the control panel instead: cargo build -p nobd-app --release"
    );
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod bulk;
#[cfg(target_os = "linux")]
mod config;
#[cfg(target_os = "linux")]
mod ctl;
#[cfg(target_os = "linux")]
mod engine;
#[cfg(target_os = "linux")]
mod evdev;
#[cfg(target_os = "linux")]
mod pad;
#[cfg(target_os = "linux")]
mod probe;
#[cfg(target_os = "linux")]
mod rt;
#[cfg(target_os = "linux")]
mod uapi;
#[cfg(target_os = "linux")]
mod uinput;

#[cfg(target_os = "linux")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("run");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };

    let code = match cmd {
        "run" => linux::run(rest),
        "list" => linux::list(),
        "probe" => linux::probe_cmd(rest),
        "tune" => linux::tune_cmd(),
        "get" | "stats" | "ping" => linux::client(cmd, rest),
        "set" => linux::client("set", rest),
        "-h" | "--help" | "help" => {
            linux::usage();
            0
        }
        "--version" | "-V" => {
            println!("nobdd {}", env!("CARGO_PKG_VERSION"));
            0
        }
        other => {
            eprintln!("nobdd: unknown command `{other}`\n");
            linux::usage();
            2
        }
    };
    std::process::exit(code);
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::{Arc, Mutex};

    use crate::config::Config;
    use crate::engine::{Engine, Exit, Source, Stats};
    use crate::evdev::EvdevSource;
    use crate::uinput::VirtualPad;

    pub fn usage() {
        println!(
            "nobdd {} — NOBD sync window for Linux\n\n\
             COMMANDS\n\
             \x20 run [--key=value ...]   start the daemon (default)\n\
             \x20 list                    list candidate sticks\n\
             \x20 probe [iters]           measure uinput -> readable latency\n\
             \x20 tune                    report available RT tunings\n\
             \x20 get | stats | ping      query a running daemon\n\
             \x20 set <key> <value>       steer a running daemon (window_ms, enabled)\n\n\
             CONFIG\n\
             \x20 {}  then  ~/.config/nobd/nobdd.conf  then  --key=value\n",
            env!("CARGO_PKG_VERSION"),
            crate::config::SYSTEM_CONFIG
        );
    }

    /// Apply `--key=value` overrides on top of the file config.
    fn apply_cli(cfg: &mut Config, args: &[String]) -> Result<(), String> {
        for a in args {
            let a = a.strip_prefix("--").unwrap_or(a);
            let (k, v) = a.split_once('=').ok_or_else(|| format!("expected --key=value, got `{a}`"))?;
            cfg.set(k, v)?;
        }
        Ok(())
    }

    pub fn list() -> i32 {
        match crate::evdev::list_gamepads() {
            Err(e) => {
                eprintln!("cannot scan /dev/input: {e}");
                1
            }
            Ok(devs) if devs.is_empty() => {
                println!("No gamepads found.");
                println!("If your stick is plugged in, you likely lack read access to /dev/input/event*.");
                println!("Install packaging/83-nobd.rules, or run as root to check.");
                0
            }
            Ok(devs) => {
                println!("{} gamepad(s):\n", devs.len());
                for d in &devs {
                    println!(
                        "  {}\n    {}  [{:04x}:{:04x} bus {:#04x}]",
                        d.path, d.name, d.id.vendor, d.id.product, d.id.bustype
                    );
                }
                match crate::bulk::BulkSource::open() {
                    Ok(Some(_)) => println!(
                        "\n  NOBD Bulk device present ({:04x}:{:04x}) — Extreme Low Latency available.",
                        crate::bulk::NOBD_BULK_VID,
                        crate::bulk::NOBD_BULK_PID
                    ),
                    Ok(None) => println!("\n  No NOBD Bulk device (stick not in NOBD Bulk mode)."),
                    Err(e) => println!("\n  NOBD Bulk device present but unusable: {e}"),
                }
                0
            }
        }
    }

    pub fn tune_cmd() -> i32 {
        let t = crate::rt::Tuning::apply(&crate::rt::TuningRequest::default());
        println!("kernel preemption model: {}", crate::rt::preempt_model());
        println!("{}", t.report());
        println!(
            "\nAnything marked `·` needs privilege the daemon does not have here.\n\
             The systemd unit in packaging/ grants exactly these and no more."
        );
        0
    }

    pub fn probe_cmd(args: &[String]) -> i32 {
        let iters = args
            .first()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(500);
        // The probe is itself a latency measurement, so tune first — otherwise
        // it reports the untuned machine and understates the product.
        let t = crate::rt::Tuning::apply(&crate::rt::TuningRequest::default());
        println!("{}\n", t.report());
        match crate::probe::measure_sink_hop(iters) {
            Ok(p) => {
                println!("uinput submit -> readable on our own event node:");
                println!("  {p}");
                println!(
                    "\nThis is the sink hop only — what a game's evdev/SDL read sees after we\n\
                     write. Add the source hop (kernel report -> our read) for end-to-end;\n\
                     `nobdd stats` reports that live as pipeline_avg_us."
                );
                0
            }
            Err(e) => {
                eprintln!("probe failed: {e}");
                1
            }
        }
    }

    pub fn client(cmd: &str, args: &[String]) -> i32 {
        let (cfg, _) = Config::load();
        let full = if args.is_empty() {
            cmd.to_string()
        } else {
            format!("{cmd} {}", args.join(" "))
        };
        match crate::ctl::request(&cfg.control_socket, &full) {
            Ok(reply) => {
                println!("{reply}");
                i32::from(reply.starts_with("error:"))
            }
            Err(e) => {
                eprintln!("cannot reach nobdd at {}: {e}", cfg.control_socket);
                eprintln!("is the daemon running?  systemctl status nobd");
                1
            }
        }
    }

    /// Open the best available source: NOBD Bulk if present and preferred,
    /// otherwise the configured/first evdev gamepad.
    fn open_source(cfg: &Config) -> std::io::Result<Source> {
        if cfg.prefer_bulk {
            match crate::bulk::BulkSource::open() {
                Ok(Some(b)) => return Ok(Source::Bulk(Box::new(b))),
                Ok(None) => {}
                // A present-but-unusable bulk device is worth reporting, not
                // swallowing — it is almost always the missing udev rule.
                Err(e) => eprintln!("NOBD Bulk unavailable, falling back to evdev: {e}"),
            }
        }
        let devs = crate::evdev::list_gamepads()?;
        let chosen = match &cfg.device {
            Some(want) => devs
                .into_iter()
                .find(|d| &d.path == want || d.name == *want)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no gamepad matching `{want}` (try: nobdd list)"),
                    )
                })?,
            None => devs.into_iter().next().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no gamepad found (try: nobdd list)",
                )
            })?,
        };
        Ok(Source::Evdev(Box::new(EvdevSource::open(chosen, cfg.grab)?)))
    }

    pub fn run(args: &[String]) -> i32 {
        let (mut cfg, notes) = Config::load();
        if let Err(e) = apply_cli(&mut cfg, args) {
            eprintln!("nobdd: {e}");
            return 2;
        }
        for n in &notes {
            println!("{n}");
        }
        cfg.publish();

        println!("nobdd {} starting", env!("CARGO_PKG_VERSION"));
        println!("kernel preemption model: {}", crate::rt::preempt_model());

        // Tuning must outlive the engine: dropping it releases the CPU latency
        // QoS request and the C-states come straight back.
        let tuning = crate::rt::Tuning::apply(&cfg.tuning);
        println!("{}", tuning.report());
        if cfg.jspoll_ms > 0 {
            println!("{}", crate::rt::set_jspoll(cfg.jspoll_ms));
        }

        let mut pad = match VirtualPad::create(cfg.identity) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("\ncannot create the virtual pad: {e}");
                return 1;
            }
        };
        println!(
            "virtual pad created{}",
            pad.sysname.as_ref().map(|s| format!(" ({s})")).unwrap_or_default()
        );

        let stats: crate::ctl::StatsHandle = Arc::new(Mutex::new(Stats::default()));
        if let Err(e) = crate::ctl::serve(&cfg.control_socket, stats.clone()) {
            // Not fatal: the sync is the product, the socket is the remote.
            eprintln!("control socket unavailable ({e}) — running without live control");
        } else {
            println!("control socket: {}", cfg.control_socket);
        }

        let mut engine = match Engine::new(cfg.settings()) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("cannot set up the event loop: {e}");
                return 1;
            }
        };
        println!(
            "window {} ms · spin {} µs · attacks {:#06x}\n",
            cfg.window_ms, cfg.spin_us, cfg.attack_mask
        );

        // Hotplug loop: a lost source is normal (unplug, mode switch), so
        // re-scan and re-adopt rather than exiting. Mirrors the Windows bulk
        // watchdog, but covers every source.
        let mut announced_wait = false;
        loop {
            match open_source(&cfg) {
                Ok(mut source) => {
                    announced_wait = false;
                    println!("source: {}", source.describe());
                    match engine.run(&mut source, &mut pad) {
                        Exit::Signal => {
                            if let Ok(mut g) = stats.lock() {
                                *g = engine.stats;
                            }
                            println!("\nshutting down — releasing all buttons");
                            pad.release_all();
                            print_summary(&engine.stats);
                            return 0;
                        }
                        Exit::SourceLost(e) => {
                            if let Ok(mut g) = stats.lock() {
                                *g = engine.stats;
                            }
                            eprintln!("source lost ({e}) — waiting for it to come back");
                            // Never leave a button held when the stick vanishes.
                            pad.release_all();
                        }
                    }
                }
                Err(e) => {
                    if !announced_wait {
                        eprintln!("waiting for a stick: {e}");
                        announced_wait = true;
                    }
                }
            }
            // Publish stats between attach attempts so `nobdd stats` stays live.
            if let Ok(mut g) = stats.lock() {
                *g = engine.stats;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    fn print_summary(s: &Stats) {
        println!("\n  commits         {}", s.commits);
        println!("  grouped (2+)    {}", s.groups);
        println!("  singles         {}", s.singles);
        if s.gap_count > 0 {
            println!(
                "  finger gap      avg {:.2} ms · max {:.2} ms  (n={})",
                s.gap_avg_us() / 1000.0,
                s.gap_max_us as f64 / 1000.0,
                s.gap_count
            );
        }
        if s.hold_count > 0 {
            println!(
                "  grouping hold   avg {:.2} ms · max {:.2} ms  (n={})",
                s.hold_avg_us() / 1000.0,
                s.hold_max_us as f64 / 1000.0,
                s.hold_count
            );
        }
        if s.pipeline_count > 0 {
            println!(
                "  daemon pipeline avg {:.0} µs · max {} µs  (n={})",
                s.pipeline_avg_us(),
                s.pipeline_max_us,
                s.pipeline_count
            );
            println!("                  (kernel event timestamp -> uinput write returning)");
        }
    }
}
