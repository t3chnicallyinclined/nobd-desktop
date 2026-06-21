use std::time::Instant;

// DirectInput rgbButtons attack bits — buttons 0-7 (face + shoulder).
// From config.ini: Y=btn0 HP, B=btn1 LK, A=btn2 LP, X=btn3 HK, LT=4, RT=5, LB=6, RB=7.
pub const ATTACK_MASK: u16 = 0x00FF; // bits 0-7 = DInput buttons 0-7

// Default window, ms. Tunable live from the tray (config::WINDOW_MS).
pub const SYNC_WINDOW_MS: u128 = 5;

// Direct port of GP2040-CE syncGpioGetAll() (gp2040.cpp:299), adapted from the
// firmware's 1000Hz GPIO loop to the game's per-frame GetDeviceState reads.
//
// Firmware model (the part we mirror byte-for-byte):
//   committed     == gamepad->debouncedGpio  — what the game is allowed to see
//   sync_new      == bits whose RISING EDGE is being held inside the window
//   just_pressed  = raw & ~committed & ~sync_new   (a genuinely new press)
//   just_released = committed & ~raw               (released → clear immediately)
//   sync_new &= raw     — a press released before the window closes is DROPPED
//   on expiry: committed |= sync_new (commit the whole group at once)
//
// Only RISING EDGES are delayed. Already-committed (held) bits and releases pass
// through instantly — so holding ← then mashing LP+HP ships ← immediately and
// only groups the fresh LP/HP, exactly like the stick.
pub struct SyncWindow {
    committed: u16,                   // == debouncedGpio (synced bits only)
    pending: Option<(Instant, u16)>,  // (window_start, sync_new)
    // Record frame-boundary saves from here? True for Defer (this runs at the
    // game's per-frame read cadence, so a window spanning two calls == two
    // frames == a real split). False for Continuous, where this runs at ~1kHz
    // and "spanning two polls" only means >1ms apart, NOT a frame straddle —
    // there the hook counts saves accurately instead.
    pub record_saves: bool,
    // Which player slot (0 = P1, 1 = P2) this window's stats are recorded under.
    pub player: usize,
}

impl Default for SyncWindow {
    fn default() -> Self {
        Self { committed: 0, pending: None, record_saves: true, player: 0 }
    }
}

impl SyncWindow {
    /// A window whose stats are recorded under the given player slot.
    pub fn with_player(player: usize) -> Self {
        Self { player, ..Self::default() }
    }
}

impl SyncWindow {
    // `attack_mask` selects which bits are "attacks" for this input layout:
    //   DInput rgbButtons  → 0x00FF (buttons 0-7)
    //   XInput wButtons    → 0xF300 (A,B,X,Y,LB,RB)
    pub fn process(&mut self, raw: u16, attack_mask: u16) -> u16 {
        // Disabled from the tray → raw passthrough so you can A/B live.
        if !crate::config::enabled() {
            self.committed = raw;
            self.pending = None;
            return raw;
        }

        let window = crate::config::window_ms(self.player);

        // Which bits are subject to the window. Firmware = all buttons; our
        // default = attacks only (directions bypass for zero motion-input lag).
        let synced_mask: u16 = if crate::config::directions_windowed() {
            0xFFFF
        } else {
            attack_mask
        };

        // Bits outside the window always reflect raw immediately.
        let passthru = raw & !synced_mask;
        let raw_s = raw & synced_mask;

        let prev = self.committed;
        // Window already open from a PRIOR poll? Used to detect a frame-boundary
        // save: a group completed across polls means the partner landed on a
        // later frame than the lead → without NOBD they'd have split.
        let was_pending = self.pending.is_some();
        let (mut start, mut sync_new) = match self.pending {
            Some((t, n)) => (Some(t), n),
            None => (None, 0u16),
        };

        let just_pressed  = raw_s & !prev & !sync_new;
        let just_released = prev & !raw_s;

        // Releases are immediate.
        self.committed &= !just_released;
        // Drop any pending press that was released before the window closed.
        sync_new &= raw_s;

        if just_pressed != 0 {
            if start.is_none() {
                start = Some(Instant::now());
                sync_new = just_pressed;
            } else {
                // A partner is joining an already-open window — the elapsed time
                // is the measured finger gap between the lead and this press.
                if let Some(t) = start {
                    crate::config::record_gap(self.player, t.elapsed().as_micros() as u64);
                }
                sync_new |= just_pressed;
            }
        }

        if let Some(t) = start {
            let held = t.elapsed();
            // Commit when the window expires OR we already hold 2+ attacks. The
            // latter is deliver-on-already-grouped: a press that arrived grouped
            // has no partner left to wait for, so commit it now (0 added frames)
            // instead of deferring to the next frame.
            let grouped = (sync_new & attack_mask).count_ones() >= 2;
            if grouped || held.as_millis() >= window {
                self.committed |= sync_new;
                crate::config::record_delivery(self.player, sync_new);
                // Real added latency = how long the leading press was held back.
                crate::config::record_latency(self.player, held.as_micros() as u64);
                // Frame-boundary save: a grouped commit whose lead press was
                // already pending from a prior poll → the partner arrived on a
                // later frame. (Same-poll already-grouped presses don't count.)
                if grouped && was_pending && self.record_saves {
                    crate::config::record_save(self.player);
                }
                sync_new = 0;
                start = None;
            }
        }

        self.pending = start.map(|t| (t, sync_new));
        passthru | self.committed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    const LP: u16 = 1 << 2; // button 2 = A = LP
    const HP: u16 = 1 << 0; // button 0 = Y = HP
    const AM: u16 = ATTACK_MASK; // DInput attack mask for tests

    fn settle() {
        thread::sleep(Duration::from_millis(SYNC_WINDOW_MS as u64 + 2));
    }

    #[test]
    fn solo_attack_delayed_then_committed() {
        let mut w = SyncWindow::default();
        assert_eq!(w.process(LP, AM), 0);   // rising edge held
        settle();
        assert_eq!(w.process(LP, AM), LP);  // window expired → committed
    }

    #[test]
    fn two_attacks_grouped() {
        let mut w = SyncWindow::default();
        assert_eq!(w.process(LP, AM), 0);            // lone LP held, waiting for partner
        // Partner arrives → 2 attacks pending → deliver-on-grouped commits NOW.
        assert_eq!(w.process(LP | HP, AM), LP | HP);
    }

    #[test]
    fn simultaneous_pair_immediate() {
        let mut w = SyncWindow::default();
        // Both attacks in one read → already grouped → 0-frame immediate commit.
        assert_eq!(w.process(LP | HP, AM), LP | HP);
    }

    #[test]
    fn early_released_press_is_dropped() {
        // Mirrors firmware sync_new &= raw: LP released before the window closed
        // is dropped; only HP (still held at expiry) survives.
        let mut w = SyncWindow::default();
        assert_eq!(w.process(LP, AM), 0);   // LP edge held
        assert_eq!(w.process(HP, AM), 0);   // LP released, HP edge held
        settle();
        assert_eq!(w.process(HP, AM), HP);  // LP dropped, HP committed
    }

    #[test]
    fn held_button_passes_through_after_commit() {
        let mut w = SyncWindow::default();
        assert_eq!(w.process(LP, AM), 0);
        settle();
        assert_eq!(w.process(LP, AM), LP);  // committed
        assert_eq!(w.process(LP, AM), LP);  // still held → immediate
        assert_eq!(w.process(0, AM), 0);    // released → immediate
    }

    #[test]
    fn directions_bypass_by_default() {
        // bits 8-9 (START/BACK per config.ini) are outside ATTACK_MASK and, with
        // DIRECTIONS_WINDOWED=false, pass through with zero delay.
        let dirs: u16 = (1 << 8) | (1 << 9);
        let mut w = SyncWindow::default();
        assert_eq!(w.process(LP | dirs, AM), dirs);  // dirs immediate, LP held
        assert_eq!(w.process(LP | dirs, AM), dirs);  // still within window
        settle();
        assert_eq!(w.process(LP | dirs, AM), LP | dirs); // LP now committed
    }
}
