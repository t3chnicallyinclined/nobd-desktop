//! NOBD Bulk source — the Extreme Low Latency path, driven straight off usbfs.
//!
//! The stick's NOBD firmware (VID `CAFE` / PID `4030`) streams 20-byte payloads
//! over a vendor bulk IN endpoint at wire rate. Reading that instead of the
//! stick's own HID reports removes the report-interval quantisation entirely:
//! the hop is the URB completion (~90 µs), not a 1 ms poll slot.
//!
//! Two deliberate choices here, both about removing hops:
//!
//! **usbfs directly, not libusb/nusb.** Every USB wrapper puts an event thread
//! and a channel between the URB completion and your code. usbfs fds are
//! pollable — the kernel raises `POLLOUT` when a URB is ready to reap — so
//! submitting and reaping by hand lets the bulk stream land in *the same*
//! `epoll_wait` as the timer and the evdev fd. One thread, one wakeup, no
//! handoff. On Windows this path needs Zadig/WinUSB binding; here it needs one
//! udev rule.
//!
//! **The firmware's own timestamp is the authority.** Each payload carries
//! `edge_us`, the MCU's microsecond clock at the button edge — upstream of USB
//! scheduling, upstream of the host controller, upstream of us. `ClockBridge`
//! translates it into `CLOCK_MONOTONIC` by tracking the *minimum* observed
//! offset, since the least-delayed sample is the best estimate of the true
//! offset. The sync window is then timed from the edge itself. Nothing else in
//! the chain can do better, because nothing else sees the edge.

use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;

use crate::pad::PadState;
use crate::uapi::{self, now_us};

pub const NOBD_BULK_VID: u16 = 0xCAFE;
pub const NOBD_BULK_PID: u16 = 0x4030;

/// Firmware payload: seq | edge_us | buttons | lt | rt | lx | ly | rx | ry.
const PAYLOAD: usize = 20;
/// URB buffer. A high-speed bulk max packet is 512; one URB can therefore carry
/// a burst of payloads if the device coalesces them.
const BUF_LEN: usize = 512;
/// URBs kept in flight. Enough that the endpoint is never idle waiting for us to
/// resubmit, few enough that a stale one is reaped promptly.
const QUEUE_DEPTH: usize = 8;

fn ioerr(ctx: &str) -> io::Error {
    io::Error::other(format!("{ctx}: {}", io::Error::last_os_error()))
}

// ---------------------------------------------------------------------------
// Clock bridge: firmware µs -> CLOCK_MONOTONIC µs
// ---------------------------------------------------------------------------

/// Tracks `monotonic_us - edge_us`. Delivery delay is always **positive**, so
/// the smallest offset seen is the least-contaminated estimate of the true
/// clock offset. We decay the minimum slowly so MCU/host crystal drift is
/// followed instead of locking to one lucky early sample forever.
pub struct ClockBridge {
    min_offset: i64,
    have: bool,
    /// When the current minimum was taken, so it can age out.
    anchored_us: u64,
    /// Re-anchor this often. 2 s at ~100 ppm relative drift is ~200 µs of
    /// accumulated error — well under the millisecond that matters here.
    reanchor_us: u64,
    pending_min: i64,
    pending_have: bool,
}

impl ClockBridge {
    pub fn new() -> Self {
        Self {
            min_offset: 0,
            have: false,
            anchored_us: 0,
            reanchor_us: 2_000_000,
            pending_min: 0,
            pending_have: false,
        }
    }

    /// Feed one observation and get the edge translated into our clock.
    pub fn translate(&mut self, edge_us: u32, arrival_us: u64) -> u64 {
        let obs = arrival_us as i64 - edge_us as i64;

        if !self.have {
            self.min_offset = obs;
            self.have = true;
            self.anchored_us = arrival_us;
        } else if obs < self.min_offset {
            self.min_offset = obs;
            self.anchored_us = arrival_us;
        }

        // Track a rolling candidate so a re-anchor has something to adopt.
        if !self.pending_have || obs < self.pending_min {
            self.pending_min = obs;
            self.pending_have = true;
        }
        if arrival_us.saturating_sub(self.anchored_us) > self.reanchor_us && self.pending_have {
            self.min_offset = self.pending_min;
            self.anchored_us = arrival_us;
            self.pending_have = false;
        }

        let t = edge_us as i64 + self.min_offset;
        // Never hand the window a timestamp from the future — a bad translation
        // must degrade to "now", not open a window that closes before it starts.
        (t.max(0) as u64).min(arrival_us)
    }

    /// Current estimate of stick→host delivery delay, in µs. This is the honest
    /// number to quote for the bulk hop.
    pub fn delivery_delay_us(&self, edge_us: u32, arrival_us: u64) -> i64 {
        if !self.have {
            return 0;
        }
        (arrival_us as i64 - edge_us as i64) - self.min_offset
    }
}

// ---------------------------------------------------------------------------
// Device discovery
// ---------------------------------------------------------------------------

fn read_hex_sysfs(path: &str) -> Option<u16> {
    let s = std::fs::read_to_string(path).ok()?;
    u16::from_str_radix(s.trim(), 16).ok()
}

fn read_u32_sysfs(path: &str) -> Option<u32> {
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse().ok()
}

/// Locate the NOBD bulk device's usbfs node by walking `/sys/bus/usb/devices`.
fn find_device(vid: u16, pid: u16) -> Option<String> {
    for e in std::fs::read_dir("/sys/bus/usb/devices").ok()? {
        let Ok(e) = e else { continue };
        let dir = e.path();
        let d = dir.to_string_lossy();
        // Interface entries contain ':' — we want the device entries only.
        if d.rsplit('/').next().map(|n| n.contains(':')).unwrap_or(true) {
            continue;
        }
        if read_hex_sysfs(&format!("{d}/idVendor")) != Some(vid) {
            continue;
        }
        if read_hex_sysfs(&format!("{d}/idProduct")) != Some(pid) {
            continue;
        }
        let bus = read_u32_sysfs(&format!("{d}/busnum"))?;
        let dev = read_u32_sysfs(&format!("{d}/devnum"))?;
        return Some(format!("/dev/bus/usb/{bus:03}/{dev:03}"));
    }
    None
}

/// Walk the config descriptors the usbfs fd exposes and return the first
/// (interface, bulk IN endpoint) pair. Reading the fd from offset 0 yields the
/// device descriptor followed by every config descriptor — the same source
/// libusb uses, without linking libusb.
fn find_bulk_in(fd: RawFd) -> Option<(u32, u8)> {
    let mut buf = [0u8; 4096];
    let n = unsafe { libc::pread(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
    if n <= 0 {
        return None;
    }
    let buf = &buf[..n as usize];

    let mut i = 0usize;
    let mut cur_iface: Option<u32> = None;
    while i + 2 <= buf.len() {
        let len = buf[i] as usize;
        let ty = buf[i + 1];
        if len < 2 || i + len > buf.len() {
            break;
        }
        match ty {
            0x04 if len >= 9 => cur_iface = Some(buf[i + 2] as u32), // INTERFACE
            0x05 if len >= 7 => {
                // ENDPOINT: bEndpointAddress, bmAttributes
                let addr = buf[i + 2];
                let attrs = buf[i + 3];
                let is_in = addr & 0x80 != 0;
                let is_bulk = attrs & 0x03 == 0x02;
                if is_in && is_bulk {
                    if let Some(iface) = cur_iface {
                        return Some((iface, addr));
                    }
                }
            }
            _ => {}
        }
        i += len;
    }
    None
}

// ---------------------------------------------------------------------------
// URB ring
// ---------------------------------------------------------------------------

#[repr(C)]
struct UrbSlot {
    urb: uapi::UsbdevfsUrb,
    buf: [u8; BUF_LEN],
}

pub struct BulkSource {
    fd: RawFd,
    interface: u32,
    endpoint: u8,
    /// Boxed so the kernel-visible pointers never move.
    slots: Vec<*mut UrbSlot>,
    state: PadState,
    clock: ClockBridge,
    last_seq: u32,
    have_seq: bool,
    pub drops: u64,
    pub payloads: u64,
    /// Rolling rate estimate (payloads/sec) — the stick→app stream freshness.
    rate_hz: u32,
    rate_count: u32,
    rate_t0: u64,
    /// Last measured stick→host delivery delay, µs.
    pub last_delay_us: i64,
}

impl BulkSource {
    /// Try to attach. `Ok(None)` means "the NOBD bulk device isn't present",
    /// which is the normal case for a stick that isn't in bulk mode — the caller
    /// falls back to evdev rather than treating it as an error.
    pub fn open() -> io::Result<Option<Self>> {
        let Some(node) = find_device(NOBD_BULK_VID, NOBD_BULK_PID) else {
            return Ok(None);
        };
        let c = CString::new(node.clone()).unwrap();
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "found NOBD bulk device at {node} but cannot open it ({}). \
                     Install packaging/83-nobd.rules.",
                    io::Error::last_os_error()
                ),
            ));
        }
        let Some((interface, endpoint)) = find_bulk_in(fd) else {
            unsafe { libc::close(fd) };
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "NOBD bulk device has no bulk IN endpoint — is the firmware in NOBD Bulk mode?",
            ));
        };

        // Take the interface, evicting any kernel driver bound to it.
        let mut dc = uapi::UsbdevfsDisconnectClaim {
            interface,
            flags: uapi::USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER,
            driver: [0u8; 256],
        };
        let claimed = unsafe { uapi::ioctl_ptr(fd, uapi::USBDEVFS_DISCONNECT_CLAIM, &mut dc) }.is_ok()
            || {
                let mut n = interface;
                unsafe { uapi::ioctl_ptr(fd, uapi::USBDEVFS_CLAIMINTERFACE, &mut n) }.is_ok()
            };
        if !claimed {
            let e = ioerr("claim NOBD bulk interface");
            unsafe { libc::close(fd) };
            return Err(e);
        }

        let mut me = Self {
            fd,
            interface,
            endpoint,
            slots: Vec::with_capacity(QUEUE_DEPTH),
            state: PadState::default(),
            clock: ClockBridge::new(),
            last_seq: 0,
            have_seq: false,
            drops: 0,
            payloads: 0,
            rate_hz: 0,
            rate_count: 0,
            rate_t0: now_us(),
            last_delay_us: 0,
        };
        me.fill_queue()?;
        Ok(Some(me))
    }

    /// Allocate and submit the whole URB ring. From here the endpoint always has
    /// transfers outstanding, so a completion is never waiting on our resubmit.
    fn fill_queue(&mut self) -> io::Result<()> {
        for _ in 0..QUEUE_DEPTH {
            let slot = Box::into_raw(Box::new(UrbSlot {
                urb: uapi::UsbdevfsUrb {
                    ty: uapi::USBDEVFS_URB_TYPE_BULK,
                    endpoint: self.endpoint,
                    status: 0,
                    flags: 0,
                    buffer: std::ptr::null_mut(),
                    buffer_length: BUF_LEN as i32,
                    actual_length: 0,
                    start_frame: 0,
                    number_of_packets: 0,
                    error_count: 0,
                    signr: 0,
                    usercontext: std::ptr::null_mut(),
                },
                buf: [0u8; BUF_LEN],
            }));
            unsafe {
                (*slot).urb.buffer = (*slot).buf.as_mut_ptr() as *mut libc::c_void;
                (*slot).urb.usercontext = slot as *mut libc::c_void;
            }
            self.slots.push(slot);
            self.submit(slot)?;
        }
        Ok(())
    }

    fn submit(&self, slot: *mut UrbSlot) -> io::Result<()> {
        unsafe {
            (*slot).urb.status = 0;
            (*slot).urb.actual_length = 0;
            uapi::ioctl_ptr(self.fd, uapi::USBDEVFS_SUBMITURB, &mut (*slot).urb)?;
        }
        Ok(())
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }
    pub fn state(&self) -> PadState {
        self.state
    }
    pub fn rate_hz(&self) -> u32 {
        self.rate_hz
    }

    /// Reap completed URBs, decode their payloads, resubmit them. Calls
    /// `on_packet(state, edge_time_us)` once per payload, with the timestamp
    /// translated from the firmware's own clock.
    ///
    /// Stops once `budget` payloads have been emitted, checked before reaping
    /// the next URB so a URB is never half-decoded. Unreaped completions keep
    /// usbfs signalling, so the loop re-enters and continues — nothing is lost.
    ///
    /// Errors here are attach-level (device gone); the caller drops the source
    /// and re-scans rather than trying to recover in place.
    pub fn drain<F: FnMut(PadState, u64)>(
        &mut self,
        budget: usize,
        mut on_packet: F,
    ) -> io::Result<usize> {
        let mut emitted = 0usize;
        // One URB carries at most BUF_LEN/PAYLOAD payloads; leave that much head
        // room so a reaped URB is always decoded in full.
        let per_urb = BUF_LEN / PAYLOAD;
        loop {
            if emitted + per_urb > budget {
                return Ok(emitted);
            }
            let mut urb_ptr: *mut uapi::UsbdevfsUrb = std::ptr::null_mut();
            let r = unsafe {
                libc::ioctl(self.fd, uapi::USBDEVFS_REAPURBNDELAY as _, &mut urb_ptr)
            };
            if r < 0 {
                let e = io::Error::last_os_error();
                return match e.raw_os_error() {
                    // EAGAIN == EWOULDBLOCK on Linux: no more completions queued.
                    Some(libc::EAGAIN) => Ok(emitted),
                    Some(libc::EINTR) => continue,
                    _ => Err(e), // ENODEV: unplugged
                };
            }
            if urb_ptr.is_null() {
                return Ok(emitted);
            }
            let slot = unsafe { (*urb_ptr).usercontext as *mut UrbSlot };
            if slot.is_null() {
                return Ok(emitted);
            }

            let arrival = now_us();
            let (status, len) = unsafe { ((*slot).urb.status, (*slot).urb.actual_length) };
            if status == 0 && len > 0 {
                let n = (len as usize).min(BUF_LEN);
                let mut off = 0usize;
                let mut b = [0u8; PAYLOAD];
                while off + PAYLOAD <= n {
                    // Copy the payload out rather than taking a reference into
                    // the URB buffer: the kernel owns that memory the moment we
                    // resubmit, and a live borrow across the resubmit would be
                    // exactly the aliasing bug this avoids.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            (*slot).buf.as_ptr().add(off),
                            b.as_mut_ptr(),
                            PAYLOAD,
                        )
                    };
                    let seq = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                    let edge = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
                    let buttons = u16::from_le_bytes([b[8], b[9]]);
                    let lt = b[10];
                    let rt = b[11];
                    let lx = i16::from_le_bytes([b[12], b[13]]);
                    let ly = i16::from_le_bytes([b[14], b[15]]);
                    let rx = i16::from_le_bytes([b[16], b[17]]);
                    let ry = i16::from_le_bytes([b[18], b[19]]);

                    if self.have_seq && seq > self.last_seq.wrapping_add(1) {
                        self.drops += (seq - self.last_seq - 1) as u64;
                    }
                    self.last_seq = seq;
                    self.have_seq = true;
                    self.payloads += 1;
                    self.rate_count += 1;

                    self.last_delay_us = self.clock.delivery_delay_us(edge, arrival);
                    let t = self.clock.translate(edge, arrival);
                    self.state = PadState { buttons, lt, rt, lx, ly, rx, ry };
                    on_packet(self.state, t);
                    emitted += 1;
                    off += PAYLOAD;
                }
            }

            // Resubmit immediately — the ring must not shrink.
            let _ = self.submit(slot);

            let dt = arrival.saturating_sub(self.rate_t0);
            if dt >= 200_000 {
                self.rate_hz = ((self.rate_count as u64 * 1_000_000) / dt) as u32;
                self.rate_count = 0;
                self.rate_t0 = arrival;
            }
        }
    }
}

impl Drop for BulkSource {
    fn drop(&mut self) {
        for &slot in &self.slots {
            unsafe {
                let _ = libc::ioctl(self.fd, uapi::USBDEVFS_DISCARDURB as _, &mut (*slot).urb);
            }
        }
        // Reap what we discarded so the kernel isn't left holding our buffers,
        // then free them.
        for _ in 0..self.slots.len() {
            let mut p: *mut uapi::UsbdevfsUrb = std::ptr::null_mut();
            unsafe { libc::ioctl(self.fd, uapi::USBDEVFS_REAPURBNDELAY as _, &mut p) };
        }
        for &slot in &self.slots {
            unsafe { drop(Box::from_raw(slot)) };
        }
        let mut n = self.interface;
        unsafe {
            let _ = uapi::ioctl_ptr(self.fd, uapi::USBDEVFS_RELEASEINTERFACE, &mut n);
            libc::close(self.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_bridge_tracks_minimum_offset() {
        let mut cb = ClockBridge::new();
        // Firmware edge at 1000 µs delivered 300 µs late, then 90 µs late.
        assert_eq!(cb.translate(1_000, 1_300), 1_300); // first sample defines the offset
        // A less-delayed sample pulls the offset down, so the same edge now
        // translates earlier — closer to the truth.
        let t = cb.translate(2_000, 2_090);
        assert_eq!(t, 2_090);
        // A *later* delivery of an earlier edge must not drag the estimate up.
        let t2 = cb.translate(3_000, 3_500);
        assert_eq!(t2, 3_090, "should use the best (90 µs) offset, not 500 µs");
    }

    #[test]
    fn clock_bridge_never_returns_future_timestamps() {
        let mut cb = ClockBridge::new();
        cb.translate(1_000, 1_090);
        // An edge claiming to be newer than arrival clamps to arrival.
        assert_eq!(cb.translate(9_999, 5_000), 5_000);
    }
}
