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

Spike / evaluation only. If the latency number beats ViGEm and the pad behaves,
the next step is a C# sidecar wired to the app's shared memory as the default
universal-sync backend (replacing `vigem-sync`), plus the 3–4 NOBD JSON profiles
(Xbox 360, PS4/PS5, Native NOBD, Fightstick).

> Note: HIDMaestro is MIT-licensed and unsigned (like all these tools); its
> prebuilt `Core.dll` scans clean on Windows Defender. `InstallDriver()` adds a
> locally-trusted self-signed cert to the machine cert store (admin, one time).
