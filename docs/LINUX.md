# NOBD on Linux (`nobdd`)

The NOBD sync window as a Linux input daemon. Same window, same `SyncWindow`,
same attack mask as the [GP2040-CE NOBD](https://github.com/t3chnicallyinclined/GP2040-CE-NOBD)
firmware and the Windows build — everything around it is different, and mostly
better.

```
  your stick ──► evdev (EVIOCGRAB)     ─┐
                 or NOBD Bulk (usbfs)   ├─► SyncWindow ──► uinput virtual pad ──► Steam / Proton / game
                 kernel timestamps      ─┘   deadline-exact      (Xbox 360 identity)
```

**Built for Bazzite first.** Nothing here layers an rpm, touches `/usr`, or needs
a reboot.

---

## Why this is a port, not a repackage

The Windows build needs a code-signed UMDF2 driver to make its virtual pad, the
HidHide kernel filter to hide your stick, WinUSB binding for the bulk stream, and
UAC elevation to tie it together. None of that survives Wine or Proton. Linux
supplies all four as kernel interfaces:

| Windows | Linux | What it costs now |
|---|---|---|
| Vendored, EV-signed HIDMaestro UMDF2 driver | `uinput` | ~200 lines. No certificate (**saves the ~$280/yr EV cert**), no reboot, no driver install |
| HidHide filter driver + reboot | `EVIOCGRAB` + one udev rule | Two ioctls |
| WinUSB / Zadig binding for bulk | `usbfs` | One udev rule |
| UAC + elevated logon scheduled task | systemd unit | One file |

And it removes a whole class of problem: the Windows build has to *fingerprint*
its own virtual pad by driving a button marker and watching which XInput slot
echoes it, because XInput exposes no device identity. On Linux the kernel just
tells us which device is ours.

---

## What makes it fast

The Windows sync loop sleeps 1 ms and re-polls. `nobdd` never polls.

**1. Kernel timestamps, not our own.** Every source is opened with
`EVIOCSCLOCKID(CLOCK_MONOTONIC)`, so each event carries the instant the *kernel*
processed the report. The window is timed from that. Our scheduling jitter
cannot widen or narrow it. On Windows the timestamp is whenever the poll thread
happened to run.

**2. The window closes on its deadline.** A `timerfd` armed with
`TFD_TIMER_ABSTIME` on exactly `start + window`, with timer slack dropped from
Linux's default 50 µs to 1 ns. A 5 ms window is 5 ms, not "the first poll after
5 ms".

**3. One epoll, no handoffs.** Source events, the window deadline, and shutdown
all land in a single `epoll_wait` on one thread. The bulk path talks to `usbfs`
directly rather than through libusb/nusb, specifically so a URB completion
doesn't have to cross an event thread and a channel to reach the sync window.

**4. The firmware's own clock, when available.** NOBD Bulk payloads carry
`edge_us` — the MCU's timestamp at the button edge, upstream of USB scheduling
and of us. `ClockBridge` translates it into `CLOCK_MONOTONIC` by tracking the
minimum observed offset. Nothing else in the chain can time the press better,
because nothing else sees the edge.

**5. Optional spin.** For the last `spin_us` (default 200) before a deadline the
loop switches from blocking to `epoll_wait(0)`, so the commit fires without a
scheduler round trip. Costs one core for those microseconds, only while a window
is open. `spin_us = 0` turns it off.

**6. RT tuning, reported honestly.** `SCHED_FIFO`, `mlockall`, timer slack, CPU
latency QoS (`/dev/cpu_dma_latency` held at 0, so deep C-state exit latency never
lands on the first press after a pause), optional CPU pinning, optional
`usbhid jspoll`. Run `nobdd tune` to see which ones *your* machine allows — the
daemon prints the same report at startup and never assumes a tuning took.

**Measure it yourself.** `nobdd probe` times uinput submit → readable on our own
event node (min/p50/p99/max). `nobdd stats` reports `pipeline_avg_us` live:
kernel event timestamp → our `write()` returning, which is everything the daemon
adds. No number in this document is one you have to take on faith.

---

## Install

```sh
tar xzf nobd-linux-<version>-x86_64.tar.gz
cd nobd-linux-<version>-x86_64
sudo ./install.sh --user "$USER"
```

Lands in `/usr/local/bin` (a symlink to `/var/usrlocal` on ostree systems —
writable and preserved across `rpm-ostree`/`bootc` updates), `/etc/udev/rules.d`,
`/etc/systemd/system`, `/etc/nobd`. Log out and back in once for the `nobd`
group to take effect.

```sh
nobdd list                # what it can see
nobdd tune                # which tunings this machine allows
nobdd probe               # measure the uinput hop here
systemctl status nobd
journalctl -u nobd -f
```

Uninstall: `sudo ./install.sh --uninstall`.

### Why not a Flatpak

Bazzite's docs rightly recommend Flatpak over `rpm-ostree` layering for
applications. This is not one. A Flatpak cannot install udev rules and
[cannot be given `/dev/uinput`](https://github.com/flatpak/flatpak/issues/696)
("File `/dev/uinput` has unsupported type"). A future *GUI* is a good Flatpak
candidate — it would talk to this daemon over the control socket and touch no
devices. The daemon itself has to be a system install, which is also what
[Handheld Daemon](https://github.com/hhd-dev/hhd) does for the same reasons, and
it ships on Bazzite.

---

## Configure

`/etc/nobd/nobdd.conf`, overridden by `~/.config/nobd/nobdd.conf`, overridden by
`--key=value` on the command line. Every key is documented in the shipped file.

Live, against a running daemon — no restart:

```sh
nobdd set window_ms 6     # tune the window between games
nobdd set enabled 0       # A/B it mid-session
nobdd stats               # grouping + measured latency
```

The control socket is a plain line protocol on `/run/nobd/nobdd.sock`, group
`nobd`, mode 0660. `socat`, `nc` or a shell script are all valid clients — which
is what a Decky plugin will use.

---

## The one thing that will bite you: double input

`EVIOCGRAB` gives us exclusive access to your stick's **evdev** node. Steam reads
controllers it recognises through **hidraw**, which a grab does not cover. So by
default a game can see *both* your raw stick and the NOBD pad — and the raw one
is ungrouped, which defeats the point.

Two fixes, pick one:

**A. Turn Steam Input off** for the physical stick (Steam → Controller settings).
Simplest, reversible in the UI, nothing to install.

**B. Hide the stick at the udev level.** `/etc/nobd/60-nobd-hide.rules.template`
— substitute your stick's IDs from `nobdd list` and drop it in
`/etc/udev/rules.d/`. Instructions are in the file. Note the `60` prefix: the tag
has to be cleared *before* udev's own `70-uaccess.rules` runs, so a
higher-numbered file would silently do nothing.

With B the stick is invisible to Steam even when `nobdd` isn't running. That's
the trade, and it's why it isn't installed by default.

---

## Known limits

* **x86_64 only** so far. aarch64 (for handhelds that aren't x86) needs a CI
  target added — the `_IOC` arithmetic in `uapi.rs` is already correct for it.
* **Handheld built-in controllers**: [Handheld Daemon](https://github.com/hhd-dev/hhd)
  already grabs those on a Deck/Ally. Point `nobdd` at an external stick
  (`device = /dev/input/eventN`); running both against the same device will
  fight.
* **Game Mode** has no tray and no desktop, so control is CLI/socket only until
  the Decky plugin lands. The daemon itself runs fine there — it's a systemd
  service and doesn't care about the session.
* **One pad.** P2 support means a second uinput device and a second window; the
  shared state already has two player slots reserved for it.
* **No GUI yet.** The gap tester and the panel are the next phase; the
  measurement plumbing (`Stats`, the probe) is already here and feeding the
  control socket.

---

## Build from source

```sh
cargo build -p nobd-linux --release     # -> target/release/nobdd
```

`libc` is the only dependency. Cross-check from any host without a linker:

```sh
cargo clippy -p nobd-linux --target x86_64-unknown-linux-gnu --all-targets -- -D warnings
```

Every ioctl number and kernel struct layout is asserted at **compile time**
(`const _: () = assert!(...)` in `uapi.rs`), cross-checked against what the C
macros expand to on 64-bit. A wrong constant fails the build rather than
returning `EINVAL` on someone's machine.

### Layout

| File | Role |
|---|---|
| `linux/src/uapi.rs` | ioctl numbers, input codes, kernel structs |
| `linux/src/pad.rs` | the XInput-bitfield pad model (shared with Windows) |
| `linux/src/evdev.rs` | universal source: grab, `CLOCK_MONOTONIC`, decode |
| `linux/src/bulk.rs` | usbfs URB ring + firmware clock bridge |
| `linux/src/uinput.rs` | the virtual pad |
| `linux/src/engine.rs` | the epoll/timerfd hot loop |
| `linux/src/rt.rs` | real-time tuning, with an honest report |
| `linux/src/config.rs`, `ctl.rs`, `probe.rs` | config, control socket, measurement |
| `shared/src/sync_window.rs` | **the window itself — identical on both platforms** |
