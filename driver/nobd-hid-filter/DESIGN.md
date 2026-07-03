# NOBD HID Filter — design

Goal: apply the NOBD sync window to a controller **in-path**, by transforming the
real device's HID input reports as they flow up the stack — so **every game**
(and the Finger Gap Tester) sees grouped inputs, with **zero added transport
latency** and **no per-game injection**. This is the firmware's `syncGpioGetAll()`
reimplemented as a Windows HID filter instead of running on the stick's MCU.

## Where it sits

```
        game / Finger Gap Tester (DirectInput / RawInput / XInput*)
                         ▲   reads input reports
                  hidclass.sys  (HID class driver)
                         ▲
                ┌─────────────────────┐
                │  NOBD HID FILTER     │  ← upper filter on the HID device
                │  transforms reports  │     (UMDF2 filter, WdfFdoInitSetFilter)
                └─────────────────────┘
                         ▲
                  HID minidriver (hidusb / Bluetooth / vendor)
                         ▲
                   physical stick
```

We attach as an **upper filter** on the HID gamepad's device stack. Input reports
the minidriver produces flow up *through us* to `hidclass`; we rewrite the button
bits in place. We are not a new device and not a hop — we are *in the wire*.

## Why "zero added latency" is real here

The report was already traversing this stack at the device's own report rate. We
don't add a queue, a virtual device, or a poll interval — we run a few
instructions in the completion path of a report that was already in flight. The
only latency we introduce is the **intentional sync hold** (a lone attack waits up
to `window` ms for its partner) — identical to the firmware. Already-grouped /
same-frame presses commit immediately (0 added). This is the same contract the
NOBD board offers.

Contrast with a virtual-pad approach (ViGEm): that *inserts a device*, inheriting
an IOCTL submit + emulated endpoint poll + xusb22 sampling — a structural ~1 ms
floor. The filter avoids all of it by never adding a device.

## The core transform

`SyncWindow.h` is a pure `(raw, now_us) -> grouped` function (ported verbatim from
`src/sync_window.rs`). The filter calls it on each input report:

```
raw   = extract button bits from the incoming HID input report
out   = sync.process(raw, ATTACK_MASK, syncedMask, now_us, window_us, enabled)
write `out` back into the report buffer, complete upward
```

Button-bit layout is per-device (HID report descriptor). v1 targets the common
"buttons in one byte/two bytes" fightstick layout; the masks live next to
`ATTACK_MASK` and are the main thing to retarget per stick.

## The hold / injection model (the hard part)

HID is **report-on-change**: the device only sends a report when state changes.
That creates one case the pure transform can't handle alone:

- **Grouped within the window** — easy. Lead press arrives (report A): we suppress
  it. Partner arrives a few ms later (report B, both bits set): `process()` commits
  both. No injection needed; report B did the work.
- **Lone press** — hard. Lead arrives (report A): we suppress it, open the window.
  No partner comes, so the device sends **no further report** until release. We
  must still *release the lone press after `window` ms* — which means the filter
  has to **inject a report upward** with nothing new from the device.

Mechanism (mirrors how input-injection filters work):
1. On a report that leaves the window open (`sync.windowOpen() == true`), arm a
   WDF timer for the remaining `window`.
2. `hidclass` keeps read requests (`IOCTL_HID_READ_REPORT`) pended down the stack.
   Hold one pending read instead of completing it.
3. When the timer fires, call `process()` again with the *same* raw (time has
   advanced past `window_us`) → it now commits the held press → complete the
   pended read with the synthesized report.
4. Cancel the timer if a real report arrives first (partner or release).

This is the only genuinely tricky kernel/WDF plumbing. Everything else is a
straight buffer rewrite. v0 (passthrough) and v1 (grouped-within-window only,
no lone-press injection) are useful, testable milestones *before* this.

## Milestones

- **v0 — passthrough.** Attach, log reports, complete unchanged. Proves we're
  in-path and measures added latency (target: ~0 vs raw). Verify with the loopback
  harness + Finger Gap Tester (should read identical to no filter).
- **v1 — group-on-arrival.** Apply `process()` per report, *without* timer
  injection. Groups presses that arrive in separate-but-close reports; a truly
  lone press is released on its next natural report (release). Finger Gap Tester
  should already flip to **GROUPING DETECTED** here for real chords.
- **v2 — lone-press injection.** Add the WDF timer + pended-read completion so a
  lone attack releases exactly on window expiry. Full firmware parity.
- **v3 — config + telemetry.** Shared-memory control (enable/window/mask) and
  stats, reusing the existing `Local\NobdSyncState` block the GUI already drives.

## Device-class caveat (read this before testing)

A HIDClass filter transforms **HID** input reports — i.e. **DirectInput / generic
HID gamepads**. **XInput / Xbox controllers** (VID_045E&PID_028E and friends) speak
a *vendor* protocol through `xusb22.sys`, **not** standard HID, so this filter will
**not** see their inputs.

- Most fightsticks have an **XInput ⇄ DInput** mode switch — run the stick in
  **DInput/HID** mode for this filter.
- The currently-connected pad on this machine is an Xbox-type pad, so it is **not**
  a valid target as-is.
- An XInput-pad equivalent would require filtering at the `xusb22` / USB layer —
  out of scope for v1.

This also shapes verification: the Finger Gap Tester currently reads **XInput**.
To verify the HID filter it needs to read the **HID** device directly (raw HID /
hidapi). Adding a "raw HID" source to the tester is the companion task — then
"filter on vs off" in the tester is the grouping proof.

## Plug-and-play strategy (handling every stick)

Requiring a DInput-mode switch hurts UX, and XInput-only (Xbox-licensed) sticks
can't switch at all. The plan to make it "just work" regardless:

1. **HID *class* filter (this driver, refined).** Instead of a per-device INF
   bound to one hardware ID, register as a **class filter** on HIDClass and bind
   automatically to any HID gamepad. The driver checks the HID usage page
   (Generic Desktop / Game Controls, usage 0x04/0x05) and only transforms actual
   game controllers. → zero latency, plug-and-play, **DInput/HID sticks**.
2. **xusb22 / USB lower filter (later).** XInput sticks speak the Xbox vendor
   protocol through `xusb22.sys`, not HID. The same in-path idea applies one layer
   over: a USB lower filter rewrites the 20-byte Xbox input report's button field.
   → zero latency, plug-and-play, **XInput sticks**. More work (vendor protocol).
3. **Virtual pad + HidHide (universal fallback).** Read the stick however it
   presents, apply the window, present ONE synced virtual pad, hide the real one.
   Works on *any* controller at a ~1 ms cost. The safety net when in-path filtering
   can't bind.

Build order: (1) now — prove the architecture on the easy class; (2) after; (3)
keep in back pocket. All three share `SyncWindow.h` and the same verification bench.

## Verification loop (the payoff)

1. Stick in DInput/HID mode, filter installed.
2. Run the Finger Gap Tester (raw-HID mode) reading that stick.
3. Filter **OFF** (enable=0 via shared mem): tester shows **GROUPING OFF — natural
   finger timing**.
4. Filter **ON**: tester shows **GROUPING DETECTED**, dead zone ≈ window, 60fps
   split rate drops. Same stick, same fingers — only the filter changed.

That is the universal-grouping proof, using the detector we already built.

## Build / sign / install

See `README.md`. Summary: WDK + VS build the UMDF2 driver; enable test-signing
(`bcdedit /set testsigning on` + reboot) for dev; install the INF as an upper
filter with `pnputil`/`devcon`. Distribution needs an EV cert + attestation
signing.
