# NOBD Desktop

**Fix the frame-boundary input problem in *Marvel vs. Capcom 2* (Fighting Collection, Steam) — in software, on modern hardware.**

NOBD Desktop brings the [GP2040-CE NOBD](https://github.com/t3chnicallyinclined/GP2040-CE-NOBD) sync window to the PC version of MvC2. It groups near-simultaneous attack presses (LP+HP for a dash, an assist call alongside an action, etc.) so they land on the **same game frame** instead of getting split into a stray jab — without a NOBD stick.

---

## The problem it fixes

Old arcade/console games like MvC2 read your controller **once per frame — 60 times a second, every 16.67 ms** — locked to the original hardware's refresh. On that hardware the controller and the game's read were tightly coupled, so two buttons pressed "together" always landed together.

On modern hardware (and emulation) your controller updates far faster (1000 Hz+) than the game still reads (60 Hz). When you press two buttons a few ms apart — your natural **finger gap** — the game's single 60 Hz read can land **between** them and see only the first button. A dash becomes a stray jab, an assist drops, a tech is missed. Not because you mis-input — because the read sampled at the wrong instant.

NOBD Desktop watches the game's input read: when it catches a lone attack, it checks whether the partner is arriving and delivers them together.

---

## How it works

The game (`MarvelVsCapcomFightingCollection.exe`) reads its pad through **`XINPUT1_3.dll`** (Steam Input presents your stick as a virtual Xbox pad). NOBD Desktop ships a `DINPUT8.dll` proxy that the game already imports; from inside the process it **inline-hooks `XInputGetState`** and applies the sync window to the pad's buttons right at the game's read.

Two components share a small block of named memory, so the app drives the in-game hook live:

```
        ┌──────── shared memory "Local\NobdSyncState" ────────┐
        │  config: enabled · window · mode · settle           │
        │  stats:  saves · groups · latency · finger-gap · fps │
        └──────────────────────────────────────────────────────┘
              ▲                                   ▲
        ┌─────┴──── nobd.exe ────┐         ┌──────┴──── DINPUT8.dll ────┐
        │  tray + control panel  │         │  XInput hook (in the game) │
        │  finger-gap tester     │         │  headless                  │
        └────────────────────────┘         └────────────────────────────┘
```

### Two latch modes

| Mode | Latency | Use |
|------|---------|-----|
| **Block** | Sub-frame (~1 ms grouped) | **Offline / training only.** Holds the game's input read open a few ms so attacks land on the same frame. |
| **Defer** *(default)* | +1 frame for a lone press; 0 frames for already-grouped | **Online-safe.** Returns instantly and delivers the group on the next frame. No thread stall. |

> **⚠ Online / rollback netplay: use Defer.** Block stalls the game thread for a few ms, which disrupts rollback netcode's frame pacing — it can cause stutter and even disconnects. Defer never stalls, so it's safe alongside netplay. (Grouping itself never desyncs — both clients still see identical inputs; it's only the *stall* that matters.)

Directions are **never** delayed by default, so motion tech (fast fly / refly, triangle dashing, wavedashes) stays frame-tight. There's a "Window directions too" option for firmware-exact testing — **not recommended for play.**

---

## Install / Use

> Portable — no installer, no admin rights. Just two files.

1. Download the latest release ZIP (or build from source, below).
2. Copy **`DINPUT8.dll`** next to the game executable:
   `…\steamapps\common\MARVEL vs. CAPCOM Fighting Collection\DINPUT8.dll`
3. Run **`nobd.exe`** — it lives in the system tray (teal dot).
   - **Left-click** the tray icon → open the control panel.
   - **Right-click** → quick settings (mode, window, enable).
4. Launch MvC2. The control panel shows **"In-game hook LIVE"** when it's wired in, and stats stream as you play.

To uninstall: delete `DINPUT8.dll` from the game folder.

### Steam Input note

The game reads the pad via XInput thanks to **Steam Input** presenting your controller as an Xbox pad. Make sure Steam Input is enabled for the game (Steam → game → Controller → enabled).

---

## The control panel (`nobd.exe`)

- **NOBD Sync** — enable, mode (Block/Defer), window size, settle; live in-game stats:
  - **Frame-boundary splits caught** — provable saves: a lone attack the poll read whose partner arrived before the next poll (would have split without NOBD).
  - Grouped / singles, added latency, your measured **finger gap** (block mode), **game frame time / FPS**, and a **recommended window**.
- **Gap Tester** — measure your finger timing offline: pairs, average/median, strays, bounces, histogram, recommended NOBD value.
- **Button Monitor** — per-button press counts, hold durations, repress gaps.

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
