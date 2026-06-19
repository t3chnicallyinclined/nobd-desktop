# NOBD Desktop

**Fix the frame-boundary input problem in *Marvel vs. Capcom 2* (Fighting Collection, Steam) — in software, on modern hardware.**

NOBD Desktop brings the [GP2040-CE NOBD](https://github.com/t3chnicallyinclined/GP2040-CE-NOBD) sync window to the PC version of MvC2. It groups near-simultaneous attack presses (LP+HP for a dash, an assist call alongside an action, etc.) so they land on the **same game frame** instead of getting split into a stray jab — without a NOBD stick.

<!-- Hero shot: the NOBD Sync tab with the hook LIVE and stats flowing -->
![NOBD Desktop control panel](docs/images/control-panel.png)

> 📖 **New here?** See the [**Usage Guide**](docs/USAGE.md) — a page-by-page walkthrough (Install, NOBD Sync, and the Finger Gap Tester, including how to read it and pick your window).

---

## The problem it fixes

Old arcade/console games like MvC2 read your controller **once per frame — 60 times a second, every 16.67 ms** — locked to the original hardware's refresh. On that hardware the controller and the game's read were tightly coupled, so two buttons pressed "together" always landed together.

On modern hardware (and emulation) your controller updates far faster (1000 Hz+) than the game still reads (60 Hz). When you press two buttons a few ms apart — your natural **finger gap** — the game's single 60 Hz read can land **between** them and see only the first button. A dash becomes a stray jab, an assist drops, a tech is missed. Not because you mis-input — because the read sampled at the wrong instant.

NOBD Desktop watches the game's input read: when it catches a lone attack, it checks whether the partner is arriving and delivers them together.

---

## How it works

The game (`MarvelVsCapcomFightingCollection.exe`) reads its pad through **`XINPUT1_3.dll`** (Steam Input presents your stick as a virtual Xbox pad). NOBD Desktop ships a `DINPUT8.dll` proxy that the game already imports; from inside the process it **inline-hooks `XInputGetState`**.

A background **~1 kHz poll thread** reads your stick continuously and runs the sync window on its *own* fine clock — exactly like the controller firmware does. The game's read then just **samples the already-grouped result** (lock-free); directions and held buttons come straight from a fresh read, so motion inputs stay frame-tight. Because the window resolves off the game's 60 Hz cadence, a lone attack only costs a frame when it lands in the last few ms before a read (**~18% of presses** in testing) instead of every time.

Two components share a small block of named memory, so the app drives the in-game hook live and reads its stats:

```
        ┌──────────── shared memory "Local\NobdSyncState" ────────────┐
        │  config: enabled · window · mode · directions               │
        │  stats:  splits caught/missed · groups · poll rate ·         │
        │          input latency · waited-a-frame · finger gap · fps   │
        └──────────────────────────────────────────────────────────────┘
              ▲                                   ▲
        ┌─────┴──── nobd.exe ────┐         ┌──────┴──── DINPUT8.dll ────┐
        │  tray + control panel  │         │  XInput hook + poll thread │
        │  finger-gap tester     │         │  (in the game, headless)   │
        └────────────────────────┘         └────────────────────────────┘
```

### Latch modes

**Continuous** is the default and the only mode exposed in the UI. The two older modes still exist in the DLL (commented out of the UI) and are documented here for context.

| Mode | What it does | Latency | Online-safe? |
|------|--------------|---------|--------------|
| **Continuous** *(default)* | A ~1 kHz poll thread runs the window on its own clock; the game samples the committed result. Like the controller firmware. | Lone press costs +1 frame **only** if it lands in the last few ms before a read (~18%); grouped presses 0. | ✅ Yes — never stalls the game thread |
| **Defer** *(legacy)* | Per-read state machine: holds a lone press and delivers the group on the next read. | +1 frame on **every** lone press; 0 for already-grouped. | ✅ Yes — no stall |
| **Block** *(legacy)* | Stalls the game's input read open a few ms to group within the *same* frame. | Sub-frame (~1 ms grouped). | ❌ **Offline/training only** |

> **Why Continuous wins:** Defer is online-safe but pays a frame on every lone attack; Block is low-latency but stalls the game thread, which disrupts rollback netcode (stutter/disconnect risk). Continuous is **non-blocking *and* low-latency** — it resolves the window on a fine clock so most presses land on the same frame anyway. Grouping never desyncs netplay (both clients see identical inputs); only a *stall* would, and Continuous never stalls.

Directions are **never** delayed, so motion tech (fast fly / refly, triangle dashing, wavedashes) stays frame-tight. A "Window directions too" option exists for firmware-exact testing — **not recommended for play.**

---

## Install / Use

> Portable — no installer, no admin rights. Just two files.

1. Download the latest release ZIP (or build from source, below). Keep **`DINPUT8.dll`** and **`nobd.exe`** together in one folder.
2. Run **`nobd.exe`** — it lives in the system tray (teal dot). **Left-click** to open; **right-click** for quick settings (window, enable).
3. On the **Install** tab, click **"Install to game"** — it auto-detects your Steam library and copies `DINPUT8.dll` next to the game. (Or copy it there yourself: `…\steamapps\common\MARVEL vs. CAPCOM Fighting Collection\DINPUT8.dll`.) The Install tab can also create a desktop shortcut.
4. Launch MvC2. A status banner at the top of the panel shows **"In-game hook LIVE"** when it's wired in, and stats stream as you play.

**Status banner** (shown on every tab): 🔴 *DLL not installed* (with a one-click jump to Install) → 🟡 *DLL installed — launch MvC2* → 🟢 *In-game hook LIVE*.

Once installed, the DLL loads with the game automatically every launch. If you install it while the game is already running, restart the game once. `nobd.exe` is just the control panel — the sync works without it open.

To uninstall: use **Uninstall** on the Install tab, or delete `DINPUT8.dll` from the game folder.

### Steam Input note

The game reads the pad via XInput thanks to **Steam Input** presenting your controller as an Xbox pad. Make sure Steam Input is enabled for the game (Steam → game → Controller → enabled).

---

## The control panel (`nobd.exe`)

- **NOBD Sync** — enable/disable the sync, set the window size, and read the live in-game stats (below).
- **Finger Gap Tester** — measure your finger timing directly from the pad (works with or without the game): pairs, average/median, fastest/slowest, strays, bounces, a histogram, and a recommended NOBD value.
- **Button Monitor** — per-button press counts, hold durations, and repress gaps.
- **Install** — auto-detect the Steam install, install/uninstall the DLL, and create a desktop shortcut.

<!-- The Finger Gap Tester tab: histogram + recommended NOBD value -->
![Finger Gap Tester](docs/images/finger-gap-tester.png)

<!-- The Install tab: auto-detected Steam path + Install/Uninstall buttons -->
![Install tab](docs/images/install-tab.png)

---

## Stats explained

All stats come from the in-game hook over shared memory and update live. The top **Reset** button clears them.

### When sync is ON

| Stat | What it means | Good values |
|------|---------------|-------------|
| **Frame-boundary splits caught** *(headline)* | Provable saves — a grouped delivery where a member actually crossed a game frame, so without NOBD it would have been read alone on an earlier frame (a split). The subtitle is `saves / groups`. | climbs as you play |
| **Poll rate** | How fast **our** background thread samples Windows' XInput API — a health check that the sampler is alive. Set by our poll loop (~1 kHz target), **not** the board's USB report rate (your GP2040 already reports to Windows at 1000 Hz upstream). | ~900–1000 Hz |
| **Input latency (press→game)** | True input latency: from the physical press (timestamped by the poll thread) to the first game read that actually delivers it — **frame quantization included**. | avg < 8 ms |
| **Waited a frame** | Of delivered presses, how many actually paid a +1-frame cost (the press was withheld across a game read). This is the real latency cost, shown as `X of Y (Z%)`. | low % (~18%) |
| **Grouped (2+)** / **Singles** | Commits delivered with 2+ attack buttons together vs. solo presses. | — |
| **Grouping hold (lead wait)** | How long the system holds a lead press waiting for a partner (the window hold on the fine clock — *not* net added latency, which is the two rows above). | ≤ window |
| **Your finger gap** | Measured time between the two presses of your grouped inputs. Drives the **recommended window**. | your natural gap |
| **Game frame time** | Frame interval derived from the game's read cadence. | ~16.67 ms (60 fps) |

<!-- Sync ON: green "splits caught" headline + the full live stats grid -->
![Live stats with sync ON](docs/images/stats-on.png)

### When sync is OFF — passive monitor

Turn the sync off and the poll thread keeps watching (it doesn't correct anything) so you can see the problem first-hand:

| Stat | What it means |
|------|---------------|
| **Frame-boundary splits MISSED** *(headline)* | Of your gapped two-button inputs, how many the game **actually split** across a frame (a missed dash / stray jab) — the same straddle test as "saves," just measuring instead of fixing. Shown as `X of Y attempts (Z%)`. |
| **Your finger gap** | Still measured, so the recommended window works even with sync off. |

Flip sync **OFF → play a set → ON → play a set** for a direct before/after: misses (off) become saves (on).

<!-- Sync OFF: red "splits MISSED" headline (passive monitor) -->
![Passive monitor with sync OFF](docs/images/stats-off.png)

---

## Build from source

Requires the [Rust toolchain](https://rustup.rs) (MSVC, x64).

```sh
cargo build --release
# DLL  → target/release/DINPUT8.dll  (copy next to the game exe)
# app  → target/release/nobd.exe
```

Workspace layout:

| Crate | Output | Role |
|-------|--------|------|
| `nobd-desktop` (root) | `DINPUT8.dll` | The in-game XInput/DInput hook (headless) |
| `app/` | `nobd.exe` | Tray control panel + finger-gap tester (egui) |
| `shared/` | lib | The shared-memory config/stats struct, mapped by both |

---

## Credits

- Sync-window concept from **[GP2040-CE NOBD](https://github.com/t3chnicallyinclined/GP2040-CE-NOBD)** firmware.
- Finger-gap tester UI adapted from the **NOBD Finger Gap Tester**.
- Inline hooking via [`retour`](https://crates.io/crates/retour); UI via [`egui`/`eframe`](https://github.com/emilk/egui).

## License

MIT — see [LICENSE](LICENSE).
