// Finger-gap input backend — direct XInput polling.
//
// We poll XInputGetState on a background thread at ~2 kHz and gate on
// `dwPacketNumber`, so we observe every USB report (the pad ships state ~once
// per 1 ms USB frame) and timestamp it within ~0.5 ms of arrival. All button
// transitions seen in one poll tick share that tick's timestamp, so buttons that
// changed in the SAME USB report read as a true 0 ms gap. We also raise the
// system timer to 1 ms (timeBeginPeriod) so ticks stay tight and rarely span two
// frames, and we measure the device's actual report interval for transparency.
//
// This replaces gilrs, whose Windows backend poll-and-diffs at only 125 Hz
// (8 ms) — it merged anything faster than ~8 ms and made sub-8 ms finger gaps
// unmeasurable. (We still use `gilrs::Button` purely as the button enum.)
//
// Honest resolution ceiling: one USB frame (~1 ms). Sub-millisecond finger
// timing is collapsed into a single report by the controller before the PC ever
// sees it — it only exists on the controller MCU (where the NOBD firmware runs).

use gilrs::Button;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
use windows_sys::Win32::Media::timeBeginPeriod;

/// Which backend feeds the input pipeline. Both emit the SAME `InputMsg` stream,
/// so everything downstream (poll/cluster/stats/verdict) is source-agnostic.
pub enum InputSourceKind {
    /// XInput / Xbox pads (up to 4 slots) — the default.
    XInput,
    /// One raw HID gamepad (DInput-mode stick) read directly, on slot 0.
    Hid(crate::hid::HidDeviceId),
}

/// Fixed session epoch for the free-running game-poll simulation. Anchored once,
/// the first time it's read — NEVER reset on input, so presses fall at random
/// phase relative to the simulated 60 fps clock (exactly like a real game poll).
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Milliseconds from the session epoch to `t` (saturating).
fn session_ms(t: Instant) -> f64 {
    let e = *EPOCH.get_or_init(Instant::now);
    t.saturating_duration_since(e).as_secs_f64() * 1000.0
}
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::UI::Input::XboxController::XINPUT_STATE;

/// `XInputGetState` resolved at runtime from the real System32 DLL.
type XInputGetStateFn = unsafe extern "system" fn(u32, *mut XINPUT_STATE) -> u32;

/// Load `XInputGetState` from `%SystemRoot%\System32\xinput1_4.dll` by absolute
/// path. We deliberately do NOT statically import it: this workspace also builds
/// an `xinput1_4.dll` proxy, and the app-directory copy would shadow the system
/// DLL in the loader search order and recurse into itself (stack overflow).
fn load_xinput_get_state() -> Option<XInputGetStateFn> {
    unsafe {
        let mut dir = [0u16; 260];
        let n = GetSystemDirectoryW(dir.as_mut_ptr(), dir.len() as u32);
        if n == 0 || n as usize >= dir.len() {
            return None;
        }
        let mut path: Vec<u16> = dir[..n as usize].to_vec();
        path.extend("\\xinput1_4.dll".encode_utf16());
        path.push(0);
        let lib = LoadLibraryW(path.as_ptr());
        if lib == 0 {
            return None;
        }
        let proc = GetProcAddress(lib, b"XInputGetState\0".as_ptr());
        proc.map(|p| std::mem::transmute::<unsafe extern "system" fn() -> isize, XInputGetStateFn>(p))
    }
}

/// Max gap between consecutive presses to keep them in one chord cluster.
const PAIR_WINDOW_MS: f64 = 50.0;
const BOUNCE_THRESHOLD_MS: f64 = 5.0;
/// Poll at ~2 kHz — 2× the 1 kHz USB report rate so adjacent USB frames land in
/// separate ticks (Nyquist) and each report is timestamped promptly. Polling
/// faster buys nothing: the pad makes a new report only once per USB frame, and
/// we skip ticks where `dwPacketNumber` hasn't advanced.
const POLL_INTERVAL: Duration = Duration::from_micros(500);
/// Analog trigger over this (0–255) counts as a digital press.
const TRIGGER_THRESHOLD: u8 = 30;
/// XInput exposes 4 controller slots.
const MAX_SLOTS: usize = 4;

/// XInput button bit → our `Button`. The two high synthetic bits (0x1_0000 /
/// 0x2_0000) are the analog triggers thresholded to digital.
const XINPUT_BUTTONS: &[(u32, Button)] = &[
    (0x1000, Button::South),         // A
    (0x2000, Button::East),          // B
    (0x4000, Button::West),          // X
    (0x8000, Button::North),         // Y
    (0x0100, Button::LeftTrigger),   // LB
    (0x0200, Button::RightTrigger),  // RB
    (0x0040, Button::LeftThumb),     // LS click
    (0x0080, Button::RightThumb),    // RS click
    (0x0020, Button::Select),        // Back
    (0x0010, Button::Start),         // Start
    (0x0001, Button::DPadUp),
    (0x0002, Button::DPadDown),
    (0x0004, Button::DPadLeft),
    (0x0008, Button::DPadRight),
    (0x1_0000, Button::LeftTrigger2),  // LT (analog)
    (0x2_0000, Button::RightTrigger2), // RT (analog)
];

pub struct ButtonPair {
    pub button_a: Button,
    pub button_b: Button,
    /// Number of distinct buttons in the chord (2 for a normal pair, 3+ for
    /// PPP/KKK/assist bursts).
    pub count: usize,
    /// Spread between the first and last press of the chord (ms).
    pub gap_ms: f64,
    /// Every button in the chord, in press order (for fixed-combo detection).
    pub buttons: Vec<Button>,
    /// First-press time in ms since the session epoch — its phase against the
    /// free-running 60 fps clock drives the game-frame split simulation.
    pub t0_ms: f64,
    pub controller: usize,
}

pub struct StrayPress {
    pub button: Button,
    pub solo_ms: f64,
    pub reason: StrayReason,
    pub off_time_ms: Option<f64>,
    pub controller: usize,
}

#[derive(Clone, Copy)]
pub enum StrayReason {
    NoPairArrived,
    ReleasedBeforePair,
}

impl StrayReason {
    pub fn label(&self) -> &'static str {
        match self {
            StrayReason::NoPairArrived => "no pair arrived",
            StrayReason::ReleasedBeforePair => "released before pair",
        }
    }
}

pub struct BounceEvent {
    pub button: Button,
    pub off_ms: f64,
    pub controller: usize,
}

#[derive(Clone)]
pub enum InputEvent {
    Pressed(Button),
    Released(Button),
}

pub(crate) enum InputMsg {
    Pressed(usize, Button, Instant),
    Released(usize, Button, Instant),
    /// Connected slots (index + name) plus the measured min report interval (ms;
    /// 0 = unknown yet).
    Connected(Vec<(usize, String)>, f64),
}

/// A burst of near-simultaneous presses = one intended chord. We measure the
/// spread (first→last press) so 2- AND 3+-button inputs are captured correctly.
struct Cluster {
    presses: Vec<(Button, Instant)>, // distinct buttons, in press order
}

impl Cluster {
    fn new(button: Button, ts: Instant) -> Self {
        Self { presses: vec![(button, ts)] }
    }
    fn contains(&self, b: Button) -> bool {
        self.presses.iter().any(|(x, _)| *x == b)
    }
    fn first(&self) -> (Button, Instant) {
        self.presses[0]
    }
    fn last(&self) -> (Button, Instant) {
        *self.presses.last().unwrap()
    }
    fn spread_ms(&self) -> f64 {
        self.last().1.duration_since(self.first().1).as_secs_f64() * 1000.0
    }
}

/// What a new press does to the open cluster.
enum ClusterAct {
    Start,
    Extend,
    Restart,
}

/// Only attack buttons participate in gap pairing — directions, Start/Select and
/// stick-clicks are ignored (they'd create false pairs and stray noise). They
/// still flow to the Button Monitor via raw events.
fn is_attack(b: Button) -> bool {
    matches!(
        b,
        Button::South
            | Button::East
            | Button::West
            | Button::North
            | Button::LeftTrigger
            | Button::RightTrigger
            | Button::LeftTrigger2
            | Button::RightTrigger2
    )
}

/// Key for per-button HashMap — wraps Button debug string since Button doesn't impl Hash.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ButtonKey(String);

impl ButtonKey {
    fn from_button(b: Button) -> Self {
        Self(format!("{:?}", b))
    }
}

/// Per-controller pairing state.
struct PadState {
    cluster: Option<Cluster>,
    last_release: HashMap<ButtonKey, Instant>,
    last_press: HashMap<ButtonKey, Instant>,
}

impl PadState {
    fn new() -> Self {
        Self { cluster: None, last_release: HashMap::new(), last_press: HashMap::new() }
    }

    fn off_time_ms(&self, button: Button) -> Option<f64> {
        let key = ButtonKey::from_button(button);
        let release = self.last_release.get(&key)?;
        let press = self.last_press.get(&key)?;
        let off = press.duration_since(*release).as_secs_f64() * 1000.0;
        if off >= 0.0 { Some(off) } else { None }
    }
}

/// Turn a closed cluster into a pair (≥2 distinct buttons) or a stray (1).
fn finalize_cluster(
    cl: &Cluster,
    pad: &PadState,
    c: usize,
    single_reason: StrayReason,
    result: &mut PollResult,
) {
    if cl.presses.len() >= 2 {
        result.pairs.push(ButtonPair {
            button_a: cl.first().0,
            button_b: cl.last().0,
            count: cl.presses.len(),
            gap_ms: cl.spread_ms(),
            buttons: cl.presses.iter().map(|(b, _)| *b).collect(),
            t0_ms: session_ms(cl.first().1),
            controller: c,
        });
    } else {
        let (b, ts) = cl.first();
        result.strays.push(StrayPress {
            button: b,
            solo_ms: ts.elapsed().as_secs_f64() * 1000.0,
            reason: single_reason,
            off_time_ms: pad.off_time_ms(b),
            controller: c,
        });
    }
}

pub struct PollResult {
    pub pairs: Vec<ButtonPair>,
    pub strays: Vec<StrayPress>,
    pub bounces: Vec<BounceEvent>,
    /// Raw press/release events, tagged with their controller index.
    pub events: Vec<(usize, InputEvent)>,
}

pub struct GamepadInput {
    rx: Receiver<InputMsg>,
    pads: Vec<PadState>,
    names: Vec<String>,
    connected: Vec<bool>,
    /// Measured minimum interval between USB reports (ms); 0 = unknown.
    report_interval_ms: f64,
}

impl GamepadInput {
    pub fn new(source: InputSourceKind) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();

        match source {
            InputSourceKind::XInput => {
                thread::spawn(move || Self::xinput_loop(tx));
            }
            InputSourceKind::Hid(id) => {
                thread::spawn(move || crate::hid::run_reader(id, tx));
            }
        }

        Ok(Self {
            rx,
            pads: Vec::new(),
            names: Vec::new(),
            connected: Vec::new(),
            report_interval_ms: 0.0,
        })
    }

    /// The XInput polling loop (runs on the background thread). Emits the SAME
    /// `InputMsg` stream as the HID reader, so the pipeline is source-agnostic.
    fn xinput_loop(tx: Sender<InputMsg>) {
            // Resolve XInput from System32 by absolute path. If it can't load,
            // the thread exits quietly — the app keeps running, just no input.
            let xinput_get_state = match load_xinput_get_state() {
                Some(f) => f,
                None => return,
            };
            // Tighten the system timer so our ~0.5 ms sleeps actually land there
            // (default Windows granularity is ~15 ms / jittery).
            unsafe { timeBeginPeriod(1) };

            // Per-slot diff state.
            let mut last_mask = [0u32; MAX_SLOTS];
            let mut last_packet = [u32::MAX; MAX_SLOTS];
            let mut connected = [false; MAX_SLOTS];
            let mut last_rescan = Instant::now() - Duration::from_secs(10);
            let mut last_name_check = Instant::now() - Duration::from_secs(10);
            // Report-rate measurement: smallest gap between consecutive reports.
            let mut last_change: Option<Instant> = None;
            let mut min_interval_ms = f64::INFINITY;

            loop {
                // One timestamp per tick: transitions from the same USB report
                // (same poll) share it, so co-pressed buttons read a 0 ms gap.
                let now = Instant::now();

                // XInputGetState on an EMPTY slot is expensive and can stall the
                // thread, so only probe disconnected slots every couple seconds
                // (Microsoft's guidance). Connected slots are polled every tick.
                let rescan = last_rescan.elapsed() >= Duration::from_secs(2);
                if rescan {
                    last_rescan = Instant::now();
                }

                for slot in 0..MAX_SLOTS {
                    if !connected[slot] && !rescan {
                        continue;
                    }
                    let mut state: XINPUT_STATE = unsafe { std::mem::zeroed() };
                    let res = unsafe { xinput_get_state(slot as u32, &mut state) };

                    if res != 0 {
                        // Not connected — reset so a later reconnect starts clean.
                        if connected[slot] {
                            connected[slot] = false;
                            last_mask[slot] = 0;
                            last_packet[slot] = u32::MAX;
                        }
                        continue;
                    }
                    if !connected[slot] {
                        connected[slot] = true;
                        last_mask[slot] = 0;
                        last_packet[slot] = u32::MAX;
                    }

                    // Skip ticks with no new USB report — nothing changed.
                    if state.dwPacketNumber == last_packet[slot] {
                        continue;
                    }
                    last_packet[slot] = state.dwPacketNumber;

                    // Track the device's report cadence (min inter-report gap).
                    if let Some(prev) = last_change {
                        let d = now.duration_since(prev).as_secs_f64() * 1000.0;
                        if d > 0.05 && d < 100.0 && d < min_interval_ms {
                            min_interval_ms = d;
                        }
                    }
                    last_change = Some(now);

                    let gp = state.Gamepad;
                    let mut mask = gp.wButtons as u32;
                    if gp.bLeftTrigger > TRIGGER_THRESHOLD {
                        mask |= 0x1_0000;
                    }
                    if gp.bRightTrigger > TRIGGER_THRESHOLD {
                        mask |= 0x2_0000;
                    }

                    let changed = mask ^ last_mask[slot];
                    if changed != 0 {
                        for &(bit, button) in XINPUT_BUTTONS {
                            if changed & bit != 0 {
                                let msg = if mask & bit != 0 {
                                    InputMsg::Pressed(slot, button, now)
                                } else {
                                    InputMsg::Released(slot, button, now)
                                };
                                if tx.send(msg).is_err() {
                                    return;
                                }
                            }
                        }
                        last_mask[slot] = mask;
                    }
                }

                // Publish the connected-slot list + report interval periodically.
                if last_name_check.elapsed() >= Duration::from_secs(1) {
                    let list: Vec<(usize, String)> = (0..MAX_SLOTS)
                        .filter(|&i| connected[i])
                        .map(|i| (i, format!("Controller {}", i + 1)))
                        .collect();
                    let interval = if min_interval_ms.is_finite() { min_interval_ms } else { 0.0 };
                    if tx.send(InputMsg::Connected(list, interval)).is_err() {
                        return; // UI dropped — exit
                    }
                    last_name_check = Instant::now();
                }

                thread::sleep(POLL_INTERVAL);
            }
    }

    /// Ensure per-slot state exists up to (and including) `slot`.
    fn ensure_slot(&mut self, slot: usize) {
        while self.pads.len() <= slot {
            self.pads.push(PadState::new());
        }
        if self.names.len() <= slot {
            self.names.resize(slot + 1, String::new());
        }
        if self.connected.len() <= slot {
            self.connected.resize(slot + 1, false);
        }
    }

    /// Receive timestamped events and detect chords/strays/bounces, per controller.
    pub fn poll(&mut self) -> PollResult {
        let mut result = PollResult {
            pairs: Vec::new(),
            strays: Vec::new(),
            bounces: Vec::new(),
            events: Vec::new(),
        };

        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                InputMsg::Connected(list, interval) => {
                    if interval > 0.0 {
                        self.report_interval_ms = interval;
                    }
                    for c in self.connected.iter_mut() {
                        *c = false;
                    }
                    for (slot, name) in list {
                        self.ensure_slot(slot);
                        self.names[slot] = name;
                        self.connected[slot] = true;
                    }
                }
                InputMsg::Pressed(c, button, timestamp) => {
                    self.ensure_slot(c);
                    result.events.push((c, InputEvent::Pressed(button)));
                    if !is_attack(button) {
                        continue; // directions / system buttons don't pair
                    }
                    let key = ButtonKey::from_button(button);
                    let pad = &mut self.pads[c];

                    if let Some(&rel_time) = pad.last_release.get(&key) {
                        let off_ms = timestamp.duration_since(rel_time).as_secs_f64() * 1000.0;
                        if off_ms < BOUNCE_THRESHOLD_MS {
                            result.bounces.push(BounceEvent { button, off_ms, controller: c });
                        }
                    }
                    pad.last_press.insert(key, timestamp);

                    // How does this press relate to the open chord cluster?
                    let act = match &pad.cluster {
                        None => ClusterAct::Start,
                        Some(cl) => {
                            let within = timestamp.duration_since(cl.last().1).as_secs_f64() * 1000.0
                                <= PAIR_WINDOW_MS;
                            if cl.contains(button) || !within {
                                ClusterAct::Restart
                            } else {
                                ClusterAct::Extend
                            }
                        }
                    };
                    match act {
                        ClusterAct::Start => {
                            pad.cluster = Some(Cluster::new(button, timestamp));
                        }
                        ClusterAct::Extend => {
                            pad.cluster.as_mut().unwrap().presses.push((button, timestamp));
                        }
                        ClusterAct::Restart => {
                            let old = pad.cluster.take().unwrap();
                            finalize_cluster(&old, pad, c, StrayReason::NoPairArrived, &mut result);
                            pad.cluster = Some(Cluster::new(button, timestamp));
                        }
                    }
                }
                InputMsg::Released(c, button, timestamp) => {
                    self.ensure_slot(c);
                    result.events.push((c, InputEvent::Released(button)));
                    if !is_attack(button) {
                        continue;
                    }
                    let key = ButtonKey::from_button(button);
                    self.pads[c].last_release.insert(key, timestamp);

                    let pad = &mut self.pads[c];
                    // Releasing a member closes the chord: a ≥2 cluster is a
                    // finished pair, a lone press released first is a stray.
                    let is_member = pad.cluster.as_ref().map_or(false, |cl| cl.contains(button));
                    if is_member {
                        let old = pad.cluster.take().unwrap();
                        finalize_cluster(&old, pad, c, StrayReason::ReleasedBeforePair, &mut result);
                    }
                }
            }
        }

        // Close clusters that have been open longer than the window (held chords).
        for (c, pad) in self.pads.iter_mut().enumerate() {
            let expired = pad
                .cluster
                .as_ref()
                .map_or(false, |cl| cl.last().1.elapsed().as_secs_f64() * 1000.0 > PAIR_WINDOW_MS);
            if expired {
                let old = pad.cluster.take().unwrap();
                finalize_cluster(&old, pad, c, StrayReason::NoPairArrived, &mut result);
            }
        }

        result
    }

    /// Currently-connected controllers as (slot index, name).
    pub fn controllers(&self) -> Vec<(usize, String)> {
        self.connected
            .iter()
            .enumerate()
            .filter(|(_, &c)| c)
            .map(|(i, _)| {
                let name = self.names.get(i).filter(|n| !n.is_empty()).cloned();
                (i, name.unwrap_or_else(|| format!("Controller {}", i + 1)))
            })
            .collect()
    }

    /// Measured device report rate in Hz (None until a few reports arrive).
    pub fn report_rate_hz(&self) -> Option<f64> {
        if self.report_interval_ms > 0.0 {
            Some(1000.0 / self.report_interval_ms)
        } else {
            None
        }
    }
}

pub fn format_button(button: Button) -> String {
    match button {
        Button::South => "A/Cross".to_string(),
        Button::East => "B/Circle".to_string(),
        Button::West => "X/Square".to_string(),
        Button::North => "Y/Triangle".to_string(),
        Button::LeftTrigger => "LB/L1".to_string(),
        Button::RightTrigger => "RB/R1".to_string(),
        Button::LeftTrigger2 => "LT/L2".to_string(),
        Button::RightTrigger2 => "RT/R2".to_string(),
        Button::LeftThumb => "LS".to_string(),
        Button::RightThumb => "RS".to_string(),
        Button::Select => "Back/Select".to_string(),
        Button::Start => "Start".to_string(),
        Button::DPadUp => "DPad Up".to_string(),
        Button::DPadDown => "DPad Down".to_string(),
        Button::DPadLeft => "DPad Left".to_string(),
        Button::DPadRight => "DPad Right".to_string(),
        other => format!("{:?}", other),
    }
}
