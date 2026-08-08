//! Raw kernel UAPI: ioctl numbers, input-event codes, and the structs we pass
//! across the syscall boundary. Hand-written rather than pulled from a wrapper
//! crate for three reasons that all serve latency:
//!
//!   1. No wrapper allocates or copies on our behalf in the hot loop.
//!   2. We need interfaces the ergonomic crates hide — `EVIOCSCLOCKID` (so the
//!      sync window is timed from the *kernel's* event timestamp, not from when
//!      userspace woke up) and usbfs URB submit/reap (so the bulk stream lands
//!      in the same epoll as everything else, with no thread hop).
//!   3. Everything here is stable kernel UAPI; it does not drift.
//!
//! `_IOC` numbers are computed with the same const arithmetic as the C macros
//! rather than pasted as magic constants, so they are auditable against
//! `include/uapi/asm-generic/ioctl.h`.

#![allow(dead_code)]

// ---------------------------------------------------------------------------
// _IOC macro family (asm-generic; correct for x86_64/aarch64 — the two targets
// Bazzite ships. The mips/parisc/sparc/alpha layouts differ and are not used.)
// ---------------------------------------------------------------------------

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;

const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;

const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u64 {
    ((dir << IOC_DIRSHIFT) | (ty << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | (size << IOC_SIZESHIFT))
        as u64
}
const fn io(ty: u32, nr: u32) -> u64 {
    ioc(IOC_NONE, ty, nr, 0)
}
const fn ior(ty: u32, nr: u32, size: u32) -> u64 {
    ioc(IOC_READ, ty, nr, size)
}
const fn iow(ty: u32, nr: u32, size: u32) -> u64 {
    ioc(IOC_WRITE, ty, nr, size)
}

// ---------------------------------------------------------------------------
// evdev — linux/input.h  (type 'E')
// ---------------------------------------------------------------------------

const E: u32 = b'E' as u32;

/// `EVIOCGRAB` — exclusive access. While held, no other evdev reader sees this
/// device. This is the HidHide equivalent for the evdev half of the problem
/// (the hidraw half is handled by a udev rule; see packaging/).
pub const EVIOCGRAB: u64 = iow(E, 0x90, 4);

/// `EVIOCSCLOCKID` — switch event timestamps to `CLOCK_MONOTONIC`.
///
/// This is the single most valuable ioctl in the daemon. By default evdev
/// stamps events with `CLOCK_REALTIME`, which is not usable as a monotonic
/// clock. With it set to `CLOCK_MONOTONIC`, every `input_event.time` is the
/// instant the *kernel* processed the report — so the sync window is timed from
/// the hardware edge, not from our wakeup. Scheduling jitter on our side stops
/// contaminating the window entirely.
pub const EVIOCSCLOCKID: u64 = iow(E, 0xa0, 4);

pub const fn eviocgname(len: u32) -> u64 {
    ioc(IOC_READ, E, 0x06, len)
}
pub const fn eviocgphys(len: u32) -> u64 {
    ioc(IOC_READ, E, 0x07, len)
}
pub const fn eviocguniq(len: u32) -> u64 {
    ioc(IOC_READ, E, 0x08, len)
}
/// `EVIOCGID` — struct input_id { bustype, vendor, product, version } (8 bytes).
pub const EVIOCGID: u64 = ior(E, 0x02, 8);
/// `EVIOCGBIT(ev, len)` — capability bitmap for event type `ev` (0 = the type map).
pub const fn eviocgbit(ev: u32, len: u32) -> u64 {
    ioc(IOC_READ, E, 0x20 + ev, len)
}
/// `EVIOCGABS(abs)` — struct input_absinfo (24 bytes on 64-bit: 6 × i32... the
/// kernel struct is 6 `__s32` = 24 bytes).
pub const fn eviocgabs(abs: u32) -> u64 {
    ior(E, 0x40 + abs, 24)
}

// ---------------------------------------------------------------------------
// uinput — linux/uinput.h  (type 'U')
// ---------------------------------------------------------------------------

const U: u32 = b'U' as u32;

pub const UI_DEV_CREATE: u64 = io(U, 1);
pub const UI_DEV_DESTROY: u64 = io(U, 2);
/// `UI_DEV_SETUP` — struct uinput_setup (input_id + 80-byte name + ff_effects_max).
pub const UI_DEV_SETUP: u64 = iow(U, 3, core::mem::size_of::<UinputSetup>() as u32);
/// `UI_ABS_SETUP` — struct uinput_abs_setup.
pub const UI_ABS_SETUP: u64 = iow(U, 4, core::mem::size_of::<UinputAbsSetup>() as u32);
pub const UI_SET_EVBIT: u64 = iow(U, 100, 4);
pub const UI_SET_KEYBIT: u64 = iow(U, 101, 4);
pub const UI_SET_RELBIT: u64 = iow(U, 102, 4);
pub const UI_SET_ABSBIT: u64 = iow(U, 103, 4);
pub const UI_SET_FFBIT: u64 = iow(U, 107, 4);
/// `UI_GET_SYSNAME(len)` — the created device's sysfs name ("input42"), which
/// lets us find our own event node to self-measure round-trip latency.
pub const fn ui_get_sysname(len: u32) -> u64 {
    ioc(IOC_READ, U, 44, len)
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct InputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UinputSetup {
    pub id: InputId,
    pub name: [u8; 80],
    pub ff_effects_max: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct InputAbsinfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UinputAbsSetup {
    pub code: u16,
    // The kernel struct has explicit padding here so `absinfo` is 4-aligned.
    pub _pad: u16,
    pub absinfo: InputAbsinfo,
}

/// `struct input_event` as the 64-bit kernel sees it: `struct timeval` is two
/// `__kernel_long_t`, so 16 bytes, then u16/u16/i32.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct InputEvent {
    pub tv_sec: i64,
    pub tv_usec: i64,
    pub ty: u16,
    pub code: u16,
    pub value: i32,
}

impl InputEvent {
    pub const SIZE: usize = core::mem::size_of::<Self>();

    pub fn new(ty: u16, code: u16, value: i32) -> Self {
        Self { tv_sec: 0, tv_usec: 0, ty, code, value }
    }

    /// Kernel timestamp in microseconds. Only meaningful after
    /// `EVIOCSCLOCKID(CLOCK_MONOTONIC)`, which every source sets on open.
    pub fn time_us(&self) -> u64 {
        (self.tv_sec as u64).wrapping_mul(1_000_000).wrapping_add(self.tv_usec as u64)
    }
}

// ---------------------------------------------------------------------------
// input-event-codes.h — only what a gamepad needs.
// ---------------------------------------------------------------------------

pub mod ev {
    pub const SYN: u16 = 0x00;
    pub const KEY: u16 = 0x01;
    pub const REL: u16 = 0x02;
    pub const ABS: u16 = 0x03;
    pub const MSC: u16 = 0x04;
    pub const FF: u16 = 0x15;
    pub const CNT: u16 = 0x20;
}

pub const SYN_REPORT: u16 = 0;

pub mod btn {
    // Note the kernel's deliberately confusing aliases:
    //   BTN_A = BTN_SOUTH = 0x130, BTN_B = BTN_EAST  = 0x131,
    //   BTN_X = BTN_NORTH = 0x133, BTN_Y = BTN_WEST  = 0x134.
    // 0x132 (BTN_C) is skipped by xpad. We mirror xpad exactly so SDL's built-in
    // "Microsoft X-Box 360 pad" mapping applies with zero configuration.
    pub const SOUTH: u16 = 0x130; // A
    pub const EAST: u16 = 0x131; // B
    pub const C: u16 = 0x132;
    pub const NORTH: u16 = 0x133; // X
    pub const WEST: u16 = 0x134; // Y
    pub const Z: u16 = 0x135;
    pub const TL: u16 = 0x136; // LB
    pub const TR: u16 = 0x137; // RB
    pub const TL2: u16 = 0x138;
    pub const TR2: u16 = 0x139;
    pub const SELECT: u16 = 0x13a; // Back
    pub const START: u16 = 0x13b;
    pub const MODE: u16 = 0x13c; // Guide
    pub const THUMBL: u16 = 0x13d;
    pub const THUMBR: u16 = 0x13e;

    pub const DPAD_UP: u16 = 0x220;
    pub const DPAD_DOWN: u16 = 0x221;
    pub const DPAD_LEFT: u16 = 0x222;
    pub const DPAD_RIGHT: u16 = 0x223;

    pub const JOYSTICK: u16 = 0x120;
    pub const TRIGGER: u16 = 0x120;
    pub const GAMEPAD: u16 = 0x130;
}

pub mod abs {
    pub const X: u16 = 0x00;
    pub const Y: u16 = 0x01;
    pub const Z: u16 = 0x02; // left trigger (xpad)
    pub const RX: u16 = 0x03;
    pub const RY: u16 = 0x04;
    pub const RZ: u16 = 0x05; // right trigger (xpad)
    pub const HAT0X: u16 = 0x10;
    pub const HAT0Y: u16 = 0x11;
    pub const CNT: u16 = 0x40;
}

pub const BUS_USB: u16 = 0x03;
pub const BUS_VIRTUAL: u16 = 0x06;

// ---------------------------------------------------------------------------
// usbfs — linux/usbdevice_fs.h  (type 'U', same letter, different device)
// ---------------------------------------------------------------------------

pub const USBDEVFS_URB_TYPE_ISO: u8 = 0;
pub const USBDEVFS_URB_TYPE_INTERRUPT: u8 = 1;
pub const USBDEVFS_URB_TYPE_CONTROL: u8 = 2;
pub const USBDEVFS_URB_TYPE_BULK: u8 = 3;

/// The kernel declares SUBMITURB as `_IOR` even though it reads from us; keep
/// the header's (arguably wrong) direction bit or the ioctl number won't match.
pub const USBDEVFS_SUBMITURB: u64 = ior(U, 10, core::mem::size_of::<UsbdevfsUrb>() as u32);
pub const USBDEVFS_DISCARDURB: u64 = io(U, 11);
pub const USBDEVFS_REAPURB: u64 = iow(U, 12, core::mem::size_of::<usize>() as u32);
pub const USBDEVFS_REAPURBNDELAY: u64 = iow(U, 13, core::mem::size_of::<usize>() as u32);
pub const USBDEVFS_CLAIMINTERFACE: u64 = ior(U, 15, 4);
pub const USBDEVFS_RELEASEINTERFACE: u64 = ior(U, 16, 4);
pub const USBDEVFS_CLEAR_HALT: u64 = ior(U, 21, 4);
pub const USBDEVFS_DISCONNECT_CLAIM: u64 =
    ior(U, 27, core::mem::size_of::<UsbdevfsDisconnectClaim>() as u32);

pub const USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER: u32 = 0x02;

#[repr(C)]
pub struct UsbdevfsUrb {
    pub ty: u8,
    pub endpoint: u8,
    pub status: i32,
    pub flags: u32,
    pub buffer: *mut libc::c_void,
    pub buffer_length: i32,
    pub actual_length: i32,
    pub start_frame: i32,
    /// union { number_of_packets; stream_id } — unused for bulk.
    pub number_of_packets: i32,
    pub error_count: i32,
    pub signr: u32,
    pub usercontext: *mut libc::c_void,
}

#[repr(C)]
pub struct UsbdevfsDisconnectClaim {
    pub interface: u32,
    pub flags: u32,
    pub driver: [u8; 256],
}

// ---------------------------------------------------------------------------
// Thin ioctl/syscall helpers. Every one returns io::Result so callers can fail
// loudly at setup time and never check errors in the hot loop.
// ---------------------------------------------------------------------------

use std::io;

/// `ioctl(fd, req, arg)` with an integer argument.
pub fn ioctl_int(fd: libc::c_int, req: u64, arg: libc::c_int) -> io::Result<libc::c_int> {
    let r = unsafe { libc::ioctl(fd, req as _, arg) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// `ioctl(fd, req, ptr)` with a pointer argument.
///
/// # Safety
/// `ptr` must point at a valid object of the size encoded in `req`.
pub unsafe fn ioctl_ptr<T>(fd: libc::c_int, req: u64, ptr: *mut T) -> io::Result<libc::c_int> {
    let r = unsafe { libc::ioctl(fd, req as _, ptr) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Monotonic microseconds — the daemon's one and only clock. Matches the domain
/// `EVIOCSCLOCKID(CLOCK_MONOTONIC)` puts on evdev timestamps, so source events
/// and our own deadlines are directly comparable with no conversion.
pub fn now_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1_000
}

// ---------------------------------------------------------------------------
// Compile-time conformance
//
// These are `const` assertions, not tests, on purpose. A wrong ioctl number or a
// struct whose padding drifts from the kernel's does not produce a test failure
// — it produces an `EINVAL` on someone's machine, or worse, a syscall that
// scribbles past the end of a buffer. Making them const means the build fails
// here, on any host, before a binary exists.
//
// Values cross-checked against what the C macros in <linux/input.h>,
// <linux/uinput.h> and <linux/usbdevice_fs.h> expand to on 64-bit.
// ---------------------------------------------------------------------------

const _: () = {
    // evdev
    assert!(EVIOCGRAB == 0x4004_4590);
    assert!(EVIOCSCLOCKID == 0x4004_45a0);
    assert!(EVIOCGID == 0x8008_4502);
    assert!(eviocgname(256) == 0x8100_4506);
    assert!(eviocgphys(256) == 0x8100_4507);
    assert!(eviocgabs(abs::X as u32) == 0x8018_4540);
    // uinput
    assert!(UI_DEV_CREATE == 0x0000_5501);
    assert!(UI_DEV_DESTROY == 0x0000_5502);
    assert!(UI_DEV_SETUP == 0x405c_5503);
    assert!(UI_ABS_SETUP == 0x401c_5504);
    assert!(UI_SET_EVBIT == 0x4004_5564);
    assert!(UI_SET_KEYBIT == 0x4004_5565);
    assert!(UI_SET_ABSBIT == 0x4004_5567);
    // usbfs
    assert!(USBDEVFS_SUBMITURB == 0x8038_550a);
    assert!(USBDEVFS_DISCARDURB == 0x0000_550b);
    assert!(USBDEVFS_REAPURB == 0x4008_550c);
    assert!(USBDEVFS_REAPURBNDELAY == 0x4008_550d);
    assert!(USBDEVFS_CLAIMINTERFACE == 0x8004_550f);
    assert!(USBDEVFS_RELEASEINTERFACE == 0x8004_5510);
    assert!(USBDEVFS_DISCONNECT_CLAIM == 0x8108_551b);
};

const _: () = {
    use core::mem::size_of;
    assert!(size_of::<InputEvent>() == 24);
    assert!(size_of::<InputId>() == 8);
    assert!(size_of::<UinputSetup>() == 92);
    assert!(size_of::<InputAbsinfo>() == 24);
    assert!(size_of::<UinputAbsSetup>() == 28);
    assert!(size_of::<UsbdevfsUrb>() == 56);
    assert!(size_of::<UsbdevfsDisconnectClaim>() == 264);
};
