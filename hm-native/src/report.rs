//! Our self-authored plain-HID gamepad descriptor + the report packer.
//!
//! We own BOTH the descriptor bytes (written to the devnode registry at create
//! time) AND the packer, so they're consistent by construction — no dependence
//! on HIDMaestro's HidDescriptorBuilder. No report ID, so the driver passes our
//! `Data[]` bytes through verbatim.
//!
//! Report layout (5 bytes): [X, Y, hat(4)|pad(4), buttons 1-8, buttons 9-14|pad(2)].

/// HID report descriptor: Game Pad, X/Y (8-bit), 8-way hat, 14 buttons.
pub const REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x05, // Usage (Game Pad)
    0xA1, 0x01, // Collection (Application)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x15, 0x00, //     Logical Minimum (0)
    0x26, 0xFF, 0x00, // Logical Maximum (255)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0xC0, //   End Collection
    0x09, 0x39, //   Usage (Hat switch)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0x07, //   Logical Maximum (7)
    0x35, 0x00, //   Physical Minimum (0)
    0x46, 0x3B, 0x01, // Physical Maximum (315)
    0x65, 0x14, //   Unit (Eng Rot: Degrees)
    0x75, 0x04, //   Report Size (4)
    0x95, 0x01, //   Report Count (1)
    0x81, 0x42, //   Input (Data,Var,Abs,Null State)
    0x75, 0x04, //   Report Size (4)  — 4-bit pad after hat
    0x95, 0x01, //   Report Count (1)
    0x81, 0x03, //   Input (Const,Var,Abs)
    0x05, 0x09, //   Usage Page (Button)
    0x19, 0x01, //   Usage Minimum (1)
    0x29, 0x0E, //   Usage Maximum (14)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0x01, //   Logical Maximum (1)
    0x75, 0x01, //   Report Size (1)
    0x95, 0x0E, //   Report Count (14)
    0x81, 0x02, //   Input (Data,Var,Abs)
    0x75, 0x01, //   Report Size (1)  — 2-bit pad after buttons
    0x95, 0x02, //   Report Count (2)
    0x81, 0x03, //   Input (Const,Var,Abs)
    0xC0, // End Collection
];

/// Bytes in our input report.
pub const REPORT_LEN: usize = 5;

// XInput wButtons bits.
const XI_DPAD_UP: u16 = 0x0001;
const XI_DPAD_DOWN: u16 = 0x0002;
const XI_DPAD_LEFT: u16 = 0x0004;
const XI_DPAD_RIGHT: u16 = 0x0008;
const XI_START: u16 = 0x0010;
const XI_BACK: u16 = 0x0020;
const XI_LTHUMB: u16 = 0x0040;
const XI_RTHUMB: u16 = 0x0080;
const XI_LB: u16 = 0x0100;
const XI_RB: u16 = 0x0200;
const XI_GUIDE: u16 = 0x0400;
const XI_A: u16 = 0x1000;
const XI_B: u16 = 0x2000;
const XI_X: u16 = 0x4000;
const XI_Y: u16 = 0x8000;

const TRIGGER_THRESHOLD: u8 = 30;

fn axis_u8(v: i16) -> u8 {
    (((v as i32) + 32768) >> 8) as u8
}

fn set(b: &mut u16, cond: bool, idx: u8) {
    if cond {
        *b |= 1 << (idx - 1);
    }
}

/// XInput dpad bits -> 8-way hat value (0=N..7=NW, 8=neutral/null).
fn hat(buttons: u16) -> u8 {
    let up = buttons & XI_DPAD_UP != 0;
    let down = buttons & XI_DPAD_DOWN != 0;
    let left = buttons & XI_DPAD_LEFT != 0;
    let right = buttons & XI_DPAD_RIGHT != 0;
    match (up, right, down, left) {
        (true, false, false, false) => 0,  // N
        (true, true, false, false) => 1,   // NE
        (false, true, false, false) => 2,  // E
        (false, true, true, false) => 3,   // SE
        (false, false, true, false) => 4,  // S
        (false, false, true, true) => 5,   // SW
        (false, false, false, true) => 6,  // W
        (true, false, false, true) => 7,   // NW
        _ => 8,                            // neutral (null)
    }
}

/// Pack a grouped XInput-style button mask + analog into our 5-byte HID report.
/// Button map: 1=A 2=B 3=X 4=Y 5=LB 6=RB 7=LT 8=RT 9=Back 10=Start 11=L3 12=R3 13=Guide.
pub fn pack(buttons: u16, lt: u8, rt: u8, lx: i16, ly: i16) -> [u8; REPORT_LEN] {
    let mut b: u16 = 0;
    set(&mut b, buttons & XI_A != 0, 1);
    set(&mut b, buttons & XI_B != 0, 2);
    set(&mut b, buttons & XI_X != 0, 3);
    set(&mut b, buttons & XI_Y != 0, 4);
    set(&mut b, buttons & XI_LB != 0, 5);
    set(&mut b, buttons & XI_RB != 0, 6);
    set(&mut b, lt > TRIGGER_THRESHOLD, 7);
    set(&mut b, rt > TRIGGER_THRESHOLD, 8);
    set(&mut b, buttons & XI_BACK != 0, 9);
    set(&mut b, buttons & XI_START != 0, 10);
    set(&mut b, buttons & XI_LTHUMB != 0, 11);
    set(&mut b, buttons & XI_RTHUMB != 0, 12);
    set(&mut b, buttons & XI_GUIDE != 0, 13);

    [
        axis_u8(lx),
        axis_u8(ly),
        hat(buttons) & 0x0F,
        (b & 0xFF) as u8,
        ((b >> 8) & 0x3F) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_center() {
        assert_eq!(axis_u8(0), 128);
        assert_eq!(axis_u8(i16::MIN), 0);
        assert_eq!(axis_u8(i16::MAX), 255);
    }

    #[test]
    fn buttons_map_to_bits() {
        // A -> button 1 (bit0 of byte3); X -> button 3 (bit2 of byte3)
        let r = pack(XI_A | XI_X, 0, 0, 0, 0);
        assert_eq!(r[3], 0b0000_0101);
        // Guide -> button 13 (bit4 of byte4)
        let r = pack(XI_GUIDE, 0, 0, 0, 0);
        assert_eq!(r[4], 0b0001_0000);
    }

    #[test]
    fn triggers_become_buttons_7_8() {
        let r = pack(0, 200, 0, 0, 0); // LT -> button 7 (bit6 of byte3)
        assert_eq!(r[3], 0b0100_0000);
        let r = pack(0, 0, 200, 0, 0); // RT -> button 8 (bit7 of byte3)
        assert_eq!(r[3], 0b1000_0000);
    }

    #[test]
    fn hat_directions() {
        assert_eq!(pack(XI_DPAD_UP, 0, 0, 0, 0)[2], 0);
        assert_eq!(pack(XI_DPAD_RIGHT, 0, 0, 0, 0)[2], 2);
        assert_eq!(pack(XI_DPAD_DOWN | XI_DPAD_LEFT, 0, 0, 0, 0)[2], 5);
        assert_eq!(pack(0, 0, 0, 0, 0)[2], 8); // neutral
    }
}
