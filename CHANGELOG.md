# Changelog

## v0.7.1

Mostly one bug, and it was making NOBD look broken on a very ordinary setup.

### Your controller wasn't being found

The hook used the XInput **user index** as the player slot. The game does not: it
claims the first index that answers, so a single stick very often lands on index 1
with index 0 empty. Everything measured about your hands then went to NOBD's player
2 record while every screen in the app reads player 1.

The result was an app that showed nothing for a stick that was working perfectly:
no finger gap, no recommendation, an empty tape. Slots are now claimed
first-come-first-served on the first successful read, so the first stick to answer
is player 1 whatever index the game found it on — and the app shows which pad it
claimed, because an assumption you can see is one you can question.

A pad on index 2 or higher was previously dropped entirely.

### The tester no longer lags behind your hands

The reader threads had no way to wake the UI. Nothing was ever lost — the channel is
drained in full — but it was only drained when the UI happened to repaint, and the
repaint schedule falls to 500 ms whenever nothing is "animating". The finger gap
tester runs in exactly that state, because the game being closed is what makes every
other liveness term false. So the first press of a burst sat unseen for up to half a
second.

Both readers now wake the UI on any edge, and there is a third repaint tier: 33 ms
while you are actively pressing, 100 ms while something is merely live, 500 ms for a
static screen.

### Fixed

- **A press whose window had expired no longer waits a frame.** The window runs on a
  1.4 kHz thread, so its commit could land up to 700 µs after the deadline it was
  aiming at. If the game's read fell in that gap, a press that was genuinely ready
  cost a full frame — 700 µs of our own scheduling slop costing 16.7 ms, on roughly
  4% of presses. The decision is now taken at the game's read.
- **The virtual-controller instructions are gone for good.** With Marvel running the
  hook DLL cannot be replaced, and the app read that as "the hook isn't the path" and
  fell back to telling you to open your game's controller settings and pick "NOBD
  Controller (Xbox 360)" — a device that has not existed since v0.7.0 replaced the
  virtual pad. A pending DLL update is not a reason to offer a different architecture.
- **Windowing directions is removed.** MvC2 SOCD-cleans to neutral immediately after
  it reads input, so a windowed UP+DOWN or LEFT+RIGHT was discarded by the game. The
  mode could delete a direction you were holding, and there was nothing to buy: at
  60 fps a frame is 16.7 ms and the window at most 16 ms, so a direction and a button
  share a frame regardless.
- **A hook that cannot reach its reader now passes input through instead of deleting
  it.** The delivered word is built as `(raw & !mask) | (committed & mask)`, so a
  player slot the reader had never published for delivered *zero* attack bits. Every
  failure in this path now degrades toward passthrough; missing sync for a few
  milliseconds is acceptable, a dead button never is.
- Minimising goes to the tray like the close button already did, and the 2 kHz reader
  stands down whenever the window is hidden — it was polling for an audience of
  nobody. 2.1% of one core in the tray, down from 2.7%.

### The window now shows what it costs

The slack control gave no feedback about its price, so "more slack" read as free. It
is not: the window delays every press, and the chance the game's read falls inside
that delay is exactly window ÷ frame.

    3 ms  ->  18% of single presses land a frame later than they would unaided
    5 ms  ->  30%   (the default)
    8 ms  ->  48%

Chords do not pay this — they commit the instant the second attack lands — so the
whole cost falls on presses that turned out to be alone, which is most of them.

The frame period is measured from the game's own input reads rather than assumed. On
MvC2 that is 16.69 ms, NTSC 59.94 Hz, one read per frame, 93% of intervals inside
15–17 ms.

### Under the hood

- A poll-cadence probe reports the game's real read cadence once per launch. It costs
  the game thread one atomic add and one relaxed store, stops after ~17 seconds, and
  does all its arithmetic on a thread that already exists.
- Self-update is present but **inert**: it does nothing until a signing key is
  configured. See `docs/SELF-UPDATE.md`.

---

## v0.7.0

NOBD now works **inside Marvel** instead of presenting a virtual controller. No
driver, no certificate, no admin prompt, no second Xbox pad in Steam, and no
requirement that your stick be in any particular mode. Install NOBD and play.

### The in-game hook

The game statically imports `DINPUT8.dll`, so naming our proxy that makes the
game load it itself — no injection, and no detour just to get in the door. From
inside, we transform the buttons the game just read. That inverts the coverage:

```
driver / filter :  universal across GAMES,   restricted by DEVICE type
hook            :  universal across DEVICES, restricted by GAME
```

Xbox pads, DirectInput sticks and PS pads through Steam Input all reach the game
through APIs we hook, so **your stick's mode no longer matters**. It is also
strictly lower latency than the virtual pad, which inherited a submit, an
emulated endpoint and `xusb22` sampling — roughly a millisecond that simply is
not there when no device is inserted.

- **Plug and play.** The app finds Marvel (Steam registry + `libraryfolders.vdf`)
  and installs the hook itself. No prompt, because there is nothing to prompt
  about: Steam grants `BUILTIN\Users` FullControl on its game folders, so this
  needs no elevation at all.
- **It self-updates, and checks properly.** The installed DLL is compared against
  the one we ship rather than merely tested for existence — a stale build had sat
  in a game folder for two months, compiled against an older shared-memory
  layout, resetting the window and re-enabling sync on every launch and then
  applying a second sync window on top. "A DLL is there" is not "the right DLL is
  there".
- **Continuous only.** The old code had a Block mode that stalled the game's
  input read by up to 8 ms to group inside one frame. Stalling the game thread is
  the one thing here that can disturb rollback netcode, and it is not coming
  back. The game's read never waits on us: a background thread runs the window on
  its own clock and the read samples the committed result.
- The window itself is the shared implementation the app and the Linux daemon
  run, with the same 18 tests — so every correctness fix from v0.6.0 applies
  in-game.

### Honest status

The app answers the three questions a player actually asks, in order: is Marvel
here, is NOBD in it, is it working right now.

- Liveness comes from the heartbeat **moving**, not from being non-zero — that
  value persists in shared memory after the game exits, so a non-zero test would
  claim the hook was live with no game running.
- It refuses to say READY while presses are passing straight through, and says
  "close Marvel to finish" instead of spinning on "installing" when the game
  holds the DLL.
- The virtual-controller UI comes off the screen entirely when the hook is the
  path. It used to sit underneath the hook card contradicting it.

### The extra Xbox controller

- **The NOBD Controller now exists if and only if NOBD is on.** Its only reason
  to exist is to carry synced input, so leaving it in Windows with sync off just
  put a second, identical Xbox pad in Steam that nothing explained — XInput
  exposes no device identity, so it could not even be told apart by name.
- Quitting removes it too, unless you ask to keep it.
- Removal works from a non-elevated app now. It used to fail with "eject requires
  elevation" and no way to act on that, which is why it could not be removed.
- A prompted migration takes the whole old stack off: driver, certificate, logon
  task and the controller. Prompted, never silent — deleting a certificate from
  the machine root store behind someone's back is what the install disclosure
  exists to prevent, even when it is the right outcome.

### Performance

The app was pegging a core, and most of it was measurable rather than guessable.

| | before | after |
|---|---|---|
| closed to tray | **100.5%** | **0.5%** |
| in game, window open | 22% | ~4% |

- **The tray spin was already in v0.6.0.** A frame counter showed `update()` was
  never called while hidden, so it was not app code at all: with the window
  invisible there is no surface to present and eframe's loop free-runs. Fixed by
  minimising and stripping the taskbar button instead of hiding — Windows reports
  a minimised window as visible, so the loop keeps blocking.
- **The app stands down while you play.** It was polling `XInputGetState` at
  2 kHz to measure a finger gap the in-game hook already measures — duplicate
  work, and a second consumer of the same API competing with the game's own input
  reads during the one activity where latency is the point.
- A `tasklist` process spawn and a 742 KB file comparison were running four times
  a second on the render thread; both moved off it, and the process spawn is now
  an in-process snapshot.
- The event log laid out all 500 rows every frame; it is virtualised now.
- The gap tester's reader backs off from 2 kHz to 8 ms after two quiet seconds
  and snaps back on the first button edge.

### Fixed

- The tape reported "NOBD off" on every row while the card above it correctly
  said the hook was live — it tested for the virtual controller, which is absent
  by design once the hook is the path.
- Rows printed the button names twice and never showed the gap.
- `enabled` was half of the virtual-controller switch, so anyone who turned that
  off carried it into a build where it means the hook does nothing at all.
- The hook's diagnostic log opened in append mode and never truncated — found at
  13.8 MB in a live game. Truncated per launch, 1 MiB ceiling.

---

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

### Packaging

- **Ships as an installer** (`NOBD-Desktop-Setup-0.6.0.exe`) rather than a ZIP:
  Program Files, Start Menu and desktop shortcuts, and an Add/Remove Programs
  entry. The portable ZIP is still attached.
- **A real uninstall.** Removing NOBD from Add/Remove Programs now runs
  `nobd.exe --uninstall`, which un-cloaks your stick *first* (deleting the files
  under an active cloak would leave it invisible in every game with nothing left
  to undo it), releases the virtual pad, removes the devnodes, the logon task,
  both driver packages, the signing certificate from Root and TrustedPublisher —
  keyed on its thumbprint, never a subject name — and the saved settings.
- The installer deliberately does **not** install the driver itself. That stays
  in the app, behind its own disclosure of what it adds to the machine.

### Docs

- The README documented a DLL-injection architecture that no longer exists —
  `DINPUT8.dll`, an Install tab, a "hook LIVE" banner, a Button Monitor tab.
  Rewritten for the virtual-controller design.

---

## v0.5.1 and earlier

See the commit log; this file starts at v0.6.0.
