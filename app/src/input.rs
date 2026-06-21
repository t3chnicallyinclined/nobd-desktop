use gilrs::{Button, EventType, GamepadId, Gilrs};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const PAIR_WINDOW_MS: f64 = 50.0;
const BOUNCE_THRESHOLD_MS: f64 = 5.0;
/// Poll gilrs every 0.125ms (~8kHz).
const POLL_INTERVAL: Duration = Duration::from_micros(125);

pub struct ButtonPair {
    pub button_a: Button,
    pub button_b: Button,
    pub gap_ms: f64,
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

enum InputMsg {
    Pressed(GamepadId, Button, Instant),
    Released(GamepadId, Button, Instant),
    Gamepads(Vec<(GamepadId, String)>),
}

struct PendingPress {
    button: Button,
    timestamp: Instant,
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
    pending: Option<PendingPress>,
    last_release: HashMap<ButtonKey, Instant>,
    last_press: HashMap<ButtonKey, Instant>,
}

impl PadState {
    fn new() -> Self {
        Self { pending: None, last_release: HashMap::new(), last_press: HashMap::new() }
    }

    fn off_time_ms(&self, button: Button) -> Option<f64> {
        let key = ButtonKey::from_button(button);
        let release = self.last_release.get(&key)?;
        let press = self.last_press.get(&key)?;
        let off = press.duration_since(*release).as_secs_f64() * 1000.0;
        if off >= 0.0 { Some(off) } else { None }
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
    idx_of: HashMap<GamepadId, usize>,
    pads: Vec<PadState>,
    names: Vec<String>,
}

impl GamepadInput {
    pub fn new() -> Result<Self, String> {
        let (init_tx, init_rx) = mpsc::sync_channel(1);
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let mut gilrs = match Gilrs::new() {
                Ok(g) => {
                    let _ = init_tx.send(Ok(()));
                    g
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e.to_string()));
                    return;
                }
            };

            let mut last_name_check = Instant::now() - Duration::from_secs(10);

            loop {
                // Periodically send the full connected-gamepad list (id + name).
                if last_name_check.elapsed() >= Duration::from_secs(1) {
                    let list: Vec<(GamepadId, String)> = gilrs
                        .gamepads()
                        .map(|(id, gp)| (id, gp.name().to_string()))
                        .collect();
                    if tx.send(InputMsg::Gamepads(list)).is_err() {
                        return;
                    }
                    last_name_check = Instant::now();
                }

                // One press per cycle so two near-simultaneous presses get
                // distinct timestamps (gilrs re-polls the OS when its buffer empties).
                loop {
                    match gilrs.next_event() {
                        Some(event) => match event.event {
                            EventType::ButtonPressed(button, _) => {
                                if tx.send(InputMsg::Pressed(event.id, button, Instant::now())).is_err() {
                                    return;
                                }
                                break;
                            }
                            EventType::ButtonReleased(button, _) => {
                                if tx.send(InputMsg::Released(event.id, button, Instant::now())).is_err() {
                                    return;
                                }
                            }
                            _ => {}
                        },
                        None => break,
                    }
                }

                thread::sleep(POLL_INTERVAL);
            }
        });

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                rx,
                idx_of: HashMap::new(),
                pads: Vec::new(),
                names: Vec::new(),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("Input thread died during init".to_string()),
        }
    }

    /// Stable 0-based index for a gamepad id (assigns one on first sight).
    fn index_of(&mut self, id: GamepadId) -> usize {
        if let Some(&i) = self.idx_of.get(&id) {
            return i;
        }
        let i = self.pads.len();
        self.idx_of.insert(id, i);
        self.pads.push(PadState::new());
        if self.names.len() <= i {
            self.names.resize(i + 1, String::new());
        }
        i
    }

    /// Receive timestamped events and detect pairs/strays/bounces, per controller.
    pub fn poll(&mut self) -> PollResult {
        let mut result = PollResult {
            pairs: Vec::new(),
            strays: Vec::new(),
            bounces: Vec::new(),
            events: Vec::new(),
        };

        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                InputMsg::Gamepads(list) => {
                    for (id, name) in list {
                        let i = self.index_of(id);
                        if self.names.len() <= i {
                            self.names.resize(i + 1, String::new());
                        }
                        self.names[i] = name;
                    }
                }
                InputMsg::Pressed(id, button, timestamp) => {
                    let c = self.index_of(id);
                    result.events.push((c, InputEvent::Pressed(button)));
                    let key = ButtonKey::from_button(button);
                    let pad = &mut self.pads[c];

                    if let Some(&rel_time) = pad.last_release.get(&key) {
                        let off_ms = timestamp.duration_since(rel_time).as_secs_f64() * 1000.0;
                        if off_ms < BOUNCE_THRESHOLD_MS {
                            result.bounces.push(BounceEvent { button, off_ms, controller: c });
                        }
                    }
                    pad.last_press.insert(key, timestamp);

                    match pad.pending.take() {
                        None => {
                            pad.pending = Some(PendingPress { button, timestamp });
                        }
                        Some(pending) => {
                            if pending.button == button {
                                pad.pending = Some(PendingPress { button, timestamp });
                            } else {
                                let gap_ms =
                                    timestamp.duration_since(pending.timestamp).as_secs_f64() * 1000.0;
                                if gap_ms <= PAIR_WINDOW_MS {
                                    result.pairs.push(ButtonPair {
                                        button_a: pending.button,
                                        button_b: button,
                                        gap_ms,
                                        controller: c,
                                    });
                                } else {
                                    let off = pad.off_time_ms(pending.button);
                                    result.strays.push(StrayPress {
                                        button: pending.button,
                                        solo_ms: gap_ms,
                                        reason: StrayReason::NoPairArrived,
                                        off_time_ms: off,
                                        controller: c,
                                    });
                                    pad.pending = Some(PendingPress { button, timestamp });
                                }
                            }
                        }
                    }
                }
                InputMsg::Released(id, button, timestamp) => {
                    let c = self.index_of(id);
                    result.events.push((c, InputEvent::Released(button)));
                    let key = ButtonKey::from_button(button);
                    self.pads[c].last_release.insert(key, timestamp);

                    let off = self.pads[c].off_time_ms(button);
                    let pad = &mut self.pads[c];
                    if let Some(ref pending) = pad.pending {
                        if pending.button == button {
                            let solo_ms =
                                timestamp.duration_since(pending.timestamp).as_secs_f64() * 1000.0;
                            result.strays.push(StrayPress {
                                button,
                                solo_ms,
                                reason: StrayReason::ReleasedBeforePair,
                                off_time_ms: off,
                                controller: c,
                            });
                            pad.pending = None;
                        }
                    }
                }
            }
        }

        // Expire stale pending presses → strays (per controller).
        for (c, pad) in self.pads.iter_mut().enumerate() {
            let expire = pad
                .pending
                .as_ref()
                .map(|p| p.timestamp.elapsed().as_secs_f64() * 1000.0)
                .filter(|&ms| ms > PAIR_WINDOW_MS);
            if let Some(solo_ms) = expire {
                let button = pad.pending.as_ref().unwrap().button;
                let off = pad.off_time_ms(button);
                result.strays.push(StrayPress {
                    button,
                    solo_ms,
                    reason: StrayReason::NoPairArrived,
                    off_time_ms: off,
                    controller: c,
                });
                pad.pending = None;
            }
        }

        result
    }

    /// Connected controllers as (index, name), index = the stable controller slot.
    pub fn controllers(&self) -> Vec<(usize, String)> {
        self.names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let name = if n.is_empty() { format!("Controller {}", i + 1) } else { n.clone() };
                (i, name)
            })
            .collect()
    }

    /// Name of the first connected controller (for the header status line).
    pub fn connected_gamepad_name(&self) -> Option<String> {
        self.names.iter().find(|n| !n.is_empty()).cloned()
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
