//! evdev input source — the universal path (any stick the kernel knows).
//!
//! Three things make this materially better than the Windows XInput poll it
//! replaces:
//!
//!   1. **No polling.** The fd is epoll'd; we wake on the report, not on a timer.
//!      The Windows loop sleeps 1 ms and re-reads, so a press waits up to a full
//!      tick before it is even seen. Here there is no such quantisation.
//!   2. **Kernel timestamps.** `EVIOCSCLOCKID(CLOCK_MONOTONIC)` makes every
//!      `input_event.time` the instant the kernel processed the USB report. The
//!      sync window is timed from *that*, so our own scheduling jitter cannot
//!      widen or narrow the window. On Windows the timestamp is whenever the
//!      poll thread happened to run.
//!   3. **Exclusive capture.** `EVIOCGRAB` is the HidHide equivalent for evdev,
//!      with no driver, no reboot and no vendored installer.
//!
//! A caveat worth stating plainly: `EVIOCGRAB` does not hide the device's
//! *hidraw* node, and Steam reads recognised pads through hidraw. The udev rule
//! in `packaging/` closes that half; see `docs/LINUX.md`.

use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;

use crate::pad::{bit, PadState};
use crate::uapi::{self, abs, btn, ev, InputEvent, InputId, SYN_REPORT};

/// A candidate input device found by scanning `/dev/input`.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub path: String,
    pub name: String,
    pub id: PadId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PadId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

/// Deadzone below which a stick axis reads as centred. Matches XInput's default
/// left-stick deadzone so behaviour is identical across platforms.
const STICK_DEADZONE: i32 = 7849;

fn cstr(path: &str) -> io::Result<CString> {
    CString::new(path).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has NUL"))
}

fn ioerr(ctx: &str) -> io::Error {
    io::Error::other(format!("{ctx}: {}", io::Error::last_os_error()))
}

/// Read a device's `EVIOCGID`.
fn read_id(fd: RawFd) -> io::Result<PadId> {
    let mut id = InputId::default();
    unsafe { uapi::ioctl_ptr(fd, uapi::EVIOCGID, &mut id)? };
    Ok(PadId { bustype: id.bustype, vendor: id.vendor, product: id.product, version: id.version })
}

/// Read a string ioctl (`EVIOCGNAME` / `EVIOCGPHYS`) into an owned `String`.
/// Returns an empty string when the device does not set that property, which
/// for `phys` is common and not an error.
fn read_str(fd: RawFd, req: u64) -> String {
    let mut buf = [0u8; 256];
    let n = unsafe { libc::ioctl(fd, req as _, buf.as_mut_ptr()) };
    if n <= 0 {
        return String::new();
    }
    let n = (n as usize).min(buf.len());
    let s = buf[..n].split(|&c| c == 0).next().unwrap_or(&[]);
    String::from_utf8_lossy(s).into_owned()
}

/// Does this device have the `EV_KEY` bits that make it a gamepad? We require
/// `BTN_GAMEPAD` (0x130), which is exactly the kernel's own "this is a pad"
/// convention and is what `ID_INPUT_JOYSTICK` keys off.
fn is_gamepad(fd: RawFd) -> bool {
    const NBITS: usize = 0x300usize.div_ceil(8);
    let mut bits = [0u8; NBITS];
    let n = unsafe {
        libc::ioctl(fd, uapi::eviocgbit(ev::KEY as u32, NBITS as u32) as _, bits.as_mut_ptr())
    };
    if n < 0 {
        return false;
    }
    let code = btn::GAMEPAD as usize;
    let idx = code / 8;
    idx < bits.len() && (bits[idx] >> (code % 8)) & 1 == 1
}

/// Enumerate `/dev/input/event*` and return everything that looks like a pad.
/// Our own virtual pad is excluded by vendor/product so the daemon can never
/// grab its own output — the Linux answer to the Windows "marker fingerprint"
/// dance in `sync_service::run_xinput`, and it is exact rather than heuristic.
pub fn list_gamepads() -> io::Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    let mut paths: Vec<String> = std::fs::read_dir("/dev/input")?
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_string_lossy().into_owned())
        .filter(|p| {
            p.rsplit('/')
                .next()
                .map(|f| f.starts_with("event"))
                .unwrap_or(false)
        })
        .collect();
    // Numeric order, so "event2" sorts before "event10" and the picked default
    // is stable across reboots.
    paths.sort_by_key(|p| {
        p.rsplit("event")
            .next()
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });

    for path in paths {
        let c = match cstr(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 {
            continue; // no permission / vanished — not our problem here
        }
        let info = (|| -> io::Result<Option<DeviceInfo>> {
            if !is_gamepad(fd) {
                return Ok(None);
            }
            // Exclude our own virtual pad by `phys`, not by vendor/product: the
            // pad deliberately masquerades as an Xbox 360 controller, so a
            // VID/PID test would also exclude every real Xbox pad — and grabbing
            // our own output would feed the sync window its own results.
            if read_str(fd, uapi::eviocgphys(256)).starts_with(crate::uinput::NOBD_PHYS_PREFIX) {
                return Ok(None);
            }
            let id = read_id(fd)?;
            Ok(Some(DeviceInfo { path: path.clone(), name: read_str(fd, uapi::eviocgname(256)), id }))
        })();
        unsafe { libc::close(fd) };
        if let Ok(Some(d)) = info {
            out.push(d);
        }
    }
    Ok(out)
}

/// How a stick reports its d-pad. Detected at open, because it changes how we
/// decode: hat axes are one axis per direction pair, buttons are one bit each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DpadKind {
    Hat,
    Buttons,
    None,
}

/// An opened, grabbed evdev device that decodes into `PadState`.
pub struct EvdevSource {
    fd: RawFd,
    pub info: DeviceInfo,
    state: PadState,
    dpad: DpadKind,
    /// Absolute-axis ranges, so we can normalise any stick to XInput's i16.
    abs_info: [Option<uapi::InputAbsinfo>; abs::CNT as usize],
    /// Set when this device reports triggers as axes (ABS_Z/ABS_RZ).
    has_axis_triggers: bool,
    grabbed: bool,
    /// Kernel timestamp of the most recent event in the packet being assembled.
    pending_time_us: u64,
    pending_dirty: bool,
    /// Read buffer sized so a burst of events is drained in one syscall. 64
    /// events covers any realistic single report with room to spare.
    buf: [u8; InputEvent::SIZE * 64],
}

impl EvdevSource {
    /// Open, switch to `CLOCK_MONOTONIC` timestamps, learn the axis layout, and
    /// (optionally) take exclusive ownership.
    pub fn open(info: DeviceInfo, grab: bool) -> io::Result<Self> {
        let c = cstr(&info.path)?;
        // O_RDONLY is enough: we never write to the physical device. NONBLOCK so
        // the drain loop can read until EAGAIN without ever stalling the engine.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(ioerr(&format!("open {}", info.path)));
        }

        // The whole reason this daemon can be honest about latency: from here on
        // every event carries the kernel's own monotonic timestamp.
        if uapi::ioctl_int(fd, uapi::EVIOCSCLOCKID, libc::CLOCK_MONOTONIC).is_err() {
            unsafe { libc::close(fd) };
            return Err(ioerr("EVIOCSCLOCKID(CLOCK_MONOTONIC)"));
        }

        let mut s = Self {
            fd,
            info,
            state: PadState::default(),
            dpad: DpadKind::None,
            abs_info: [None; abs::CNT as usize],
            has_axis_triggers: false,
            grabbed: false,
            pending_time_us: 0,
            pending_dirty: false,
            buf: [0u8; InputEvent::SIZE * 64],
        };
        s.probe_axes();

        if grab {
            // A failed grab is not fatal — it just means something else already
            // owns the device (usually a stale daemon). Report it; the caller
            // decides whether to continue with double input or bail.
            match uapi::ioctl_int(fd, uapi::EVIOCGRAB, 1) {
                Ok(_) => s.grabbed = true,
                Err(e) => {
                    unsafe { libc::close(fd) };
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!("EVIOCGRAB failed on {} ({e}) — another process holds it", s.info.path),
                    ));
                }
            }
        }
        Ok(s)
    }

    fn probe_axes(&mut self) {
        for code in 0..abs::CNT {
            let mut ai = uapi::InputAbsinfo::default();
            let r = unsafe {
                libc::ioctl(self.fd, uapi::eviocgabs(code as u32) as _, &mut ai as *mut _)
            };
            if r >= 0 && (ai.minimum != 0 || ai.maximum != 0) {
                self.abs_info[code as usize] = Some(ai);
            }
        }
        self.has_axis_triggers =
            self.abs_info[abs::Z as usize].is_some() && self.abs_info[abs::RZ as usize].is_some();
        self.dpad = if self.abs_info[abs::HAT0X as usize].is_some()
            || self.abs_info[abs::HAT0Y as usize].is_some()
        {
            DpadKind::Hat
        } else {
            // Assume digital d-pad buttons; if the stick has neither, the decode
            // simply never sets those bits.
            DpadKind::Buttons
        };
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    pub fn state(&self) -> PadState {
        self.state
    }

    pub fn is_grabbed(&self) -> bool {
        self.grabbed
    }

    /// Normalise an absolute axis to XInput's full i16 range.
    fn norm_axis(&self, code: u16, v: i32, invert: bool) -> i16 {
        let Some(ai) = self.abs_info[code as usize] else {
            return 0;
        };
        let (lo, hi) = (ai.minimum as f32, ai.maximum as f32);
        if hi <= lo {
            return 0;
        }
        let t = ((v as f32 - lo) / (hi - lo)).clamp(0.0, 1.0);
        let t = if invert { 1.0 - t } else { t };
        // Map 0..1 to -32768..32767.
        let scaled = (t * 65535.0 - 32768.0).round();
        let out = scaled.clamp(-32768.0, 32767.0) as i32;
        if out.unsigned_abs() as i32 <= STICK_DEADZONE {
            0
        } else {
            out as i16
        }
    }

    fn norm_trigger(&self, code: u16, v: i32) -> u8 {
        let Some(ai) = self.abs_info[code as usize] else {
            return 0;
        };
        let (lo, hi) = (ai.minimum as f32, ai.maximum as f32);
        if hi <= lo {
            return 0;
        }
        (((v as f32 - lo) / (hi - lo)).clamp(0.0, 1.0) * 255.0).round() as u8
    }

    /// Apply one event. Returns true if it changed the pad state.
    fn apply(&mut self, e: &InputEvent) -> bool {
        let before = self.state;
        match e.ty {
            ev::KEY => {
                let down = e.value != 0;
                let b = match e.code {
                    btn::SOUTH => bit::A,
                    btn::EAST => bit::B,
                    btn::NORTH => bit::X,
                    btn::WEST => bit::Y,
                    btn::TL => bit::LEFT_SHOULDER,
                    btn::TR => bit::RIGHT_SHOULDER,
                    btn::SELECT => bit::BACK,
                    btn::START => bit::START,
                    btn::MODE => bit::GUIDE,
                    btn::THUMBL => bit::LEFT_THUMB,
                    btn::THUMBR => bit::RIGHT_THUMB,
                    btn::DPAD_UP => bit::DPAD_UP,
                    btn::DPAD_DOWN => bit::DPAD_DOWN,
                    btn::DPAD_LEFT => bit::DPAD_LEFT,
                    btn::DPAD_RIGHT => bit::DPAD_RIGHT,
                    // Sticks without axis triggers report TL2/TR2 as buttons.
                    btn::TL2 if !self.has_axis_triggers => {
                        self.state.lt = if down { 255 } else { 0 };
                        return self.state != before;
                    }
                    btn::TR2 if !self.has_axis_triggers => {
                        self.state.rt = if down { 255 } else { 0 };
                        return self.state != before;
                    }
                    _ => return false,
                };
                if down {
                    self.state.buttons |= b;
                } else {
                    self.state.buttons &= !b;
                }
            }
            ev::ABS => match e.code {
                abs::X => self.state.lx = self.norm_axis(abs::X, e.value, false),
                // evdev Y grows downward; XInput grows upward.
                abs::Y => self.state.ly = self.norm_axis(abs::Y, e.value, true),
                abs::RX => self.state.rx = self.norm_axis(abs::RX, e.value, false),
                abs::RY => self.state.ry = self.norm_axis(abs::RY, e.value, true),
                abs::Z if self.has_axis_triggers => {
                    self.state.lt = self.norm_trigger(abs::Z, e.value)
                }
                abs::RZ if self.has_axis_triggers => {
                    self.state.rt = self.norm_trigger(abs::RZ, e.value)
                }
                abs::HAT0X => {
                    self.state.buttons &= !(bit::DPAD_LEFT | bit::DPAD_RIGHT);
                    if e.value < 0 {
                        self.state.buttons |= bit::DPAD_LEFT;
                    } else if e.value > 0 {
                        self.state.buttons |= bit::DPAD_RIGHT;
                    }
                }
                abs::HAT0Y => {
                    self.state.buttons &= !(bit::DPAD_UP | bit::DPAD_DOWN);
                    if e.value < 0 {
                        self.state.buttons |= bit::DPAD_UP;
                    } else if e.value > 0 {
                        self.state.buttons |= bit::DPAD_DOWN;
                    }
                }
                _ => return false,
            },
            _ => return false,
        }
        self.state != before
    }

    /// Drain pending events. Calls `on_packet(state, kernel_time_us)` once per
    /// `SYN_REPORT` that actually changed something — i.e. once per physical
    /// report, with the kernel's timestamp for that report.
    ///
    /// Draining before handing control back matters: if two buttons arrive in
    /// the same report (or two reports land while we were descheduled) they must
    /// be presented to the window with their own timestamps, in order, not
    /// collapsed into one "now".
    ///
    /// Stops once `budget` packets have been emitted, **without** consuming more
    /// from the fd — so nothing is ever silently dropped. The epoll registration
    /// is level-triggered, so the loop re-enters here immediately and picks up
    /// exactly where it left off. Returns the number emitted.
    pub fn drain<F: FnMut(PadState, u64)>(
        &mut self,
        budget: usize,
        mut on_packet: F,
    ) -> io::Result<usize> {
        let mut emitted = 0usize;
        loop {
            // A single `read` can yield at most `buf.len() / 2` packets (each
            // needs one non-SYN event plus its SYN), so leaving that much head
            // room guarantees the budget is never exceeded mid-read.
            if emitted + (self.buf.len() / InputEvent::SIZE) / 2 > budget {
                return Ok(emitted);
            }
            let n = unsafe {
                libc::read(self.fd, self.buf.as_mut_ptr() as *mut libc::c_void, self.buf.len())
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                return match err.raw_os_error() {
                    // EAGAIN == EWOULDBLOCK on Linux: the queue is drained.
                    Some(libc::EAGAIN) => Ok(emitted),
                    Some(libc::EINTR) => continue,
                    // ENODEV means unplugged — the engine handles re-adoption.
                    _ => Err(err),
                };
            }
            if n == 0 {
                return Ok(emitted);
            }
            let count = n as usize / InputEvent::SIZE;
            for i in 0..count {
                // SAFETY: the buffer is `InputEvent`-sized and the kernel writes
                // whole events; `i` is bounded by the byte count it returned.
                let e: InputEvent = unsafe {
                    std::ptr::read_unaligned(
                        self.buf.as_ptr().add(i * InputEvent::SIZE) as *const InputEvent
                    )
                };
                if e.ty == ev::SYN && e.code == SYN_REPORT {
                    if self.pending_dirty {
                        self.pending_dirty = false;
                        on_packet(self.state, self.pending_time_us);
                        emitted += 1;
                    }
                    continue;
                }
                // Record the timestamp before applying, so a packet whose only
                // meaningful event is the last one still carries the right time.
                self.pending_time_us = e.time_us();
                if self.apply(&e) {
                    self.pending_dirty = true;
                }
            }
            // A short read means the queue is drained; skip the extra syscall.
            if (n as usize) < self.buf.len() {
                return Ok(emitted);
            }
        }
    }
}

impl Drop for EvdevSource {
    fn drop(&mut self) {
        if self.grabbed {
            let _ = uapi::ioctl_int(self.fd, uapi::EVIOCGRAB, 0);
        }
        unsafe { libc::close(self.fd) };
    }
}
