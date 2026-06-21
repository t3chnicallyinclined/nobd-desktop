use eframe::egui;
use egui::{Color32, RichText, ScrollArea, Ui};
use egui_plot::{Bar, BarChart, Plot};

use crate::input::{format_button, GamepadInput, InputEvent};
use crate::monitor::ButtonMonitor;
use crate::stats::GapStats;

const TEAL: Color32 = Color32::from_rgb(0, 180, 216);
const GREEN: Color32 = Color32::from_rgb(80, 200, 80);
const YELLOW: Color32 = Color32::from_rgb(220, 180, 40);
const RED: Color32 = Color32::from_rgb(220, 60, 60);
const ORANGE: Color32 = Color32::from_rgb(220, 140, 40);
const LOG_MAX: usize = 500;

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
        attempt: usize,
        button_a: String,
        button_b: String,
        gap_ms: f64,
        running_avg: f64,
        pre_fire: bool,
    },
    Stray {
        button: String,
        solo_ms: f64,
        reason: &'static str,
        off_time_ms: Option<f64>,
    },
    Bounce {
        button: String,
        off_ms: f64,
    },
}

pub struct FingerGapApp {
    input: Option<GamepadInput>,
    stats: GapStats,
    gap_log: Vec<GapLogEntry>,
    stray_count: usize,
    bounce_count: usize,
    monitor: ButtonMonitor,
    active_tab: Tab,
    error_msg: Option<String>,
    tray: Option<crate::tray::Tray>,
    game_path: String,
    install_msg: String,
    last_cfg: crate::persist::Cfg,
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
            stats: GapStats::new(),
            gap_log: Vec::new(),
            stray_count: 0,
            bounce_count: 0,
            monitor: ButtonMonitor::new(),
            active_tab: Tab::NobdSync,
            error_msg,
            tray: crate::tray::spawn(ctx.clone()),
            game_path,
            install_msg: String::new(),
            last_cfg,
        }
    }
}

impl FingerGapApp {
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

        // Poll gamepad - get pair detection + strays + bounces + raw events
        if let Some(ref mut input) = self.input {
            let result = input.poll();

            // Feed raw events to button monitor
            for ev in &result.events {
                match ev {
                    InputEvent::Pressed(btn) => self.monitor.on_press(*btn),
                    InputEvent::Released(btn) => self.monitor.on_release(*btn),
                }
            }

            // Record gap pair
            if let Some(pair) = result.pair {
                self.stats.record(pair.gap_ms);
                let avg = self.stats.average();
                let pre_fire = pair.gap_ms >= 1.0;
                self.gap_log.push(GapLogEntry::Pair {
                    attempt: self.stats.count(),
                    button_a: format_button(pair.button_a),
                    button_b: format_button(pair.button_b),
                    gap_ms: pair.gap_ms,
                    running_avg: avg,
                    pre_fire,
                });
                if self.gap_log.len() > LOG_MAX {
                    self.gap_log.remove(0);
                }
            }

            // Record strays
            for stray in result.strays {
                self.stray_count += 1;
                self.gap_log.push(GapLogEntry::Stray {
                    button: format_button(stray.button),
                    solo_ms: stray.solo_ms,
                    reason: stray.reason.label(),
                    off_time_ms: stray.off_time_ms,
                });
                if self.gap_log.len() > LOG_MAX {
                    self.gap_log.remove(0);
                }
            }

            // Record bounces
            for bounce in result.bounces {
                self.bounce_count += 1;
                self.gap_log.push(GapLogEntry::Bounce {
                    button: format_button(bounce.button),
                    off_ms: bounce.off_ms,
                });
                if self.gap_log.len() > LOG_MAX {
                    self.gap_log.remove(0);
                }
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
                        // Local gap-tester stats…
                        self.stats.clear();
                        self.gap_log.clear();
                        self.stray_count = 0;
                        self.bounce_count = 0;
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
        });

        match self.active_tab {
            Tab::NobdSync => draw_nobd_sync(ctx, hook_live, dll_installed),
            Tab::GapTester => draw_gap_tester(
                ctx,
                &self.stats,
                &self.gap_log,
                self.stray_count,
                self.bounce_count,
            ),
            Tab::ButtonMonitor => draw_button_monitor(ctx, &self.monitor),
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
        let mode = s.mode.load(Ordering::Relaxed);
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

        // ── Window size ──
        ui.label(RichText::new("Sync window").strong());
        let mut win = s.window_ms.load(Ordering::Relaxed);
        if ui.add(egui::Slider::new(&mut win, 1..=16).suffix(" ms")).changed() {
            s.window_ms.store(win, Ordering::Relaxed);
        }
        ui.weak(
            "Capped at 16 ms = one frame — the game's original \"same-frame\" window. A larger \
             window would group presses the game itself would have split across frames (an unfair \
             reach), so 16 ms is the honest maximum. Bigger is more forgiving (helps loose \
             execution) but adds latency to a lone press; set it from the Finger Gap Tester.",
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

        // ── Live stats from the in-game hook ──
        ui.label(RichText::new("Live in-game stats").strong().size(16.0));
        let groups = s.groups.load(Ordering::Relaxed);
        let singles = s.singles.load(Ordering::Relaxed);
        let saves = s.saves.load(Ordering::Relaxed);
        let (lat_avg, lat_max) = s.latency_ms();
        let (gap_avg, gap_max) = s.finger_gap_ms();
        let rec = s.recommended_window_ms();
        let frame_us = s.frame_us.load(Ordering::Relaxed);
        let poll_hz = s.poll_hz.load(Ordering::Relaxed);
        let (gp_avg, gp_max) = s.game_perceived_ms();

        // Headline: when sync is ON, frame-boundary splits we CAUGHT (saves).
        // When OFF, the passive monitor shows the splits that ACTUALLY occurred.
        let attempts = s.attempts.load(Ordering::Relaxed);
        let misses = s.misses.load(Ordering::Relaxed);
        if enabled {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{saves}")).size(34.0).strong().color(GREEN));
                ui.vertical(|ui| {
                    ui.label(RichText::new("frame-boundary splits caught").size(15.0));
                    // saves ⊆ groups (a save is a group that crossed a frame).
                    let rate = if groups > 0 {
                        saves as f64 / groups as f64 * 100.0
                    } else { 0.0 };
                    ui.weak(format!(
                        "{rate:.0}% of grouped inputs would have split across a frame without NOBD"
                    ));
                });
            });
        } else {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{misses}")).size(34.0).strong().color(RED));
                ui.vertical(|ui| {
                    ui.label(RichText::new("frame-boundary splits MISSED \u{2014} sync is OFF")
                        .size(15.0).color(RED));
                    let rate = if attempts > 0 {
                        misses as f64 / attempts as f64 * 100.0
                    } else { 0.0 };
                    ui.weak(format!(
                        "{rate:.0}% of {attempts} gapped multi-button inputs split across a frame \u{2014} turn sync ON to fix"
                    ));
                });
            });
        }

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

        egui::Grid::new("nobd_stats").num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
            ui.label("Grouped (2+ buttons):");
            ui.colored_label(TEAL, format!("{groups}"));
            ui.end_row();
            ui.label("Singles (solo press):");
            ui.label(format!("{singles}"));
            ui.end_row();
            ui.label("Grouping hold (lead wait):");
            ui.colored_label(
                if lat_avg < 2.0 { GREEN } else if lat_avg < 5.0 { YELLOW } else { ORANGE },
                format!("avg {lat_avg:.1} ms   max {lat_max:.1} ms"),
            );
            ui.end_row();
            ui.label("Your finger gap:");
            if gap_max > 0.0 {
                ui.label(format!("avg {gap_avg:.1} ms   max {gap_max:.1} ms"));
            } else {
                ui.weak("— (do a few dashes / 2-button inputs)");
            }
            ui.end_row();
            ui.label("Game frame time:");
            if frame_us > 0 {
                let fps = 1_000_000.0 / frame_us as f64;
                let ms = frame_us as f64 / 1000.0;
                ui.colored_label(
                    if ms <= 17.5 { GREEN } else { ORANGE },
                    format!("{ms:.2} ms  ({fps:.0} fps)"),
                );
            } else {
                ui.weak("—");
            }
            ui.end_row();

            // Continuous-mode health + the headline latency number.
            if mode == 2 {
                ui.label("Poll rate:");
                if poll_hz > 0 {
                    ui.colored_label(
                        if poll_hz >= 500 { GREEN } else { ORANGE },
                        format!("{poll_hz} Hz"),
                    );
                } else {
                    ui.weak("— (start the game / press a button)");
                }
                ui.end_row();

                ui.label("Input latency (press\u{2192}game):");
                if gp_max > 0.0 {
                    ui.colored_label(
                        if gp_avg < 8.0 { GREEN } else if gp_avg < 16.0 { YELLOW } else { ORANGE },
                        format!("avg {gp_avg:.1} ms   max {gp_max:.1} ms"),
                    );
                } else {
                    ui.weak("— (press an attack)");
                }
                ui.end_row();

                ui.label("Waited a frame:");
                let waits = s.frame_waits.load(Ordering::Relaxed);
                let dels = s.gp_lat_count.load(Ordering::Relaxed);
                if dels > 0 {
                    let pct = waits as f64 / dels as f64 * 100.0;
                    ui.colored_label(
                        if pct < 35.0 { GREEN } else if pct < 60.0 { YELLOW } else { ORANGE },
                        format!("{waits} of {dels}  ({pct:.0}%)"),
                    );
                } else {
                    ui.weak("—");
                }
                ui.end_row();
            }
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if rec > 0 {
                ui.label("Recommended window:");
                ui.colored_label(TEAL, RichText::new(format!("{rec} ms")).strong());
                if ui.button(format!("Apply {rec} ms")).clicked() {
                    s.window_ms.store(rec, Ordering::Relaxed);
                }
            } else {
                ui.weak("Recommended window appears after measuring your finger gap.");
            }
        });

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

fn draw_gap_tester(
    ctx: &egui::Context,
    stats: &GapStats,
    log: &[GapLogEntry],
    stray_count: usize,
    bounce_count: usize,
) {
    egui::TopBottomPanel::bottom("gap_log")
        .min_height(120.0)
        .resizable(true)
        .show(ctx, |ui| {
            ui.heading("Event Log");
            ui.separator();
            ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    for entry in log.iter().rev() {
                        match entry {
                            GapLogEntry::Pair {
                                attempt,
                                button_a,
                                button_b,
                                gap_ms,
                                running_avg,
                                pre_fire,
                            } => {
                                let pre_fire_str = if *pre_fire {
                                    format!(
                                        "  ** PRE-FIRE: {} solo ~{} frame(s)",
                                        button_a,
                                        (*gap_ms as u32).max(1)
                                    )
                                } else {
                                    String::new()
                                };
                                ui.monospace(format!(
                                    "#{:>3}  {} + {}  gap: {:5.1}ms  (avg: {:.1}ms){}",
                                    attempt, button_a, button_b, gap_ms, running_avg, pre_fire_str,
                                ));
                            }
                            GapLogEntry::Stray {
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
                                                RichText::new("STRAY")
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
                            GapLogEntry::Bounce { button, off_ms } => {
                                egui::Frame::new()
                                    .inner_margin(egui::vec2(8.0, 3.0))
                                    .corner_radius(4.0)
                                    .fill(Color32::from_rgb(50, 35, 10))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new("BOUNCE")
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
        if stats.count() > 0 || stray_count > 0 {
            ui.add_space(8.0);

            // OBD / macro detection warning
            let zero_pct = stats.zero_gap_pct();
            if zero_pct > 50.0 {
                egui::Frame::new()
                    .inner_margin(12.0)
                    .corner_radius(8.0)
                    .stroke(egui::Stroke::new(2.0, YELLOW))
                    .fill(Color32::from_rgb(40, 35, 15))
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("OBD / MACRO DETECTED")
                                    .size(16.0)
                                    .strong()
                                    .color(YELLOW),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "{:.0}% of your presses have 0ms gap — this looks like OBD or a macro button.",
                                    zero_pct
                                ))
                                .size(13.0)
                                .color(Color32::GRAY),
                            );
                            ui.label(
                                RichText::new("Turn off OBD to measure your natural finger gap.")
                                    .size(12.0)
                                    .color(Color32::GRAY),
                            );
                        });
                    });
                ui.add_space(4.0);
            }

            if stats.count() > 0 {
                egui::Frame::new()
                    .inner_margin(12.0)
                    .corner_radius(8.0)
                    .stroke(egui::Stroke::new(2.0, TEAL))
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("RECOMMENDED NOBD VALUE")
                                    .size(14.0)
                                    .color(Color32::GRAY),
                            );
                            ui.label(
                                RichText::new(format!("{} ms", stats.recommended_nobd()))
                                    .size(48.0)
                                    .strong()
                                    .color(TEAL),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "based on your average gap of {:.1}ms + 1ms headroom",
                                    stats.average()
                                ))
                                .size(12.0)
                                .color(Color32::GRAY),
                            );
                        });
                    });
                ui.add_space(8.0);
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

        let available = ui.available_size();
        ui.horizontal_top(|ui| {
            // Left: Live stats
            ui.vertical(|ui| {
                ui.set_min_width(180.0);
                ui.heading("Live Stats");
                ui.add_space(8.0);

                // Pair stats
                draw_stat(ui, "Pairs", &format!("{}", stats.count()));
                if stats.count() > 0 {
                    draw_stat(ui, "Average", &format!("{:.1}ms", stats.average()));
                    draw_stat(ui, "Median", &format!("{:.1}ms", stats.median()));
                    draw_stat(ui, "Fastest", &format!("{:.1}ms", stats.min()));
                    draw_stat(ui, "Slowest", &format!("{:.1}ms", stats.max()));
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(4.0);

                // Stray stats
                let total_sequences = stats.count() + stray_count;
                let stray_pct = if total_sequences > 0 {
                    stray_count as f64 / total_sequences as f64 * 100.0
                } else {
                    0.0
                };
                draw_stat_colored(
                    ui,
                    "Strays",
                    &format!("{}", stray_count),
                    if stray_count > 0 { RED } else { Color32::WHITE },
                );
                if total_sequences > 0 {
                    draw_stat_colored(
                        ui,
                        "Stray Rate",
                        &format!("{:.1}%", stray_pct),
                        if stray_pct > 10.0 {
                            RED
                        } else if stray_pct > 0.0 {
                            YELLOW
                        } else {
                            GREEN
                        },
                    );
                }

                // Bounce count
                draw_stat_colored(
                    ui,
                    "Bounces",
                    &format!("{}", bounce_count),
                    if bounce_count > 0 { ORANGE } else { Color32::WHITE },
                );

                // Pre-fire count
                if stats.count() > 0 {
                    let pf_count = stats.pre_fire_count();
                    let pf_pct = pf_count as f64 / stats.count() as f64 * 100.0;
                    draw_stat(
                        ui,
                        "Pre-fire",
                        &format!("{} ({:.0}%)", pf_count, pf_pct),
                    );
                }
            });

            ui.separator();

            // Right: Histogram
            ui.vertical(|ui| {
                ui.heading("Distribution");
                ui.add_space(4.0);

                let buckets = stats.histogram_buckets();
                if buckets.is_empty() {
                    ui.colored_label(Color32::DARK_GRAY, "No data yet");
                } else {
                    let bars: Vec<Bar> = buckets
                        .iter()
                        .enumerate()
                        .filter(|(_, (_, count, _))| *count > 0)
                        .map(|(i, (_label, count, _pct))| {
                            Bar::new(i as f64, *count as f64)
                                .width(0.7)
                                .fill(TEAL)
                        })
                        .collect();

                    let labels: Vec<(usize, String)> = buckets
                        .iter()
                        .enumerate()
                        .map(|(i, (label, _, _))| (i, label.clone()))
                        .collect();

                    let chart_height = (available.y * 0.45).max(120.0).min(250.0);

                    Plot::new("gap_histogram")
                        .height(chart_height)
                        .allow_drag(false)
                        .allow_zoom(false)
                        .allow_scroll(false)
                        .allow_boxed_zoom(false)
                        .show_axes([true, true])
                        .x_axis_formatter(move |val, _range| {
                            let idx = val.value.round() as usize;
                            labels
                                .iter()
                                .find(|(i, _)| *i == idx)
                                .map(|(_, l)| l.clone())
                                .unwrap_or_default()
                        })
                        .y_axis_formatter(|val, _range| format!("{}", val.value as u32))
                        .show(ui, |plot_ui| {
                            plot_ui.bar_chart(BarChart::new("gaps".to_string(), bars));
                        });
                }
            });
        });
    });
}

// ─── BUTTON MONITOR TAB ───

fn draw_button_monitor(ctx: &egui::Context, monitor: &ButtonMonitor) {
    egui::TopBottomPanel::bottom("monitor_log")
        .min_height(150.0)
        .resizable(true)
        .show(ctx, |ui| {
            ui.heading("Event Log");
            ui.separator();
            ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    for entry in monitor.event_log().iter().rev() {
                        ui.horizontal(|ui| {
                            let color = if entry.event_type == "PRESS" {
                                GREEN
                            } else {
                                Color32::GRAY
                            };
                            ui.monospace(
                                RichText::new(format!(
                                    "{:<14} {:<8} {}",
                                    entry.button_name, entry.event_type, entry.detail,
                                ))
                                .color(color),
                            );
                        });
                    }
                });
        });

    egui::CentralPanel::default().show(ctx, |ui| {
        let infos = monitor.button_infos();

        if infos.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Press any button to start monitoring")
                        .size(16.0)
                        .color(Color32::GRAY),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Shows hold duration, repress timing, and activation stats")
                        .size(13.0)
                        .color(Color32::DARK_GRAY),
                );
            });
            return;
        }

        // Live button states
        ui.add_space(8.0);
        ui.heading("Active Buttons");
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            for info in &infos {
                let (color, text_color) = if info.held {
                    (TEAL, Color32::BLACK)
                } else {
                    (Color32::from_rgb(40, 40, 50), Color32::GRAY)
                };
                egui::Frame::new()
                    .inner_margin(egui::vec2(12.0, 6.0))
                    .corner_radius(4.0)
                    .fill(color)
                    .show(ui, |ui| {
                        ui.label(RichText::new(&info.name).strong().color(text_color));
                    });
            }
        });

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        // Per-button stats table
        ui.heading("Button Stats");
        ui.add_space(4.0);

        egui::Grid::new("button_stats")
            .striped(true)
            .min_col_width(80.0)
            .show(ui, |ui| {
                // Header
                ui.label(RichText::new("Button").strong().color(TEAL));
                ui.label(RichText::new("Presses").strong().color(TEAL));
                ui.label(RichText::new("Last Hold").strong().color(TEAL));
                ui.label(RichText::new("Avg Hold").strong().color(TEAL));
                ui.label(RichText::new("Last Repress").strong().color(TEAL));
                ui.label(RichText::new("Avg Repress").strong().color(TEAL));
                ui.label(RichText::new("State").strong().color(TEAL));
                ui.end_row();

                for info in &infos {
                    ui.label(&info.name);
                    ui.label(format!("{}", info.press_count));
                    ui.label(if info.last_hold_ms > 0.0 {
                        format!("{:.1}ms", info.last_hold_ms)
                    } else {
                        "-".to_string()
                    });
                    ui.label(if info.avg_hold_ms > 0.0 {
                        format!("{:.1}ms", info.avg_hold_ms)
                    } else {
                        "-".to_string()
                    });
                    ui.label(if info.last_repress_ms > 0.0 {
                        format!("{:.1}ms", info.last_repress_ms)
                    } else {
                        "-".to_string()
                    });
                    ui.label(if info.avg_repress_ms > 0.0 {
                        format!("{:.1}ms", info.avg_repress_ms)
                    } else {
                        "-".to_string()
                    });
                    let (state_text, state_color) = if info.held {
                        ("HELD", GREEN)
                    } else {
                        ("--", Color32::DARK_GRAY)
                    };
                    ui.label(RichText::new(state_text).color(state_color));
                    ui.end_row();
                }
            });
    });
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
