//! Self-measurement. A latency claim you cannot reproduce on the user's machine
//! is marketing; this subcommand turns it into a number they can run themselves.
//!
//! `nobdd probe` measures the **sink hop**: `write()` on /dev/uinput → the value
//! readable on our own event node, which is exactly what a game's SDL/evdev read
//! observes. It is the Linux counterpart of the Windows build's "Track-B" probe
//! (submit → `XInputGetState`), and unlike that one it needs no marker
//! fingerprinting, because `UI_GET_SYSNAME` tells us which device is ours.
//!
//! `nobdd bench` additionally reports the source hop by watching real presses:
//! kernel event timestamp → our submit returning.

use std::ffi::CString;
use std::io;

use crate::pad::PadState;
use crate::uapi::{self, ev, now_us, InputEvent};
use crate::uinput::{Identity, VirtualPad, NOBD_PHYS_PREFIX};

/// Find the `/dev/input/eventN` node belonging to the virtual pad we just made.
/// Prefers the kernel-reported sysname; falls back to the `phys` marker.
pub fn find_own_event_node(pad: &VirtualPad) -> Option<String> {
    if let Some(sysname) = &pad.sysname {
        // /sys/devices/virtual/input/<sysname>/eventN
        let dir = format!("/sys/devices/virtual/input/{sysname}");
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("event") {
                    return Some(format!("/dev/input/{name}"));
                }
            }
        }
    }
    // Fallback: scan for our phys marker.
    let rd = std::fs::read_dir("/dev/input").ok()?;
    for e in rd.flatten() {
        let path = e.path().to_string_lossy().into_owned();
        if !path.contains("/event") {
            continue;
        }
        let Ok(c) = CString::new(path.clone()) else { continue };
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 {
            continue;
        }
        let mut buf = [0u8; 256];
        let n = unsafe { libc::ioctl(fd, uapi::eviocgphys(256) as _, buf.as_mut_ptr()) };
        let hit = n > 0
            && String::from_utf8_lossy(&buf[..(n as usize).min(buf.len())])
                .starts_with(NOBD_PHYS_PREFIX);
        unsafe { libc::close(fd) };
        if hit {
            return Some(path);
        }
    }
    None
}

pub struct Percentiles {
    pub min: u64,
    pub p50: u64,
    pub p99: u64,
    pub max: u64,
    pub mean: f64,
    pub n: usize,
}

pub fn percentiles(mut v: Vec<u64>) -> Percentiles {
    if v.is_empty() {
        return Percentiles { min: 0, p50: 0, p99: 0, max: 0, mean: 0.0, n: 0 };
    }
    v.sort_unstable();
    let n = v.len();
    let idx = |q: f64| -> usize { (((n - 1) as f64) * q).round() as usize };
    Percentiles {
        min: v[0],
        p50: v[idx(0.50)],
        p99: v[idx(0.99)],
        max: v[n - 1],
        mean: v.iter().sum::<u64>() as f64 / n as f64,
        n,
    }
}

impl std::fmt::Display for Percentiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "min {} µs · p50 {} µs · p99 {} µs · max {} µs · mean {:.1} µs · n={}",
            self.min, self.p50, self.p99, self.max, self.mean, self.n
        )
    }
}

/// Write a unique left-stick value, then read our own event node until it shows
/// up. Repeats `iters` times and returns the distribution.
pub fn measure_sink_hop(iters: usize) -> io::Result<Percentiles> {
    let mut pad = VirtualPad::create(Identity::Xbox360)?;
    // Give udev a moment to create the node; without this the first open races.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let node = find_own_event_node(&pad).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "could not locate our own event node")
    })?;
    let c = CString::new(node.clone()).unwrap();
    let rfd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if rfd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("open {node}: {}", io::Error::last_os_error()),
        ));
    }
    // Same clock domain as everything else.
    let _ = uapi::ioctl_int(rfd, uapi::EVIOCSCLOCKID, libc::CLOCK_MONOTONIC);

    let mut samples = Vec::with_capacity(iters);
    let mut buf = [0u8; InputEvent::SIZE * 64];
    let mut value: i16 = 1000;

    for _ in 0..iters {
        value = value.wrapping_add(101);
        if value == 0 {
            value = 1;
        }
        let st = PadState { lx: value, ..Default::default() };
        let t0 = now_us();
        pad.submit(st)?;

        // Spin-read until our value comes back or we give up. Spinning is right
        // here: we are measuring microseconds, so a blocking read's wakeup would
        // be most of what we're trying to measure.
        let deadline = t0 + 50_000;
        let mut seen = None;
        while now_us() < deadline && seen.is_none() {
            let n = unsafe {
                libc::read(rfd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n <= 0 {
                continue;
            }
            let count = n as usize / InputEvent::SIZE;
            for i in 0..count {
                let e: InputEvent = unsafe {
                    std::ptr::read_unaligned(
                        buf.as_ptr().add(i * InputEvent::SIZE) as *const InputEvent
                    )
                };
                if e.ty == ev::ABS && e.code == uapi::abs::X && e.value == value as i32 {
                    seen = Some(now_us());
                    break;
                }
            }
        }
        if let Some(t1) = seen {
            samples.push(t1 - t0);
        }
    }
    unsafe { libc::close(rfd) };
    if samples.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "no samples — the virtual pad produced no readable events",
        ));
    }
    Ok(percentiles(samples))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_edges() {
        let p = percentiles(vec![10]);
        assert_eq!((p.min, p.p50, p.max, p.n), (10, 10, 10, 1));
        let p = percentiles((1..=100).collect());
        assert_eq!(p.min, 1);
        assert_eq!(p.max, 100);
        // idx(0.50) = (99 * 0.50).round() = 50 -> v[50] = 51
        assert_eq!(p.p50, 51);
        // idx(0.99) = (99 * 0.99).round() = 98 -> v[98] = 99
        assert_eq!(p.p99, 99);
        assert!((p.mean - 50.5).abs() < 1e-9);
    }

    #[test]
    fn empty_is_zeroed_not_panicking() {
        let p = percentiles(vec![]);
        assert_eq!(p.n, 0);
        assert_eq!(p.max, 0);
    }
}
