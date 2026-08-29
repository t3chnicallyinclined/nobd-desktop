# NOBD Desktop

> ### NOBD keeps your inputs honest and your intent intact.
> **Execution is back. No more fighting a broken pipeline between your hands and the game.**

Frame-sync for the PC version of *Marvel vs. Capcom 2* (Fighting Collection, Steam). Your stick updates at 1000 Hz; the game reads at 60. NOBD groups your near-simultaneous attack presses so they land on the **same game frame** — your dash is a dash, not a stray jab — no NOBD stick required. It brings the [GP2040-CE NOBD](https://github.com/t3chnicallyinclined/GP2040-CE-NOBD) sync window to PC, in software.

<!-- Hero shot: the one screen, sync working -->
![NOBD Desktop control panel](docs/images/control-panel.png)

> 📖 **New here?** See the [**Usage Guide**](docs/USAGE.md) — a page-by-page walkthrough (Install, NOBD Sync, and the Finger Gap Tester, including how to read it and pick your window).

> ## ⚡ You're using the free fix. There's a board coming.
>
> NOBD Desktop is the sync window in software. It is also built into **the most over-engineered fightstick PCB ever designed**: dual MCU, up to 16 kHz USB, native retro consoles, a 40x-faster LAN target, fully open. Pre-prototype, built in public.
>
> **The Founding 100 is open: the first 100 reservations lock $150 (retail lands at $199).** No payment now, nothing binding. Show support, or are you just insane enough to want the fastest, most over-engineered fightstick ever made? **[Reserve yours at zero.nobd.net/build →](https://zero.nobd.net/build?utm_source=nobd-desktop-repo&utm_medium=repo)**

---

## The problem it fixes

Old arcade/console games like MvC2 read your controller **once per frame — 60 times a second, every 16.67 ms** — locked to the original hardware's refresh. On that hardware the controller and the game's read were tightly coupled, so two buttons pressed "together" always landed together.

On modern hardware (and emulation) your controller updates far faster (1000 Hz+) than the game still reads (60 Hz). When you press two buttons a few ms apart — your natural **finger gap** — the game's single 60 Hz read can land **between** them and see only the first button. A dash becomes a stray jab, an assist drops, a tech is missed. Not because you mis-input — because the read sampled at the wrong instant.

NOBD Desktop watches the game's input read: when it catches a lone attack, it checks whether the partner is arriving and delivers them together.

**It's not your execution — it's the read. NOBD fixes the read.**

---

## How it works

NOBD adds a **virtual controller** to Windows — the *NOBD Controller* — reads your real stick, and gives the game the grouped result. There is no DLL to copy into a game folder and nothing is injected into any process; it works in every game, not just one.

```
   your stick  ──▶  nobd.exe  ──▶  NOBD Controller  ──▶  your game
                    (sync window)   (virtual pad Windows
                                     and Steam can see)
```

A background **~1 kHz loop** reads your stick and runs the sync window on its own fine clock, exactly like the controller firmware does. When it catches a lone attack it holds it for at most the window; if a partner arrives inside that window, **both leave in one report**, so the game physically cannot read one without the other. Directions are never delayed, so motion tech (fast fly / refly, triangle dashing, wavedashes) stays frame-tight.

### The contract

NOBD changes **when** an edge reports. It never changes **which** buttons report, and never **how long** you held them.

- A press is held for at most the window, and is delivered early the moment two attacks are held — nothing left to wait for, so no added delay.
- **A press is never deleted.** A tap shorter than the window is still delivered, shifted later.
- **Pulse width is preserved.** A release is delayed by the same amount its press was, so a 40 ms hold reaches the game as a 40 ms hold.

The same window runs in three places from one implementation (`shared/src/sync_window.rs`): the Windows app, the Linux daemon (`nobdd`), and the UMDF HID filter. The C++ port is kept behaviour-identical by a parity test suite.

### Input sources

| Source | When it's used |
|---|---|
| **XInput** | Xbox pads and anything Windows presents as one. The default. |
| **DirectInput** | A stick Windows does not expose to XInput — chosen automatically, overridable under Details. |
| **NOBD Bulk** | A NOBD stick streaming over WinUSB (~10 kHz) instead of its USB poll. Auto-detected. |

---

## Install / Use

1. Download **`NOBD-Desktop-Setup-x.y.z.exe`** and run it. (A portable ZIP is also attached if you prefer — keep `nobd.exe` and the `driver/` folder together.)
2. Open NOBD and click **Install NOBD Controller**. Windows asks for admin once — this adds a small signed driver and the virtual controller. The app tells you exactly what it installs before you click.
3. In your game's controller settings, select **"NOBD Controller (Xbox 360)"**. Nothing changes in-game until you do.

That's it. The controller stays in Windows after a restart, so it keeps working without opening the app. Tick **Start with Windows** if you want the tray app back at login too.

The app answers one question on its front screen: **are your dashes coming out?** Press two punches together and it tells you how many landed together, how far apart your fingers actually were, and how many would have split across a frame without it.

**Removing it:** uninstall *NOBD Desktop* from Add/Remove Programs. That removes everything it added — the controller, the driver package, the signing certificate, the logon task — and un-hides your stick. To drop just the virtual controller and keep NOBD installed, use *Details → Remove NOBD Controller*.

---

## The app

- **One screen** — the sync switch, the window, and a live answer to "is it working".
- **Every two-button press** is listed as *what your hands did → what the game got*, so the before/after is visible while you play.
- **Details** — your finger-gap breakdown, the event log, input/output overrides, and removal.

---

## What the numbers mean

Everything on the front screen is measured **inside the sync loop**, so it is true on every input source — including NOBD Bulk, where Windows does not see your stick as a controller at all.

| Number | What it means |
|---|---|
| **dashes landed** | Two-button presses NOBD delivered in one report. The game cannot split these — the risk is exactly zero, at any timing. |
| **dropped** | Two-button presses that did not land together. If this is climbing, the window is tighter than your hands; the app offers you a wider one. |
| **saved by NOBD** | How many of the grouped presses would have split across a 60 Hz frame unaided. An expectation (`gap ÷ 16.67 ms`) summed over your presses, not a count of events — we cannot see the game's clock, so this is the odds, honestly labelled. |
| **your fingers, apart** | The raw gap between your two presses, measured before the window touches it. |

The event log's right-hand column shows the odds a 60 Hz game splits each press — *about 1 in 5* rather than a percentage. Where the answer is certain it says so: *always safe*, or *drops every time*.

---

## Build from source

Requires the [Rust toolchain](https://rustup.rs) (MSVC, x64).

```sh
cargo build --release
# app  → target/release/nobd.exe
```

Workspace layout:

| Crate | Output | Role |
|-------|--------|------|
| `app/` | `nobd.exe` | Tray control panel + finger-gap tester (egui) |
| `shared/` | lib | The shared-memory config/stats struct, mapped by both |

---

## Credits

- Sync-window concept from **[GP2040-CE NOBD](https://github.com/t3chnicallyinclined/GP2040-CE-NOBD)** firmware.
- Finger-gap tester UI adapted from the **NOBD Finger Gap Tester**.
- Inline hooking via [`retour`](https://crates.io/crates/retour); UI via [`egui`/`eframe`](https://github.com/emilk/egui).

## License

MIT — see [LICENSE](LICENSE).
