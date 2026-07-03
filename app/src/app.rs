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
    /// NOBD Controller mode (branded HID vs XInput).
    pad_type: PadType,
    /// Whether the virtual NOBD pad currently exists in Windows. Tracked (not
    /// polled per-frame) — set at startup + updated on Enable/Disable.
    controller_present: bool,
    /// Last Enable failure, shown under the button (None once it succeeds).
    setup_msg: Option<String>,
    /// In-flight background Enable — `Some` while setup runs off the UI thread
    /// (shows a spinner); resolves to Ok/Err on completion.
    setup_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// Hide the physical stick from games via HidHide (HID source only), so only
    /// the NOBD Controller shows up in Steam.
    hide_stick: bool,
    /// One-shot: reconcile HidHide cloak with intent on the first frame (restores
    /// hiding after a relaunch, or clears a stale cloak after a crash).
    startup_hide_done: bool,
    /// Auto-detect the input controller on turn-on. Cleared the moment the user
    /// pins a controller in Advanced, so their choice is respected.
    auto_input: bool,
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
        // The sync service reads the SAME source as the Gap Tester (a HID stick
        // if one is selected, else XInput).
        let sync_src = match (source_kind, &selected_hid) {
            (SourceKind::Hid, Some(id)) => crate::sync_service::SyncSource::Hid(id.clone()),
            _ => crate::sync_service::SyncSource::XInput,
        };
        // A restored HID pick counts as "pinned"; otherwise auto-detect on turn-on.
        let auto_input = selected_hid.is_none();
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
            sync_service: crate::sync_service::SyncService::start(pad_type, sync_src),
            pad_type,
            controller_present: crate::nobd_setup::device_present(),
            setup_msg: None,
            setup_rx: None,
            hide_stick: ui_cfg.hide_stick != 0,
            startup_hide_done: false,
            auto_input,
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

    /// Shared input-source picker (XInput vs a raw HID stick + device). Used on
    /// both tabs: it drives the Gap Tester's reader AND the sync service's input,
    /// so the synced output follows whichever stick you select here. To feed a
    /// non-XInput stick into an XInput-only game, pick "DirectInput fightstick".
    fn input_source_picker(&mut self, ui: &mut Ui) {
        // Work on LOCAL copies inside the egui closures, then write back + apply
        // after — avoids nested mutable borrows of `self`.
        let mut kind = self.source_kind;
        let mut selected = self.selected_hid.clone();
        let mut label = self.selected_hid_label.clone();
        // Only your real sticks — hide the NOBD virtual pad (our pid.codes VID).
        let devices: Vec<_> = self
            .hid_devices
            .clone()
            .into_iter()
            .filter(|d| d.id.vid != 0x1209)
            .collect();
        let mut do_refresh = false;
        let mut pending_source: Option<InputSourceKind> = None;

        ui.horizontal(|ui| {
            ui.label(RichText::new("Controller").size(12.0).color(Color32::GRAY));
            egui::ComboBox::from_id_salt("input_source")
                .selected_text(match kind {
                    SourceKind::XInput => "Xbox / XInput sticks",
                    SourceKind::Hid => "DirectInput fightstick",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut kind, SourceKind::XInput, "Xbox / XInput sticks (auto)");
                    ui.selectable_value(&mut kind, SourceKind::Hid, "DirectInput fightstick");
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
                if let Some(d) = self.hid_devices.iter().find(|d| d.id.vid != 0x1209) {
                    self.selected_hid = Some(d.id());
                    self.selected_hid_label = d.product.clone();
                    pending_source = Some(InputSourceKind::Hid(d.id()));
                }
            }
        }
        if let Some(src) = pending_source {
            self.auto_input = false; // user pinned a controller — stop auto-detecting
            self.rebuild_input(src);
            self.restart_sync_if_present();
            self.persist_ui();
        }
    }

    /// Pick the input controller automatically: prefer a single non-Xbox HID
    /// stick (hides cleanly, no XInput collision); else fall back to XInput
    /// (Xbox pads / anything else). Advanced lets the user override.
    fn autodetect_input(&self) -> (SourceKind, Option<HidDeviceId>) {
        let stick = self
            .hid_devices
            .iter()
            .find(|d| d.id.vid != 0x1209 && d.id.vid != 0x045E);
        match stick {
            Some(d) => (SourceKind::Hid, Some(d.id())),
            None => (SourceKind::XInput, None),
        }
    }

    /// Friendly name of the controller currently feeding the sync (for status).
    fn active_input_name(&self) -> String {
        match self.source_kind {
            SourceKind::Hid if !self.selected_hid_label.is_empty() => {
                self.selected_hid_label.clone()
            }
            SourceKind::Hid => "controller".to_owned(),
            SourceKind::XInput => "Xbox controller".to_owned(),
        }
    }

    /// Turn NOBD on: auto-detect the input (unless pinned), point the reader at
    /// it, then create the device off the UI thread (spinner). The result is
    /// applied in `update()`, which also auto-hides the stick.
    fn begin_turn_on(&mut self) {
        self.hid_devices = list_hid_gamepads();
        if self.auto_input {
            let (kind, sel) = self.autodetect_input();
            self.source_kind = kind;
            self.selected_hid = sel;
            self.selected_hid_label = self
                .selected_hid
                .as_ref()
                .and_then(|id| self.hid_devices.iter().find(|d| d.id == *id))
                .map(|d| d.product.clone())
                .unwrap_or_default();
        }
        let input = match (self.source_kind, &self.selected_hid) {
            (SourceKind::Hid, Some(id)) => InputSourceKind::Hid(id.clone()),
            _ => InputSourceKind::XInput,
        };
        self.rebuild_input(input);
        self.persist_ui();

        let (tx, rx) = std::sync::mpsc::channel();
        let mode = self.pad_type;
        std::thread::spawn(move || {
            let r = crate::nobd_setup::run_setup(mode).map_err(|e| e.to_string());
            let _ = tx.send(r);
        });
        self.setup_rx = Some(rx);
        self.setup_msg = None;
    }

    /// The input source the sync service should read — mirrors the app-wide
    /// picker (a HID stick if one is selected, else XInput).
    fn sync_source(&self) -> crate::sync_service::SyncSource {
        match (self.source_kind, &self.selected_hid) {
            (SourceKind::Hid, Some(id)) => crate::sync_service::SyncSource::Hid(id.clone()),
            _ => crate::sync_service::SyncSource::XInput,
        }
    }

    /// Restart the sync service on the current source — but only while the NOBD
    /// device is present (i.e. sync is meant to be running). Called when the
    /// input source changes so the synced output follows the selected stick.
    fn restart_sync_if_present(&mut self) {
        if self.controller_present {
            self.sync_service =
                crate::sync_service::SyncService::start(self.pad_type, self.sync_source());
        }
        self.apply_stick_hiding();
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
            hide_stick: self.hide_stick as u32,
        });
    }

    /// Apply (or clear) HidHide cloaking to match the current intent: hide the
    /// selected stick from games when hiding is on AND we're syncing from a HID
    /// source; otherwise make sure nothing stays cloaked. Best-effort (no-ops if
    /// HidHide isn't installed). Shell-outs, so call on events, never per-frame.
    fn apply_stick_hiding(&self) {
        if !crate::hidhide::is_installed() {
            return;
        }
        let hide_target = if self.hide_stick && self.controller_present {
            match (self.source_kind, &self.selected_hid) {
                (SourceKind::Hid, Some(id)) => Some(id.path.clone()),
                _ => None,
            }
        } else {
            None
        };
        match hide_target {
            Some(path) => {
                let _ = crate::hidhide::whitelist_self();
                let _ = crate::hidhide::hide_device(&path);
                let _ = crate::hidhide::cloak(true);
            }
            None => {
                // Un-cloak so no stick is left hidden when hiding is off / not HID.
                let _ = crate::hidhide::cloak(false);
                if let Some(id) = &self.selected_hid {
                    let _ = crate::hidhide::unhide_device(&id.path);
                }
            }
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

        // A background Enable finished — apply its result on the UI thread.
        if let Some(rx) = &self.setup_rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(()) => {
                        self.controller_present = true;
                        self.setup_msg = None;
                        self.sync_service = crate::sync_service::SyncService::start(
                            self.pad_type,
                            self.sync_source(),
                        );
                        self.apply_stick_hiding();
                    }
                    Err(e) => self.setup_msg = Some(format!("Enable failed: {e}")),
                }
                self.setup_rx = None;
            }
        }

        // Reconcile HidHide cloak with intent once, on the first frame — restores
        // hiding after a login-task relaunch, or clears a stale cloak from a crash.
        if !self.startup_hide_done {
            self.apply_stick_hiding();
            self.startup_hide_done = true;
        }

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
            });

            // Decision window — the grouping verdict is judged over only the last
            // N chords, so it re-decides live and flips when you toggle NOBD
            // mid-session (no Reset needed). Only relevant on the Gap Tester tab.
            if self.active_tab == Tab::GapTester {
                self.input_source_picker(ui);

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

            // System-wide sync tab: the NOBD Controller is the automatic synced
            // output — the user just deals with sync + their stock stick. The
            // device *type* is an Advanced detail, defaulting to the branded
            // "NOBD Controller" (HID).
            if self.active_tab == Tab::NobdSync {
                // ── Master switch — one control does everything: create the NOBD
                // Controller, auto-bind the active pad, hide it, start syncing. ──
                ui.horizontal(|ui| {
                    if self.setup_rx.is_some() {
                        ui.spinner();
                        ui.label(
                            RichText::new("Turning NOBD on\u{2026}").size(14.0).color(Color32::GRAY),
                        );
                    } else if self.controller_present {
                        ui.colored_label(GREEN, RichText::new("\u{25CF}").size(16.0));
                        if ui
                            .button(RichText::new("Turn NOBD Off").size(15.0).color(RED))
                            .clicked()
                            && crate::nobd_setup::eject().is_ok()
                        {
                            self.controller_present = false;
                            self.sync_service = crate::sync_service::SyncService::stopped();
                            self.apply_stick_hiding(); // un-cloak: stick is normal again
                            self.persist_ui();
                        }
                    } else if crate::nobd_setup::is_elevated() {
                        if ui
                            .button(RichText::new("Turn NOBD On").size(15.0).strong())
                            .clicked()
                        {
                            self.begin_turn_on();
                        }
                    } else if ui
                        .button(RichText::new("Turn NOBD On").size(15.0).strong())
                        .clicked()
                        && crate::nobd_setup::relaunch_elevated_for_setup(self.pad_type).is_ok()
                    {
                        std::process::exit(0);
                    }
                });

                // ── Status line ──
                if self.controller_present {
                    ui.label(
                        RichText::new(format!(
                            "Syncing your {} \u{2192} NOBD Controller",
                            self.active_input_name()
                        ))
                        .size(12.0)
                        .color(GREEN),
                    );
                    // First-run nudge: with a HID stick and no HidHide, offer to
                    // hide the physical pad so only the NOBD Controller shows.
                    if self.source_kind == SourceKind::Hid && !crate::hidhide::is_installed() {
                        ui.horizontal(|ui| {
                            if ui.button("Show only NOBD Controller\u{2026}").clicked() {
                                let _ = crate::hidhide::run_installer();
                            }
                            ui.label(
                                RichText::new("optional \u{2014} hides your stick from games (installs HidHide, needs a reboot)")
                                    .size(11.0)
                                    .color(Color32::GRAY),
                            );
                        });
                    }
                } else if let Some(msg) = &self.setup_msg {
                    ui.colored_label(RED, RichText::new(msg).size(12.0));
                } else if self.setup_rx.is_none() {
                    ui.label(
                        RichText::new("Off \u{2014} your controller works normally.")
                            .size(12.0)
                            .color(Color32::GRAY),
                    );
                }

                // ── Advanced drawer — power-user overrides; normal users skip it. ──
                let mut pt = self.pad_type;
                let mut pt_changed = false;
                ui.collapsing(
                    RichText::new("Advanced").size(12.0).color(Color32::GRAY),
                    |ui| {
                        ui.label(RichText::new("Output").size(11.0).color(Color32::GRAY));
                        ui.radio_value(
                            &mut pt,
                            PadType::Xinput,
                            "XInput / Xbox pad (works in the most games)",
                        );
                        ui.radio_value(
                            &mut pt,
                            PadType::Hid,
                            "Branded \u{201C}NOBD Controller\u{201D} (Steam / DirectInput games)",
                        );
                        pt_changed = pt != self.pad_type;

                        ui.add_space(6.0);
                        ui.label(
                            RichText::new("Input controller (auto by default)")
                                .size(11.0)
                                .color(Color32::GRAY),
                        );
                        self.input_source_picker(ui);

                        // Device hiding — only meaningful with a HID stick + HidHide.
                        if self.source_kind == SourceKind::Hid && crate::hidhide::is_installed() {
                            let mut hide = self.hide_stick;
                            if ui
                                .checkbox(
                                    &mut hide,
                                    "Hide my stick from games (show only NOBD Controller)",
                                )
                                .changed()
                            {
                                self.hide_stick = hide;
                                self.apply_stick_hiding();
                                self.persist_ui();
                            }
                        }
                    },
                );
                if pt_changed {
                    self.pad_type = pt;
                    // New output mode needs its own devnode — turn off until the
                    // user turns it back on for the new mode.
                    self.controller_present = false;
                    self.sync_service = crate::sync_service::SyncService::stopped();
                    self.apply_stick_hiding();
                    self.persist_ui();
                }
            }
        });

        match self.active_tab {
            Tab::NobdSync => draw_nobd_sync(ctx, &self.sync_service, self.pad_type),
            Tab::GapTester => self.draw_gap_tester(ctx),
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

fn draw_nobd_sync(ctx: &egui::Context, sync: &crate::sync_service::SyncService, pad: PadType) {
    use std::sync::atomic::Ordering;
    let s = nobd_shared::state();
    let steam_name = match pad {
        PadType::Hid => "NOBD Controller",
        PadType::Xinput => "NOBD Controller (Xbox 360 / XInput)",
    };

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("System-wide sync");

        // Runtime hint only — the master switch above owns on/off + status. Here
        // we surface just the two things worth flagging mid-session.
        let err = sync.error();
        if err == crate::sync_service::ERR_NO_XINPUT {
            ui.colored_label(RED, "\u{25CF} XInput unavailable on this system");
            ui.separator();
        } else if sync.is_active() && !sync.real_present() {
            ui.colored_label(YELLOW, "\u{25CF} Controller connected — press a button to start it reporting…");
            ui.separator();
        }

        // ── Controls (what you actually touch) ──
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
        ui.weak("Set this from your finger gap on the Finger Gap Tester tab. 16 ms = one 60fps frame (the honest max).");

        // ── Everything explanatory folds in here — open it once, then forget it. ──
        ui.add_space(10.0);
        egui::CollapsingHeader::new(RichText::new("\u{24D8}  How it works & setup").color(TEAL))
            .default_open(false)
            .show(ui, |ui| {
                ui.label(RichText::new("Setup").strong());
                ui.label("1.  Connect your controller.");
                ui.label("2.  Click Turn NOBD On (one-time admin prompt). It auto-detects your pad, creates the NOBD Controller, and starts syncing.");
                ui.label("3.  Turn on the sync window above; set it from the Finger Gap Tester.");
                ui.label(format!(
                    "4.  In your game's controller settings, select \"{steam_name}\" \u{2014} your stick drives it, grouped."
                ));

                ui.add_space(8.0);
                ui.label(RichText::new("How it works").strong());
                ui.label(
                    "A ~1 kHz background thread reads your stick and runs the sync window on its own \
                     clock, like the controller firmware. The grouped result is delivered as the \
                     native NOBD Controller \u{2014} universal, not tied to one game. Near-simultaneous \
                     attacks land on the same frame; a lone press costs a frame only if it lands in \
                     the last few ms before a read. Directions are never delayed.",
                );

                ui.add_space(8.0);
                ui.label(RichText::new("The frame-boundary issue").strong());
                ui.label(
                    "Old games like MvC2 read your controller exactly ONCE per frame \u{2014} 60 times \
                     a second, every 16.67 ms. On modern hardware your stick updates far faster \
                     (1000 Hz+) than the game reads (60 Hz), so two buttons a few ms apart \u{2014} your \
                     natural finger gap \u{2014} can land on either side of a single read: a dash \
                     becomes a stray jab. NOBD groups near-simultaneous presses so they reach the game \
                     together, on the frame it actually reads. It changes WHEN a press reports, never \
                     WHICH buttons.",
                );
            });
    });
}

// ─── GAP TESTER TAB ───

impl FingerGapApp {
fn draw_gap_tester(&self, ctx: &egui::Context) {
    // All present controllers (unfiltered). The dashboard's target selector
    // labels our own companion "NOBD output" rather than hiding it — YOU pick
    // what to analyze, so the verdict never flips between raw and synced on its own.
    let nobd_slot = self.sync_service.virtual_slot();
    let all_controllers: Vec<(usize, String)> =
        self.input.as_ref().map(|i| i.controllers()).unwrap_or_default();
    let log = &self.gap_log;
    let report_hz = self.input.as_ref().and_then(|i| i.report_rate_hz());

    // Two side-by-side logs: your raw stick timing (left) vs the synced NOBD
    // output (right), split by the fingerprinted companion slot.
    let nobd_slot_usize = nobd_slot.map(|s| s as usize);
    egui::TopBottomPanel::bottom("gap_log")
        .default_height(150.0)
        .min_height(90.0)
        .resizable(true)
        .show(ctx, |ui| {
            ui.columns(2, |cols| {
                cols[0].label(RichText::new("Raw \u{2014} your stick").strong().color(YELLOW));
                cols[0].separator();
                ScrollArea::vertical()
                    .id_salt("raw_log")
                    .auto_shrink(false)
                    .show(&mut cols[0], |ui| {
                        for e in log
                            .iter()
                            .rev()
                            .filter(|e| Some(log_entry_slot(e)) != nobd_slot_usize)
                        {
                            render_log_entry(ui, e);
                        }
                    });

                cols[1].label(RichText::new("NOBD \u{2014} synced output").strong().color(TEAL));
                cols[1].separator();
                ScrollArea::vertical()
                    .id_salt("sync_log")
                    .auto_shrink(false)
                    .show(&mut cols[1], |ui| {
                        if nobd_slot_usize.is_none() {
                            ui.label(
                                RichText::new("Turn NOBD on to see the synced output.")
                                    .size(11.0)
                                    .color(Color32::DARK_GRAY),
                            );
                        }
                        for e in log
                            .iter()
                            .rev()
                            .filter(|e| Some(log_entry_slot(e)) == nobd_slot_usize)
                        {
                            render_log_entry(ui, e);
                        }
                    });
            });
        });

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.add_space(4.0);
        // Compact header: the prompt + which input is live, on one line. The full
        // explanation lives under "How it works" on the Sync tab — not repeated here.
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new("Press two attack buttons together (e.g. LP+HP).")
                    .size(12.0)
                    .color(Color32::GRAY),
            );
            match self.source_kind {
                SourceKind::XInput => {
                    ui.label(RichText::new("· Source: XInput").size(12.0).color(Color32::GRAY));
                }
                SourceKind::Hid => {
                    let label = if self.selected_hid_label.is_empty() {
                        "(no device)"
                    } else {
                        self.selected_hid_label.as_str()
                    };
                    ui.label(RichText::new(format!("· Source: {label}")).size(12.0).color(TEAL));
                }
            }
        });
        ui.separator();
        if all_controllers.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Connect a controller and press two buttons together")
                        .size(16.0).color(Color32::GRAY),
                );
                ui.label(RichText::new("(like LP+HP for a dash)").size(13.0).color(Color32::DARK_GRAY));
            });
            return;
        }

        // The tester's job is YOUR RAW finger gap, so the dashboard always shows
        // your real stick (never the synced pad). Raw = the pad the sync reads;
        // standalone (no sync) = the first present non-companion controller.
        let real_slot = self.sync_service.real_slot();
        let raw_slot = real_slot
            .or_else(|| {
                all_controllers
                    .iter()
                    .map(|(s, _)| *s as u32)
                    .find(|s| Some(*s) != nobd_slot)
            })
            .map(|s| s as usize);
        let empty = GapStats::new();
        let stats: &GapStats = raw_slot.and_then(|s| self.stats.get(s)).unwrap_or(&empty);

        if stats.count() == 0 {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Press two buttons at the same time").size(15.0).color(Color32::GRAY));
                ui.label(RichText::new("(like LP+HP for a dash)").size(12.0).color(Color32::DARK_GRAY));
            });
        } else {
            let grouping_active = stats.grouping_active();
            draw_grouping_verdict(ui, stats);

            // Recommended value + stat tiles SIDE BY SIDE — one compact, fixed
            // block that fits without scrolling. Recommended never disappears
            // (shows "paused" when grouping corrupts the average).
            let drop = stats.split_probability() * 100.0;
            let split_col = if drop > 20.0 { RED } else if drop > 5.0 { YELLOW } else { GREEN };
            let seen = stats.simulated_split_count();
            let rec = stats.recommended_nobd();
            let rec_col = rec_color(rec);
            ui.columns(2, |c| {
                egui::Frame::new()
                    .inner_margin(10.0)
                    .corner_radius(8.0)
                    .stroke(egui::Stroke::new(2.0, if grouping_active { Color32::GRAY } else { rec_col }))
                    .show(&mut c[0], |ui| {
                        ui.set_min_height(96.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("RECOMMENDED NOBD").size(11.0).color(Color32::GRAY));
                            if grouping_active {
                                ui.label(RichText::new("paused").size(28.0).strong().color(Color32::GRAY));
                                ui.label(RichText::new("turn NOBD off to measure").size(10.0).color(Color32::GRAY));
                            } else {
                                ui.label(RichText::new(format!("{rec} ms")).size(40.0).strong().color(rec_col));
                                ui.label(RichText::new(format!("p95 {:.1}ms + 1ms", stats.percentile(0.95))).size(10.0).color(Color32::GRAY));
                            }
                        });
                    });
                c[1].vertical(|ui| {
                    ui.columns(2, |t| {
                        draw_stat_tile(&mut t[0], &format!("{:.1}ms", stats.average()), "Avg gap", TEAL);
                        draw_stat_tile(&mut t[1], &format!("{:.1}\u{2013}{:.1}", stats.min(), stats.max()), "Range ms", TEAL);
                    });
                    ui.add_space(4.0);
                    ui.columns(2, |t| {
                        draw_stat_tile(&mut t[0], &format!("{drop:.0}%"), "Split @60", split_col);
                        draw_stat_tile(&mut t[1], &format!("{seen}/{}", stats.count()), "Splits seen",
                            if seen > 0 { YELLOW } else { GREEN });
                    });
                });
            });

            // One-line raw-vs-NOBD proof.
            let win_ms = nobd_shared::state()
                .window_ms[0]
                .load(std::sync::atomic::Ordering::Relaxed)
                .clamp(1, 16);
            let n = stats.count();
            let raw_splits = stats.simulated_split_count();
            let synced_splits = stats.synced_split_count(win_ms as f64);
            let grouped = stats.grouped_count(win_ms as f64);
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Stick vs NOBD:").strong().size(12.0));
                ui.colored_label(if raw_splits > 0 { RED } else { GREEN }, format!("raw {raw_splits}/{n} split"));
                ui.label(RichText::new("\u{2192}").color(Color32::GRAY));
                ui.colored_label(if synced_splits > 0 { YELLOW } else { GREEN }, format!("NOBD @{win_ms}ms {synced_splits}/{n} split"));
                ui.label(RichText::new(format!("({grouped} grouped)")).size(11.0).color(Color32::GRAY));
            });

            // Report rate — tiny footnote (low ≈ Steam Input resampling).
            if let Some(hz) = report_hz {
                ui.label(RichText::new(format!("report rate ~{hz:.0} Hz")).size(10.0)
                    .color(if hz >= 500.0 { Color32::DARK_GRAY } else { YELLOW }));
            }
        }

        // ── NOBD sync output (secondary) — the virtual controller's grouping,
        // shown UNDER your raw gap as proof the sync is working. ──
        if let Some(ns) = nobd_slot
            .map(|s| s as usize)
            .filter(|s| all_controllers.iter().any(|(cs, _)| cs == s))
        {
            let empty2 = GapStats::new();
            let comp = self.stats.get(ns).unwrap_or(&empty2);
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("\u{25C9} NOBD sync output").color(TEAL).strong().size(12.0));
                if comp.count() > 0 {
                    let sf = comp.same_frame_pct();
                    let grouped = comp.count().saturating_sub(comp.simulated_split_count());
                    ui.label(
                        RichText::new(format!("{sf:.0}% same-frame"))
                            .color(if sf >= 70.0 { GREEN } else { YELLOW })
                            .strong()
                            .size(12.0),
                    );
                    ui.label(
                        RichText::new(format!("\u{00B7} {grouped}/{} grouped", comp.count()))
                            .color(Color32::GRAY)
                            .size(11.0),
                    );
                } else {
                    ui.label(RichText::new("waiting for input\u{2026}").color(Color32::GRAY).size(11.0));
                }
            });
        }
    });
}
}

/// A colorful stat tile — big value + small label in a bordered card. Matches
/// the look of the grouping-verdict / recommended-value cards.
fn draw_stat_tile(ui: &mut Ui, value: &str, label: &str, color: Color32) {
    egui::Frame::new()
        .inner_margin(egui::vec2(12.0, 8.0))
        .corner_radius(8.0)
        .fill(Color32::from_rgb(22, 22, 28))
        .stroke(egui::Stroke::new(1.5, color))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(value).size(24.0).strong().color(color));
                ui.label(RichText::new(label).size(11.0).color(Color32::GRAY));
            });
        });
}

/// The controller slot an event-log entry belongs to (for the raw/NOBD split).
fn log_entry_slot(e: &GapLogEntry) -> usize {
    match e {
        GapLogEntry::Pair { controller, .. }
        | GapLogEntry::Stray { controller, .. }
        | GapLogEntry::Bounce { controller, .. } => *controller,
    }
}

/// Render one event-log entry, compactly (no role tag — the column shows raw vs
/// NOBD). Same chord appears on both sides, so you can compare gap + split.
fn render_log_entry(ui: &mut Ui, e: &GapLogEntry) {
    match e {
        GapLogEntry::Pair { attempt, button_a, button_b, count, gap_ms, split, .. } => {
            let chord = if *count > 2 { format!(" ({} btn)", count) } else { String::new() };
            ui.horizontal(|ui| {
                ui.monospace(format!("#{attempt:>3}  {button_a}+{button_b}{chord}  {gap_ms:>5.1}ms"));
                if *split {
                    ui.monospace(RichText::new("SPLIT").strong().color(RED));
                } else {
                    ui.monospace(RichText::new("1 frame").color(GREEN));
                }
            });
        }
        GapLogEntry::Stray { button, solo_ms, reason, .. } => {
            ui.horizontal(|ui| {
                ui.label(RichText::new("STRAY").size(12.0).strong().color(RED));
                ui.monospace(
                    RichText::new(format!("{button} {solo_ms:.1}ms ({reason})"))
                        .color(Color32::from_rgb(255, 160, 160)),
                );
            });
        }
        GapLogEntry::Bounce { button, off_ms, .. } => {
            ui.horizontal(|ui| {
                ui.label(RichText::new("BOUNCE").size(12.0).strong().color(ORANGE));
                ui.monospace(
                    RichText::new(format!("{button} +{off_ms:.1}ms"))
                        .color(Color32::from_rgb(255, 200, 120)),
                );
            });
        }
    }
}

/// The headline NOBD/grouping verdict — a banner you can watch flip when you
/// toggle the firmware's sync window on and off. Judged over the sliding window.
fn draw_grouping_verdict(ui: &mut Ui, stats: &GapStats) {
    use crate::stats::Grouping;

    let sf = stats.same_frame_pct();

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
            "GROUPING OFF",
            format!("{sf:.0}% same-frame — natural finger timing"),
        ),
        Grouping::Window => {
            let win = stats
                .estimated_window_ms()
                .map(|w| format!(" (~{w:.0}ms window)"))
                .unwrap_or_default();
            (TEAL, "GROUPING DETECTED", format!("{sf:.0}% same-frame{win} — presses grouped"))
        }
        Grouping::AlwaysOn => (
            TEAL,
            "GROUPING DETECTED",
            format!("{sf:.0}% same-frame — consistent grouping"),
        ),
        Grouping::Hint => (
            YELLOW,
            "INCONCLUSIVE",
            format!("{sf:.0}% same-frame — keep going"),
        ),
    };

    banner(ui, accent, Color32::from_rgb(22, 28, 32), title, &body, None);
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
            // Fixed (small) height so the card doesn't grow/shrink as the verdict
            // text changes between states and shove everything below it around.
            ui.set_min_height(34.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(title).size(15.0).strong().color(accent));
                ui.label(RichText::new(body).size(11.0).color(Color32::LIGHT_GRAY));
                if let Some(d) = detail {
                    ui.label(RichText::new(d).size(11.0).color(Color32::GRAY));
                }
            });
        });
    ui.add_space(6.0);
}

// (ViGEmBus install helpers removed — the app is all-HIDMaestro now.)
