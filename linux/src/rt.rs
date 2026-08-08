//! Real-time tuning. This is the part with no Windows equivalent — Linux hands
//! you the knobs that decide whether a 5 ms sync window is honoured to within
//! 20 µs or within 2 ms, and every one of them is applied here at startup.
//!
//! Each returns a report line rather than failing hard: a tuning that needs a
//! privilege we don't have should degrade, not refuse to run. `Tuning::report`
//! then tells the user exactly which knobs took, so a latency claim is never
//! made on an assumption.
//!
//! Ranked by how much they actually matter for this workload:
//!
//! 1. **Timer slack → 0.** Linux gives every thread 50 µs of default slack, and
//!    it applies to the timerfd that closes our sync window. Left alone, a 5 ms
//!    window is really "5.00–5.05 ms". This is one `prctl` and it is free.
//! 2. **CPU latency QoS → 0.** Holding `/dev/cpu_dma_latency` open at 0 stops
//!    the CPU entering deep C-states. Exit latency from a deep package C-state
//!    is tens to hundreds of microseconds and lands squarely on the USB
//!    completion interrupt for the *first* press after any idle gap — precisely
//!    the press a fighting game cares about. Costs idle power; that is the trade
//!    a latency-first daemon should make, and it is a config toggle.
//! 3. **`SCHED_FIFO`.** Removes scheduler queueing behind normal-priority work.
//!    Priority is kept deliberately modest (default 20, well under the 50 that
//!    audio and `irq/*` threads use) so a bug in here cannot starve the box.
//! 4. **`mlockall`.** A major fault in the hot loop is a millisecond. We touch
//!    little memory, so locking it costs almost nothing.
//! 5. **CPU affinity.** Optional. Pinning to one core keeps the working set in
//!    that core's L1/L2 and stops migration between packages.

use std::io;
use std::os::unix::io::RawFd;

/// Which tunings were requested.
#[derive(Clone, Copy, Debug)]
pub struct TuningRequest {
    pub timer_slack: bool,
    pub cpu_dma_latency: bool,
    pub sched_fifo: Option<i32>,
    pub mlock: bool,
    pub cpu_affinity: Option<usize>,
}

impl Default for TuningRequest {
    fn default() -> Self {
        Self {
            timer_slack: true,
            cpu_dma_latency: true,
            sched_fifo: Some(20),
            mlock: true,
            cpu_affinity: None,
        }
    }
}

/// Which tunings actually took, and why the others didn't.
pub struct Tuning {
    /// Held open for the process lifetime — the QoS request is released the
    /// moment this fd closes, so it must outlive the engine.
    cpu_dma_fd: Option<RawFd>,
    pub lines: Vec<String>,
}

fn ok(lines: &mut Vec<String>, what: &str, detail: &str) {
    lines.push(format!("  ✓ {what:<22} {detail}"));
}
fn skip(lines: &mut Vec<String>, what: &str, why: &str) {
    lines.push(format!("  · {what:<22} {why}"));
}

impl Tuning {
    pub fn apply(req: &TuningRequest) -> Self {
        let mut lines = Vec::new();
        let mut cpu_dma_fd = None;

        // 1. Timer slack. PR_SET_TIMERSLACK(0) means "use the default", so the
        //    minimum achievable is 1 ns — which is what we ask for.
        if req.timer_slack {
            let r = unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 1usize, 0, 0, 0) };
            if r == 0 {
                ok(&mut lines, "timer slack", "1 ns (default is 50 µs)");
            } else {
                skip(&mut lines, "timer slack", &format!("{}", io::Error::last_os_error()));
            }
        }

        // 2. CPU latency QoS. Write a 32-bit 0 to /dev/cpu_dma_latency and keep
        //    the fd open. Needs CAP_SYS_ADMIN-ish access to the node.
        if req.cpu_dma_latency {
            match Self::pin_cpu_latency() {
                Ok(fd) => {
                    cpu_dma_fd = Some(fd);
                    ok(&mut lines, "cpu latency QoS", "0 µs — deep C-states disabled");
                }
                Err(e) => skip(&mut lines, "cpu latency QoS", &format!("{e}")),
            }
        }

        // 3. SCHED_FIFO on the calling (engine) thread.
        if let Some(prio) = req.sched_fifo {
            let lo = unsafe { libc::sched_get_priority_min(libc::SCHED_FIFO) };
            let hi = unsafe { libc::sched_get_priority_max(libc::SCHED_FIFO) };
            let prio = prio.clamp(lo, hi);
            let param = libc::sched_param { sched_priority: prio };
            let r = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
            if r == 0 {
                ok(&mut lines, "scheduler", &format!("SCHED_FIFO prio {prio} (max {hi})"));
            } else {
                skip(
                    &mut lines,
                    "scheduler",
                    &format!(
                        "{} — grant CAP_SYS_NICE or raise RLIMIT_RTPRIO",
                        io::Error::last_os_error()
                    ),
                );
            }
        }

        // 4. Lock every page, current and future.
        if req.mlock {
            let r = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
            if r == 0 {
                ok(&mut lines, "memory", "mlockall — no major faults in the loop");
            } else {
                skip(
                    &mut lines,
                    "memory",
                    &format!("{} — grant CAP_IPC_LOCK", io::Error::last_os_error()),
                );
            }
        }

        // 5. Affinity.
        if let Some(cpu) = req.cpu_affinity {
            let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
            unsafe {
                libc::CPU_ZERO(&mut set);
                libc::CPU_SET(cpu, &mut set);
            }
            let r = unsafe {
                libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set)
            };
            if r == 0 {
                ok(&mut lines, "affinity", &format!("pinned to CPU {cpu}"));
            } else {
                skip(&mut lines, "affinity", &format!("{}", io::Error::last_os_error()));
            }
        }

        Self { cpu_dma_fd, lines }
    }

    fn pin_cpu_latency() -> io::Result<RawFd> {
        let path = std::ffi::CString::new("/dev/cpu_dma_latency").unwrap();
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let zero: i32 = 0;
        let n = unsafe {
            libc::write(fd, &zero as *const i32 as *const libc::c_void, 4)
        };
        if n != 4 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(fd)
    }

    pub fn report(&self) -> String {
        if self.lines.is_empty() {
            "  (no tuning requested)".to_string()
        } else {
            self.lines.join("\n")
        }
    }
}

impl Drop for Tuning {
    fn drop(&mut self) {
        if let Some(fd) = self.cpu_dma_fd.take() {
            unsafe { libc::close(fd) };
        }
    }
}

/// Force the USB HID polling interval for joysticks down to 1 ms.
///
/// `usbhid` honours a device's `bInterval`, but many sticks advertise a lazier
/// interval than they can actually sustain, and the module's `jspoll` parameter
/// overrides it. This only affects devices bound *after* the write, so we also
/// emit a `modprobe.d` drop-in at install time to make it stick across boots.
///
/// Returns a human-readable outcome; never fatal.
pub fn set_jspoll(ms: u32) -> String {
    const PATH: &str = "/sys/module/usbhid/parameters/jspoll";
    match std::fs::read_to_string(PATH) {
        Err(e) => format!("  · usbhid jspoll        unavailable ({e})"),
        Ok(cur) => {
            let cur = cur.trim().to_string();
            if cur == ms.to_string() {
                return format!("  ✓ usbhid jspoll        already {ms} ms");
            }
            match std::fs::write(PATH, format!("{ms}")) {
                Ok(_) => format!(
                    "  ✓ usbhid jspoll        {cur} → {ms} ms (applies to devices bound from now)"
                ),
                Err(e) => format!("  · usbhid jspoll        {e} (need root)"),
            }
        }
    }
}

/// Best-effort: is this kernel a PREEMPT_RT or at least PREEMPT_DYNAMIC build?
/// Reported, not required — it changes what worst-case numbers to expect.
pub fn preempt_model() -> String {
    if let Ok(v) = std::fs::read_to_string("/sys/kernel/debug/sched/preempt") {
        // e.g. "none voluntary (full) lazy"
        if let Some(start) = v.find('(') {
            if let Some(end) = v[start..].find(')') {
                return v[start + 1..start + end].to_string();
            }
        }
    }
    match std::fs::read_to_string("/proc/version") {
        Ok(v) if v.contains("PREEMPT_RT") => "rt".into(),
        Ok(v) if v.contains("PREEMPT_DYNAMIC") => "dynamic".into(),
        Ok(v) if v.contains("PREEMPT") => "preempt".into(),
        _ => "unknown".into(),
    }
}
