use eframe::egui;
use egui::{Color32, RichText, ScrollArea, Ui};

use crate::input::{format_button, GamepadInput, InputEvent};
use crate::monitor::ButtonMonitor;
use crate::stats::GapStats;

const TEAL: Color32 = Color32::from_rgb(0, 180, 216);
const GREEN: Color32 = Color32::from_rgb(80, 200, 80);
const YELLOW: Color32 = Color32::from_rgb(220, 180, 40);
const RED: Color32 = Color32::from_rgb(220, 60, 60);
const ORANGE: Color32 = Color32::from_rgb(220, 140, 40);
const LOG_MAX: usize = 500;

// Color for a recommended-window / finger-gap value (ms). The whole 0–16ms range
// is legitimate (16ms = one frame, the original contract); the color tracks
// latency + consistency over four tiers:
//   ≤5 green (within debounce — essentially simultaneous) · 6–9 yellow (good,
//   ~avg) · 10–12 orange (looser) · 13–16 red (near the frame ceiling)
fn rec_color(ms: u32) -> Color32 {
    if ms <= 5 {
        GREEN
    } else if ms <= 9 {
        YELLOW
    } else if ms <= 12 {
        ORANGE
    } else {
        RED
    }
}

/// One player's live in-game stats, drawn into a column.
fn draw_player_live(
    ui: &mut Ui,
    ps: &nobd_shared::PlayerStats,
    p: usize,
    enabled: bool,
    s: &nobd_shared::SharedState,
) {
    use std::sync::atomic::Ordering;
    let groups = ps.groups.load(Ordering::Relaxed);
    let singles = ps.singles.load(Ordering::Relaxed);
    let saves = ps.saves.load(Ordering::Relaxed);
    let misses = ps.misses.load(Ordering::Relaxed);
    let (gap_avg, gap_max) = ps.finger_gap_ms();
    let rec = ps.recommended_window_ms();
    let frame_us = ps.frame_us.load(Ordering::Relaxed);
    let (gp_avg, gp_max) = ps.game_perceived_ms();
    let waits = ps.frame_waits.load(Ordering::Relaxed);
    let dels = ps.gp_lat_count.load(Ordering::Relaxed);

    let head = if ps.active() { TEAL } else { Color32::GRAY };
    ui.label(RichText::new(format!("Player {}", p + 1)).strong().size(15.0).color(head));

    if enabled {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{saves}")).size(26.0).strong().color(GREEN));
            ui.label("splits caught");
        });
    } else {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{misses}")).size(26.0).strong().color(RED));
            ui.label(RichText::new("splits MISSED").color(RED));
        });
    }

    egui::Grid::new(format!("pstats_{p}")).num_columns(2).spacing([10.0, 3.0]).show(ui, |ui| {
        ui.label("Grouped / singles:");
        ui.label(format!("{groups} / {singles}"));
        ui.end_row();
        ui.label("Input latency:");
        if gp_max > 0.0 {
            ui.colored_label(
                if gp_avg < 8.0 { GREEN } else if gp_avg < 16.0 { YELLOW } else { ORANGE },
                format!("{gp_avg:.1} / {gp_max:.1} ms"),
            );
        } else { ui.weak("—"); }
        ui.end_row();
        ui.label("Waited a frame:");
        if dels > 0 {
            let pct = waits as f64 / dels as f64 * 100.0;
            ui.colored_label(
                if pct < 35.0 { GREEN } else if pct < 60.0 { YELLOW } else { ORANGE },
                format!("{waits}/{dels} ({pct:.0}%)"),
            );
        } else { ui.weak("—"); }
        ui.end_row();
        ui.label("Finger gap:");
        if gap_max > 0.0 {
            ui.colored_label(rec_color(gap_avg.round() as u32), format!("{gap_avg:.1} / {gap_max:.1} ms"));
        } else { ui.weak("—"); }
        ui.end_row();
        ui.label("Frame time:");
        if frame_us > 0 {
            ui.label(format!("{:.2} ms", frame_us as f64 / 1000.0));
        } else { ui.weak("—"); }
        ui.end_row();
    });

    // This player's own sync window.
    let mut win = s.window_ms[p].load(Ordering::Relaxed);
    if ui.add(egui::Slider::new(&mut win, 1..=16).suffix(" ms").text("window")).changed() {
        s.window_ms[p].store(win, Ordering::Relaxed);
    }
    if rec > 0 {
        ui.horizontal(|ui| {
            ui.label("Rec:");
            ui.colored_label(rec_color(rec), RichText::new(format!("{rec} ms")).strong());
            if ui.small_button("Apply").clicked() {
                s.window_ms[p].store(rec, Ordering::Relaxed);
            }
        });
    }
}

// Last DLL heartbeat we saw, to detect whether the in-game hook is actively polling.
static LAST_HB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    NobdSync,
    GapTester,
    ButtonMonitor,
    Install,
}

enum GapLogEntry {
    Pair {
        controller: usize,
        attempt: usize,
        button_a: String,
        button_b: String,
        count: usize,
        gap_ms: f64,
        running_avg: f64,
        /// Would a 60 fps game have read the two presses on different frames?
        split: bool,
    },
    Stray {
        controller: usize,
        button: String,
        solo_ms: f64,
        reason: &'static str,
        off_time_ms: Option<f64>,
    },
    Bounce {
        controller: usize,
        button: String,
        off_ms: f64,
    },
}

pub struct FingerGapApp {
    input: Option<GamepadInput>,
    // Per-controller finger-gap stats / counts (index = controller slot).
    stats: Vec<GapStats>,
    stray_counts: Vec<usize>,
    bounce_counts: Vec<usize>,
    // Monotonic chords per controller (the log's "#N", independent of the window).
    total_pairs: Vec<usize>,
    gap_log: Vec<GapLogEntry>,
    monitor: ButtonMonitor,
    active_tab: Tab,
    error_msg: Option<String>,
    tray: Option<crate::tray::Tray>,
    game_path: String,
    install_msg: String,
    last_cfg: crate::persist::Cfg,
    /// Sliding-window size (recent chords) the grouping verdict is judged over.
    decision_window: usize,
}

impl FingerGapApp {
    pub fn new(ctx: &egui::Context) -> Self {
        // Restore saved settings into shared memory before anything reads it.
        let last_cfg = crate::persist::load();
        let (input, error_msg) = match GamepadInput::new() {
            Ok(gi) => (Some(gi), None),
            Err(e) => (None, Some(format!("Gamepad init failed: {e}"))),
        };
        let game_path = crate::install::find_game_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        Self {
            input,
            stats: Vec::new(),
            stray_counts: Vec::new(),
            bounce_counts: Vec::new(),
            total_pairs: Vec::new(),
            gap_log: Vec::new(),
            monitor: ButtonMonitor::new(),
            active_tab: Tab::NobdSync,
            error_msg,
            tray: crate::tray::spawn(ctx.clone()),
            game_path,
            install_msg: String::new(),
            last_cfg,
            decision_window: crate::stats::DEFAULT_WINDOW,
        }
    }
}

impl FingerGapApp {
    /// Grow the per-controller vectors so index `c` is valid.
    fn ensure_pad(&mut self, c: usize) {
        if self.stats.len() <= c {
            self.stats.resize_with(c + 1, GapStats::new);
            self.stray_counts.resize(c + 1, 0);
            self.bounce_counts.resize(c + 1, 0);
            self.total_pairs.resize(c + 1, 0);
        }
    }

    fn push_log(&mut self, entry: GapLogEntry) {
        self.gap_log.push(entry);
        if self.gap_log.len() > LOG_MAX {
            self.gap_log.remove(0);
        }
    }

    fn draw_install(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("Install NOBD into MvC2");
            ui.add_space(8.0);

            let dll_ok = crate::install::dll_source().is_some();
            if !dll_ok {
                ui.colored_label(
                    RED,
                    "\u{26A0} DINPUT8.dll isn't next to nobd.exe. Keep both files in the same folder.",
                );
                ui.add_space(6.0);
            }

            ui.label("MvC2 game folder:");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.game_path).desired_width(460.0));
                if ui.button("Re-detect").clicked() {
                    match crate::install::find_game_dir() {
                        Some(p) => {
                            self.game_path = p.display().to_string();
                            self.install_msg = "Found the game folder.".into();
                        }
                        None => {
                            self.install_msg =
                                "Couldn't auto-detect \u{2014} paste the game folder path above.".into()
                        }
                    }
                }
            });

            let game_dir = std::path::PathBuf::from(self.game_path.trim());
            let path_set = !self.game_path.trim().is_empty();
            let has_game = path_set && crate::install::has_game(&game_dir);
            let installed = path_set && crate::install::is_installed(&game_dir);

            ui.add_space(4.0);
            if !path_set {
                ui.colored_label(YELLOW, "No game folder set.");
            } else if !has_game {
                ui.colored_label(YELLOW, "That folder doesn't contain the MvC2 executable.");
            } else if installed {
                ui.colored_label(GREEN, "\u{2713} NOBD is installed here.");
            } else {
                ui.colored_label(TEAL, "Game found \u{2014} ready to install.");
            }

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(dll_ok && has_game, egui::Button::new("  Install to game  "))
                    .clicked()
                {
                    self.install_msg = match crate::install::install(&game_dir) {
                        Ok(()) => "Installed \u{2713}  \u{2014} launch MvC2 (close it first if it's open).".into(),
                        Err(e) => format!("Install failed: {e}"),
                    };
                }
                if ui.add_enabled(installed, egui::Button::new("  Uninstall  ")).clicked() {
                    self.install_msg = match crate::install::uninstall(&game_dir) {
                        Ok(()) => "Uninstalled.".into(),
                        Err(e) => format!("Uninstall failed: {e}"),
                    };
                }
                if ui.button("  Create desktop shortcut  ").clicked() {
                    self.install_msg = match crate::install::create_desktop_shortcut() {
                        Ok(()) => "Desktop shortcut created.".into(),
                        Err(e) => format!("Shortcut failed: {e}"),
                    };
                }
            });

            if !self.install_msg.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new(&self.install_msg).strong());
            }

            ui.add_space(14.0);
            ui.separator();
            ui.label(RichText::new("Notes").strong());
            ui.label("\u{2022} Close the game before Install / Uninstall \u{2014} the DLL is locked while it runs.");
            ui.label("\u{2022} Enable Steam Input for MvC2 (Steam \u{2192} game \u{2192} Controller).");
            ui.label("\u{2022} Windows SmartScreen may warn on first run (unsigned) \u{2014} that's expected.");
            ui.label("\u{2022} To fully remove: Uninstall here, then delete nobd.exe + DINPUT8.dll.");
        });
    }
}

impl eframe::App for FingerGapApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Close button → hide to the tray instead of quitting (Quit is in the menu).
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        // Tray ("Open NOBD" / left-click) asked to show the window — do it here on
        // the main thread, which is reliable. Restore from minimize + raise too.
        if crate::tray::WANT_SHOW.swap(false, std::sync::atomic::Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        // Keep the loop ticking even while hidden so the show flag + tray check
        // marks are picked up promptly.
        ctx.request_repaint_after(std::time::Duration::from_millis(50));

        // Keep the tray menu's check marks in sync with the live config.
        if let Some(tray) = &self.tray {
            tray.refresh_checks();
        }

        // Poll gamepad — pairs/strays/bounces are tagged per controller now.
        let poll = self.input.as_mut().map(|i| i.poll());
        if let Some(result) = poll {
            for (c, ev) in &result.events {
                match ev {
                    InputEvent::Pressed(btn) => self.monitor.on_press(*c, *btn),
                    InputEvent::Released(btn) => self.monitor.on_release(*c, *btn),
                }
            }
            // Measured USB frame size (ms) so same-frame bucketing adapts to the
            // device cadence; and the current decision window. Read once here to
            // avoid borrowing self.input while mutating self.stats below.
            let frame_ms = self
                .input
                .as_ref()
                .and_then(|i| i.report_rate_hz())
                .filter(|h| *h > 0.0)
                .map(|h| 1000.0 / h);
            let dw = self.decision_window;

            for pair in result.pairs {
                let c = pair.controller;
                self.ensure_pad(c);
                self.stats[c].set_window(dw);
                if let Some(fm) = frame_ms {
                    self.stats[c].set_frame_ms(fm);
                }
                self.stats[c].record_chord(pair.gap_ms, &pair.buttons, pair.t0_ms);
                let running_avg = self.stats[c].average();
                self.total_pairs[c] += 1;
                let attempt = self.total_pairs[c];
                // Would a free-running 60fps game poll have split this chord?
                let split = crate::stats::game_frame_split(pair.t0_ms, pair.gap_ms);
                self.push_log(GapLogEntry::Pair {
                    controller: c,
                    attempt,
                    button_a: format_button(pair.button_a),
                    button_b: format_button(pair.button_b),
                    count: pair.count,
                    gap_ms: pair.gap_ms,
                    running_avg,
                    split,
                });
            }
            for stray in result.strays {
                let c = stray.controller;
                self.ensure_pad(c);
                self.stats[c].set_window(dw);
                // A solo = a single attack button that registered alone — the tell
                // that singles still pass through (sync window, not an OBD macro).
                self.stats[c].record_solo();
                self.stray_counts[c] += 1;
                self.push_log(GapLogEntry::Stray {
                    controller: c,
                    button: format_button(stray.button),
                    solo_ms: stray.solo_ms,
                    reason: stray.reason.label(),
                    off_time_ms: stray.off_time_ms,
                });
            }
            for bounce in result.bounces {
                let c = bounce.controller;
                self.ensure_pad(c);
                self.bounce_counts[c] += 1;
                self.push_log(GapLogEntry::Bounce {
                    controller: c,
                    button: format_button(bounce.button),
                    off_ms: bounce.off_ms,
                });
            }
        }

        ctx.request_repaint_after(std::time::Duration::from_millis(1));

        // DLL install + in-game hook status, computed once per frame and shown as
        // a banner on every tab. (hook_live uses LAST_HB, so compute it only here.)
        let game_dir = std::path::PathBuf::from(self.game_path.trim());
        let dll_installed =
            !self.game_path.trim().is_empty() && crate::install::is_installed(&game_dir);
        let hb = nobd_shared::state()
            .dll_heartbeat
            .load(std::sync::atomic::Ordering::Relaxed);
        let hook_live = {
            let prev = LAST_HB.swap(hb, std::sync::atomic::Ordering::Relaxed);
            hb != prev && hb != 0
        };

        // === TOP BAR ===
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("NOBD INPUT TESTER").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Reset").clicked() {
                        // Local gap-tester stats (all controllers)…
                        self.stats.clear();
                        self.stray_counts.clear();
                        self.bounce_counts.clear();
                        self.total_pairs.clear();
                        self.gap_log.clear();
                        self.monitor.clear();
                        // …and the live in-game (shared-memory) NOBD stats.
                        nobd_shared::state().reset_stats();
                    }
                });
            });

            // Controller status
            if let Some(ref err) = self.error_msg {
                ui.colored_label(Color32::RED, format!("Error: {err}"));
            } else if let Some(ref input) = self.input {
                if let Some(name) = input.connected_gamepad_name() {
                    ui.horizontal(|ui| {
                        ui.colored_label(GREEN, "\u{25CF}");
                        ui.label(format!("Controller: {name}"));
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.colored_label(YELLOW, "\u{25CF}");
                        ui.label("No controller detected. Connect a gamepad.");
                    });
                }
            }

            // DLL / in-game hook status banner — guides install if it's missing.
            if hook_live {
                ui.colored_label(GREEN, "\u{25CF} In-game hook LIVE");
            } else if dll_installed {
                ui.colored_label(YELLOW, "\u{25CF} DLL installed \u{2014} launch MvC2 to activate the sync");
            } else {
                ui.horizontal(|ui| {
                    ui.colored_label(RED, "\u{25CF} DLL not installed.");
                    if ui.button("Open Install tab").clicked() {
                        self.active_tab = Tab::Install;
                    }
                    ui.label("so the sync loads with MvC2.");
                });
            }

            ui.separator();

            // Tabs
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut self.active_tab,
                    Tab::NobdSync,
                    RichText::new("  NOBD Sync  ").size(15.0),
                );
                ui.selectable_value(
                    &mut self.active_tab,
                    Tab::GapTester,
                    RichText::new("  Finger Gap Tester  ").size(15.0),
                );
                ui.selectable_value(
                    &mut self.active_tab,
                    Tab::ButtonMonitor,
                    RichText::new("  Button Monitor  ").size(15.0),
                );
                ui.selectable_value(
                    &mut self.active_tab,
                    Tab::Install,
                    RichText::new("  Install  ").size(15.0),
                );
            });

            // Decision window — the grouping verdict is judged over only the last
            // N chords, so it re-decides live and flips when you toggle NOBD
            // mid-session (no Reset needed). Only relevant on the Gap Tester tab.
            if self.active_tab == Tab::GapTester {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Decision window").size(12.0).color(Color32::GRAY));
                    ui.add(
                        egui::Slider::new(
                            &mut self.decision_window,
                            crate::stats::MIN_WINDOW..=crate::stats::MAX_WINDOW,
                        )
                        .suffix(" chords"),
                    )
                    .on_hover_text("How many recent chords the ON/OFF verdict is based on. Lower = flips faster when you toggle NOBD; higher = steadier.");
                });
            }
        });

        match self.active_tab {
            Tab::NobdSync => draw_nobd_sync(ctx, hook_live, dll_installed),
            Tab::GapTester => self.draw_gap_tester(ctx),
            Tab::ButtonMonitor => self.draw_button_monitor(ctx),
            Tab::Install => self.draw_install(ctx),
        }

        // Persist settings whenever they change (from the panel or the tray).
        let cfg = crate::persist::current();
        if cfg != self.last_cfg {
            crate::persist::save(&cfg);
            self.last_cfg = cfg;
        }

        // Repaint continuously so live DLL stats / gamepad input stay current.
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    }
}

// ─── NOBD SYNC TAB (controls the live DINPUT8.dll over shared memory) ───

fn draw_nobd_sync(ctx: &egui::Context, hook_live: bool, dll_installed: bool) {
    use std::sync::atomic::Ordering;
    let s = nobd_shared::state();

    egui::CentralPanel::default().show(ctx, |ui| {
        // Connection status (computed once per frame in update()).
        ui.horizontal(|ui| {
            if hook_live {
                ui.colored_label(GREEN, "\u{25CF}");
                ui.label(RichText::new("In-game hook LIVE").color(GREEN));
            } else if dll_installed {
                ui.colored_label(YELLOW, "\u{25CF}");
                ui.label("DLL installed \u{2014} launch MvC2 to activate");
            } else {
                ui.colored_label(RED, "\u{25CF}");
                ui.label("DLL not installed \u{2014} open the Install tab to set it up");
            }
        });
        // Scope note: the sync is an in-game DLL hook, not a system-wide driver.
        ui.label(
            RichText::new(
                "Note: the NOBD sync runs inside the game via the injected DLL. It only conditions \
                 inputs while MvC2 is running with the hook LIVE (above) \u{2014} it does not change \
                 your controller system-wide or in other apps.",
            )
            .size(11.0)
            .color(Color32::GRAY),
        );
        ui.separator();

        // ── Master control ──
        let mut enabled = s.enabled.load(Ordering::Relaxed) != 0;
        if ui.checkbox(&mut enabled, RichText::new("NOBD sync window").size(16.0)).changed() {
            s.enabled.store(enabled as u32, Ordering::Relaxed);
        }

        // ── Latch mode ──
        // Continuous is now the only mode (best latency + online-safe). Defer and
        // Block remain implemented in the DLL; the multi-mode selector below is
        // commented out, not deleted, so they can be re-exposed easily.
        ui.add_space(4.0);
        ui.label(RichText::new("Latch mode").strong());
        s.mode.store(2, Ordering::Relaxed); // force Continuous
        s.block_in_frame.store(0, Ordering::Relaxed);
        ui.colored_label(
            TEAL,
            "\u{25C9} Continuous: a ~1kHz background thread runs the sync window on its own clock and \
             the game samples the result \u{2014} like the stick's firmware. No thread stall \
             (online-safe), and most presses land on the same frame anyway (no unconditional +1 frame). \
             Watch \u{201C}Poll rate\u{201D} and \u{201C}Waited a frame\u{201D} below.",
        );
        /* Multi-mode selector \u{2014} disabled (Continuous-only). Uncomment to restore Defer/Block:
        let mut mode = s.mode.load(Ordering::Relaxed);
        ui.horizontal(|ui| {
            for (m, label) in [(0u32, "  Defer  "), (1u32, "  Block  "), (2u32, "  Continuous  ")] {
                if ui.selectable_label(mode == m, RichText::new(label).size(15.0)).clicked() {
                    mode = m;
                    s.mode.store(m, Ordering::Relaxed);
                    s.block_in_frame.store((m == 1) as u32, Ordering::Relaxed);
                }
            }
        });
        match mode {
            1 => { ui.colored_label(RED, "\u{26A0} Block: OFFLINE ONLY. ..."); }
            2 => { ui.colored_label(TEAL, "\u{25C9} Continuous: ..."); }
            _ => { ui.colored_label(GREEN, "\u{2713} Defer: online-safe. ..."); }
        }
        */

        ui.add_space(6.0);

        // ── Directions: testing only, clearly warned ──
        let mut dirs = s.directions_windowed.load(Ordering::Relaxed) != 0;
        if ui.checkbox(&mut dirs, "Window directions too").changed() {
            s.directions_windowed.store(dirs as u32, Ordering::Relaxed);
        }
        ui.colored_label(
            ORANGE,
            "\u{26A0} Testing only \u{2014} not recommended. Applies the window to directions as well, \
             which delays directional inputs and hurts motion tech (fast fly / refly, triangle \
             dashing, wavedashes). Leave OFF for play.",
        );

        ui.add_space(8.0);

        // ── Window size (per player — each gets its own slider in the columns below) ──
        ui.label(RichText::new("Sync window").strong());
        ui.weak(
            "Each player sets their own window below. Capped at 16 ms = one frame — the game's \
             original \"same-frame\" window. A larger window would group presses the game itself \
             would have split (an unfair reach), so 16 ms is the honest maximum. Bigger is more \
             forgiving but adds latency to a lone press; set it from each player's finger gap.",
        );

        // Settle is a Block-only knob (3-button straggler wait); no effect in
        // Continuous, so it's hidden. Uncomment if Block is re-exposed.
        /*
        ui.horizontal(|ui| {
            ui.label("Settle (3-button straggler):");
            let mut settle = s.settle_ms.load(Ordering::Relaxed);
            if ui.add(egui::Slider::new(&mut settle, 0..=3).suffix(" ms")).changed() {
                s.settle_ms.store(settle, Ordering::Relaxed);
            }
        });
        */

        ui.add_space(10.0);
        ui.separator();

        // ── Live in-game stats — both players side by side ──
        ui.horizontal(|ui| {
            ui.label(RichText::new("Live in-game stats").strong().size(16.0));
            ui.add_space(10.0);
            ui.label("Poll rate:");
            let poll_hz = s.poll_hz.load(Ordering::Relaxed);
            if poll_hz > 0 {
                ui.colored_label(if poll_hz >= 500 { GREEN } else { ORANGE }, format!("{poll_hz} Hz"));
            } else {
                ui.weak("—");
            }
        });
        ui.add_space(4.0);
        ui.columns(nobd_shared::NUM_PLAYERS, |cols| {
            for p in 0..nobd_shared::NUM_PLAYERS {
                draw_player_live(&mut cols[p], &s.players[p], p, enabled, s);
            }
        });

        egui::CollapsingHeader::new(RichText::new("\u{24D8}  What is the frame-boundary issue?").color(TEAL))
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    "Old arcade & console games like MvC2 were built to read your controller \
                     exactly ONCE per frame \u{2014} 60 times a second, every 16.67ms \u{2014} locked \
                     to the hardware's fixed refresh. On the original hardware the controller and \
                     the game's read were tightly coupled, so pressing two buttons together always \
                     landed them on the same frame.",
                );
                ui.add_space(4.0);
                ui.label(
                    "On modern hardware (and emulation) your controller updates far faster \
                     (1000Hz+) than the game still reads (60Hz). When you press two buttons a few \
                     ms apart \u{2014} your natural \u{201C}finger gap\u{201D} \u{2014} the game's single \
                     60Hz read can land BETWEEN them and see only the first button. That's the \
                     frame-boundary issue: a dash becomes a stray jab, an assist drops, a tech is \
                     missed \u{2014} not because you mis-input, but because the read sampled at the \
                     wrong instant.",
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "NOBD watches for it: when a read catches a lone button, it briefly holds \
                         the frame open to see if the partner is arriving, then delivers them \
                         together \u{2014} fixing the split with sub-frame latency. The number above \
                         counts only the cases we can prove the poll would have split.",
                    ).color(GREEN),
                );
            });
        ui.add_space(6.0);

        ui.add_space(6.0);
        if ui.button("Reset stats").clicked() {
            s.reset_stats();
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label(RichText::new("How it works").strong());
        ui.label(
            "A ~1kHz background thread reads your stick continuously and runs the sync window on \
             its own clock, just like the controller's firmware. The game samples the already-grouped \
             result whenever it reads \u{2014} no thread stall (online-safe). Near-simultaneous attacks \
             land on the same frame; a lone press only costs a frame if it lands in the last few ms \
             before a read (see \u{201C}Waited a frame\u{201D}). Directions are never delayed.",
        );
    });
}

// ─── GAP TESTER TAB ───

impl FingerGapApp {
fn draw_gap_tester(&self, ctx: &egui::Context) {
    let controllers = self
        .input
        .as_ref()
        .map(|i| i.controllers())
        .unwrap_or_default();
    let log = &self.gap_log;
    let report_hz = self.input.as_ref().and_then(|i| i.report_rate_hz());

    egui::TopBottomPanel::bottom("gap_log")
        .min_height(120.0)
        .resizable(true)
        .show(ctx, |ui| {
            ui.heading("Event Log (all controllers)");
            ui.separator();
            ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    for entry in log.iter().rev() {
                        match entry {
                            GapLogEntry::Pair {
                                controller,
                                attempt,
                                button_a,
                                button_b,
                                count,
                                gap_ms,
                                running_avg,
                                split,
                            } => {
                                let chord = if *count > 2 {
                                    format!(" ({} buttons)", count)
                                } else {
                                    String::new()
                                };
                                ui.horizontal(|ui| {
                                    ui.monospace(format!(
                                        "[C{}] #{:>3}  {} + {}{}  gap: {:5.1}ms  (avg: {:.1}ms)",
                                        controller + 1, attempt, button_a, button_b, chord, gap_ms, running_avg,
                                    ));
                                    // At 60fps, would the game read both on the same frame?
                                    if *split {
                                        ui.monospace(RichText::new("→ 60fps: SPLIT").strong().color(RED));
                                    } else {
                                        ui.monospace(RichText::new("→ 60fps: same frame").color(GREEN));
                                    }
                                });
                            }
                            GapLogEntry::Stray {
                                controller,
                                button,
                                solo_ms,
                                reason,
                                off_time_ms,
                            } => {
                                let off_str = if let Some(ot) = off_time_ms {
                                    format!("  [off: {:.1}ms]", ot)
                                } else {
                                    String::new()
                                };
                                egui::Frame::new()
                                    .inner_margin(egui::vec2(8.0, 4.0))
                                    .corner_radius(4.0)
                                    .fill(Color32::from_rgb(60, 15, 15))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(format!("C{} STRAY", controller + 1))
                                                    .size(14.0)
                                                    .strong()
                                                    .color(RED),
                                            );
                                            ui.monospace(
                                                RichText::new(format!(
                                                    "{} solo {:.1}ms  ({}){}",
                                                    button, solo_ms, reason, off_str,
                                                ))
                                                .color(Color32::from_rgb(255, 160, 160)),
                                            );
                                        });
                                    });
                            }
                            GapLogEntry::Bounce { controller, button, off_ms } => {
                                egui::Frame::new()
                                    .inner_margin(egui::vec2(8.0, 3.0))
                                    .corner_radius(4.0)
                                    .fill(Color32::from_rgb(50, 35, 10))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(format!("C{} BOUNCE", controller + 1))
                                                    .size(13.0)
                                                    .strong()
                                                    .color(ORANGE),
                                            );
                                            ui.monospace(
                                                RichText::new(format!(
                                                    "{} re-pressed after {:.1}ms",
                                                    button, off_ms,
                                                ))
                                                .color(Color32::from_rgb(255, 200, 120)),
                                            );
                                        });
                                    });
                            }
                        }
                    }
                });
        });

    egui::CentralPanel::default().show(ctx, |ui| {
        // Scope note: the tester reads the controller directly (XInput), so it
        // reflects the CONTROLLER's own behavior/firmware — it does not see this
        // app's in-game NOBD sync (that runs in the game via the DLL).
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Reads your controller directly \u{2014} this shows the controller's own input behavior \
                 (e.g. firmware-level grouping). It does NOT reflect this app's in-game NOBD sync, \
                 which conditions MvC2's inputs separately.",
            )
            .size(11.0)
            .color(Color32::GRAY),
        );
        ui.separator();
        if controllers.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Connect a controller and press two buttons together")
                        .size(16.0).color(Color32::GRAY),
                );
                ui.label(
                    RichText::new("(like LP+HP for a dash) — each controller is measured separately")
                        .size(13.0).color(Color32::DARK_GRAY),
                );
            });
            return;
        }
        // One column per connected controller — all visible at once.
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.columns(controllers.len(), |cols| {
        for (ci, (cidx, cname)) in controllers.iter().enumerate() {
        let ui = &mut cols[ci];
        let empty = GapStats::new();
        let stats: &GapStats = self.stats.get(*cidx).unwrap_or(&empty);
        ui.label(
            RichText::new(format!("C{}: {cname}", cidx + 1))
                .strong().size(14.0)
                .color(if stats.count() > 0 { TEAL } else { Color32::GRAY }),
        );
        ui.separator();

        if stats.count() > 0 {
            ui.add_space(8.0);

            // Grouping / NOBD-on detection. After the timing fix, genuinely
            // simultaneous presses read ~0ms, so a high same-frame rate means
            // presses are being grouped upstream (sync window / OBD / macro).
            let grouping_active = stats.grouping_active();
            draw_grouping_verdict(ui, stats);

            // A grouped sample corrupts the average, so the recommendation is
            // only meaningful when presses aren't being grouped upstream.
            if stats.count() > 0 && !grouping_active {
                let rec = stats.recommended_nobd();
                let col = rec_color(rec);
                egui::Frame::new()
                    .inner_margin(12.0)
                    .corner_radius(8.0)
                    .stroke(egui::Stroke::new(2.0, col))
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("RECOMMENDED NOBD VALUE")
                                    .size(14.0)
                                    .color(Color32::GRAY),
                            );
                            ui.label(
                                RichText::new(format!("{rec} ms")).size(48.0).strong().color(col),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "covers 95% of your gaps (p95 {:.1}ms) + 1ms headroom",
                                    stats.percentile(0.95)
                                ))
                                .size(12.0)
                                .color(Color32::GRAY),
                            );
                        });
                    });
                ui.add_space(6.0);
            }
        } else {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Press two buttons at the same time to start measuring")
                        .size(16.0)
                        .color(Color32::GRAY),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new("(like LP+HP for a dash)")
                        .size(13.0)
                        .color(Color32::DARK_GRAY),
                );
            });
            ui.add_space(20.0);
        }

        ui.separator();

        // Minimal stats — just what you need to read your finger gap.
        if stats.count() > 0 {
            draw_stat(ui, "Average gap", &format!("{:.1}ms", stats.average()));
            draw_stat(ui, "Range", &format!("{:.1}–{:.1}ms", stats.min(), stats.max()));
            // How often a frame boundary would split your chord without NOBD.
            let drop = stats.split_probability() * 100.0;
            draw_stat_colored(ui, "Frame-split chance @60fps", &format!("{drop:.0}%"),
                if drop > 20.0 { RED } else if drop > 5.0 { YELLOW } else { GREEN });
            draw_stat(ui, "Splits seen @60fps",
                &format!("{} / {}", stats.simulated_split_count(), stats.count()));
            draw_stat(ui, "Samples (window)", &format!("{} / {}", stats.count(), stats.window()));

            ui.add_space(6.0);
            ui.label(RichText::new("— Grouping evidence —").size(12.0).color(Color32::DARK_GRAY));
            let sf = stats.same_frame_pct();
            draw_stat_colored(ui, "Same-frame rate", &format!("{sf:.0}%"),
                if sf >= 30.0 { TEAL } else { GREEN });
            draw_stat(ui, "Dead zone", &format!("{} frame(s)", stats.dead_zone_frames()));
            draw_stat(ui, "Solo presses", &format!("{}", stats.solo_count()));
            draw_stat(ui, "Distinct chords", &format!("{}", stats.distinct_chords()));
            draw_stat(ui, "USB frame size", &format!("{:.2}ms", stats.frame_ms()));
        }

        // Report-rate footnote (low ≈ Steam Input resampling; use native XInput).
        if let Some(hz) = report_hz {
            ui.add_space(2.0);
            ui.label(
                RichText::new(format!("report rate ~{hz:.0} Hz"))
                    .size(11.0)
                    .color(if hz >= 500.0 { Color32::DARK_GRAY } else { YELLOW }),
            );
        }

        }
        });
        });
    });
}
}

// ─── BUTTON MONITOR TAB ───

impl FingerGapApp {
fn draw_button_monitor(&self, ctx: &egui::Context) {
    let controllers = self
        .input
        .as_ref()
        .map(|i| i.controllers())
        .unwrap_or_default();

    egui::TopBottomPanel::bottom("monitor_log")
        .min_height(140.0)
        .resizable(true)
        .show(ctx, |ui| {
            ui.heading("Event Log (all controllers)");
            ui.separator();
            ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                for entry in self.monitor.event_log().iter().rev() {
                    ui.horizontal(|ui| {
                        let color = if entry.event_type == "PRESS" { GREEN } else { Color32::GRAY };
                        ui.monospace(
                            RichText::new(format!(
                                "[C{}] {:<14} {:<8} {}",
                                entry.controller + 1, entry.button_name, entry.event_type, entry.detail,
                            ))
                            .color(color),
                        );
                    });
                }
            });
        });

    egui::CentralPanel::default().show(ctx, |ui| {
        if controllers.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Connect a controller and press any button")
                        .size(16.0).color(Color32::GRAY),
                );
                ui.label(
                    RichText::new("Hold duration, repress timing & activation stats — per controller")
                        .size(13.0).color(Color32::DARK_GRAY),
                );
            });
            return;
        }
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.columns(controllers.len(), |cols| {
        for (ci, (cidx, cname)) in controllers.iter().enumerate() {
            let ui = &mut cols[ci];
            let infos = self.monitor.button_infos(*cidx);
            ui.label(
                RichText::new(format!("C{}: {cname}", cidx + 1))
                    .strong().size(14.0)
                    .color(if infos.is_empty() { Color32::GRAY } else { TEAL }),
            );
            ui.separator();
            if infos.is_empty() {
                ui.weak("Press any button…");
                continue;
            }

            // Active buttons
            ui.horizontal_wrapped(|ui| {
                for info in &infos {
                    let (color, tc) = if info.held {
                        (TEAL, Color32::BLACK)
                    } else {
                        (Color32::from_rgb(40, 40, 50), Color32::GRAY)
                    };
                    egui::Frame::new()
                        .inner_margin(egui::vec2(8.0, 4.0))
                        .corner_radius(4.0)
                        .fill(color)
                        .show(ui, |ui| {
                            ui.label(RichText::new(&info.name).strong().color(tc));
                        });
                }
            });
            ui.add_space(6.0);

            // Per-button stats (compact for the column).
            egui::Grid::new(format!("bstats_{cidx}")).striped(true).min_col_width(48.0).show(ui, |ui| {
                ui.label(RichText::new("Btn").strong().color(TEAL));
                ui.label(RichText::new("#").strong().color(TEAL));
                ui.label(RichText::new("Avg hold").strong().color(TEAL));
                ui.label(RichText::new("Avg repress").strong().color(TEAL));
                ui.end_row();
                for info in &infos {
                    ui.label(&info.name);
                    ui.label(format!("{}", info.press_count));
                    ui.label(if info.avg_hold_ms > 0.0 { format!("{:.0}ms", info.avg_hold_ms) } else { "-".to_string() });
                    ui.label(if info.avg_repress_ms > 0.0 { format!("{:.0}ms", info.avg_repress_ms) } else { "-".to_string() });
                    ui.end_row();
                }
            });
        }
        });
        });
    });
}
}

fn draw_stat(ui: &mut Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{label}:")).color(Color32::GRAY));
        ui.label(RichText::new(value).strong());
    });
}

fn draw_stat_colored(ui: &mut Ui, label: &str, value: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{label}:")).color(Color32::GRAY));
        ui.label(RichText::new(value).strong().color(color));
    });
}

/// The headline NOBD/grouping verdict — a banner you can watch flip when you
/// toggle the firmware's sync window on and off. Judged over the sliding window.
fn draw_grouping_verdict(ui: &mut Ui, stats: &GapStats) {
    use crate::stats::Grouping;

    let sf = stats.same_frame_pct();
    let grouping_active = stats.grouping_active();

    let grp = match stats.grouping() {
        Some(g) => g,
        None => {
            let left = stats.samples_until_verdict();
            banner(
                ui,
                Color32::from_rgb(40, 40, 50),
                Color32::from_rgb(24, 24, 32),
                "COLLECTING…",
                &format!("Press two buttons together {left} more time(s) for a verdict."),
                None,
            );
            return;
        }
    };

    let (accent, title, body): (Color32, &str, String) = match grp {
        Grouping::Natural => (
            GREEN,
            "GROUPING OFF — natural finger timing",
            format!(
                "Only {sf:.0}% of chords landed in the same USB frame; the rest spread across frames like real fingers. \
                 No sync window / buffering detected on this controller."
            ),
        ),
        Grouping::Window => {
            let win = stats
                .estimated_window_ms()
                .map(|w| format!(" Estimated window ≈ {w:.0} ms."))
                .unwrap_or_default();
            (
                TEAL,
                "GROUPING DETECTED — sync window",
                format!(
                    "{sf:.0}% of chords collapsed onto the same USB frame.{win} \
                     Near-simultaneous presses are being grouped, while single buttons still register on their own."
                ),
            )
        }
        Grouping::AlwaysOn => (
            TEAL,
            "GROUPING DETECTED",
            format!(
                "{sf:.0}% of chords landed on a single USB frame, consistently the same button set. \
                 Multi-button presses are being aligned to the same frame."
            ),
        ),
        Grouping::Hint => (
            YELLOW,
            "INCONCLUSIVE — some same-frame chords",
            format!(
                "{sf:.0}% landed same-frame — could be light grouping or just very fast hands. \
                 Keep going, or mash two buttons together repeatedly."
            ),
        ),
    };

    let dz = stats.dead_zone_frames();
    const GROUPING_NOTE: &str =
        "Grouping detected: several buttons are being committed on the same USB frame — \
         deliberate input conditioning, not your finger timing. Single buttons are unaffected. \
         Press a few singles and vary your timing to characterize it fully.";
    let detail = if grouping_active {
        if dz >= 1 {
            Some(format!(
                "{GROUPING_NOTE} Dead zone: {dz} empty frame(s) before your first real gap."
            ))
        } else {
            Some(GROUPING_NOTE.to_string())
        }
    } else {
        Some(format!(
            "Without grouping, at 60fps ~{:.0}% of recent chords would split across a game-frame boundary ({} of {}).",
            stats.simulated_split_rate() * 100.0,
            stats.simulated_split_count(),
            stats.count()
        ))
    };

    banner(ui, accent, Color32::from_rgb(22, 30, 30), title, &body, detail.as_deref());
}

/// Shared bordered banner used by the grouping verdict.
fn banner(
    ui: &mut Ui,
    accent: Color32,
    fill: Color32,
    title: &str,
    body: &str,
    detail: Option<&str>,
) {
    egui::Frame::new()
        .inner_margin(12.0)
        .corner_radius(8.0)
        .stroke(egui::Stroke::new(2.0, accent))
        .fill(fill)
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(title).size(16.0).strong().color(accent));
                ui.add_space(2.0);
                ui.label(RichText::new(body).size(12.0).color(Color32::LIGHT_GRAY));
                if let Some(d) = detail {
                    ui.add_space(2.0);
                    ui.label(RichText::new(d).size(11.0).color(Color32::GRAY));
                }
            });
        });
    ui.add_space(6.0);
}
