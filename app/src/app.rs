use eframe::egui;
use egui::{Color32, RichText, ScrollArea, Ui};

use crate::hid::{list_hid_gamepads, HidDeviceId, HidDeviceInfo};
use crate::input::{format_button, GamepadInput, InputEvent, InputSourceKind};
use crate::monitor::ButtonMonitor;
use crate::stats::GapStats;
use crate::sync_service::PadType;

/// Which input backend the Finger Gap Tester reads from.
#[derive(PartialEq, Clone, Copy)]
enum SourceKind {
    XInput,
    Hid,
}

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

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    NobdSync,
    GapTester,
    ButtonMonitor,
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
    last_cfg: crate::persist::Cfg,
    /// Sliding-window size (recent chords) the grouping verdict is judged over.
    decision_window: usize,
    /// Input source for the Finger Gap Tester (XInput vs raw HID).
    source_kind: SourceKind,
    /// Cached HID gamepad list for the device picker.
    hid_devices: Vec<HidDeviceInfo>,
    /// Selected HID device (when source_kind == Hid).
    selected_hid: Option<HidDeviceId>,
    /// Cached display label for the active HID device (for the source note).
    selected_hid_label: String,
    /// System-wide sync (read real pad → group → virtual pad). Runs by default;
    /// dropping it unplugs the virtual pad.
    sync_service: crate::sync_service::SyncService,
    /// Virtual-pad identity (Xbox 360 vs DualShock 4).
    pad_type: PadType,
    /// Was the ViGEmBus driver present at launch? (Drives the install button.)
    vigem_installed: bool,
}

impl FingerGapApp {
    pub fn new(ctx: &egui::Context) -> Self {
        // Restore saved settings into shared memory before anything reads it.
        let last_cfg = crate::persist::load();
        let ui_cfg = crate::persist::load_ui();
        let hid_devices = list_hid_gamepads();
        let pad_type = PadType::from_u32(ui_cfg.pad_type);

        // Resolve the desired input source, falling back to XInput if a saved HID
        // device is no longer present.
        let (source_kind, selected_hid, selected_hid_label, source) = if ui_cfg.input_source == 1 {
            match hid_devices.iter().find(|d| d.id.path == ui_cfg.hid_device) {
                Some(d) => (
                    SourceKind::Hid,
                    Some(d.id()),
                    d.product.clone(),
                    InputSourceKind::Hid(d.id()),
                ),
                None => (SourceKind::XInput, None, String::new(), InputSourceKind::XInput),
            }
        } else {
            (SourceKind::XInput, None, String::new(), InputSourceKind::XInput)
        };

        let (input, error_msg) = match GamepadInput::new(source) {
            Ok(gi) => (Some(gi), None),
            Err(e) => (None, Some(format!("Gamepad init failed: {e}"))),
        };
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
            last_cfg,
            decision_window: crate::stats::DEFAULT_WINDOW,
            source_kind,
            hid_devices,
            selected_hid,
            selected_hid_label,
            sync_service: crate::sync_service::SyncService::start(pad_type),
            pad_type,
            vigem_installed: crate::sync_service::vigem_present(),
        }
    }

    /// Clear local gap-tester state (stats/counts/log/monitor) — reused by the
    /// Reset button and by an input-source switch.
    fn reset_local_stats(&mut self) {
        self.stats.clear();
        self.stray_counts.clear();
        self.bounce_counts.clear();
        self.total_pairs.clear();
        self.gap_log.clear();
        self.monitor.clear();
    }

    /// Drop the current input backend and start a new one on `source`. Dropping
    /// the old `GamepadInput` ends its background thread (its channel sender
    /// errors on the next send). Local stats are cleared since per-source button
    /// identity / slots differ.
    fn rebuild_input(&mut self, source: InputSourceKind) {
        match GamepadInput::new(source) {
            Ok(gi) => {
                self.input = Some(gi);
                self.error_msg = None;
            }
            Err(e) => {
                self.input = None;
                self.error_msg = Some(format!("Gamepad init failed: {e}"));
            }
        }
        self.reset_local_stats();
    }

    /// Persist the current input-source choice (separate from shared-mem Cfg).
    fn persist_ui(&self) {
        crate::persist::save_ui(&crate::persist::UiCfg {
            input_source: match self.source_kind {
                SourceKind::XInput => 0,
                SourceKind::Hid => 1,
            },
            hid_device: self
                .selected_hid
                .as_ref()
                .map(|id| id.to_persist())
                .unwrap_or_default(),
            pad_type: self.pad_type.as_u32(),
        });
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

        // === TOP BAR ===
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("NOBD").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Reset").clicked() {
                        self.reset_local_stats();
                    }
                });
            });

            // System-wide sync status banner.
            let err = self.sync_service.error();
            if err == crate::sync_service::ERR_NO_VIGEM {
                ui.colored_label(RED, "\u{25CF} ViGEmBus not found — install it to enable system-wide sync");
            } else if err == crate::sync_service::ERR_NO_XINPUT {
                ui.colored_label(RED, "\u{25CF} XInput unavailable");
            } else if self.sync_service.is_active() {
                ui.colored_label(GREEN, "\u{25CF} System-wide sync ACTIVE — virtual NOBD pad is live");
            } else if self.sync_service.real_slot().is_none() {
                ui.colored_label(YELLOW, "\u{25CF} Waiting for a controller…");
            } else {
                ui.colored_label(YELLOW, "\u{25CF} Starting sync…");
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
            });

            // Decision window — the grouping verdict is judged over only the last
            // N chords, so it re-decides live and flips when you toggle NOBD
            // mid-session (no Reset needed). Only relevant on the Gap Tester tab.
            if self.active_tab == Tab::GapTester {
                // Input source selector (XInput vs raw HID + device picker). Work
                // on LOCAL copies inside the egui closures, then write back +
                // apply after — avoids nested mutable borrows of `self`.
                let mut kind = self.source_kind;
                let mut selected = self.selected_hid.clone();
                let mut label = self.selected_hid_label.clone();
                let devices = self.hid_devices.clone();
                let mut do_refresh = false;
                let mut pending_source: Option<InputSourceKind> = None;

                ui.horizontal(|ui| {
                    ui.label(RichText::new("Input source").size(12.0).color(Color32::GRAY));
                    egui::ComboBox::from_id_salt("input_source")
                        .selected_text(match kind {
                            SourceKind::XInput => "XInput",
                            SourceKind::Hid => "Raw HID",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut kind, SourceKind::XInput, "XInput");
                            ui.selectable_value(&mut kind, SourceKind::Hid, "Raw HID");
                        });

                    if kind == SourceKind::Hid {
                        let sel_text = if label.is_empty() {
                            "Select device…".to_owned()
                        } else {
                            label.clone()
                        };
                        egui::ComboBox::from_id_salt("hid_device")
                            .selected_text(sel_text)
                            .show_ui(ui, |ui| {
                                if devices.is_empty() {
                                    ui.label(
                                        RichText::new("No HID gamepads — Xbox pads aren't usable here; use a DInput stick")
                                            .size(11.0)
                                            .color(Color32::GRAY),
                                    );
                                }
                                for d in &devices {
                                    let chosen = selected.as_ref() == Some(&d.id);
                                    let l = format!("{} ({:04x}:{:04x})", d.product, d.id.vid, d.id.pid);
                                    if ui.selectable_label(chosen, l).clicked() {
                                        selected = Some(d.id());
                                        label = d.product.clone();
                                        pending_source = Some(InputSourceKind::Hid(d.id()));
                                    }
                                }
                            });
                        if ui.button("Refresh").clicked() {
                            do_refresh = true;
                        }
                    }
                });

                // Detect a source-kind change.
                if kind != self.source_kind {
                    self.source_kind = kind;
                    match kind {
                        SourceKind::XInput => pending_source = Some(InputSourceKind::XInput),
                        SourceKind::Hid => do_refresh = true, // refresh + auto-pick below
                    }
                }
                self.selected_hid = selected;
                self.selected_hid_label = label;

                if do_refresh {
                    self.hid_devices = list_hid_gamepads();
                    if self.source_kind == SourceKind::Hid && self.selected_hid.is_none() {
                        if let Some(d) = self.hid_devices.first() {
                            self.selected_hid = Some(d.id());
                            self.selected_hid_label = d.product.clone();
                            pending_source = Some(InputSourceKind::Hid(d.id()));
                        }
                    }
                }
                if let Some(src) = pending_source {
                    self.rebuild_input(src);
                    self.persist_ui();
                }

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

            // Virtual-pad identity picker (System-wide sync tab only). DualShock 4
            // shows distinctly in Steam so it's tell-apart-able from a real Xbox stick.
            if self.active_tab == Tab::NobdSync {
                let mut pt = self.pad_type;
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Virtual pad").size(12.0).color(Color32::GRAY));
                    egui::ComboBox::from_id_salt("pad_type")
                        .selected_text(match pt {
                            PadType::Xbox360 => "Xbox 360",
                            PadType::DualShock4 => "DualShock 4",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut pt, PadType::Xbox360, "Xbox 360 (XInput-native)");
                            ui.selectable_value(&mut pt, PadType::DualShock4, "DualShock 4 (distinct from a real Xbox stick)");
                        });
                })
                .response
                .on_hover_text("Pick DualShock 4 if your real stick is an Xbox pad — Steam shows it as a separate \"Wireless Controller\" so you can select the right one. Xbox 360 is needed for raw-XInput games outside Steam.");
                if pt != self.pad_type {
                    self.pad_type = pt;
                    self.sync_service = crate::sync_service::SyncService::start(pt);
                    self.persist_ui();
                }
            }
        });

        match self.active_tab {
            Tab::NobdSync => draw_nobd_sync(ctx, &self.sync_service, self.pad_type, self.vigem_installed),
            Tab::GapTester => self.draw_gap_tester(ctx),
            Tab::ButtonMonitor => self.draw_button_monitor(ctx),
        }

        // Persist settings whenever they change (from the panel or the tray).
        let cfg = crate::persist::current();
        if cfg != self.last_cfg {
            crate::persist::save(&cfg);
            self.last_cfg = cfg;
        }

        // Repaint continuously so live status / gamepad input stay current.
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    }
}

// ─── SYSTEM-WIDE SYNC TAB (drives the in-GUI SyncService → virtual NOBD pad) ───

fn draw_nobd_sync(
    ctx: &egui::Context,
    sync: &crate::sync_service::SyncService,
    pad: PadType,
    vigem_installed: bool,
) {
    use std::sync::atomic::Ordering;
    let s = nobd_shared::state();
    let steam_name = match pad {
        PadType::Xbox360 => "Xbox 360 Controller",
        PadType::DualShock4 => "Wireless Controller (DualShock 4)",
    };

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("System-wide sync");

        // Service status.
        let err = sync.error();
        ui.horizontal(|ui| {
            if err == crate::sync_service::ERR_NO_VIGEM {
                ui.colored_label(RED, "\u{25CF}");
                ui.label(RichText::new("ViGEmBus driver not found").color(RED));
            } else if err == crate::sync_service::ERR_NO_XINPUT {
                ui.colored_label(RED, "\u{25CF}");
                ui.label(RichText::new("XInput unavailable").color(RED));
            } else if sync.is_active() {
                ui.colored_label(GREEN, "\u{25CF}");
                ui.label(RichText::new("ACTIVE — virtual NOBD pad is live").color(GREEN));
                if !sync.real_present() {
                    ui.colored_label(YELLOW, "(real pad not reporting)");
                }
            } else if sync.real_slot().is_none() {
                ui.colored_label(YELLOW, "\u{25CF}");
                ui.label("Waiting for a controller…");
            } else {
                ui.colored_label(YELLOW, "\u{25CF}");
                ui.label("Starting…");
            }
        });
        if err == crate::sync_service::ERR_NO_VIGEM {
            ui.label(
                RichText::new("Install the ViGEmBus driver (free), then restart NOBD.")
                    .size(12.0)
                    .color(Color32::GRAY),
            );
            ui.hyperlink("https://github.com/nefarius/ViGEmBus/releases/latest");
        }
        ui.separator();

        // ── Setup: the one dependency + how to wire it into a game ──
        ui.label(RichText::new("Setup").strong().size(15.0));
        ui.horizontal(|ui| {
            if vigem_installed {
                ui.colored_label(GREEN, "1.  \u{2713} ViGEmBus driver installed");
            } else {
                ui.label("1.");
                if ui.button("Install ViGEmBus").clicked() {
                    install_vigembus();
                }
                ui.label(
                    RichText::new("(one-time, free — approve the UAC prompt, then restart NOBD)")
                        .size(12.0)
                        .color(Color32::GRAY),
                );
            }
        });
        ui.label("2.  Connect your controller (XInput). NOBD plugs in the virtual NOBD pad automatically — the banner above turns green.");
        ui.label("3.  Turn on \"NOBD sync window\" below and set your window (find your number on the Finger Gap Tester tab).");
        ui.label(format!(
            "4.  In your game/emulator's controller settings, select the virtual \"{steam_name}\" \u{2014} your real stick drives it underneath, grouped."
        ));
        ui.separator();

        // ── Master control ──
        let mut enabled = s.enabled.load(Ordering::Relaxed) != 0;
        if ui.checkbox(&mut enabled, RichText::new("NOBD sync window").size(16.0)).changed() {
            s.enabled.store(enabled as u32, Ordering::Relaxed);
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Sync window:");
            let mut w = s.window_ms[0].load(Ordering::Relaxed).clamp(1, 16);
            if ui.add(egui::Slider::new(&mut w, 1..=16).suffix(" ms")).changed() {
                s.window_ms[0].store(w, Ordering::Relaxed);
            }
        });
        ui.weak(
            "Capped at 16 ms = one 60fps frame (the game's original \"same-frame\" window — the \
             honest maximum). Set it from your finger gap on the Finger Gap Tester tab.",
        );

        ui.add_space(10.0);
        egui::Frame::new()
            .inner_margin(10.0)
            .corner_radius(8.0)
            .stroke(egui::Stroke::new(2.0, TEAL))
            .show(ui, |ui| {
                ui.label(RichText::new("Tips").strong().color(TEAL));
                ui.label(
                    "\u{2022}  Two identical \"Xbox 360 Controller\" entries in the list? Your real stick is \
                     Xbox too. Set \"Virtual pad\" (top bar) to DualShock 4 \u{2014} the NOBD pad then shows \
                     as a separate \"Wireless Controller\" so you can pick the right one.",
                );
                ui.add_space(2.0);
                ui.label(
                    "\u{2022}  DualShock 4 mode works in Steam, emulators, and DInput games. Use Xbox 360 mode \
                     for raw-XInput-only games launched outside Steam.",
                );
                ui.add_space(2.0);
                ui.label(
                    "\u{2022}  If a game grabs every controller at once and you get doubled inputs, it needs \
                     the real pad hidden (HidHide) \u{2014} an optional advanced step, and a non-issue on \
                     native-HID NOBD hardware.",
                );
            });

        ui.add_space(10.0);
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
                     (1000Hz+) than the game still reads (60Hz). Press two buttons a few ms apart \
                     \u{2014} your natural \u{201C}finger gap\u{201D} \u{2014} and the game's single \
                     60Hz read can land BETWEEN them and see only the first button. A dash becomes a \
                     stray jab, an assist drops, a tech is missed \u{2014} not because you mis-input, \
                     but because the read sampled at the wrong instant.",
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "NOBD groups your near-simultaneous presses so they reach the game together, \
                         on the frame it actually reads \u{2014} with sub-frame latency. It changes \
                         WHEN a real press reports, never WHICH buttons. Nothing invented, nothing \
                         automated.",
                    )
                    .color(GREEN),
                );
            });

        ui.add_space(10.0);
        ui.separator();
        ui.label(RichText::new("How it works").strong());
        ui.label(
            "A ~1kHz background thread reads your stick continuously and runs the sync window on its \
             own clock, just like the controller's firmware. The grouped result is presented as a \
             virtual Xbox pad that any game can read \u{2014} so the sync is universal, not tied to a \
             single game. Near-simultaneous attacks land on the same frame; a lone press only costs a \
             frame if it lands in the last few ms before a read. Directions are never delayed.",
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
        // Which XInput slots are the real stick vs the synced NOBD pad (Xbox 360
        // pad type only). Copy locals so the columns closure doesn't borrow self.
        let nobd_slot = self.sync_service.virtual_slot();
        let real_slot = self.sync_service.real_slot();
        let sync_active = self.sync_service.is_active();
        let is_xinput = self.source_kind == SourceKind::XInput;

        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Reads your controller directly. With System-wide sync running you'll see TWO pads here \
                 \u{2014} your real stick AND the virtual NOBD pad: the NOBD pad reads GROUPING DETECTED, the \
                 real one your raw finger timing. That side-by-side is the proof the sync works.",
            )
            .size(11.0)
            .color(Color32::GRAY),
        );
        // Which input path is live — the whole point during filter verification.
        let (source_line, source_color) = match self.source_kind {
            SourceKind::XInput => ("Source: XInput".to_owned(), Color32::GRAY),
            SourceKind::Hid => {
                let label = if self.selected_hid_label.is_empty() {
                    "(no device)".to_owned()
                } else {
                    self.selected_hid_label.clone()
                };
                (format!("Source: Raw HID — {label}"), TEAL)
            }
        };
        ui.label(RichText::new(source_line).size(11.0).color(source_color));

        // Call out which column is the NOBD pad to choose in-game.
        if is_xinput && sync_active {
            if let Some(vs) = nobd_slot {
                egui::Frame::new()
                    .inner_margin(8.0)
                    .corner_radius(6.0)
                    .stroke(egui::Stroke::new(2.0, TEAL))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!(
                                "\u{25C9} System-wide sync is ON \u{2014} column C{} below is the NOBD VIRTUAL PAD. \
                                 Select THAT controller in your game; the other column is your real stick.",
                                vs + 1
                            ))
                            .size(12.0)
                            .color(TEAL),
                        );
                    });
            }
        }
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
        let slot = *cidx as u32;
        if is_xinput && nobd_slot == Some(slot) {
            ui.label(RichText::new(format!("\u{25C9} C{}: NOBD VIRTUAL PAD", cidx + 1)).strong().size(14.0).color(TEAL));
            ui.label(RichText::new("\u{2190} select this one in your game").size(11.0).color(TEAL));
        } else if is_xinput && real_slot == Some(slot) {
            ui.label(RichText::new(format!("C{}: {cname}  (your real stick)", cidx + 1)).strong().size(14.0).color(Color32::GRAY));
        } else {
            ui.label(
                RichText::new(format!("C{}: {cname}", cidx + 1))
                    .strong().size(14.0)
                    .color(if stats.count() > 0 { TEAL } else { Color32::GRAY }),
            );
        }
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

/// Find a bundled ViGEmBus installer (any *vigembus*.exe) sitting next to nobd.exe.
fn vigembus_installer_path() -> Option<std::path::PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.extension().map_or(false, |e| e.eq_ignore_ascii_case("exe"))
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map_or(false, |n| n.to_lowercase().contains("vigembus"))
        })
}

/// Launch the bundled ViGEmBus installer (UAC prompts for the driver install), or
/// open the download page if no installer is bundled next to nobd.exe.
fn install_vigembus() {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let verb = wide("open");
    let target = vigembus_installer_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "https://github.com/nefarius/ViGEmBus/releases/latest".to_owned());
    let file = wide(&target);
    unsafe {
        ShellExecuteW(0, verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), 1 /* SW_SHOWNORMAL */);
    }
}
