//! uinput sink — the virtual pad the game actually reads.
//!
//! This is the whole of what `hm-native` + a vendored, EV-code-signed UMDF2
//! driver does on Windows: ~200 lines against a kernel interface that has been
//! stable for a decade. No driver package, no certificate, no reboot, no
//! elevation beyond write access to one device node.
//!
//! Latency notes:
//!   * Every submit is **one `write()`** carrying only the events that actually
//!     changed plus `SYN_REPORT`. One syscall, one input packet, no partial
//!     states visible to the consumer.
//!   * We suppress no-op submits entirely, so a held button costs nothing.
//!   * By default the device presents the **exact identity of a wired Xbox 360
//!     pad** (`045e:028e`, bus USB, name "Microsoft X-Box 360 pad"). SDL derives
//!     its controller GUID from bus/vendor/product/version + name, so matching
//!     xpad byte-for-byte means SDL, Steam Input and Proton apply their built-in
//!     mapping with zero configuration. Branding it "NOBD Controller" is a
//!     config option, not the default, because a novel GUID means an unmapped
//!     stick until the user configures one.

use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;

use crate::pad::{bit, PadState};
use crate::uapi::{self, abs, btn, ev, InputEvent, InputId, SYN_REPORT};

/// Wired Xbox 360 controller, as `xpad` presents it.
pub const X360_VENDOR: u16 = 0x045e;
pub const X360_PRODUCT: u16 = 0x028e;
pub const X360_VERSION: u16 = 0x0114;
pub const X360_NAME: &str = "Microsoft X-Box 360 pad";

/// Our `phys` string. Set on the virtual device so enumeration can exclude our
/// own output *exactly* — without it, masquerading as an Xbox pad would make us
/// indistinguishable from a real one and the daemon could grab its own output.
/// (The Windows build has to fingerprint its companion by driving a button
/// marker and watching for it; on Linux the kernel just tells us.)
pub const NOBD_PHYS: &str = "nobd/virtual0";
/// Prefix enumeration matches against, so extra virtual pads (P2, future
/// per-player devices) are all excluded by one test.
pub const NOBD_PHYS_PREFIX: &str = "nobd/";

/// `UI_SET_PHYS` — `_IOW(UINPUT_IOCTL_BASE, 108, char*)`.
const UI_SET_PHYS: u64 = {
    // Recomputed here rather than exported, since it is the only pointer-sized
    // uinput setter we use.
    const U: u32 = b'U' as u32;
    ((1u32 << 30) | (U << 8) | 108 | ((core::mem::size_of::<usize>() as u32) << 16)) as u64
};

/// Trigger range. xpad's classic 0..255 — matched so SDL's mapping is exact.
const TRIGGER_MAX: i32 = 255;
const STICK_MIN: i32 = -32768;
const STICK_MAX: i32 = 32767;
/// xpad's stick fuzz/flat. Reported verbatim so anything reading `absinfo` (SDL
/// included) sees the same deadzone shape it expects from a real pad.
const STICK_FUZZ: i32 = 16;
const STICK_FLAT: i32 = 128;

/// Every (XInput bit -> BTN_ code) pair we emit, in a fixed order.
const BUTTON_MAP: [(u16, u16); 15] = [
    (bit::A, btn::SOUTH),
    (bit::B, btn::EAST),
    (bit::X, btn::NORTH),
    (bit::Y, btn::WEST),
    (bit::LEFT_SHOULDER, btn::TL),
    (bit::RIGHT_SHOULDER, btn::TR),
    (bit::BACK, btn::SELECT),
    (bit::START, btn::START),
    (bit::GUIDE, btn::MODE),
    (bit::LEFT_THUMB, btn::THUMBL),
    (bit::RIGHT_THUMB, btn::THUMBR),
    (bit::DPAD_UP, btn::DPAD_UP),
    (bit::DPAD_DOWN, btn::DPAD_DOWN),
    (bit::DPAD_LEFT, btn::DPAD_LEFT),
    (bit::DPAD_RIGHT, btn::DPAD_RIGHT),
];

/// How the virtual pad identifies itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Identity {
    /// Byte-identical to xpad — zero-config mapping everywhere. The default.
    Xbox360,
    /// Branded, visible as "NOBD Controller". Needs a controller mapping.
    Nobd,
}

pub struct VirtualPad {
    fd: RawFd,
    last: PadState,
    /// Set once the first submit has happened, so the initial state is always
    /// written even if it is all-zero.
    primed: bool,
    /// Preallocated event batch — the hot path never allocates.
    batch: [InputEvent; 32],
    /// sysfs name of the created device ("input42"), used to locate our own
    /// event node for the self-measuring latency probe.
    pub sysname: Option<String>,
}

fn ioerr(ctx: &str) -> io::Error {
    io::Error::other(format!("{ctx}: {}", io::Error::last_os_error()))
}

impl VirtualPad {
    pub fn create(identity: Identity) -> io::Result<Self> {
        // /dev/uinput on modern kernels; /dev/input/uinput on very old ones.
        let fd = ["/dev/uinput", "/dev/input/uinput"]
            .iter()
            .find_map(|p| {
                let c = CString::new(*p).ok()?;
                let fd =
                    unsafe { libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
                if fd >= 0 {
                    Some(fd)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "cannot open /dev/uinput ({}). Load the module and install the udev rule: \
                         `sudo modprobe uinput` + packaging/83-nobd.rules",
                        io::Error::last_os_error()
                    ),
                )
            })?;

        let mut me = Self {
            fd,
            last: PadState::default(),
            primed: false,
            batch: [InputEvent::default(); 32],
            sysname: None,
        };
        if let Err(e) = me.configure(identity) {
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(me)
    }

    fn configure(&mut self, identity: Identity) -> io::Result<()> {
        let fd = self.fd;

        uapi::ioctl_int(fd, uapi::UI_SET_EVBIT, ev::KEY as i32).map_err(|_| ioerr("UI_SET_EVBIT KEY"))?;
        uapi::ioctl_int(fd, uapi::UI_SET_EVBIT, ev::ABS as i32).map_err(|_| ioerr("UI_SET_EVBIT ABS"))?;

        for (_, code) in BUTTON_MAP {
            uapi::ioctl_int(fd, uapi::UI_SET_KEYBIT, code as i32)
                .map_err(|_| ioerr("UI_SET_KEYBIT"))?;
        }

        // Axis set and ranges exactly as xpad reports them.
        let axes: [(u16, i32, i32, i32, i32); 6] = [
            (abs::X, STICK_MIN, STICK_MAX, STICK_FUZZ, STICK_FLAT),
            (abs::Y, STICK_MIN, STICK_MAX, STICK_FUZZ, STICK_FLAT),
            (abs::RX, STICK_MIN, STICK_MAX, STICK_FUZZ, STICK_FLAT),
            (abs::RY, STICK_MIN, STICK_MAX, STICK_FUZZ, STICK_FLAT),
            (abs::Z, 0, TRIGGER_MAX, 0, 0),
            (abs::RZ, 0, TRIGGER_MAX, 0, 0),
        ];
        for (code, min, max, fuzz, flat) in axes {
            uapi::ioctl_int(fd, uapi::UI_SET_ABSBIT, code as i32)
                .map_err(|_| ioerr("UI_SET_ABSBIT"))?;
            let mut setup = uapi::UinputAbsSetup {
                code,
                _pad: 0,
                absinfo: uapi::InputAbsinfo {
                    value: 0,
                    minimum: min,
                    maximum: max,
                    fuzz,
                    flat,
                    resolution: 0,
                },
            };
            unsafe { uapi::ioctl_ptr(fd, uapi::UI_ABS_SETUP, &mut setup) }
                .map_err(|_| ioerr("UI_ABS_SETUP"))?;
        }

        // The d-pad goes out as buttons (BTN_DPAD_*), not hat axes. Both are
        // valid; buttons keep the four directions independent, which is what a
        // fightstick needs — a hat axis cannot express left+right at all, and
        // SOCD handling is the stick's business, not ours to silently apply.

        // phys — how we recognise our own device later.
        let phys = CString::new(NOBD_PHYS).unwrap();
        let _ = unsafe { libc::ioctl(fd, UI_SET_PHYS as _, phys.as_ptr()) };

        let (vendor, product, version, name) = match identity {
            Identity::Xbox360 => (X360_VENDOR, X360_PRODUCT, X360_VERSION, X360_NAME),
            Identity::Nobd => (0x1d50, 0x6080, 0x0100, "NOBD Controller"),
        };
        let mut setup = uapi::UinputSetup {
            id: InputId { bustype: uapi::BUS_USB, vendor, product, version },
            name: [0u8; 80],
            ff_effects_max: 0,
        };
        let nb = name.as_bytes();
        let n = nb.len().min(setup.name.len() - 1);
        setup.name[..n].copy_from_slice(&nb[..n]);

        unsafe { uapi::ioctl_ptr(fd, uapi::UI_DEV_SETUP, &mut setup) }
            .map_err(|_| ioerr("UI_DEV_SETUP"))?;
        uapi::ioctl_int(fd, uapi::UI_DEV_CREATE, 0).map_err(|_| ioerr("UI_DEV_CREATE"))?;

        // Ask the kernel what it called us, so the probe can find our own event
        // node without guessing.
        let mut buf = [0u8; 64];
        let r = unsafe {
            libc::ioctl(fd, uapi::ui_get_sysname(buf.len() as u32) as _, buf.as_mut_ptr())
        };
        if r >= 0 {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            self.sysname = Some(String::from_utf8_lossy(&buf[..end]).into_owned());
        }
        Ok(())
    }

    /// Publish a state. Emits only what changed, in a single `write()`, and
    /// returns `false` without a syscall when nothing changed at all.
    pub fn submit(&mut self, s: PadState) -> io::Result<bool> {
        if self.primed && s == self.last {
            return Ok(false);
        }
        let prev = self.last;
        let first = !self.primed;
        let mut n = 0usize;

        macro_rules! push {
            ($ty:expr, $code:expr, $val:expr) => {{
                self.batch[n] = InputEvent::new($ty, $code, $val);
                n += 1;
            }};
        }

        let changed = s.buttons ^ prev.buttons;
        for (mask, code) in BUTTON_MAP {
            if first || changed & mask != 0 {
                push!(ev::KEY, code, i32::from(s.buttons & mask != 0));
            }
        }
        if first || s.lx != prev.lx {
            push!(ev::ABS, abs::X, s.lx as i32);
        }
        if first || s.ly != prev.ly {
            // XInput Y is up-positive; evdev is down-positive. Negate, clamping
            // so i16::MIN does not wrap on the way out.
            push!(ev::ABS, abs::Y, -(s.ly as i32).clamp(STICK_MIN + 1, STICK_MAX));
        }
        if first || s.rx != prev.rx {
            push!(ev::ABS, abs::RX, s.rx as i32);
        }
        if first || s.ry != prev.ry {
            push!(ev::ABS, abs::RY, -(s.ry as i32).clamp(STICK_MIN + 1, STICK_MAX));
        }
        if first || s.lt != prev.lt {
            push!(ev::ABS, abs::Z, s.lt as i32);
        }
        if first || s.rt != prev.rt {
            push!(ev::ABS, abs::RZ, s.rt as i32);
        }

        if n == 0 {
            self.last = s;
            self.primed = true;
            return Ok(false);
        }
        push!(ev::SYN, SYN_REPORT, 0);

        let bytes = n * InputEvent::SIZE;
        let w = unsafe {
            libc::write(self.fd, self.batch.as_ptr() as *const libc::c_void, bytes)
        };
        if w < 0 {
            return Err(ioerr("uinput write"));
        }
        self.last = s;
        self.primed = true;
        Ok(true)
    }

    /// Release everything. Called on shutdown so a killed daemon never leaves a
    /// button stuck down in the game.
    pub fn release_all(&mut self) {
        let _ = self.submit(PadState::default());
    }

}

impl Drop for VirtualPad {
    fn drop(&mut self) {
        self.release_all();
        let _ = uapi::ioctl_int(self.fd, uapi::UI_DEV_DESTROY, 0);
        unsafe { libc::close(self.fd) };
    }
}

// `_IOW('U', 108, char*)` on 64-bit — checked at compile time for the same
// reason as the numbers in `uapi`: a wrong value here silently fails to set the
// phys string, and enumeration would then grab our own output.
const _: () = assert!(UI_SET_PHYS == 0x4008_556c);

// Every attack bit must have a BTN_ code to emit, or a grouped press would be
// computed and then dropped on the floor.
const _: () = {
    let mut mapped: u16 = 0;
    let mut i = 0;
    while i < BUTTON_MAP.len() {
        mapped |= BUTTON_MAP[i].0;
        i += 1;
    }
    assert!(mapped & crate::pad::ATTACK_MASK == crate::pad::ATTACK_MASK);
};
