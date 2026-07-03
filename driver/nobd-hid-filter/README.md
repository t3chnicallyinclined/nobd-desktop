# NOBD HID Filter (UMDF2)

Apply the NOBD sync window to a controller **in-path** by transforming its HID
input reports — universal across games, zero added transport latency, no per-game
injection. The firmware's grouping, as a Windows HID filter. See `DESIGN.md` for
the architecture and `SyncWindow.h` for the (portable, tested) core.

> **Status: scaffold.** The sync core + design are done. The driver glue targets
> the standard UMDF2 HID-filter pattern and must be built with the WDK (not
> available in CI / the dev shell here — build on a dev box).

## Prerequisites (one-time, on a dev machine)

1. **Visual Studio 2022** + **Desktop C++** workload.
2. **Windows Driver Kit (WDK)** matching your VS + the matching **WDK VS
   extension** (gives the "User Mode Driver, Empty (UMDF V2)" project template).
3. A **HID/DirectInput** fightstick (NOT an Xbox/XInput pad — see DESIGN.md
   "Device-class caveat"). Put the stick in **DInput mode**.
4. A **non-production** test machine you can put in test-signing mode.

## Build

The cleanest path is the VS template (hand-writing the `.vcxproj` is brittle):

1. VS → New Project → **"User Mode Driver, Empty (UMDF V2)"** → name `nobd-hid-filter`.
2. Add `Driver.cpp`, `SyncWindow.h` from this folder to the project.
3. Set the INF to `nobd-hid-filter.inf`.
4. Build **x64 / Release**. Output: `nobd-hid-filter.dll` + `.inf` + `.cat`.

(Or, with the **EWDK** mounted, `msbuild nobd-hid-filter.vcxproj /p:Configuration=Release /p:Platform=x64`.)

## Sign (dev / test-signing)

On the **test machine** (this reboots):
```powershell
bcdedit /set testsigning on        # then REBOOT
# create + trust a test cert, sign the .cat (one-time)
# (use the WDK's inf2cat + signtool, or VS "Driver Signing" = Test Sign)
```
Distribution to real users needs an **EV cert + Microsoft attestation signing** —
out of scope for the PoC.

## Install as an upper filter

Find the stick's device instance (DInput mode):
```powershell
Get-PnpDevice -Class HIDClass | Where-Object FriendlyName -match 'game|controller'
```
Install the filter INF:
```powershell
pnputil /add-driver nobd-hid-filter.inf /install
# then bind it as an UpperFilter on the target HID device and re-enumerate:
#   Update-PnpDevice / Disable-PnpDevice+Enable-PnpDevice on the instance,
#   or reuse devcon update for the matching hardware ID.
```
Confirm it loaded: Device Manager → the stick → Details → **Upper filters** should
list `nobdhidfilter`. Driver logs go to a WPP/ETW trace or `OutputDebugString`
(DebugView).

## Verify (the payoff)

1. Stick in DInput/HID mode, filter installed and bound.
2. Run the **Finger Gap Tester** in **raw-HID mode** (companion task — it must read
   the HID device directly, since it currently reads XInput; see DESIGN.md).
3. Toggle the filter (`enable` in the shared-memory config, or reinstall/disable):
   - **OFF** → tester reads **GROUPING OFF — natural finger timing**.
   - **ON**  → tester reads **GROUPING DETECTED**, dead zone ≈ your window, the
     60fps split rate drops.
4. Same effect should show up in any DInput/RawInput game — that's the universal win.

## Uninstall

```powershell
# remove the UpperFilter binding, then:
pnputil /delete-driver oem<NN>.inf /uninstall   # the oemNN.inf pnputil assigned
bcdedit /set testsigning off                    # when done testing; reboot
```

## Roadmap (see DESIGN.md "Milestones")

- **v0** passthrough (prove in-path, ~0 latency)
- **v1** group-on-arrival (Finger Gap Tester flips to GROUPING DETECTED)
- **v2** lone-press timer injection (full firmware parity)
- **v3** shared-memory config + telemetry (reuse `Local\NobdSyncState`)
