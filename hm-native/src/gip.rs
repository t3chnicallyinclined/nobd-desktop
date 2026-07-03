//! GIP (Xbox/XInput) input packer for the XUSB companion path.
//!
//! The prebuilt `HMXInput.dll` companion reads these 14 bytes from `GipData[14]`
//! of the shared section (offset 264) and synthesizes the XInput GET_STATE
//! responses raw-XInput games (e.g. MvC2) read. We only produce the 14 bytes;
//! the companion does all the IOCTL work.
//!
//! Layout (from HMController.cs packer + companion.c decoder):
//!   [0..2]   LX  u16 LE   (companion: i16 = u16 - 32768  → write lx + 32768)
//!   [2..4]   LY  u16 LE   (companion: i16 = 32767 - u16  → write 32767 - ly)
//!   [4..6]   RX  u16 LE
//!   [6..8]   RY  u16 LE
//!   [8..10]  LT  u16 LE   (low 10 bits, 0..1023 = u8*1023/255)
//!   [10..12] RT  u16 LE   (low 10 bits)
//!   [12] btnLow : A 0x01 B 0x02 X 0x04 Y 0x08 LB 0x10 RB 0x20 L3 0x40 R3 0x80
//!   [13] btnHigh: Back 0x01 Start 0x02 | (hat&0xF)<<2 | Guide 0x40

pub const GIP_LEN: usize = 14;

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

/// XInput dpad bits -> GIP 4-bit hat (1=N,2=NE,3=E,4=SE,5=S,6=SW,7=W,8=NW, 0=neutral).
fn hat(buttons: u16) -> u8 {
    let up = buttons & XI_DPAD_UP != 0;
    let down = buttons & XI_DPAD_DOWN != 0;
    let left = buttons & XI_DPAD_LEFT != 0;
    let right = buttons & XI_DPAD_RIGHT != 0;
    match (up, right, down, left) {
        (true, false, false, false) => 1,
        (true, true, false, false) => 2,
        (false, true, false, false) => 3,
        (false, true, true, false) => 4,
        (false, false, true, false) => 5,
        (false, false, true, true) => 6,
        (false, false, false, true) => 7,
        (true, false, false, true) => 8,
        _ => 0,
    }
}

/// Pack a grouped XInput-style frame into the 14-byte GIP buffer.
pub fn pack(buttons: u16, lt: u8, rt: u8, lx: i16, ly: i16, rx: i16, ry: i16) -> [u8; GIP_LEN] {
    let lxu = (lx as i32 + 32768) as u16;
    let lyu = (32767 - ly as i32) as u16; // companion inverts Y
    let rxu = (rx as i32 + 32768) as u16;
    let ryu = (32767 - ry as i32) as u16;
    let ltu = ((lt as u32 * 1023 / 255) as u16) & 0x03FF;
    let rtu = ((rt as u32 * 1023 / 255) as u16) & 0x03FF;

    let mut low: u8 = 0;
    if buttons & XI_A != 0 { low |= 0x01; }
    if buttons & XI_B != 0 { low |= 0x02; }
    if buttons & XI_X != 0 { low |= 0x04; }
    if buttons & XI_Y != 0 { low |= 0x08; }
    if buttons & XI_LB != 0 { low |= 0x10; }
    if buttons & XI_RB != 0 { low |= 0x20; }
    if buttons & XI_LTHUMB != 0 { low |= 0x40; }
    if buttons & XI_RTHUMB != 0 { low |= 0x80; }

    let mut high: u8 = 0;
    if buttons & XI_BACK != 0 { high |= 0x01; }
    if buttons & XI_START != 0 { high |= 0x02; }
    high |= (hat(buttons) & 0x0F) << 2;
    if buttons & XI_GUIDE != 0 { high |= 0x40; }

    let mut g = [0u8; GIP_LEN];
    g[0..2].copy_from_slice(&lxu.to_le_bytes());
    g[2..4].copy_from_slice(&lyu.to_le_bytes());
    g[4..6].copy_from_slice(&rxu.to_le_bytes());
    g[6..8].copy_from_slice(&ryu.to_le_bytes());
    g[8..10].copy_from_slice(&ltu.to_le_bytes());
    g[10..12].copy_from_slice(&rtu.to_le_bytes());
    g[12] = low;
    g[13] = high;
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_centered() {
        let g = pack(0, 0, 0, 0, 0, 0, 0);
        assert_eq!(u16::from_le_bytes([g[0], g[1]]), 0x8000); // LX center
        assert_eq!(u16::from_le_bytes([g[2], g[3]]), 0x7FFF); // LY center (32767)
        assert_eq!(g[12], 0);
        assert_eq!(g[13], 0);
    }

    #[test]
    fn buttons_and_hat() {
        let g = pack(XI_A | XI_START | XI_DPAD_UP, 0, 0, 0, 0, 0, 0);
        assert_eq!(g[12] & 0x01, 0x01); // A
        assert_eq!(g[13] & 0x02, 0x02); // Start
        assert_eq!((g[13] >> 2) & 0x0F, 1); // hat = North
    }

    #[test]
    fn triggers_10bit() {
        let g = pack(0, 255, 0, 0, 0, 0, 0);
        assert_eq!(u16::from_le_bytes([g[8], g[9]]), 1023); // LT full
    }
}
