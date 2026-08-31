use eframe::egui;
use egui::{Color32, RichText, ScrollArea, Ui};

use crate::hid::{list_hid_gamepads, HidDeviceId, HidDeviceInfo};
use crate::input::{format_button, GamepadInput, InputSourceKind};
use crate::stats::GapStats;
use crate::sync_service::PadType;

/// Which input backend the Finger Gap Tester reads from.
#[derive(PartialEq, Clone, Copy)]
enum SourceKind {
    XInput,
    Hid,
}

use crate::palette::{ACTION, BROKEN, HAIRLINE, INK, INK_DIM, INK_FAINT, LIVE, NEEDS_YOU, SURFACE, WELL};
const LOG_MAX: usize = 500;

enum GapLogEntry {
    Pair {
        controller: usize,
        attempt: usize,
        button_a: String,
        button_b: String,
        count: usize,
        gap_ms: f64,
        /// Chance a 60 fps game reads the two presses on different frames.
        /// Deliberately the odds, not a verdict: our clock has no relationship
        /// to the game's poll phase, so a per-chord yes/no was a coin flip.
        risk: f64,
    },
    Stray {
        controller: usize,
        button: String,
        solo_ms: f64,
        reason: &'static str,
    },
    Bounce {
        controller: usize,
        button: String,
        off_ms: f64,
    },
}

/// Turn a probability into something a person can picture.
///
/// "19% split risk" is a sentence for an engineer. A player who knows they miss
/// dashes reads "about 1 in 5" instantly and has never needed a percentage. The
/// two certain ends are stated as certainties, not as 0% and 100%.
fn odds_phrase(risk: f64) -> String {
    if risk <= 0.0 {
        "always safe".to_owned()
    } else if risk >= 1.0 {
        "drops every time".to_owned()
    } else {
        let n = (1.0 / risk).round().max(2.0);
        format!("about 1 in {n:.0}")
    }
}

/// A status dot, PAINTED rather than typed.
///
/// egui's default font set has no glyph for U+25CF/U+25CB, so a text dot came
/// out as an empty box. `flow_arrow` exists for the same reason (U+2192).
fn status_dot(ui: &mut Ui, color: Color32, filled: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    let c = rect.center();
    if filled {
        ui.painter().circle_filled(c, 5.0, color);
    } else {
        ui.painter().circle_stroke(c, 5.0, egui::Stroke::new(2.0, color));
    }
}

/// A small chevron for the "stick -> NOBD -> game" strip.
fn flow_arrow(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(20.0, 14.0), egui::Sense::hover());
    let c = rect.center();
    let st = egui::Stroke::new(2.0, HAIRLINE);
    ui.painter()
        .line_segment([c + egui::vec2(-4.0, -4.0), c + egui::vec2(2.0, 0.0)], st);
    ui.painter()
        .line_segment([c + egui::vec2(2.0, 0.0), c + egui::vec2(-4.0, 4.0)], st);
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
    error_msg: Option<String>,
    tray: Option<crate::tray::Tray>,
    last_cfg: crate::persist::Cfg,
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
    /// Where Marvel lives, if we found it. `None` = not installed on this PC.
    game_dir: Option<std::path::PathBuf>,
    /// Is OUR current DINPUT8.dll in that folder? Byte-compared, not merely
    /// present - a stale build is worse than none (see gameinstall::is_current).
    hook_installed: bool,
    /// Last problem from the auto-install, shown verbatim.
    hook_msg: Option<String>,
    /// Heartbeat tracking. The in-game DLL bumps `dll_heartbeat` on every read,
    /// so a value that MOVED since we last looked means the hook is live in a
    /// running game. A value that is merely non-zero proves nothing - it
    /// persists in shared memory after the game exits.
    hb_last: u64,
    hb_seen_at: Option<std::time::Instant>,
    hb_poll_at: std::time::Instant,
    /// Is Marvel running? Cached from the same 4 Hz poll as the heartbeat -
    /// answering it costs a `tasklist` spawn, so it must never be asked during
    /// painting.
    game_is_running: bool,
    /// Leave the NOBD Controller in Windows after NOBD closes. Off by default:
    /// the devnode outlives the process, so keeping it left a phantom Xbox pad in
    /// Steam with the app shut and the stick unplugged.
    keep_controller: bool,
    /// One-shot: reconcile HidHide cloak with intent on the first frame (restores
    /// hiding after a relaunch, or clears a stale cloak after a crash).
    startup_hide_done: bool,
    /// Auto-detect the input controller on turn-on. Cleared the moment the user
    /// pins a controller in Advanced, so their choice is respected.
    auto_input: bool,
    /// Cached "start with Windows" state (the elevated logon task). Queried once at
    /// startup + updated on toggle -- never polled per frame (schtasks spawns a process).
    autostart_enabled: bool,
    /// Hotplug watchdog: re-adopt the bulk stick when it (re)appears so a replug doesn't need a manual
    /// NOBD toggle. `hotplug_at` throttles the ~1 Hz poll; `bulk_was_present` edge-detects absent->present.
    hotplug_at: std::time::Instant,
    bulk_was_present: bool,
    /// Commit heartbeat: the last (groups+singles) total we saw, and when it
    /// last moved. Drives the "it just did something" dot - a window is open for
    /// only a few ms, so a raw live flag would never be visible at frame rate.
    proof_seen: u64,
    proof_pulse: Option<std::time::Instant>,
    /// The DriverStore holds an OLDER NOBD driver than the one we ship. Cached
    /// (it spawns pnputil); drives the update prompt, which is the only way an
    /// existing user ever reaches the upgrade path at all.
    driver_stale: bool,
    /// Whether the driver package is already in the DriverStore. Decides
    /// "Install NOBD Controller" vs "Add NOBD Controller back" - re-adding after
    /// a removal skips certutil/pnputil entirely, so it is not install-class.
    /// Cached: it walks the DriverStore, so never poll it per frame.
    driver_installed: bool,
    /// Overlay state. These are popups, not inline expansions, so opening one
    /// cannot change the height of the page behind it.
    window_popup: bool,
    install_popup: bool,
    /// Is the Details drawer open?
    details_open: bool,
    /// The user has confirmed they picked NOBD Controller in their game, so the
    /// last-step card collapses to a single line and returns its height to the tape.
    last_step_done: bool,
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
        let bulk_present =
            crate::bulk::find_device_path(crate::bulk::NOBD_BULK_VID, crate::bulk::NOBD_BULK_PID).is_some();
        let sync_src = if bulk_present {
            crate::sync_service::SyncSource::Bulk // Extreme Low Latency: stick is in NOBD Bulk mode
        } else {
            match (source_kind, &selected_hid) {
                (SourceKind::Hid, Some(id)) => crate::sync_service::SyncSource::Hid(id.clone()),
                _ => crate::sync_service::SyncSource::XInput,
            }
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
            error_msg,
            tray: crate::tray::spawn(ctx.clone()),
            last_cfg,
            source_kind,
            hid_devices,
            selected_hid,
            selected_hid_label,
            // Only run the virtual-pad loop when that controller actually
            // exists. The in-game hook is the default path now; starting a
            // second sync loop beside it is how you get two windows stacked.
            sync_service: if crate::nobd_setup::device_present() {
                crate::sync_service::SyncService::start(pad_type, sync_src)
            } else {
                crate::sync_service::SyncService::stopped()
            },
            pad_type,
            controller_present: crate::nobd_setup::device_present(),
            setup_msg: None,
            setup_rx: None,
            hide_stick: ui_cfg.hide_stick != 0,
            game_dir: crate::gameinstall::find_game_dir(),
            hook_installed: false,
            hook_msg: None,
            hb_last: 0,
            hb_seen_at: None,
            hb_poll_at: std::time::Instant::now(),
            game_is_running: false,
            keep_controller: ui_cfg.keep_controller != 0,
            startup_hide_done: false,
            auto_input,
            autostart_enabled: crate::nobd_setup::login_task_present(),
            hotplug_at: std::time::Instant::now(),
            bulk_was_present: bulk_present,
            proof_seen: 0,
            proof_pulse: None,
            driver_installed: crate::nobd_setup::driver_installed(pad_type),
            driver_stale: crate::nobd_setup::driver_stale(pad_type),
            window_popup: false,
            install_popup: false,
            details_open: false,
            last_step_done: false,
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
            ui.label(RichText::new("Controller").size(12.0).color(INK_DIM));
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
                                    .color(INK_DIM),
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
        // Extreme Low Latency: if the stick is in NOBD Bulk mode (streaming), read THAT regardless of
        // the XInput/HID picker -- the bulk hop is ~90 us vs the ~500 us XInput poll.
        if crate::bulk::find_device_path(crate::bulk::NOBD_BULK_VID, crate::bulk::NOBD_BULK_PID).is_some() {
            return crate::sync_service::SyncSource::Bulk;
        }
        match (self.source_kind, &self.selected_hid) {
            (SourceKind::Hid, Some(id)) => crate::sync_service::SyncSource::Hid(id.clone()),
            _ => crate::sync_service::SyncSource::XInput,
        }
    }

    /// The master switch. The NOBD Controller exists if and only if NOBD is on.
    ///
    /// The virtual pad's only reason to exist is to carry synced input, so
    /// leaving it in Windows with sync off just put a second, identical Xbox
    /// controller in Steam that nothing explained — XInput exposes no device
    /// identity, so it cannot even be told apart from the real one by name.
    ///
    /// `enabled` survives as a BYPASS (Details, and the tray) for A/B testing:
    /// that keeps the pad bound while passing presses through, so a game does
    /// not have its controller yanked mid-match just to compare.
    fn set_nobd_on(&mut self, on: bool) {
        use std::sync::atomic::Ordering;
        let st = nobd_shared::state();
        if on {
            st.enabled.store(1, Ordering::Relaxed);
            st.reset_stats();
            if !self.controller_present {
                if crate::nobd_setup::is_elevated() {
                    self.begin_turn_on();
                } else if crate::nobd_setup::relaunch_elevated_for_setup(self.pad_type).is_ok() {
                    std::process::exit(0);
                } else {
                    self.setup_msg = Some("Cancelled \u{2014} nothing was installed.".to_owned());
                }
            }
            return;
        }

        // Off: stop syncing AND take the controller back out of Windows.
        st.enabled.store(0, Ordering::Relaxed);
        st.reset_stats();
        self.sync_service = crate::sync_service::SyncService::stopped();
        let r = if crate::nobd_setup::is_elevated() {
            crate::nobd_setup::eject().map(|_| ())
        } else {
            crate::nobd_setup::relaunch_elevated_for_eject()
        };
        match r {
            Ok(()) => {
                self.controller_present = false;
                self.setup_msg = None;
            }
            Err(e) => self.setup_msg = Some(format!("Couldn't remove the controller: {e}")),
        }
        self.apply_stick_hiding();
        self.persist_ui();
    }

    /// Replace the running sync service, OLD ONE FIRST.
    ///
    /// `self.sync_service = SyncService::start(..)` looks equivalent but is not:
    /// Rust builds the new value before dropping the old, so the new thread calls
    /// `NobdController::open` while the previous loop is still submitting. That
    /// open runs `InputChannel::create`, which `write_bytes(0)` over the WHOLE
    /// shared section — so the companion briefly reads an all-zero report, i.e. a
    /// dropped input mid-match, and two loops write the section at once until the
    /// old thread joins. Dropping first makes the handoff clean: assigning
    /// `stopped()` joins the old thread, and only then do we start the new one.
    fn swap_sync(&mut self, source: crate::sync_service::SyncSource) {
        self.sync_service = crate::sync_service::SyncService::stopped();
        self.sync_service = crate::sync_service::SyncService::start(self.pad_type, source);
    }

    /// Restart the sync service on the current source — but only while the NOBD
    /// device is present (i.e. sync is meant to be running). Called when the
    /// input source changes so the synced output follows the selected stick.
    fn restart_sync_if_present(&mut self) {
        if self.controller_present {
            let src = self.sync_source();
            self.swap_sync(src);
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
            keep_controller: self.keep_controller as u32,
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
                        self.driver_installed = true;
                        self.driver_stale = false;
                        self.setup_msg = None;
                        let src = self.sync_source();
                        self.swap_sync(src);
                        self.apply_stick_hiding();
                    }
                    Err(e) => self.setup_msg = Some(format!("Enable failed: {e}")),
                }
                self.setup_rx = None;
            }
        }

        // Hotplug watchdog: the sync resolves its source once at start, so a bulk stick that's
        // unplugged then replugged can leave it stuck on the old source until a manual NOBD toggle.
        // Poll the bulk device at ~1 Hz; the moment it (re)appears, re-resolve + restart the sync --
        // exactly what toggling NOBD off/on does, but automatic.
        if self.controller_present {
            let now = std::time::Instant::now();
            if now.duration_since(self.hotplug_at) >= std::time::Duration::from_millis(1000) {
                self.hotplug_at = now;
                let present = crate::bulk::find_device_path(
                    crate::bulk::NOBD_BULK_VID,
                    crate::bulk::NOBD_BULK_PID,
                )
                .is_some();
                if present && !self.bulk_was_present {
                    self.restart_sync_if_present(); // device just (re)appeared -> re-adopt it
                }
                self.bulk_was_present = present;
            }
        }

        // Reconcile HidHide cloak with intent once, on the first frame — restores
        // hiding after a login-task relaunch, or clears a stale cloak from a crash.
        if !self.startup_hide_done {
            self.apply_stick_hiding();
            self.startup_hide_done = true;
        }

        // Commit heartbeat for the proof panel: any new commit (grouped or
        // single) lights the dot briefly, so you can see the loop reacting to
        // your hands without staring at a counter.
        {
            use std::sync::atomic::Ordering as O;
            let p = &nobd_shared::state().players[0];
            let n = p.groups.load(O::Relaxed) + p.singles.load(O::Relaxed);
            if n != self.proof_seen {
                self.proof_seen = n;
                self.proof_pulse = Some(std::time::Instant::now());
            }
        }

        // Hook liveness + self-install. Polled at ~4 Hz, never per frame: the
        // install check hashes a 370 KB file and the game check spawns tasklist.
        if self.hb_poll_at.elapsed() >= std::time::Duration::from_millis(250) {
            self.hb_poll_at = std::time::Instant::now();
            let hb = nobd_shared::state()
                .dll_heartbeat
                .load(std::sync::atomic::Ordering::Relaxed);
            if hb != self.hb_last {
                self.hb_last = hb;
                self.hb_seen_at = Some(std::time::Instant::now());
            }
            self.game_is_running = crate::gameinstall::game_running();
            if let Some(dir) = self.game_dir.clone() {
                let current = crate::gameinstall::is_current(&dir);
                if current != self.hook_installed {
                    self.hook_installed = current;
                }
                // Plug and play: put ourselves in place without being asked.
                // No elevation involved - Steam grants Users FullControl on its
                // game folders - so there is nothing to prompt about.
                if !current && !self.hook_live() && !self.game_is_running {
                    match crate::gameinstall::ensure_installed(&dir) {
                        Ok(true) => {
                            self.hook_installed = true;
                            self.hook_msg = None;
                        }
                        Ok(false) => {}
                        Err(e) => self.hook_msg = Some(e),
                    }
                }
            }
        }

        // Keep the tray menu's check marks in sync with the live config.
        if let Some(tray) = &self.tray {
            tray.refresh_checks();
        }

        // Poll gamepad — pairs/strays/bounces are tagged per controller now.
        let poll = self.input.as_mut().map(|i| i.poll());
        if let Some(result) = poll {
            // Measured USB frame size (ms) so same-frame bucketing adapts to the
            // device cadence; and the current decision window. Read once here to
            // avoid borrowing self.input while mutating self.stats below.
            let frame_ms = self
                .input
                .as_ref()
                .and_then(|i| i.report_rate_hz())
                .filter(|h| *h > 0.0)
                .map(|h| 1000.0 / h);

            for pair in result.pairs {
                let c = pair.controller;
                self.ensure_pad(c);
                self.stats[c].set_window(crate::stats::DEFAULT_WINDOW);
                if let Some(fm) = frame_ms {
                    self.stats[c].set_frame_ms(fm);
                }
                self.stats[c].record_chord(pair.gap_ms, &pair.buttons, pair.t0_ms);
                self.total_pairs[c] += 1;
                let attempt = self.total_pairs[c];
                let risk = crate::stats::split_risk(pair.gap_ms);
                self.push_log(GapLogEntry::Pair {
                    controller: c,
                    attempt,
                    button_a: format_button(pair.button_a),
                    button_b: format_button(pair.button_b),
                    count: pair.count,
                    gap_ms: pair.gap_ms,
                    risk,
                });
            }
            for stray in result.strays {
                let c = stray.controller;
                self.ensure_pad(c);
                self.stats[c].set_window(crate::stats::DEFAULT_WINDOW);
                // A solo = a single attack button that registered alone — the tell
                // that singles still pass through (sync window, not an OBD macro).
                self.stats[c].record_solo();
                self.stray_counts[c] += 1;
                self.push_log(GapLogEntry::Stray {
                    controller: c,
                    button: format_button(stray.button),
                    solo_ms: stray.solo_ms,
                    reason: stray.reason.label(),
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

        });

        self.draw_main(ctx);

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

// ─── THE ONE SCREEN ───
//
// Five fixed zones and exactly ONE elastic zone. State changes what a zone
// SAYS, never whether it exists or where it sits, and every bit of height
// slack lands in the single elastic zone (the tape). That is what makes "no
// scrollbar at the default size" mechanically true rather than hoped for:
//
//   A  header strip   30px   TopBottomPanel::top     stick > NOBD > game
//   B  hero          ~150px  CentralPanel            is it working / the one fix
//   C  last step      ~96px  CentralPanel            what you still have to do
//   D  tape         elastic  CentralPanel            live proof, absorbs all slack
//   E  control bar    38px   TopBottomPanel::bottom  sync switch + window
//
// The previous version stacked everything in one outer ScrollArea, so opening
// Details grew the page and produced a scrollbar — and 54% of the default
// window was empty while the top was a dense stack of same-weight lines.

/// Set the sync window for every player slot, and clear the counters.
///
/// Writing all slots (not just `[0]`) keeps the tray's quick-set check marks
/// agreeing with the panel. Resetting matters just as much: the headline is
/// "N of M landed together", so a run of misses on a too-tight window would
/// otherwise poison the very number the user is reading to judge the setting
/// they just fixed.
fn set_window_ms(ms: u32) {
    use std::sync::atomic::Ordering;
    let st = nobd_shared::state();
    for w in &st.window_ms {
        w.store(ms.clamp(1, 16), Ordering::Relaxed);
    }
    st.reset_stats();
}

/// What the screen is currently about. Computed once per frame so the header,
/// the hero and the control bar can never contradict each other — the old code
/// derived them separately and could show "NOBD is On" above "NOT RUNNING".
#[derive(PartialEq, Clone, Copy)]
enum Phase {
    /// The driver has never been installed here.
    NotInstalled,
    /// Installed, but the virtual controller has been removed.
    NeedsDevice,
    /// Running, but nothing is reporting.
    NoStick,
    /// Running with a stick, sync deliberately bypassed.
    SyncOff,
    /// Running, syncing, no two-button press seen yet.
    Waiting,
    /// Pressing chords, but the window is narrower than the finger gap.
    TooTight,
    /// Working.
    Working,
}

impl FingerGapApp {
    fn phase(&self) -> Phase {
        use std::sync::atomic::Ordering;
        if !self.controller_present {
            return if self.driver_installed {
                Phase::NeedsDevice
            } else {
                Phase::NotInstalled
            };
        }
        let st = nobd_shared::state();
        if st.enabled.load(Ordering::Relaxed) == 0 {
            return Phase::SyncOff;
        }
        if !self.sync_service.real_present() {
            return Phase::NoStick;
        }
        let p = &st.players[0];
        let groups = p.groups.load(Ordering::Relaxed);
        let attempts = p.attempts.load(Ordering::Relaxed);
        if groups > 0 {
            Phase::Working
        } else if attempts > 0 {
            Phase::TooTight
        } else {
            Phase::Waiting
        }
    }

    fn draw_main(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("header")
            .exact_height(30.0)
            .show(ctx, |ui| self.draw_header(ui));

        egui::TopBottomPanel::bottom("controls")
            .exact_height(38.0)
            .show(ctx, |ui| self.draw_control_bar(ui));

        // The Details drawer grows UPWARD out of the control bar, eating the
        // tape's zone. The header and hero never move.
        if self.details_open {
            egui::TopBottomPanel::bottom("details")
                .resizable(true)
                .default_height(300.0)
                .show(ctx, |ui| {
                    ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| self.draw_details(ui));
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            self.draw_hook_status(ui);
            ui.add_space(10.0);
            self.draw_hero(ui);
            self.draw_update_note(ui);
            ui.add_space(12.0);
            self.draw_last_step(ui);
            ui.add_space(12.0);
            self.draw_tape(ui);
        });

        self.draw_popups(ctx);
    }

    /// A — the path your input takes, and the only place the detected stick is
    /// named. Its dead left segment is the primary no-controller diagnostic,
    /// which is why it keeps 30px rather than being deleted.
    fn draw_header(&mut self, ui: &mut Ui) {
        let phase = self.phase();
        let live = !matches!(phase, Phase::NotInstalled | Phase::NeedsDevice);
        let have_stick = live && self.sync_service.real_present();
        ui.horizontal_centered(|ui| {
            let stick = if have_stick {
                self.active_input_name()
            } else if live {
                "no stick detected".to_owned()
            } else {
                "your stick".to_owned()
            };
            ui.label(
                RichText::new(stick)
                    .size(13.0)
                    .color(if have_stick { ACTION } else { INK_FAINT }),
            );
            flow_arrow(ui);
            ui.label(
                RichText::new("NOBD")
                    .size(13.0)
                    .strong()
                    .color(if phase == Phase::Working { LIVE } else { INK_DIM }),
            );
            flow_arrow(ui);
            // This used to read a static "your game", which named nothing and
            // never changed - decoration in a strip whose other two segments do
            // diagnostic work. It now names the pad the game will see, which is
            // exactly what the user has to select.
            let out_name = match self.pad_type {
                PadType::Hid => "NOBD Controller",
                PadType::Xinput => "NOBD Controller (Xbox 360)",
            };
            ui.label(
                RichText::new(if live { out_name } else { "your game" })
                    .size(13.0)
                    .color(if live { INK_DIM } else { INK_FAINT }),
            );

            // The bulk stream is a property of the input path, not a zone of its
            // own — as a chip it costs zero vertical pixels and cannot make the
            // layout appear and disappear.
            let rate = self.sync_service.bulk_rate();
            if rate > 0 {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{:.0} kHz stream", rate as f32 / 1000.0))
                            .size(11.0)
                            .color(LIVE),
                    );
                });
            }
        });
    }

    /// B — the hero. Always answers "is it working", and when the answer is no
    /// it also HOLDS the button that fixes it, at the same geometry every time.
    /// Is the in-game hook running RIGHT NOW? True only while the heartbeat is
    /// still moving - it goes false a second after the game closes.
    fn hook_live(&self) -> bool {
        self.hb_seen_at
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(1500))
    }

    /// The whole answer, in the order a player asks it: is Marvel here, is NOBD
    /// in it, and is it working right now.
    fn draw_hook_status(&mut self, ui: &mut Ui) {
        let live = self.hook_live();
        let (accent, title, body) = match (&self.game_dir, self.hook_installed, live) {
            (None, _, _) => (
                NEEDS_YOU,
                "MARVEL NOT FOUND",
                "NOBD couldn't find Marvel vs Capcom in your Steam library.".to_owned(),
            ),
            // A loaded DLL cannot be replaced, so say so rather than sitting on
            // "installing" forever while retrying a copy that cannot succeed.
            (Some(_), false, true) => (
                LIVE,
                "NOBD IS LIVE IN MARVEL",
                "An update is ready. It will be applied next time you close the game.".to_owned(),
            ),
            (Some(_), false, false) if self.game_is_running => (
                NEEDS_YOU,
                "CLOSE MARVEL TO FINISH",
                "NOBD has an update for your game folder, and Windows won't let it replace a file the game is using.".to_owned(),
            ),
            (Some(_), false, false) => (
                NEEDS_YOU,
                "INSTALLING\u{2026}",
                self.hook_msg
                    .clone()
                    .unwrap_or_else(|| "Putting NOBD into your Marvel folder.".to_owned()),
            ),
            (Some(_), true, false) => (
                INK_DIM,
                "READY \u{2014} LAUNCH MARVEL",
                "NOBD is installed. It starts with the game; there is nothing to turn on.".to_owned(),
            ),
            (Some(_), true, true) => (
                LIVE,
                "NOBD IS LIVE IN MARVEL",
                "Running inside the game. Your dashes are being grouped as you play.".to_owned(),
            ),
        };

        egui::Frame::new()
            .inner_margin(12.0)
            .corner_radius(10.0)
            .fill(SURFACE)
            .stroke(egui::Stroke::new(1.0, HAIRLINE))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    status_dot(ui, accent, live);
                    ui.add_space(8.0);
                    ui.label(RichText::new(title).size(20.0).strong().color(INK));
                });
                ui.label(RichText::new(body).size(13.0).color(INK_DIM));
                if let Some(dir) = &self.game_dir {
                    ui.label(
                        RichText::new(dir.display().to_string())
                            .size(10.0)
                            .color(INK_FAINT),
                    );
                }
                if let Some(m) = &self.hook_msg {
                    ui.colored_label(NEEDS_YOU, RichText::new(m).size(11.0));
                }
            });
    }

    fn draw_hero(&mut self, ui: &mut Ui) {
        use std::sync::atomic::Ordering;
        let phase = self.phase();
        let p = &nobd_shared::state().players[0];
        let groups = p.groups.load(Ordering::Relaxed);
        let attempts = p.attempts.load(Ordering::Relaxed).max(groups);
        let saved = p.expected_splits_saved();
        let (gap_avg, gap_max) = p.raw_finger_gap_ms();
        let (hold_avg, hold_max) = p.latency_ms();
        let rec = p.recommended_window_ms();

        let lit = self.sync_service.window_open()
            || self
                .proof_pulse
                .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(250));

        // The dot is the ONLY thing that carries state by colour — the title is
        // plain ink and the words name the state, so the card stays legible and
        // a deuteranope can still read it. NEEDS_YOU only where the fix is on
        // screen; sync off is a state the user chose, not an alarm.
        let accent = match phase {
            Phase::Working => LIVE,
            Phase::TooTight | Phase::NoStick => NEEDS_YOU,
            _ => HAIRLINE,
        };

        egui::Frame::new()
            .inner_margin(14.0)
            .corner_radius(10.0)
            .fill(SURFACE)
            // A neutral border. State is carried by the dot and the words, not
            // by a thick coloured frame wrapped around everything.
            .stroke(egui::Stroke::new(1.0, HAIRLINE))
            .show(ui, |ui| {
                ui.set_min_height(112.0);
                ui.set_width(ui.available_width());

                // Headline — the largest type on the screen.
                ui.horizontal(|ui| {
                    if matches!(phase, Phase::Working | Phase::Waiting) {
                        status_dot(ui, if lit { accent } else { HAIRLINE }, true);
                        ui.add_space(8.0);
                    }
                    // Written for someone who knows they miss dashes and has
                    // never heard of a millisecond.
                    let title = match phase {
                        Phase::NotInstalled => "SET UP NOBD",
                        Phase::NeedsDevice => "NOBD IS OFF",
                        Phase::NoStick => "NO CONTROLLER FOUND",
                        Phase::SyncOff => "NOBD IS OFF",
                        Phase::Waiting => "TRY A DASH",
                        Phase::TooTight => "YOUR DASHES ARE STILL DROPPING",
                        Phase::Working => "YOUR DASHES ARE COMING OUT",
                    };
                    ui.label(RichText::new(title).size(22.0).strong().color(INK));
                });

                let sub = match phase {
                    Phase::NotInstalled => "One click. NOBD adds a controller to Windows, and your game reads that instead of your stick.".to_owned(),
                    Phase::NeedsDevice => "Put it back — one click, nothing to reinstall.".to_owned(),
                    Phase::NoStick => "Plug your stick in and press any button.".to_owned(),
                    Phase::SyncOff => "Your dashes will drop the same as they always did.".to_owned(),
                    Phase::Waiting => "Press two punches together, the way you would in a match.".to_owned(),
                    Phase::TooTight => format!("None of your last {attempts} landed. Your hands need a little more slack."),
                    Phase::Working if groups == attempts => format!("{groups} in a row, none dropped."),
                    Phase::Working => format!("{groups} of your last {attempts} landed."),
                };
                ui.label(RichText::new(sub).size(14.0).color(INK));
                ui.add_space(8.0);

                match phase {
                    // Not working -> the hero holds the one click that fixes it.
                    Phase::NotInstalled | Phase::NeedsDevice => {
                        ui.horizontal(|ui| {
                            let label = if phase == Phase::NotInstalled {
                                "Install NOBD Controller"
                            } else {
                                "Turn NOBD on"
                            };
                            if ui
                                .add_sized([260.0, 36.0], egui::Button::new(RichText::new(label).size(15.0).strong()))
                                .clicked()
                            {
                                self.set_nobd_on(true);
                            }
                            if phase == Phase::NotInstalled
                                && ui.button(RichText::new("What this installs").size(11.0).color(ACTION)).clicked()
                            {
                                self.install_popup = !self.install_popup;
                            }
                        });
                    }
                    Phase::SyncOff => {
                        // Bypass: the controller is still there, presses are just
                        // passing through ungrouped.
                        if ui
                            .add_sized([160.0, 34.0], egui::Button::new(RichText::new("Resume syncing").size(14.0).strong()))
                            .clicked()
                        {
                            let st = nobd_shared::state();
                            st.enabled.store(1, Ordering::Relaxed);
                            st.reset_stats();
                        }
                    }
                    Phase::TooTight => {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("NOBD is waiting a shorter time than your hands actually take.")
                                    .size(12.0)
                                    .color(INK),
                            );
                            if rec > 0
                                && ui
                                    .button(RichText::new("Give me more slack").size(13.0).strong())
                                    .on_hover_text(format!("Widens the wait to {rec} ms, measured from your own hands."))
                                    .clicked()
                            {
                                set_window_ms(rec);
                            }
                        });
                    }
                    Phase::Working => {
                        // The detail line, split at its own delimiters into a
                        // band that spends the width instead of wrapping.
                        // Answers "sync is on, so why do I still see a finger
                        // gap?" The gap is YOUR HANDS, measured before the
                        // window. What the game got is the middle cell: grouped
                        // presses leave in ONE report, so their gap is 0. The
                        // raw number used to sit alone inside this card, which
                        // made it read as NOBD's output and the product look
                        // like it was doing nothing.
                        // Three counts, no units. A player does not need a
                        // millisecond to understand landed / dropped / saved.
                        ui.columns(3, |c| {
                            hero_stat(&mut c[0], &format!("{groups}"), "dashes landed");
                            hero_stat(&mut c[1], &format!("{}", attempts.saturating_sub(groups)), "dropped");
                            hero_stat(&mut c[2], &format!("~{saved:.0}"), "saved by NOBD");
                        });
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(format!(
                                "Your two fingers land about {gap_avg:.1} thousandths of a second apart (worst {gap_max:.1}). NOBD waits {hold_avg:.1} of those to catch the second one \u{2014} a fraction of a single frame (at most {hold_max:.1})."
                            ))
                            .size(11.0)
                            .color(INK_DIM),
                        );
                    }
                    Phase::NoStick | Phase::Waiting => {
                        ui.label(
                            RichText::new("Nothing has reached NOBD yet. Wrong stick? Choose it under Details.")
                                .size(12.0)
                                .color(INK_FAINT),
                        );
                    }
                }
                // Hard failures, surfaced in the one place the user is looking.
                if self.controller_present {
                    let err = self.sync_service.error();
                    if err == crate::sync_service::ERR_NO_XINPUT {
                        ui.colored_label(BROKEN, RichText::new("XInput is unavailable on this system.").size(12.0));
                    } else if err == crate::sync_service::ERR_NO_NOBD {
                        ui.colored_label(BROKEN, RichText::new("Couldn't open the NOBD Controller \u{2014} try removing and re-adding it under Details.").size(12.0));
                    } else if !self.sync_service.is_active() {
                        ui.colored_label(NEEDS_YOU, RichText::new("Starting the sync loop\u{2026}").size(12.0));
                    }
                }
                if let Some(msg) = &self.setup_msg {
                    ui.colored_label(BROKEN, RichText::new(msg).size(12.0));
                }
            });
    }

    /// This build ships a newer driver than the one installed.
    ///
    /// Without this the upgrade is unreachable: setup is only offered when no
    /// controller exists, so anyone with a working NOBD Controller kept the
    /// previous release's driver indefinitely while the app reported Working.
    fn draw_update_note(&mut self, ui: &mut Ui) {
        if !self.controller_present || !self.driver_stale {
            return;
        }
        ui.add_space(8.0);
        egui::Frame::new()
            .inner_margin(10.0)
            .corner_radius(8.0)
            .fill(SURFACE)
            .stroke(egui::Stroke::new(1.0, NEEDS_YOU))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("\u{26A0}").size(14.0).color(NEEDS_YOU));
                    ui.label(
                        RichText::new("This version of NOBD ships a newer controller driver than the one installed.")
                            .size(12.0)
                            .color(INK),
                    );
                    if ui.button(RichText::new("Update it").size(12.0).strong()).clicked() {
                        if crate::nobd_setup::is_elevated() {
                            self.begin_turn_on();
                        } else if crate::nobd_setup::relaunch_elevated_for_setup(self.pad_type).is_ok() {
                            std::process::exit(0);
                        } else {
                            self.setup_msg = Some("Update cancelled.".to_owned());
                        }
                    }
                });
            });
    }

    /// C — the step without which none of the proof above reaches the game. It
    /// used to be two 11px labels, smaller than the card it sat under.
    fn draw_last_step(&mut self, ui: &mut Ui) {
        if self.last_step_done {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Last step done.").size(11.0).color(INK_FAINT));
                if ui.button(RichText::new("show").size(11.0)).clicked() {
                    self.last_step_done = false;
                }
            });
            return;
        }
        let live = self.controller_present;
        let ink = if live { INK } else { INK_FAINT };
        let name = match self.pad_type {
            PadType::Hid => "NOBD Controller",
            PadType::Xinput => "NOBD Controller (Xbox 360)",
        };
        egui::Frame::new()
            .inner_margin(10.0)
            .corner_radius(8.0)
            .fill(SURFACE)
            .stroke(egui::Stroke::new(1.0, HAIRLINE))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("LAST STEP \u{2014} IN YOUR GAME")
                            .size(11.0)
                            .strong()
                            .color(if live { ACTION } else { INK_FAINT }),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if live && ui.button(RichText::new("done").size(11.0)).clicked() {
                            self.last_step_done = true;
                        }
                    });
                });
                ui.label(
                    RichText::new("Open your game's controller settings and select:")
                        .size(13.0)
                        .color(ink),
                );
                ui.label(RichText::new(name).size(15.0).strong().color(ink));

                // XInput exposes no device identity - every XInput pad is just
                // "Xbox 360 Controller" - so when your stick is also XInput the
                // two are literally indistinguishable by name. We DO know which
                // slot is ours (we fingerprint it at startup), and the slot
                // number is the one thing that tells them apart.
                if live {
                    if let Some(slot) = self.sync_service.virtual_slot() {
                        ui.label(
                            RichText::new(format!(
                                "If your game lists two Xbox controllers, NOBD is player {}.",
                                slot + 1
                            ))
                            .size(11.0)
                            .color(INK_DIM),
                        );
                    }
                }

                if live && self.source_kind == SourceKind::XInput && self.pad_type == PadType::Xinput {
                    ui.horizontal_wrapped(|ui| {
                        // NotoEmoji is drawn at scale 0.81, so an inline warning
                        // sign in an 11px string lands at ~9px and reads runty.
                        // Its own label at 14px matches the 11px prose beside it.
                        ui.label(RichText::new("\u{26A0}").size(14.0).color(NEEDS_YOU));
                        ui.label(
                            RichText::new("Your game will see two Xbox controllers \u{2014} yours and NOBD's. If it picks yours, none of this reaches the game.")
                                .size(11.0)
                                .color(NEEDS_YOU),
                        );
                        if !crate::hidhide::is_installed()
                            && ui.button(RichText::new("Hide my stick").size(11.0)).clicked()
                        {
                            if let Err(e) = crate::hidhide::run_installer() {
                                self.setup_msg = Some(format!("Couldn't start the HidHide installer: {e}"));
                            }
                        }
                    });
                }
            });
    }

    /// D — the only elastic zone. Every pixel of slack lands here, so the lower
    /// half is never blank and the page can never produce a scrollbar.
    fn draw_tape(&mut self, ui: &mut Ui) {
        let nobd_slot = self.sync_service.virtual_slot().map(|s| s as usize);
        let st = nobd_shared::state();
        let window_ms = st.window_ms[0].load(std::sync::atomic::Ordering::Relaxed).clamp(1, 16) as f64;
        // Passthrough when sync is off - the after-column must say so.
        let sync_on = self.controller_present && st.enabled.load(std::sync::atomic::Ordering::Relaxed) != 0;

        // Before and after, on the same row. Two separate readouts made the app
        // look like it was reporting a finger gap it had failed to close.
        ui.horizontal(|ui| {
            ui.label(RichText::new("EVERY TWO-BUTTON PRESS").size(11.0).strong().color(INK_DIM));
            ui.add_space(6.0);
            ui.label(RichText::new("what your hands did").size(11.0).color(INK_DIM));
            flow_arrow(ui);
            ui.label(RichText::new("what the game got").size(11.0).color(INK_DIM));
        });
        let h = ui.available_height().clamp(72.0, 320.0);
        egui::Frame::new()
            .fill(WELL)
            .inner_margin(6.0)
            .corner_radius(6.0)
            .show(ui, |ui| {
                ui.set_min_height(h);
                ui.set_width(ui.available_width());
                ScrollArea::vertical()
                    .id_salt("tape")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut any = false;
                        for e in self
                            .gap_log
                            .iter()
                            .rev()
                            .filter(|e| Some(log_entry_slot(e)) != nobd_slot)
                        {
                            render_log_entry(ui, e, window_ms, sync_on);
                            any = true;
                        }
                        if !any {
                            ui.label(RichText::new("Nothing yet.").size(11.0).color(INK_FAINT));
                        }
                    });
            });
    }

    /// E — the coldest zone on the screen. The sync switch and the sync window
    /// are the same subject; they used to render as two unrelated strata.
    fn draw_control_bar(&mut self, ui: &mut Ui) {
        use std::sync::atomic::Ordering;
        let st = nobd_shared::state();
        let live = self.controller_present;
        let on = st.enabled.load(Ordering::Relaxed) != 0;
        let cur = st.window_ms[0].load(Ordering::Relaxed).clamp(1, 16);
        let rec = st.players[0].recommended_window_ms();
        let name = match cur {
            0..=3 => "Tight",
            4..=6 => "Normal",
            _ => "Loose",
        };

        ui.horizontal_centered(|ui| {
            ui.add_enabled_ui(live, |ui| {
                status_dot(ui, if on && live { LIVE } else { INK_FAINT }, on && live);
                ui.add_space(4.0);
                ui.label(
                    RichText::new(if !live {
                        "NOBD off"
                    } else if on {
                        "NOBD on"
                    } else {
                        "bypassed"
                    })
                    .size(13.0),
                );
                ui.label(RichText::new("\u{00B7}").size(13.0).color(INK_FAINT));
                // The preset NAME is the control; the number is for whoever
                // wants it. A player picking "Normal" needs no unit at all.
                ui.label(RichText::new(format!("slack: {name}")).size(13.0).color(INK_DIM));
                ui.label(RichText::new(format!("({cur} ms)")).size(11.0).color(INK_FAINT));
                if ui.button(RichText::new("Change").size(12.0).color(ACTION)).clicked() {
                    self.window_popup = !self.window_popup;
                }
                if rec > 0 && rec != cur {
                    let looser = rec > cur;
                    if ui
                        .button(
                            RichText::new(if looser { "More slack" } else { "Less slack" })
                                .size(12.0)
                                .strong(),
                        )
                        .on_hover_text(format!("Measured from your own hands: {rec} ms."))
                        .clicked()
                    {
                        set_window_ms(rec);
                    }
                }
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(RichText::new(if self.details_open { "Hide details" } else { "Details" }).size(12.0))
                    .clicked()
                {
                    self.details_open = !self.details_open;
                }
                if ui
                    .button(RichText::new(if live { "Turn NOBD off" } else { "Turn NOBD on" }).size(12.0))
                    .clicked()
                {
                    self.set_nobd_on(!live);
                }
            });
        });
    }

    /// Expansions are OVERLAYS, never inline. Inline they pushed the page and
    /// changed the layout's height with state — the direct cause of the
    /// scrollbar this redesign exists to remove.
    fn draw_popups(&mut self, ctx: &egui::Context) {
        if self.window_popup {
            let mut open = true;
            egui::Window::new("sync_window_popup")
                .title_bar(false)
                .resizable(false)
                .collapsible(false)
                .anchor(egui::Align2::LEFT_BOTTOM, [12.0, -48.0])
                .show(ctx, |ui| {
                    ui.label(RichText::new("SYNC WINDOW").size(11.0).strong().color(ACTION));
                    let cur = nobd_shared::state().window_ms[0]
                        .load(std::sync::atomic::Ordering::Relaxed)
                        .clamp(1, 16);
                    ui.horizontal(|ui| {
                        for (ms, label) in [(3u32, "Tight"), (5, "Normal"), (8, "Loose")] {
                            if ui
                                .selectable_label(cur == ms, RichText::new(format!("{label}  {ms} ms")).size(13.0))
                                .clicked()
                            {
                                set_window_ms(ms);
                            }
                        }
                    });
                    let mut w = cur;
                    if ui.add(egui::Slider::new(&mut w, 1..=16).suffix(" ms")).changed() {
                        set_window_ms(w);
                    }
                    ui.label(
                        RichText::new("Two attacks pressed within this many ms are delivered together, in one report. A lone press waits up to this long.")
                            .size(11.0)
                            .color(INK_DIM),
                    );
                    if ui.button(RichText::new("Close").size(11.0)).clicked() {
                        open = false;
                    }
                });
            if !open {
                self.window_popup = false;
            }
        }

        if self.install_popup {
            let mut open = true;
            egui::Window::new("install_popup")
                .title_bar(false)
                .resizable(false)
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_max_width(460.0);
                    ui.label(RichText::new("WHAT THIS INSTALLS").size(12.0).strong().color(ACTION));
                    ui.add_space(4.0);
                    ui.label(RichText::new("A controller driver \u{2014} a small driver package, added to Windows' driver store.").size(12.0));
                    ui.label(RichText::new("Its signing certificate \u{2014} Windows won't load a driver it can't verify, so NOBD's signing certificate goes on this PC's trusted list. This is the same step HidHide and ViGEmBus take.").size(12.0));
                    ui.label(RichText::new("The NOBD Controller \u{2014} appears in Windows and Steam next to your stick, and stays there after a restart.").size(12.0));
                    ui.add_space(6.0);
                    ui.label(RichText::new("Remove NOBD Controller, under Details, takes the controller back off. The driver and certificate stay installed \u{2014} there is no full uninstall yet.").size(11.0).color(NEEDS_YOU));
                    ui.add_space(6.0);
                    if ui.button("Close").clicked() {
                        open = false;
                    }
                });
            if !open {
                self.install_popup = false;
            }
        }
    }
}

/// One cell of the hero's stat band: a big measured value over a quiet label.
fn hero_stat(ui: &mut Ui, value: &str, label: &str) {
    ui.vertical(|ui| {
        ui.label(RichText::new(value).monospace().size(20.0).color(INK));
        ui.label(RichText::new(label).size(11.0).color(INK_DIM));
    });
}

impl FingerGapApp {
    /// Everything demoted off the primary screen. Nothing in here is needed to
    /// get NOBD working — it is for tuning, diagnosis, and undo.
    fn draw_details(&mut self, ui: &mut Ui) {
        egui::CollapsingHeader::new(RichText::new("Details").size(13.0).color(INK_DIM))
            .default_open(false)
            .show(ui, |ui| {
                self.draw_gap_detail(ui);
                ui.add_space(10.0);
                ui.separator();

                // Genuinely measured (500 samples), but ONCE at sync start and by
                // tight-spinning XInputGetState - which is not how a game reads.
                // Both caveats are stated rather than implied.
                if let Some((mn, avg, mx)) = self.sync_service.latency() {
                    ui.label(RichText::new("COMPANION DELIVERY \u{2014} measured once at startup").size(11.0).strong().color(INK_DIM));
                    ui.label(
                        RichText::new(format!("{avg} \u{00B5}s average (min {mn} / max {mx}), 500 samples \u{2014} submit to readable. A real USB pad polls every ~500\u{2013}1000 \u{00B5}s. Transport only: the game's own 60 Hz read still dominates what you feel."))
                            .size(11.0)
                            .color(INK_DIM),
                    );
                    ui.add_space(10.0);
                }

                ui.label(RichText::new("INPUT \u{2014} the stick NOBD reads").size(11.0).strong().color(INK_DIM));
                if self.sync_service.bulk_rate() > 0 {
                    ui.label(RichText::new("NOBD Bulk stick, auto-detected. Nothing to choose.").size(11.0).color(LIVE));
                } else {
                    // Auto-detect picks this for you on turn-on; the picker is an
                    // override for when it picks wrong. It cannot simply be
                    // deleted: a DirectInput-only stick is invisible to XInput.
                    ui.label(RichText::new("Chosen automatically. Override it only if NOBD picked the wrong stick.").size(11.0).color(INK_FAINT));
                    self.input_source_picker(ui);
                }

                ui.add_space(10.0);
                ui.label(RichText::new("OUTPUT \u{2014} what games see").size(11.0).strong().color(INK_DIM));
                let mut pt = self.pad_type;
                ui.radio_value(&mut pt, PadType::Xinput, RichText::new("XInput / Xbox pad (works in the most games)").size(12.0));
                ui.radio_value(&mut pt, PadType::Hid, RichText::new("Branded \u{201C}NOBD Controller\u{201D} (Steam / DirectInput games)").size(12.0));
                if pt != self.pad_type {
                    // Remove the OLD mode's devnode FIRST. Without this it stays
                    // live in Windows, Steam and every game, driven by nothing -
                    // and since `controller_present` goes false just below, the
                    // Remove button vanishes too, so it could never be taken out
                    // through the UI at all.
                    if let Err(e) = crate::nobd_setup::eject() {
                        self.setup_msg = Some(format!("Couldn't remove the old controller: {e}"));
                    }
                    self.pad_type = pt;
                    self.driver_installed = crate::nobd_setup::driver_installed(pt);
                    self.driver_stale = crate::nobd_setup::driver_stale(pt);
                    // A new output mode needs its own devnode.
                    self.controller_present = false;
                    self.sync_service = crate::sync_service::SyncService::stopped();
                    self.apply_stick_hiding();
                    self.persist_ui();
                }

                if self.source_kind == SourceKind::Hid && crate::hidhide::is_installed() {
                    let mut hide = self.hide_stick;
                    if ui.checkbox(&mut hide, RichText::new("Hide my stick from games (show only NOBD Controller)").size(12.0)).changed() {
                        self.hide_stick = hide;
                        self.apply_stick_hiding();
                        self.persist_ui();
                    }
                }

                ui.add_space(10.0);
                let mut autostart = self.autostart_enabled;
                if ui
                    .checkbox(&mut autostart, RichText::new("Start with Windows").size(12.0))
                    .on_hover_text("Runs NOBD in the tray at login (elevated, no UAC).")
                    .changed()
                {
                    let res = if autostart {
                        crate::nobd_setup::register_login_task()
                    } else {
                        crate::nobd_setup::unregister_login_task()
                    };
                    match res {
                        Ok(()) => self.autostart_enabled = autostart,
                        Err(e) => self.setup_msg = Some(format!("Couldn't change auto-start: {e}")),
                    }
                }

                ui.add_space(10.0);
                ui.separator();
                if self.controller_present {
                    let mut keep = self.keep_controller;
                    if ui
                        .checkbox(&mut keep, RichText::new("Keep the NOBD Controller when NOBD isn't running").size(12.0))
                        .on_hover_text("Off (default): the controller is removed when you quit, so it never shows up in Steam while NOBD is closed. On: it stays, and works without opening the app.")
                        .changed()
                    {
                        self.keep_controller = keep;
                        self.persist_ui();
                    }
                }
                if self.controller_present
                    && ui
                        .button(RichText::new("Remove NOBD Controller").size(12.0).color(INK_DIM))
                        .on_hover_text("Takes the virtual controller back out of Windows. The driver stays installed, so adding it back is instant.")
                        .clicked()
                {
                    // The failure used to be swallowed by `&& eject().is_ok()`,
                    // so a non-elevated click was a completely silent no-op.
                    // Elevate if we have to, the same way Install does. Without
                    // this a non-elevated user got "eject requires elevation" and
                    // no way to act on it - the controller was unremovable.
                    let r = if crate::nobd_setup::is_elevated() {
                        crate::nobd_setup::eject().map(|_| ())
                    } else {
                        crate::nobd_setup::relaunch_elevated_for_eject()
                    };
                    match r {
                        Ok(()) => {
                            self.controller_present = false;
                            self.sync_service = crate::sync_service::SyncService::stopped();
                            self.apply_stick_hiding();
                            self.setup_msg = None;
                            self.persist_ui();
                        }
                        Err(e) => self.setup_msg = Some(format!("Couldn't remove it: {e}")),
                    }
                }
                ui.label(RichText::new("There is no full uninstall yet \u{2014} the driver package and its signing certificate stay on this PC.").size(10.0).color(INK_FAINT));

                ui.add_space(10.0);
                egui::CollapsingHeader::new(RichText::new("How it works").size(12.0).color(ACTION))
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.label("Old games like MvC2 read your controller exactly ONCE per frame \u{2014} 60 times a second, every 16.67 ms. Your stick updates far faster than that, so two buttons a few ms apart \u{2014} your natural finger gap \u{2014} can land on either side of a single read: a dash becomes a stray jab.");
                        ui.add_space(6.0);
                        ui.label("NOBD holds a fresh attack press for up to the sync window. If a second attack arrives inside it, both are delivered in the SAME report, so the game cannot read one without the other. It changes WHEN a press reports, never WHICH buttons, and never how long you held them.");
                        ui.add_space(6.0);
                        ui.label(RichText::new("A lone press costs at most the window. Directions are never delayed.").color(INK_DIM));
                    });
            });
    }

    /// Your finger gap, measured two ways. The sync loop's number is the one that
    /// always works; the gilrs dashboard below it can only see a stick Windows
    /// exposes as a controller (so: nothing at all in NOBD Bulk mode).
    fn draw_gap_detail(&mut self, ui: &mut Ui) {
        use std::sync::atomic::Ordering;
        let p = &nobd_shared::state().players[0];
        let (raw_avg, raw_max) = p.raw_finger_gap_ms();
        let n = p.raw_gap_count.load(Ordering::Relaxed);

        ui.label(RichText::new("YOUR FINGER GAP").size(11.0).strong().color(INK_DIM));
        if n == 0 {
            ui.label(RichText::new("Press two attack buttons at once a few times.").size(11.0).color(INK_FAINT));
        } else {
            let risk = crate::stats::split_risk(raw_avg) * 100.0;
            ui.label(
                RichText::new(format!(
                    "{raw_avg:.1} ms apart on average, worst {raw_max:.1} ms, over {n} attempts \u{2014} measured inside the sync loop"
                ))
                .size(12.0),
            );
            ui.label(
                RichText::new(format!(
                    "Unaided, a 60 Hz game would split about {risk:.0}% of those. Anything NOBD groups is delivered in one report, so its split chance is exactly zero."
                ))
                .size(11.0)
                .color(INK_DIM),
            );
        }

        // The gilrs-side dashboard: only meaningful when Windows sees the stick.
        let real = self.sync_service.real_slot().map(|s| s as usize);
        let empty = GapStats::new();
        let stats: &GapStats = real.and_then(|s| self.stats.get(s)).unwrap_or(&empty);
        if stats.count() == 0 {
            return;
        }
        ui.add_space(8.0);
        if stats.source_too_slow() {
            banner(
                ui,
                NEEDS_YOU,
                SURFACE,
                "CAN'T TELL",
                "Your controller only reports every few ms, so anything faster already looks simultaneous. Turn off Steam Input (or select the stick directly) to measure your real gap.",
                None,
            );
        } else {
            draw_grouping_verdict(ui, stats);
        }

        let drop = stats.split_probability() * 100.0;
        // Commandment 2: no colour is chosen by a measurement. These are facts
        // about the user's hands, not a grade on them.
        let worst = crate::stats::split_risk(stats.max()) * 100.0;
        ui.columns(2, |t| {
            draw_stat_tile(&mut t[0], &format!("{:.1}ms", stats.average()), "Avg gap");
            draw_stat_tile(&mut t[1], &format!("{:.1}\u{2013}{:.1}", stats.min(), stats.max()), "Range ms");
        });
        ui.add_space(4.0);
        ui.columns(2, |t| {
            draw_stat_tile(&mut t[0], &format!("{drop:.0}%"), "Split risk");
            draw_stat_tile(&mut t[1], &format!("{worst:.0}%"), "Worst chord");
        });

        // Before/after, both deterministic: chords inside the window cannot split.
        let win_ms = nobd_shared::state().window_ms[0].load(Ordering::Relaxed).clamp(1, 16);
        let with_nobd = stats.split_probability_with_window(win_ms as f64) * 100.0;
        let grouped = stats.grouped_count(win_ms as f64);
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Without sync:").strong().size(12.0));
            ui.colored_label(INK, format!("~{drop:.0}% split risk"));
            flow_arrow(ui);
            // The ONE sanctioned two-channel device: the "before" number is the
            // world without NOBD (neutral); the "after" is NOBD's own
            // contribution and is the only thing allowed to be green.
            ui.colored_label(LIVE, format!("with sync @{win_ms} ms  ~{with_nobd:.0}%"));
            ui.label(
                RichText::new(format!("({grouped} of your last {} land together \u{2014} those can't split at all)", stats.count()))
                    .size(11.0)
                    .color(INK_DIM),
            );
        });

        if let Some(hz) = self.input.as_ref().and_then(|i| i.report_rate_hz()) {
            ui.label(
                RichText::new(format!("peak report rate ~{hz:.0} Hz"))
                    .size(10.0)
                    .color(INK_FAINT),
            );
        }
    }

}

/// One measured number plus its label, in a HUELESS card.
///
/// It used to take a `color` argument, and that parameter was the mechanism of
/// the "your hands are broken" bug: it let a measurement pick its own colour, so
/// a perfectly legitimate 14 ms finger gap rendered in alarm red. Numbers have
/// no hue — see the charter in `palette.rs`.
fn draw_stat_tile(ui: &mut Ui, value: &str, label: &str) {
    egui::Frame::new()
        .inner_margin(egui::vec2(12.0, 8.0))
        .corner_radius(8.0)
        .fill(SURFACE)
        .stroke(egui::Stroke::new(1.0, HAIRLINE))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(value).monospace().size(24.0).color(INK));
                ui.label(RichText::new(label).size(11.0).color(INK_DIM));
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

/// Render one event-log entry from your raw stick.
///
/// The right-hand column is the ODDS a 60 Hz game splits this chord, not a
/// verdict on it. It used to print "SPLIT" or "1 frame" from a simulated poll
/// clock anchored to our own session epoch — which has no relationship to the
/// game's phase, so two identical 2.1 ms gaps would legitimately print opposite
/// answers on adjacent rows. Only the two ends are certain, and those we state
/// outright.
fn render_log_entry(ui: &mut Ui, e: &GapLogEntry, window_ms: f64, sync_on: bool) {
    match e {
        GapLogEntry::Pair { attempt, button_a, button_b, count, gap_ms, risk, .. } => {
            let chord = if *count > 2 { format!(" ({} btn)", count) } else { String::new() };
            // The after-value is DERIVED, not simulated: the window's rule is
            // exactly "pressed within the window -> committed in one output
            // word", so a gap inside the window reaches the game as 0 ms, and a
            // gap outside it arrives unchanged (NOBD shifts an edge, it never
            // widens one). No phase, no guessing.
            //
            // `sync_on` is load-bearing. With sync off `SyncWindow::process`
            // returns raw passthrough, so the game gets the gap the fingers
            // made. Deriving this from the window alone claimed "0.0 ms one
            // report" on every row of a switched-OFF app.
            let grouped = sync_on && *gap_ms <= window_ms;
            ui.horizontal(|ui| {
                ui.monospace(RichText::new(format!("#{attempt:>3}")).color(INK_FAINT));
                ui.monospace(
                    RichText::new(format!("{:>20}", format!("{button_a}+{button_b}{chord}")))
                        .color(INK),
                );
                flow_arrow(ui);
                if grouped {
                    // The one place green is earned: NOBD's own contribution.
                    ui.monospace(RichText::new("landed together").color(LIVE));
                } else if !sync_on {
                    ui.monospace(
                        RichText::new(format!("NOBD off \u{2014} {}", odds_phrase(*risk))).color(INK_DIM),
                    );
                } else {
                    ui.monospace(
                        RichText::new(format!("too far apart \u{2014} {}", odds_phrase(*risk)))
                            .color(INK_DIM),
                    );
                }
                ui.monospace(
                    RichText::new(format!("  {button_a}+{button_b}{chord}")).color(INK_FAINT),
                );
            });
        }
        GapLogEntry::Stray { button, solo_ms, reason, .. } => {
            ui.horizontal(|ui| {
                ui.label(RichText::new("STRAY").size(11.0).color(INK_DIM));
                ui.monospace(
                    RichText::new(format!("{button} {solo_ms:.1}ms ({reason})"))
                        .color(INK),
                );
            });
        }
        GapLogEntry::Bounce { button, off_ms, .. } => {
            ui.horizontal(|ui| {
                ui.label(RichText::new("BOUNCE").size(11.0).color(INK_DIM));
                ui.monospace(
                    RichText::new(format!("{button} +{off_ms:.1}ms"))
                        .color(INK),
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
                HAIRLINE,
                SURFACE,
                "COLLECTING…",
                &format!("Press two buttons together {left} more time(s) for a verdict."),
                None,
            );
            return;
        }
    };

    let (accent, title, body): (Color32, &str, String) = match grp {
        Grouping::Natural => (
            LIVE,
            "GROUPING OFF",
            format!("{sf:.0}% same-frame — natural finger timing"),
        ),
        Grouping::Window => {
            let win = stats
                .estimated_window_ms()
                .map(|w| format!(" (~{w:.0}ms window)"))
                .unwrap_or_default();
            (ACTION, "GROUPING DETECTED", format!("{sf:.0}% same-frame{win} — presses grouped"))
        }
        Grouping::AlwaysOn => (
            ACTION,
            "GROUPING DETECTED",
            format!("{sf:.0}% same-frame — consistent grouping"),
        ),
        Grouping::Hint => (
            NEEDS_YOU,
            "INCONCLUSIVE",
            format!("{sf:.0}% same-frame — keep going"),
        ),
    };

    banner(ui, accent, SURFACE, title, &body, None);
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
                ui.label(RichText::new(body).size(11.0).color(INK));
                if let Some(d) = detail {
                    ui.label(RichText::new(d).size(11.0).color(INK_DIM));
                }
            });
        });
    ui.add_space(6.0);
}

// (ViGEmBus install helpers removed — the app is all-HIDMaestro now.)
