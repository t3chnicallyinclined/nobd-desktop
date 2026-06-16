use gilrs::{Button, EventType, Gilrs};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const PAIR_WINDOW_MS: f64 = 50.0;
const BOUNCE_THRESHOLD_MS: f64 = 5.0;
/// Poll gilrs every 0.125ms (~8kHz) — fast enough to separate events
/// across different USB frames (1ms apart) while keeping CPU usage low.
const POLL_INTERVAL: Duration = Duration::from_micros(125);

pub struct ButtonPair {
    pub button_a: Button,
    pub button_b: Button,
    pub gap_ms: f64,
}

pub struct StrayPress {
    pub button: Button,
    pub solo_ms: f64,
    pub reason: StrayReason,
    pub off_time_ms: Option<f64>,
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
}

#[derive(Clone)]
pub enum InputEvent {
    Pressed(Button),
    Released(Button),
}

enum InputMsg {
    Pressed(Button, Instant),
    Released(Button, Instant),
    GamepadName(Option<String>),
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

pub struct PollResult {
    pub pair: Option<ButtonPair>,
    pub strays: Vec<StrayPress>,
    pub bounces: Vec<BounceEvent>,
    pub events: Vec<InputEvent>,
}

pub struct GamepadInput {
    rx: Receiver<InputMsg>,
    pending: Option<PendingPress>,
    gamepad_name: Option<String>,
    /// Per-button last release time for off-time + bounce tracking.
    last_release: HashMap<ButtonKey, Instant>,
    /// Per-button last press time for off-time on strays.
    last_press: HashMap<ButtonKey, Instant>,
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

            // Force immediate gamepad name check on first iteration
            let mut last_name_check = Instant::now() - Duration::from_secs(10);

            loop {
                // Periodically send gamepad connection status
                if last_name_check.elapsed() >= Duration::from_secs(1) {
                    let name = gilrs
                        .gamepads()
                        .next()
                        .map(|(_, gp)| gp.name().to_string());
                    if tx.send(InputMsg::GamepadName(name)).is_err() {
                        return; // UI thread dropped, exit
                    }
                    last_name_check = Instant::now();
                }

                // Process events — only ONE ButtonPressed per cycle.
                // When gilrs (XInput) detects two state changes in one poll,
                // it buffers both events. By taking only one press and sleeping,
                // the second press gets its own timestamp on the next cycle.
                // Events from different USB frames naturally get different
                // timestamps because gilrs re-polls the OS when the buffer
                // is empty.
                loop {
                    match gilrs.next_event() {
                        Some(event) => match event.event {
                            EventType::ButtonPressed(button, _) => {
                                if tx
                                    .send(InputMsg::Pressed(button, Instant::now()))
                                    .is_err()
                                {
                                    return;
                                }
                                break; // One press per cycle
                            }
                            EventType::ButtonReleased(button, _) => {
                                if tx.send(InputMsg::Released(button, Instant::now())).is_err() {
                                    return;
                                }
                            }
                            _ => {}
                        },
                        None => break, // No more events
                    }
                }

                thread::sleep(POLL_INTERVAL);
            }
        });

        // Wait for init result from thread
        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                rx,
                pending: None,
                gamepad_name: None,
                last_release: HashMap::new(),
                last_press: HashMap::new(),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("Input thread died during init".to_string()),
        }
    }

    /// Calculate off-time for a button (time since last release → this press).
    fn off_time_ms(&self, button: Button) -> Option<f64> {
        let key = ButtonKey::from_button(button);
        let release = self.last_release.get(&key)?;
        let press = self.last_press.get(&key)?;
        let off = press.duration_since(*release).as_secs_f64() * 1000.0;
        if off >= 0.0 { Some(off) } else { None }
    }

    /// Receive timestamped events from the input thread and detect pairs/strays/bounces.
    pub fn poll(&mut self) -> PollResult {
        let mut result = PollResult {
            pair: None,
            strays: Vec::new(),
            bounces: Vec::new(),
            events: Vec::new(),
        };

        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                InputMsg::Pressed(button, timestamp) => {
                    result.events.push(InputEvent::Pressed(button));
                    let key = ButtonKey::from_button(button);

                    // Bounce detection: re-press very quickly after release
                    if let Some(&rel_time) = self.last_release.get(&key) {
                        let off_ms = timestamp.duration_since(rel_time).as_secs_f64() * 1000.0;
                        if off_ms < BOUNCE_THRESHOLD_MS {
                            result.bounces.push(BounceEvent {
                                button,
                                off_ms,
                            });
                        }
                    }

                    // Record press time for off-time tracking
                    self.last_press.insert(key, timestamp);

                    match self.pending.take() {
                        None => {
                            self.pending = Some(PendingPress { button, timestamp });
                        }
                        Some(pending) => {
                            if pending.button == button {
                                // Same button pressed again — refresh timestamp
                                self.pending = Some(PendingPress { button, timestamp });
                            } else {
                                let gap_ms = timestamp
                                    .duration_since(pending.timestamp)
                                    .as_secs_f64()
                                    * 1000.0;

                                if gap_ms <= PAIR_WINDOW_MS {
                                    // Pair detected
                                    result.pair = Some(ButtonPair {
                                        button_a: pending.button,
                                        button_b: button,
                                        gap_ms,
                                    });
                                } else {
                                    // Gap too large — old pending is a stray
                                    result.strays.push(StrayPress {
                                        button: pending.button,
                                        solo_ms: gap_ms,
                                        reason: StrayReason::NoPairArrived,
                                        off_time_ms: self.off_time_ms(pending.button),
                                    });
                                    self.pending = Some(PendingPress { button, timestamp });
                                }
                            }
                        }
                    }
                }
                InputMsg::Released(button, timestamp) => {
                    result.events.push(InputEvent::Released(button));
                    let key = ButtonKey::from_button(button);
                    self.last_release.insert(key, timestamp);

                    // If the released button is the pending press, it's a stray
                    if let Some(ref pending) = self.pending {
                        if pending.button == button {
                            let solo_ms = timestamp
                                .duration_since(pending.timestamp)
                                .as_secs_f64()
                                * 1000.0;
                            result.strays.push(StrayPress {
                                button,
                                solo_ms,
                                reason: StrayReason::ReleasedBeforePair,
                                off_time_ms: self.off_time_ms(button),
                            });
                            self.pending = None;
                        }
                    }
                }
                InputMsg::GamepadName(name) => {
                    self.gamepad_name = name;
                }
            }
        }

        // Expire stale pending press → stray
        if let Some(ref pending) = self.pending {
            let elapsed_ms = pending.timestamp.elapsed().as_secs_f64() * 1000.0;
            if elapsed_ms > PAIR_WINDOW_MS {
                result.strays.push(StrayPress {
                    button: pending.button,
                    solo_ms: elapsed_ms,
                    reason: StrayReason::NoPairArrived,
                    off_time_ms: self.off_time_ms(pending.button),
                });
                self.pending = None;
            }
        }

        result
    }

    pub fn connected_gamepad_name(&self) -> Option<String> {
        self.gamepad_name.clone()
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
