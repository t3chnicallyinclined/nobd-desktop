//! The hot loop. One thread, one `epoll_wait`, no polling.
//!
//! ```text
//!   epoll ──┬── source fd   (evdev report, or a reaped bulk URB)
//!           ├── timerfd     (armed on the exact window-expiry deadline)
//!           └── signalfd    (SIGINT/SIGTERM — clean release of held buttons)
//! ```
//!
//! Why this beats the Windows loop it replaces, which sleeps 1 ms and re-polls:
//!
//! * **A press is seen when it happens**, not on the next tick. No 0–1 ms of
//!   sampling jitter added before the window even opens.
//! * **The window closes on its deadline.** `timerfd` with `TFD_TIMER_ABSTIME`
//!   plus 1 ns timer slack lands the commit within tens of microseconds of
//!   `start + window`, instead of "the first poll after expiry".
//! * **The clock is the kernel's, not ours.** Every source hands us the time the
//!   *kernel* stamped the event, so our own wakeup latency never widens or
//!   narrows the window.
//! * **Idle costs nothing.** Blocked in `epoll_wait`, zero wakeups, zero CPU —
//!   which is what makes the optional spin phase affordable.
//!
//! The optional spin phase: for the last `spin_us` before a deadline we switch
//! from blocking to `epoll_wait(timeout = 0)`. That keeps *both* jobs at full
//! speed in the window's final microseconds — the partner press is still noticed
//! immediately (it is an fd event either way), and the commit itself fires
//! without a scheduler round trip. Off by default at 0; the default 200 µs costs
//! one core only for those microseconds, and only while a window is open.

use std::io;
use std::os::unix::io::RawFd;
use std::sync::atomic::Ordering;

use nobd_shared::sync_window::SyncWindow;

use crate::evdev::EvdevSource;
use crate::pad::{PadState, ATTACK_MASK};
use crate::uapi::now_us;
use crate::uinput::VirtualPad;

/// Where the engine reads the real stick.
///
/// `EvdevSource` is boxed because it carries a 1.5 KB read buffer and the ABS
/// table inline; an unboxed enum would make every `Source` move copy all of it.
pub enum Source {
    Evdev(Box<EvdevSource>),
    Bulk(Box<crate::bulk::BulkSource>),
}

impl Source {
    fn fd(&self) -> RawFd {
        match self {
            Source::Evdev(s) => s.fd(),
            Source::Bulk(s) => s.fd(),
        }
    }
    fn drain<F: FnMut(PadState, u64)>(&mut self, budget: usize, f: F) -> io::Result<usize> {
        match self {
            Source::Evdev(s) => s.drain(budget, f),
            Source::Bulk(s) => s.drain(budget, f),
        }
    }
    fn state(&self) -> PadState {
        match self {
            Source::Evdev(s) => s.state(),
            Source::Bulk(s) => s.state(),
        }
    }
    pub fn describe(&self) -> String {
        match self {
            Source::Evdev(s) => format!(
                "evdev {} — {} ({:04x}:{:04x}){}",
                s.info.path,
                s.info.name,
                s.info.id.vendor,
                s.info.id.product,
                if s.is_grabbed() { ", grabbed" } else { ", NOT grabbed" }
            ),
            Source::Bulk(s) => {
                format!("NOBD Bulk (usbfs, {} payloads/s)", s.rate_hz())
            }
        }
    }
}

/// Runtime knobs the engine re-reads every iteration (all relaxed atomics, all
/// settable live from the control socket — same contract as the Windows build).
pub struct Settings {
    pub attack_mask: u16,
    pub synced_mask: u16,
    pub spin_us: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self { attack_mask: ATTACK_MASK, synced_mask: ATTACK_MASK, spin_us: 200 }
    }
}

/// Live counters. Deliberately mirrors the Windows `PlayerStats` shape so the
/// eventual GUI reads one model on both platforms.
#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub commits: u64,
    pub groups: u64,
    pub singles: u64,
    /// Grouping hold: how long a lead press waited for its partner.
    pub hold_sum_us: u64,
    pub hold_max_us: u64,
    pub hold_count: u64,
    /// Measured finger gap between the first and last press of a group.
    pub gap_sum_us: u64,
    pub gap_max_us: u64,
    pub gap_count: u64,
    /// **The number that matters on Linux and cannot be measured on Windows:**
    /// kernel event timestamp → our `write()` returning. Everything the daemon
    /// itself adds, scheduling included, for a press that is *not* held by the
    /// window. Quote this, not a poll rate.
    pub pipeline_sum_us: u64,
    pub pipeline_max_us: u64,
    pub pipeline_count: u64,
    pub source_events: u64,
}

impl Stats {
    pub fn pipeline_avg_us(&self) -> f64 {
        if self.pipeline_count == 0 {
            0.0
        } else {
            self.pipeline_sum_us as f64 / self.pipeline_count as f64
        }
    }
    pub fn hold_avg_us(&self) -> f64 {
        if self.hold_count == 0 {
            0.0
        } else {
            self.hold_sum_us as f64 / self.hold_count as f64
        }
    }
    pub fn gap_avg_us(&self) -> f64 {
        if self.gap_count == 0 {
            0.0
        } else {
            self.gap_sum_us as f64 / self.gap_count as f64
        }
    }
}

/// Tracks press edges on the **raw** stream, independently of what the window
/// commits.
///
/// It has to be raw and it has to run *before* `SyncWindow::process`: the press
/// that completes a chord makes the window commit and close in the same call, so
/// asking the window "are you still pending?" afterwards answers no — and the
/// finger gap would then be measured to the *previous* press, reporting 0 ms for
/// every chord. That is the whole reason this is a separate, tested unit rather
/// than three lines inline.
#[derive(Default)]
struct EdgeTracker {
    prev_raw: u16,
    /// Timestamp of the press that opened the current group.
    open_us: u64,
    /// Timestamp of the most recent press in the current group.
    last_press_us: u64,
}

impl EdgeTracker {
    /// Feed one raw sample. `window_was_open` is `SyncWindow::pending_since()`
    /// sampled *before* `process` runs for this event.
    fn observe(&mut self, raw: u16, synced_mask: u16, t: u64, window_was_open: bool) {
        let pressed = raw & !self.prev_raw & synced_mask;
        self.prev_raw = raw;
        if pressed == 0 {
            return;
        }
        if !window_was_open {
            self.open_us = t;
        }
        self.last_press_us = t;
    }

    /// Time between the first and last press of the current group.
    fn gap_us(&self) -> u64 {
        self.last_press_us.saturating_sub(self.open_us)
    }
}

// ---------------------------------------------------------------------------
// epoll / timerfd / signalfd plumbing
// ---------------------------------------------------------------------------

const TOK_SOURCE: u64 = 1;
const TOK_TIMER: u64 = 2;
const TOK_SIGNAL: u64 = 3;

fn ioerr(ctx: &str) -> io::Error {
    io::Error::other(format!("{ctx}: {}", io::Error::last_os_error()))
}

fn epoll_add(epfd: RawFd, fd: RawFd, token: u64, events: u32) -> io::Result<()> {
    let mut ev = libc::epoll_event { events, u64: token };
    if unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, &mut ev) } < 0 {
        return Err(ioerr("epoll_ctl ADD"));
    }
    Ok(())
}

/// Block SIGINT/SIGTERM and deliver them as a pollable fd instead, so shutdown
/// is handled in the loop rather than in a signal handler.
fn make_signalfd() -> io::Result<RawFd> {
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGINT);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        if libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) != 0 {
            return Err(ioerr("pthread_sigmask"));
        }
        let fd = libc::signalfd(-1, &mask, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC);
        if fd < 0 {
            return Err(ioerr("signalfd"));
        }
        Ok(fd)
    }
}

/// Arm a `timerfd` on an absolute `CLOCK_MONOTONIC` deadline. `None` disarms.
fn arm_timer(fd: RawFd, deadline_us: Option<u64>) -> io::Result<()> {
    let spec = match deadline_us {
        // it_value all-zero disarms; a deadline already in the past must not, so
        // clamp to 1 ns in the future — the kernel then fires immediately.
        None => libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: 0, tv_nsec: 0 },
        },
        Some(us) => {
            let us = us.max(1);
            libc::itimerspec {
                it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
                it_value: libc::timespec {
                    tv_sec: (us / 1_000_000) as libc::time_t,
                    tv_nsec: ((us % 1_000_000) * 1_000) as libc::c_long,
                },
            }
        }
    };
    let r = unsafe {
        libc::timerfd_settime(fd, libc::TFD_TIMER_ABSTIME, &spec, std::ptr::null_mut())
    };
    if r < 0 {
        return Err(ioerr("timerfd_settime"));
    }
    Ok(())
}

/// Why `run` returned.
pub enum Exit {
    /// SIGINT/SIGTERM.
    Signal,
    /// The source went away (unplug); the caller should re-scan and re-enter.
    SourceLost(io::Error),
}

pub struct Engine {
    epfd: RawFd,
    timerfd: RawFd,
    sigfd: RawFd,
    pub settings: Settings,
    pub stats: Stats,
}

impl Engine {
    pub fn new(settings: Settings) -> io::Result<Self> {
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(ioerr("epoll_create1"));
        }
        let timerfd = unsafe {
            libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC)
        };
        if timerfd < 0 {
            return Err(ioerr("timerfd_create"));
        }
        let sigfd = make_signalfd()?;
        epoll_add(epfd, timerfd, TOK_TIMER, libc::EPOLLIN as u32)?;
        epoll_add(epfd, sigfd, TOK_SIGNAL, libc::EPOLLIN as u32)?;
        Ok(Self { epfd, timerfd, sigfd, settings, stats: Stats::default() })
    }

    /// Run until the source dies or we're signalled. `source` is registered for
    /// the duration and removed on return, so the caller can re-enter with a new
    /// one after a hotplug without rebuilding the epoll set.
    pub fn run(&mut self, source: &mut Source, pad: &mut VirtualPad) -> Exit {
        let sfd = source.fd();
        // Bulk (usbfs) signals "URBs ready to reap" with POLLOUT, not POLLIN —
        // this is usbfs's convention, not a typo. evdev is a normal readable fd.
        let want = match source {
            Source::Evdev(_) => libc::EPOLLIN as u32,
            Source::Bulk(_) => libc::EPOLLOUT as u32,
        };
        if let Err(e) = epoll_add(self.epfd, sfd, TOK_SOURCE, want) {
            return Exit::SourceLost(e);
        }
        let exit = self.run_inner(source, pad);
        unsafe {
            libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_DEL, sfd, std::ptr::null_mut());
        }
        let _ = arm_timer(self.timerfd, None);
        exit
    }

    fn run_inner(&mut self, source: &mut Source, pad: &mut VirtualPad) -> Exit {
        let mut sync = SyncWindow::new();
        let mut events: [libc::epoll_event; 8] =
            unsafe { std::mem::zeroed() };
        let mut armed: Option<u64> = None;
        // Timestamp of the press that opened the current window, for the hold
        // and gap stats and for the pipeline measurement.
        let mut window_open_us: u64 = 0;
        let mut prev_committed: u16 = 0;
        let mut edges = EdgeTracker::default();
        let mut drain_buf: [(PadState, u64); 64] = [(PadState::default(), 0); 64];

        loop {
            let cfg = nobd_shared::state();
            let enabled = cfg.enabled.load(Ordering::Relaxed) != 0;
            let window_us = cfg.window_ms[0].load(Ordering::Relaxed).clamp(0, 16) * 1000;

            // ---- decide how to wait -------------------------------------
            let deadline = sync.next_deadline_us(window_us);
            let now = now_us();
            let spinning = match (deadline, self.settings.spin_us) {
                (Some(d), s) if s > 0 => d.saturating_sub(now) <= s,
                _ => false,
            };

            let timeout_ms = if spinning {
                // Hot phase: no blocking, no timer. We are inside the last
                // `spin_us` of the window, checking the clock and the fd as fast
                // as the CPU allows.
                0
            } else {
                // Arm on (deadline - spin_us) so we wake just before the edge and
                // enter the spin, or exactly on it when spin is off.
                let want = deadline.map(|d| d.saturating_sub(self.settings.spin_us).max(1));
                if want != armed {
                    if let Err(e) = arm_timer(self.timerfd, want) {
                        return Exit::SourceLost(e);
                    }
                    armed = want;
                }
                -1
            };

            let n = unsafe {
                libc::epoll_wait(self.epfd, events.as_mut_ptr(), events.len() as i32, timeout_ms)
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Exit::SourceLost(e);
            }

            let mut source_ready = spinning; // in spin mode always try a drain
            let mut timer_fired = false;
            for ev in events.iter().take(n as usize) {
                match ev.u64 {
                    TOK_SOURCE => source_ready = true,
                    TOK_TIMER => {
                        timer_fired = true;
                        let mut buf = [0u8; 8];
                        unsafe {
                            libc::read(self.timerfd, buf.as_mut_ptr() as *mut libc::c_void, 8)
                        };
                        armed = None;
                    }
                    TOK_SIGNAL => {
                        let mut buf = [0u8; 128];
                        unsafe {
                            libc::read(self.sigfd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                        };
                        return Exit::Signal;
                    }
                    _ => {}
                }
            }

            // ---- 1. source events -------------------------------------------
            // Collect first, process after: `drain` borrows the source mutably
            // and the closure needs `&mut self` for stats. The budget is the
            // buffer size — anything beyond it stays queued in the kernel and is
            // picked up on the next pass, because the fd is level-triggered.
            let mut count = 0usize;
            if source_ready {
                let r = source.drain(drain_buf.len(), |st, t| {
                    drain_buf[count] = (st, t);
                    count += 1;
                });
                if let Err(e) = r {
                    return Exit::SourceLost(e);
                }
            }

            for &(st, event_us) in drain_buf.iter().take(count) {
                self.stats.source_events += 1;

                // Raw press edges first — see `EdgeTracker` for why the order
                // matters.
                let was_pending = sync.pending_since().is_some();
                edges.observe(st.buttons, self.settings.synced_mask, event_us, was_pending);
                window_open_us = edges.open_us;

                let grouped = sync.process(
                    st.buttons,
                    self.settings.attack_mask,
                    self.settings.synced_mask,
                    event_us,
                    window_us,
                    enabled,
                );

                if grouped != prev_committed {
                    let out = st.with_buttons(grouped);
                    if pad.submit(out).is_ok() {
                        self.record_commit(
                            grouped,
                            prev_committed,
                            event_us,
                            window_open_us,
                            edges.gap_us(),
                            sync.pending_since().is_none(),
                        );
                    }
                    prev_committed = grouped;
                } else {
                    // Analog moved but no button change: still deliver, because
                    // directions are never windowed.
                    let out = st.with_buttons(grouped);
                    let _ = pad.submit(out);
                }
            }

            // ---- 2. deadline ------------------------------------------------
            // Both the timer path and the spin path land here. Re-running the
            // window at `now` is what actually commits an expired lone press —
            // the same call the Windows poll loop makes, but at the edge.
            if timer_fired || spinning || count > 0 {
                let now = now_us();
                if let Some(d) = sync.next_deadline_us(window_us) {
                    if now >= d {
                        let st = source.state();
                        let grouped = sync.process(
                            st.buttons,
                            self.settings.attack_mask,
                            self.settings.synced_mask,
                            now,
                            window_us,
                            enabled,
                        );
                        if grouped != prev_committed {
                            let out = st.with_buttons(grouped);
                            if pad.submit(out).is_ok() {
                                self.record_commit(
                                    grouped,
                                    prev_committed,
                                    d, // credit the deadline, not our wakeup
                                    window_open_us,
                                    edges.gap_us(),
                                    true,
                                );
                            }
                            prev_committed = grouped;
                        }
                        armed = None;
                    }
                }
            }
        }
    }

    /// Book-keeping for one delivered change. `event_us` is the kernel/firmware
    /// timestamp that caused it, so `pipeline` is a true end-to-end measure of
    /// what the daemon added.
    fn record_commit(
        &mut self,
        grouped: u16,
        prev: u16,
        event_us: u64,
        window_open_us: u64,
        gap_us: u64,
        window_closed: bool,
    ) {
        let newly_pressed = grouped & !prev & self.settings.attack_mask;
        if newly_pressed == 0 {
            return; // a release, or a non-attack bit — not a commit we measure
        }
        self.stats.commits += 1;
        if newly_pressed.count_ones() >= 2 {
            self.stats.groups += 1;
            // Finger gap = first press to last press of this group, measured
            // by `EdgeTracker` off the raw stream.
            self.stats.gap_sum_us += gap_us;
            self.stats.gap_max_us = self.stats.gap_max_us.max(gap_us);
            self.stats.gap_count += 1;
        } else {
            self.stats.singles += 1;
        }

        if window_closed {
            let hold = event_us.saturating_sub(window_open_us);
            self.stats.hold_sum_us += hold;
            self.stats.hold_max_us = self.stats.hold_max_us.max(hold);
            self.stats.hold_count += 1;
        }

        // Everything we added, measured from the kernel's own stamp. For a
        // grouped press this excludes the deliberate window hold (the hold stat
        // covers that) because `event_us` is the press that closed the group.
        let pipeline = now_us().saturating_sub(event_us);
        // Guard against a translated bulk timestamp that lands ahead of us.
        if pipeline < 1_000_000 {
            self.stats.pipeline_sum_us += pipeline;
            self.stats.pipeline_max_us = self.stats.pipeline_max_us.max(pipeline);
            self.stats.pipeline_count += 1;
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.timerfd);
            libc::close(self.sigfd);
            libc::close(self.epfd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pad::bit;
    use nobd_shared::sync_window::SyncWindow;

    const AM: u16 = ATTACK_MASK;
    const W: u32 = 5_000;

    /// Drive the tracker exactly the way `run_inner` does — sampling the window's
    /// pending state *before* `process` — so these are regression tests for the
    /// real call order, not for the tracker in isolation.
    fn run(events: &[(u16, u64)]) -> (u64, u64) {
        let mut edges = EdgeTracker::default();
        let mut sync = SyncWindow::new();
        for &(raw, t) in events {
            let was_pending = sync.pending_since().is_some();
            edges.observe(raw, AM, t, was_pending);
            sync.process(raw, AM, AM, t, W, true);
        }
        (edges.open_us, edges.gap_us())
    }

    #[test]
    fn lone_press_has_no_gap() {
        let (open, gap) = run(&[(bit::A, 1_000)]);
        assert_eq!(open, 1_000);
        assert_eq!(gap, 0);
    }

    #[test]
    fn chord_gap_includes_the_press_that_closes_the_group() {
        // The regression: the second press both completes the chord AND makes
        // SyncWindow commit + clear `pending` in the same call. Measuring off the
        // window's state afterwards reported 0 ms for every chord.
        let (open, gap) = run(&[(bit::A, 1_000), (bit::A | bit::B, 4_000)]);
        assert_eq!(open, 1_000);
        assert_eq!(gap, 3_000, "chord gap must be first press -> closing press");
    }

    #[test]
    fn three_button_chord_measures_to_the_last_press() {
        // The window commits at the second press (2+ attacks held), so the third
        // opens a fresh group rather than extending the first.
        let (open, gap) = run(&[
            (bit::A, 1_000),
            (bit::A | bit::B, 3_000),
            (bit::A | bit::B | bit::X, 3_500),
        ]);
        assert_eq!(open, 3_500, "third press starts a new group");
        assert_eq!(gap, 0);
    }

    #[test]
    fn simultaneous_press_is_a_zero_gap() {
        let (open, gap) = run(&[(bit::A | bit::B, 2_000)]);
        assert_eq!(open, 2_000);
        assert_eq!(gap, 0);
    }

    #[test]
    fn releases_and_unsynced_bits_do_not_open_a_group() {
        let mut edges = EdgeTracker::default();
        // A d-pad press is outside the synced mask, so it must not start a group.
        edges.observe(bit::DPAD_LEFT, AM, 500, false);
        assert_eq!(edges.open_us, 0);
        // A real attack press does.
        edges.observe(bit::DPAD_LEFT | bit::A, AM, 900, false);
        assert_eq!(edges.open_us, 900);
        // Releasing it is not a new edge.
        edges.observe(bit::DPAD_LEFT, AM, 1_500, true);
        assert_eq!(edges.last_press_us, 900);
    }
}
