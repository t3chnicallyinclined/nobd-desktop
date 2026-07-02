# hidmaestro-sync — NOBD universal sync on HIDMaestro (SPIKE)

A spike to evaluate replacing the ViGEmBus virtual-pad backend of NOBD's
universal sync with a **[HIDMaestro](https://github.com/hifihedgehog/HIDMaestro)**
UMDF2 user-mode virtual controller.

## Why

The current universal sync (`../vigem-sync`) reads the real stick, runs the NOBD
sync window, and presents a **ViGEmBus** virtual Xbox pad. ViGEmBus is a *kernel*
driver: a bug can BSOD, it must be installed separately, and the virtual-pad hop
has a structural ~1 ms latency floor.

**HIDMaestro** does the same job in **user mode**:

| | ViGEmBus (current) | HIDMaestro |
|---|---|---|
| Framework | kernel bus driver | user-mode UMDF2 (via in-box `mshidumdf.sys`) |
| Crash risk | bug → BSOD | cannot blue-screen |
| Signing | redistribute pre-signed kernel driver | self-signed local cert; no EV cert, no test-signing mode |
| Device identity | fixed Xbox360/DS4 | full custom VID/PID/descriptor per profile |
| License | BSD-3 | MIT |

This spike answers the one open question — **added latency vs ViGEm** — and
proves the NOBD sync window drives a HIDMaestro pad end-to-end.

## What it does

`Program.cs` mirrors `../vigem-sync/src/main.rs`: reads the real controller via
XInput, runs `SyncWindow` (a verbatim port of `../shared/src/sync_window.rs`) on
the attack buttons, and submits the grouped result to a HIDMaestro
`xbox-360-wired` pad. Live config (enabled + window) comes from the same
`Local\NobdSyncState` shared memory the GUI already drives.

## Prerequisites

- **.NET 10 SDK** (`net10.0-windows10.0.26100.0`)
- The **prebuilt** `HIDMaestro.Core.dll` (the driver is embedded in it) —
  **no WDK required**. Fetch it:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\fetch-hidmaestro.ps1
```

## Build

```powershell
dotnet build -c Release
```

## Run

```powershell
# Parity tests — no admin, no driver. Proves the sync port is faithful.
dotnet run -c Release -- --selftest

# The real thing (needs an ELEVATED terminal — InstallDriver/CreateController
# require admin; first run installs the HIDMaestro driver + a self-signed cert):
dotnet run -c Release -- --window 5

# Latency A/B vs ViGEm (same methodology as `vigem-sync --latency`):
dotnet run -c Release -- --latency 400
# then, for comparison, in ../vigem-sync:  cargo run --release -- --latency
```

## Status

**Proven.** The native "NOBD Controller" identity shows in joy.cpl, Steam, and
games, with the NOBD sync window live on top — all user-mode, no WDK, no kernel
driver. Adopted over ViGEm for the branding (ViGEm is actually slightly faster;
both are ~0.1 ms, irrelevant at 60 Hz).

Use `run.cmd --window 5` from an **elevated** terminal (it stops any prior
instance first, so `dotnet run`'s rebuild never hits a file lock).

### Branding & Steam Input — the two name layers

- **Device-list name** (joy.cpl / Steam list): from the USB product string + the
  three OEM registry tables (`HMOemNameOverride`). Reads **"NOBD Controller"**. ✅
- **Steam Input *type* name**: when Steam Input takes over the device it
  classifies it by its VID:PID controller database. Our PID isn't in that DB, so
  it falls back to the generic **"Input Controller N"**.
  - **Interim:** disable Steam Input for the NOBD Controller → games read it
    directly as "NOBD Controller" (and it drops Steam Input's extra remap/latency
    layer, which we don't want on top of the sync anyway).
  - **Ship-prep:** register a dedicated pid.codes PID and submit an SDL
    `gamecontrollerdb` mapping (VID:PID → "NOBD Controller" + button layout) so
    Steam classifies + labels it correctly even under Steam Input.

> Why the VID:PID matters: `0x1209:0x0001` (the shared pid.codes prototyping PID)
> is already in Steam's DB as "TapSync Gamepad", which overrode our name. We moved
> to `0x1209:0x4E42` (unregistered → Steam falls back to our product string).

### Known gaps (spike TODO)

- **Analog triggers not passed through.** The current descriptor is stick + 14
  buttons + hat, no analog triggers, so `NobdProfile.cs` drops the real pad's
  L2/R2 analog values. Fine for a digital fightstick; add `.AddTrigger("Left"/
  "Right", 8)` + trigger passthrough if analog LT/RT is needed.
- **Generic Steam button labels** until the SDL mapping above is submitted.
- **Integration:** fold this into the `nobd-desktop` app as the default backend
  (driven by the existing `Local\NobdSyncState` shared memory), alongside/replacing
  `vigem-sync`.

> Note: HIDMaestro is MIT-licensed and unsigned (like all these tools); its
> prebuilt `Core.dll` scans clean on Windows Defender. `InstallDriver()` adds a
> locally-trusted self-signed cert to the machine cert store (admin, one time).
