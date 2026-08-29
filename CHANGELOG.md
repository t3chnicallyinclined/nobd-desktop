# Changelog

## v0.6.0

The sync window is now a real delay line, the app is one screen written for
players rather than engineers, and the installer can actually replace an older
version. Several of the fixes below are input-correctness bugs that could drop or
stick a button in a match.

### Sync window — correctness

- **A press is never deleted.** A press released inside the window used to be
  dropped outright: a 2 ms tap at a 5 ms window produced *nothing at all*, and at
  a 16 ms window an ordinary 10 ms jab vanished too. Raising the window made
  inputs disappear.
- **Pulse width is preserved.** Releases used to pass through instantly while
  presses were delayed, so every press reached the game up to a full window
  *shorter* than you held it. A release is now delayed by the same amount its own
  press was.
- **A re-press during a release debt groups again.** Tapping an attack and then,
  within the window, pressing it together with another left the second button
  waiting a full extra window for a partner that was already down — a whole frame
  of added latency on ordinary mashing.
- **A late or backwards clock can no longer strand a button.** The delay a press
  owes its release is clamped to the window, and time arithmetic saturates. A
  long source stall or a backwards timestamp from the Linux bulk clock could
  previously compute a release deadline minutes away, holding the button down.
- **Toggling sync off clears the debt.** Old delays no longer carry across an
  A/B toggle and lengthen a press that was never held back.
- Event-driven callers wake on `next_deadline_us()`, which covers outstanding
  releases as well as the window — without it a delayed release hung until the
  next unrelated input.

All of the above ship in the shared implementation, so the Windows app, the Linux
daemon and the C++ HID filter get them together. 18 Rust tests and 24 C++ parity
checks cover them.

### Never hand the game a stuck button

- **Every exit path releases the virtual pad.** The devnode outlives the app and
  the driver keeps publishing the last report, so quitting mid-press left the
  NOBD Controller holding that button — in every game, until the app ran again.
- **Losing the stick releases it too.** All three sync loops now feed a neutral
  report while the source is absent. Unplugging a stick mid-press used to leave
  the buttons held for the whole outage.
- **Restarting sync no longer blanks the live report.** Building the new service
  before dropping the old one meant two loops ran at once and the new one zeroed
  the shared section the old one was still writing — a dropped frame on every
  hotplug re-adopt.

### Installer

- **An older install is now actually replaced.** Setup skipped the driver install
  whenever *any* version was present, so shipping a new NOBD to an existing user
  silently kept the old driver forever. The bundled `DriverVer` is compared
  against the DriverStore, stale packages are removed, and the new one installed.
- **The upgrade is reachable.** Setup was only offered when no controller
  existed, so anyone with a working NOBD could never reach it. The app now
  detects a stale driver and offers a one-click update.
- **Install before delete.** Removing the old package first stripped the driver
  from the live devnode; if setup then failed, the machine was left with a
  reboot-surviving driverless controller that the app still counted as present.
- `uninstall_hidmaestro()` matched the literal `hidmaestro.inf`, which is not a
  substring of `hidmaestro_xusb.inf` — the XInput driver survived every uninstall
  while the code reported success.
- A failed `pnputil` no longer reports success on the strength of a leftover
  copy, an unreadable `DriverVer` no longer deletes every installed package, and
  dateless / commented / three-part versions parse correctly.
- **Switching output mode removes the old controller** instead of orphaning it in
  Windows with no way to remove it through the UI.
- Setup no longer registers an elevated logon task as a silent side effect — the
  "Start with Windows" checkbox is the only thing that creates it.

### First run

- **The window is visible.** It started hidden unless a debug environment
  variable was set, so a first-timer double-clicking `nobd.exe` got nothing, and
  the UAC relaunch made the app vanish at the exact moment they clicked Install.
  Hiding is now opt-in via `--tray`, which is what the logon task passes.
- A single-instance guard stops the logon task and a manual launch fighting over
  the same virtual pad; the setup run is exempt from it, because claiming it
  there could make an install silently do nothing.
- Failures that used to be silent now speak: a non-elevated remove, a missing
  HidHide installer, a cancelled UAC prompt.

### The app

- **One screen.** The two tabs are merged. Five fixed zones and one elastic one,
  so nothing scrolls at the default size.
- **Written for players.** States read *YOUR DASHES ARE COMING OUT*, *SYNC TOO
  TIGHT*, *TRY A DASH*. Milliseconds are off the front screen; the window is
  *slack: Normal*, and tuning is *More slack* / *Less slack*.
- **Before and after on one row.** Every two-button press shows what your hands
  did next to what the game got.
- **Honest numbers.** The per-press verdict was a coin flip against a simulated
  clock with no relationship to the game's — two identical 2.1 ms gaps could
  print opposite answers. It now shows the odds (*about 1 in 5*), and states the
  two certainties outright. The `saves` counter accumulates the expectation
  rather than counting flips.
- The recommendation is measured from the *raw* finger gap, so it can suggest a
  wider window. The old one was computed from grouped presses only, which made it
  mathematically incapable of telling you your window was too tight — and it was
  labelled `p95` when nearest-rank made it the maximum at any sample under 12.
- `GROUPING DETECTED` no longer fires because you pressed a single button on its
  own, and a controller reporting too slowly to judge now says `CAN'T TELL`
  instead of claiming perfect grouping.
- A colour charter (`app/src/palette.rs`): one meaning per colour, and **no
  measurement picks its own colour** — a legitimate 14 ms finger gap used to
  render in alarm red.
- Fixed a light-on-dark theme bug (egui followed the system theme, so panels came
  back white) and replaced glyphs the bundled font cannot draw — the arrows and
  status dots were rendering as empty boxes.

### Docs

- The README documented a DLL-injection architecture that no longer exists —
  `DINPUT8.dll`, an Install tab, a "hook LIVE" banner, a Button Monitor tab.
  Rewritten for the virtual-controller design.

---

## v0.5.1 and earlier

See the commit log; this file starts at v0.6.0.
