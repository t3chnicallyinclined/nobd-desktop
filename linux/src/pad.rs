//! The pad state model, shared by every source and the sink.
//!
//! Deliberately the **same XInput `wButtons` bitfield the Windows build uses**,
//! for three reasons:
//!   * `SyncWindow` and `ATTACK_MASK` are then byte-for-byte the same logic on
//!     both platforms — one algorithm, one set of parity tests.
//!   * The NOBD Bulk firmware payload is already in XInput format, so the
//!     lowest-latency source needs zero translation.
//!   * Windows-side stats/telemetry code ports across unchanged.
//!
//! evdev in and uinput out are just codecs around this.

/// XInput `wButtons` bits (`XINPUT_GAMEPAD_*`).
pub mod bit {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

/// The six attack buttons of a fighting-game layout: A B X Y LB RB.
/// Identical to `ATTACK_MASK` in the Windows `sync_service`.
pub const ATTACK_MASK: u16 = bit::A | bit::B | bit::X | bit::Y | bit::LEFT_SHOULDER | bit::RIGHT_SHOULDER;

/// Everything a source produces and the sink consumes.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct PadState {
    pub buttons: u16,
    pub lt: u8,
    pub rt: u8,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
}

impl PadState {
    /// Replace only the windowed bits, keeping analog/passthrough fresh. The
    /// sync window owns `buttons`; everything else is delivered untouched, so
    /// motion inputs are never delayed.
    pub fn with_buttons(self, buttons: u16) -> Self {
        Self { buttons, ..self }
    }
}

/// Human-readable name for a button bit. Used by the gap tester and the event
/// log (phase 3); kept here because it belongs with the bit definitions.
#[allow(dead_code)]
pub fn button_name(b: u16) -> &'static str {
    match b {
        bit::DPAD_UP => "Up",
        bit::DPAD_DOWN => "Down",
        bit::DPAD_LEFT => "Left",
        bit::DPAD_RIGHT => "Right",
        bit::START => "Start",
        bit::BACK => "Back",
        bit::LEFT_THUMB => "LS",
        bit::RIGHT_THUMB => "RS",
        bit::LEFT_SHOULDER => "LB",
        bit::RIGHT_SHOULDER => "RB",
        bit::GUIDE => "Guide",
        bit::A => "A",
        bit::B => "B",
        bit::X => "X",
        bit::Y => "Y",
        _ => "?",
    }
}

/// Parse a comma-separated attack-button list ("a,b,x,y,lb,rb") into a mask, so
/// a player on a 4-button or 8-button layout can scope the window to exactly the
/// buttons they chord. Returns `None` on an unknown token.
pub fn parse_mask(s: &str) -> Option<u16> {
    let mut m = 0u16;
    for tok in s.split(',').map(|t| t.trim()).filter(|t| !t.is_empty()) {
        m |= match tok.to_ascii_lowercase().as_str() {
            "a" => bit::A,
            "b" => bit::B,
            "x" => bit::X,
            "y" => bit::Y,
            "lb" | "l1" => bit::LEFT_SHOULDER,
            "rb" | "r1" => bit::RIGHT_SHOULDER,
            "ls" | "l3" => bit::LEFT_THUMB,
            "rs" | "r3" => bit::RIGHT_THUMB,
            "start" => bit::START,
            "back" | "select" => bit::BACK,
            "up" => bit::DPAD_UP,
            "down" => bit::DPAD_DOWN,
            "left" => bit::DPAD_LEFT,
            "right" => bit::DPAD_RIGHT,
            _ => return None,
        };
    }
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_parsing() {
        assert_eq!(parse_mask("a,b,x,y,lb,rb"), Some(ATTACK_MASK));
        assert_eq!(parse_mask(" A , B "), Some(bit::A | bit::B));
        assert_eq!(parse_mask(""), Some(0));
        assert_eq!(parse_mask("a,nope"), None);
    }
}
