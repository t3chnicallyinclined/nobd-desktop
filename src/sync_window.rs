use std::time::Instant;

// XInput wButtons attack bits — face + shoulder buttons only.
// Directions (DPAD), start, back, thumbsticks pass through unchanged.
//   A  = LP  0x1000
//   B  = LK  0x2000
//   X  = HK  0x4000
//   Y  = HP  0x8000
//   LB = A1  0x0100
//   RB = A2  0x0200
pub const ATTACK_MASK: u16 = 0x1000 | 0x2000 | 0x4000 | 0x8000 | 0x0100 | 0x0200;

// How long to hold the window open waiting for more attack buttons (ms).
// Ported from gp2040-custom SYNC_WINDOW_CYCLES (15 cycles ≈ 5-15ms depending
// on USB poll rate). WHY-NOBD.md measures average finger gap at 2-8ms, so 5ms
// covers the vast majority while staying well under one game frame (16.67ms).
pub const SYNC_WINDOW_MS: u128 = 5;

// State machine — mirrors gp2040-custom main.rs attack_pending / sync_timer logic
// but time-based (Instant) instead of USB-cycle-based.
//
// IDLE:       no attack pending, pass through everything immediately.
// COLLECTING: first attack edge seen, window is open.
//             - Suppress ALL attack bits from XInputGetState returns so the
//               game never sees a partial press.
//             - Any additional attack bits that appear during the window get
//               OR'd into accumulated.
//             - When the window expires, deliver the full accumulated set.
pub struct SyncWindow {
    collecting: Option<(Instant, u16)>, // (window_start, accumulated_attacks)
    prev: u16,                          // previous wButtons snapshot for edge detection
}

impl Default for SyncWindow {
    fn default() -> Self {
        Self { collecting: None, prev: 0 }
    }
}

impl SyncWindow {
    pub fn process(&mut self, buttons: u16) -> u16 {
        let dirs   = buttons & !ATTACK_MASK;
        let atks   = buttons & ATTACK_MASK;
        let rising = atks & !self.prev; // newly-pressed attack bits this call
        self.prev  = buttons;

        if let Some((t, acc)) = self.collecting {
            // Window is open — accumulate any attacks now visible.
            // This catches the HP that arrives after the window opened on LP.
            let new_acc = acc | atks;

            if t.elapsed().as_millis() >= SYNC_WINDOW_MS {
                // Window expired — deliver the full accumulated set.
                self.collecting = None;
                dirs | new_acc
            } else {
                // Still collecting — suppress attacks, keep waiting.
                self.collecting = Some((t, new_acc));
                dirs
            }
        } else if rising != 0 {
            // New attack edge — open the window.
            self.collecting = Some((Instant::now(), atks));
            dirs // suppress until window closes
        } else {
            // No window open, no new attack edges — pass through unchanged.
            dirs | atks
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    const LP: u16 = 0x1000;
    const HP: u16 = 0x8000;

    #[test]
    fn solo_attack_delayed_then_delivered() {
        let mut w = SyncWindow::default();
        // First call: LP pressed, window opens, suppressed.
        assert_eq!(w.process(LP), 0);
        // Wait for window to expire.
        thread::sleep(Duration::from_millis(SYNC_WINDOW_MS as u64 + 1));
        // Second call: LP still held, window expired → delivered.
        assert_eq!(w.process(LP), LP);
    }

    #[test]
    fn two_attacks_grouped() {
        let mut w = SyncWindow::default();
        // LP detected, window opens.
        assert_eq!(w.process(LP), 0);
        // HP arrives within the window.
        assert_eq!(w.process(LP | HP), 0);
        // Window expires.
        thread::sleep(Duration::from_millis(SYNC_WINDOW_MS as u64 + 1));
        // Both delivered together.
        assert_eq!(w.process(LP | HP), LP | HP);
    }

    #[test]
    fn blip_press_survives() {
        // LP pressed and released before the window expires.
        // HP arrives during window. LP must still be in the delivery.
        let mut w = SyncWindow::default();
        assert_eq!(w.process(LP), 0);     // LP: window opens
        assert_eq!(w.process(HP), 0);     // LP released, HP pressed: accumulated = LP|HP
        thread::sleep(Duration::from_millis(SYNC_WINDOW_MS as u64 + 1));
        assert_eq!(w.process(HP), LP | HP); // window expired: deliver LP|HP even though LP gone
    }

    #[test]
    fn directions_always_pass_through() {
        let dirs: u16 = 0x0001 | 0x0002; // DPAD_UP | DPAD_DOWN
        let mut w = SyncWindow::default();
        // Even while window is open, directions are immediate.
        assert_eq!(w.process(LP | dirs), dirs);
        assert_eq!(w.process(HP | dirs), dirs);
        thread::sleep(Duration::from_millis(SYNC_WINDOW_MS as u64 + 1));
        assert_eq!(w.process(HP | dirs), LP | HP | dirs);
    }
}
