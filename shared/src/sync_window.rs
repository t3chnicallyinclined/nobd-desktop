//! Pure NOBD sync window — `(raw, now_us) -> grouped`, no telemetry, no OS deps.
//!
//! Direct port of the firmware's syncGpioGetAll() and the verified C++
//! `driver/nobd-hid-filter/SyncWindow.h`. The caller passes the current time and
//! window in microseconds, so this is testable in isolation and reusable
//! anywhere: the in-app sync service, the HID filter, or the Linux daemon.
//!
//! # The contract
//!
//! NOBD changes **when** an edge reports, never **which** buttons report and
//! never **how long** you held them. Concretely:
//!
//!  * Only RISING EDGES of `synced` bits are delayed. Bits outside `synced_mask`
//!    (directions) pass through untouched.
//!  * A press is held for at most `window_us`, and is delivered early the moment
//!    2+ attacks are held (deliver-on-grouped — nothing left to wait for).
//!  * **A press is never deleted.** A tap shorter than the window is still
//!    delivered, shifted later — it is not swallowed.
//!  * **Pulse width is preserved.** A release is delayed by exactly the same
//!    amount its own press was, so a 40 ms hold reaches the game as a 40 ms
//!    hold, not as `40 - window`.
//!
//! The last two points are why this is a delay line and not just a latch: an
//! implementation that lets releases through immediately silently shortens every
//! press by up to `window_us`, and erases any press shorter than that outright.
//!
//! Event-driven callers (the Linux daemon) must re-enter `process` on
//! [`SyncWindow::next_deadline_us`], which covers both the window expiry and any
//! outstanding release, otherwise a delayed release has nothing to land on.

/// Number of button bits tracked (the width of the `u16` button word).
const NBITS: usize = 16;

/// `rel_us` sentinel: this pending bit is still physically held.
const HELD: u64 = u64::MAX;

/// Iterate the set bit indices of a mask, low to high.
fn bits(mut m: u16) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        if m == 0 {
            None
        } else {
            let b = m.trailing_zeros() as usize;
            m &= m - 1;
            Some(b)
        }
    })
}

pub struct SyncWindow {
    committed: u16, // bits the consumer is allowed to see (== debouncedGpio)
    sync_new: u16,  // rising edges held inside the open window
    start_us: u64,  // when the window opened
    pending: bool,  // is a window currently open?

    // --- pending-phase bookkeeping (valid for bits set in `sync_new`) ---
    /// When each pending bit was pressed. Its delay is measured from here.
    press_us: [u64; NBITS],
    /// When each pending bit was physically released, or `HELD`. A pending bit
    /// that is released stays pending — it is delivered at window close and its
    /// release is then scheduled, rather than being dropped.
    rel_us: [u64; NBITS],

    // --- committed-phase bookkeeping (valid for bits set in `committed`) ---
    /// How long each committed bit's press was held back. Its release owes the
    /// same, which is what keeps the pulse width honest.
    delay_us: [u32; NBITS],
    /// Committed bits that are physically up but still waiting out that debt.
    releasing: u16,
    /// When each bit in `releasing` may actually leave the output.
    release_at_us: [u64; NBITS],
    /// Committed bits the player has just re-pressed OUT of their release debt.
    ///
    /// Such a bit never leaves `committed`, so it is in neither `committed`'s
    /// "new press" test nor `sync_new` - which made it invisible to
    /// deliver-on-grouped. A partner pressed with it then sat out a whole extra
    /// window waiting for a button that was already down.
    resumed: u16,
    /// Largest delay applied by the most recent commit. This is the real cost
    /// the window added; a caller measuring it as `now - first_press` instead
    /// gets the time since the player first touched ANY attack, which is
    /// unbounded when a button is being held.
    last_commit_delay_us: u32,
}

impl Default for SyncWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncWindow {
    pub fn new() -> Self {
        Self {
            committed: 0,
            sync_new: 0,
            start_us: 0,
            pending: false,
            press_us: [0; NBITS],
            rel_us: [HELD; NBITS],
            delay_us: [0; NBITS],
            releasing: 0,
            release_at_us: [0; NBITS],
            resumed: 0,
            last_commit_delay_us: 0,
        }
    }

    /// Delay (µs) the most recent commit actually applied - the largest across
    /// the bits it committed. Only meaningful on a tick where `process` returned
    /// a newly-pressed bit.
    pub fn last_commit_delay_us(&self) -> u32 {
        self.last_commit_delay_us
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Absolute time at which an open window expires, or `None` when nothing is
    /// pending. This covers presses only — see [`Self::next_deadline_us`] for
    /// the deadline an event-driven caller must actually wake on.
    pub fn pending_until(&self, window_us: u32) -> Option<u64> {
        if self.pending {
            Some(self.start_us + window_us as u64)
        } else {
            None
        }
    }

    /// The next absolute time `process` must be re-entered: the earlier of the
    /// open window's expiry and any outstanding delayed release. `None` when the
    /// window is idle and nothing is owed.
    ///
    /// A poll-driven caller doesn't need this — it calls `process` every tick and
    /// each deadline lands on whichever tick follows it. An event-driven caller
    /// does: the Linux daemon arms a `timerfd` on exactly this, so a commit lands
    /// at the window edge (~20 µs, not ~1 ms) and, just as importantly, so a
    /// delayed release still fires when the stick has gone quiet.
    pub fn next_deadline_us(&self, window_us: u32) -> Option<u64> {
        let mut next = self.pending_until(window_us);
        for b in bits(self.releasing) {
            let d = self.release_at_us[b];
            next = Some(match next {
                Some(n) if n <= d => n,
                _ => d,
            });
        }
        next
    }

    /// When the currently-open window started, if any. Used for the "grouping
    /// hold" stat (how long a lead press actually waited).
    pub fn pending_since(&self) -> Option<u64> {
        if self.pending {
            Some(self.start_us)
        } else {
            None
        }
    }

    /// `raw`         : current raw button bits
    /// `attack_mask` : which bits count as attacks (>=2 of these => a chord)
    /// `synced_mask` : which bits are subject to the window (attacks only, or all)
    /// `now_us`      : monotonic time in microseconds
    /// `window_us`   : sync window width in microseconds
    /// `enabled`     : false => raw passthrough (live A/B toggle)
    pub fn process(
        &mut self,
        raw: u16,
        attack_mask: u16,
        synced_mask: u16,
        now_us: u64,
        window_us: u32,
        enabled: bool,
    ) -> u16 {
        if !enabled {
            // Live A/B toggle: drop every debt so re-enabling starts clean.
            // `delay_us` IS the debt - leaving it behind meant a press that had
            // been held back before the toggle charged its old delay to a press
            // that was never held at all, lengthening the pulse instead of
            // preserving it. It also left the telemetry reporting a stale hold
            // while sync was switched off.
            self.committed = raw;
            self.sync_new = 0;
            self.pending = false;
            self.releasing = 0;
            self.resumed = 0;
            self.delay_us = [0; NBITS];
            self.last_commit_delay_us = 0;
            return raw;
        }

        let passthru = raw & !synced_mask;
        let raw_s = raw & synced_mask;

        // ---- 1. presses ---------------------------------------------------
        // Re-pressed while its release debt was still running: keep it down and
        // cancel the release, so a bounce reads as one continuous press. Remember
        // that it happened, because to the player this IS a fresh press and it
        // must count toward deliver-on-grouped. Masking by `raw_s` each tick
        // drops a bit that goes up again.
        self.resumed = (self.resumed | (self.releasing & raw_s)) & raw_s;
        self.releasing &= !raw_s;

        // New = held, not already visible, not already waiting in the window.
        let just_pressed = raw_s & !self.committed & !self.sync_new;
        if just_pressed != 0 {
            if !self.pending {
                self.start_us = now_us;
                self.pending = true;
            }
            self.sync_new |= just_pressed;
            for b in bits(just_pressed) {
                self.press_us[b] = now_us;
                self.rel_us[b] = HELD;
            }
        }

        // ---- 2. physical releases -----------------------------------------
        // A pending bit that goes up is NOT dropped: we note when it went up and
        // let it commit normally, so a tap shorter than the window still lands.
        // A pending bit that is back down had a bounce — forget the release.
        for b in bits(self.sync_new & raw_s) {
            self.rel_us[b] = HELD;
        }
        for b in bits(self.sync_new & !raw_s) {
            if self.rel_us[b] == HELD {
                self.rel_us[b] = now_us;
            }
        }

        // A committed bit that goes up owes exactly the delay its press was held
        // by — that is what preserves the pulse width the player actually made.
        let just_released = self.committed & !raw_s & !self.releasing;
        for b in bits(just_released) {
            self.release_at_us[b] = now_us + self.delay_us[b] as u64;
        }
        self.releasing |= just_released;

        // ---- 3. close the window ------------------------------------------
        if self.pending {
            // saturating: a caller can hand us a `now_us` that stepped
            // backwards (the Linux bulk path translates a firmware clock), and a
            // bare subtraction wrapped to ~1.8e19 - instantly "expiring" the
            // window and, worse, poisoning the debt below.
            let held = now_us.saturating_sub(self.start_us);
            // Commit on window expiry OR once 2+ attacks are HELD (deliver-on-
            // grouped: nothing left to wait for => 0 added frames).
            //
            // Deliberately NOT "commit as soon as every pending button is up".
            // That looks like a latency win for a short tap, but contact bounce
            // is indistinguishable from a release at this layer, so it lets a
            // bouncing button flush its own window and split the chord it was
            // about to group with — the exact failure NOBD exists to prevent.
            // Waiting out the window costs a few ms on a tap and never splits.
            let grouped =
                ((self.sync_new | self.resumed) & raw_s & attack_mask).count_ones() >= 2;
            if grouped || held >= window_us as u64 {
                self.committed |= self.sync_new;
                self.last_commit_delay_us = 0;
                for b in bits(self.sync_new) {
                    // Clamped to the window BY CONTRACT: a press is never held
                    // back longer than that. Without the clamp a long source
                    // outage (the loops skip process() entirely while the stick
                    // is absent) or a backwards clock produced a debt of seconds
                    // - or, after the `as u32` truncation, up to 71 minutes -
                    // and the button hung down for all of it.
                    let d = now_us
                        .saturating_sub(self.press_us[b])
                        .min(window_us as u64) as u32;
                    self.delay_us[b] = d;
                    self.last_commit_delay_us = self.last_commit_delay_us.max(d);
                    // Released before it ever went out: deliver it now and
                    // schedule the release its own delay later, so the game sees
                    // the same pulse width, just shifted.
                    if self.rel_us[b] != HELD {
                        // Width = commit time + how long it was ACTUALLY held.
                        // Deriving the deadline from `rel + delay` instead put it
                        // in the PAST whenever the commit landed late (a sparse
                        // tick, an event-driven caller, a source stall), which
                        // released the bit on the very tick it committed and
                        // re-deleted the tap this whole delay line exists to save.
                        let hold = self.rel_us[b].saturating_sub(self.press_us[b]);
                        self.release_at_us[b] = now_us + hold;
                        self.releasing |= 1 << b;
                    }
                }
                self.sync_new = 0;
                self.resumed = 0;
                self.pending = false;
            }
        }

        // ---- 4. apply releases whose debt has run out ----------------------
        for b in bits(self.releasing) {
            if now_us >= self.release_at_us[b] {
                self.committed &= !(1u16 << b);
                self.releasing &= !(1u16 << b);
            }
        }

        passthru | self.committed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const LP: u16 = 1 << 2;
    const HP: u16 = 1 << 0;
    const AM: u16 = 0x00FF;
    const W: u32 = 5000;

    /// Drive a 1 kHz poll loop over a script of `(ms, raw)` change points and
    /// return, per bit, the ms range it was visible on the output.
    fn visible(script: &[(u64, u16)], window_ms: u32, until_ms: u64, bit: u16) -> Option<(u64, u64)> {
        let mut w = SyncWindow::new();
        let (mut on, mut off) = (None, None);
        let mut raw = 0u16;
        for ms in 0..=until_ms {
            if let Some((_, r)) = script.iter().rev().find(|(t, _)| *t <= ms) {
                raw = *r;
            }
            let out = w.process(raw, AM, AM, ms * 1000, window_ms * 1000, true);
            if out & bit != 0 && on.is_none() {
                on = Some(ms);
            }
            if on.is_some() && out & bit == 0 && off.is_none() {
                off = Some(ms);
            }
        }
        on.map(|o| (o, off.unwrap_or(until_ms)))
    }

    #[test]
    fn solo_delayed_then_committed() {
        let mut w = SyncWindow::new();
        assert_eq!(w.process(LP, AM, AM, 0, W, true), 0);
        assert_eq!(w.process(LP, AM, AM, (W + 1000) as u64, W, true), LP);
    }

    #[test]
    fn pair_grouped_immediately() {
        let mut w = SyncWindow::new();
        assert_eq!(w.process(LP, AM, AM, 0, W, true), 0);
        assert_eq!(w.process(LP | HP, AM, AM, 1000, W, true), LP | HP);
    }

    #[test]
    fn simultaneous_immediate() {
        let mut w = SyncWindow::new();
        assert_eq!(w.process(LP | HP, AM, AM, 0, W, true), LP | HP);
    }

    #[test]
    fn disabled_passthrough() {
        let mut w = SyncWindow::new();
        assert_eq!(w.process(LP, AM, AM, 0, W, false), LP);
    }

    #[test]
    fn directions_are_never_delayed() {
        let mut w = SyncWindow::new();
        let dirs: u16 = 0x0F00; // outside AM
        assert_eq!(w.process(LP | dirs, AM, AM, 0, W, true), dirs);
    }

    // ---- regressions: the window used to eat and shorten presses -----------

    #[test]
    fn short_tap_is_delivered_not_swallowed() {
        // A 2 ms tap under a 5 ms window used to produce NOTHING at all.
        let v = visible(&[(0, LP), (2, 0)], 5, 40, LP);
        assert_eq!(v, Some((5, 7)), "2 ms tap must still reach the game");
    }

    #[test]
    fn short_tap_is_delivered_at_the_widest_window() {
        // A 10 ms tap — an ordinary jab — used to vanish entirely at 16 ms.
        let v = visible(&[(0, LP), (10, 0)], 16, 60, LP);
        assert_eq!(v, Some((16, 26)), "10 ms tap must survive a 16 ms window");
    }

    #[test]
    fn pulse_width_is_preserved() {
        // 40 ms hold, 16 ms window: the game must see 40 ms, not 40 - 16.
        let (on, off) = visible(&[(0, LP), (40, 0)], 16, 90, LP).expect("delivered");
        assert_eq!(off - on, 40, "the window must not shorten the press");
        assert_eq!(on, 16, "press delayed by exactly the window");
    }

    #[test]
    fn grouped_press_adds_no_delay_to_the_late_button() {
        // LP at 0, HP at 3, both released at 40. Deliver-on-grouped fires at 3.
        let script = [(0u64, LP), (3, LP | HP), (40, 0)];
        let lp = visible(&script, 5, 90, LP).expect("LP delivered");
        let hp = visible(&script, 5, 90, HP).expect("HP delivered");
        assert_eq!(lp.0, hp.0, "the chord must land on the same tick");
        assert_eq!(lp.0, 3, "HP closing the group commits immediately");
        assert_eq!(lp.1 - lp.0, 40, "LP width preserved");
        assert_eq!(hp.1 - hp.0, 37, "HP width preserved");
    }

    #[test]
    fn bounce_inside_the_window_reads_as_one_press() {
        // Press, 1 ms bounce, press again — one continuous 30 ms pulse.
        let (on, off) = visible(&[(0, LP), (1, 0), (2, LP), (30, 0)], 5, 70, LP)
            .expect("delivered");
        assert_eq!((on, off - on), (5, 30), "bounce must not split the press");
    }

    #[test]
    fn bounce_on_the_lead_button_still_groups_the_chord() {
        // LP presses at 0, bounces up for 1 ms, and HP joins at 3. The bounce
        // must not flush LP's window early and split what is really a chord.
        let script = [(0u64, LP), (1, 0), (2, LP), (3, LP | HP), (40, 0)];
        let lp = visible(&script, 5, 90, LP).expect("LP delivered");
        let hp = visible(&script, 5, 90, HP).expect("HP delivered");
        assert_eq!(lp.0, hp.0, "a bounced lead button must still group");
        assert_eq!(lp.0, 3, "deliver-on-grouped still fires at the partner");
    }

    #[test]
    fn window_size_changes_grouping() {
        // A 3 ms finger gap splits at 1 ms and groups at 5 ms — the slider works.
        let script = [(0u64, LP), (3, LP | HP), (40, 0)];
        let tight = (
            visible(&script, 1, 90, LP).unwrap().0,
            visible(&script, 1, 90, HP).unwrap().0,
        );
        let loose = (
            visible(&script, 5, 90, LP).unwrap().0,
            visible(&script, 5, 90, HP).unwrap().0,
        );
        assert_ne!(tight.0, tight.1, "1 ms window must let the chord split");
        assert_eq!(loose.0, loose.1, "5 ms window must group the chord");
    }

    #[test]
    fn a_repress_during_the_release_debt_still_groups() {
        // Tap an attack, then within the window press it again TOGETHER with a
        // partner — ordinary mashing. The re-pressed bit never left `committed`,
        // so deliver-on-grouped could not see it and the partner sat out a whole
        // extra window with nothing to wait for.
        const WW: u32 = 16_000;
        let mut w = SyncWindow::new();
        assert_eq!(w.process(LP, AM, AM, 0, WW, true), 0);
        assert_eq!(w.process(LP, AM, AM, 16_000, WW, true), LP); // commits, owes 16 ms
        assert_eq!(w.process(0, AM, AM, 20_000, WW, true), LP); // released, debt running
        assert_eq!(
            w.process(LP | HP, AM, AM, 25_000, WW, true),
            LP | HP,
            "HP waited a full window for a partner that was already down"
        );
    }

    #[test]
    fn a_tap_survives_a_commit_that_lands_late() {
        // Sparse ticks: the caller does not poll between the press and long
        // after the window expired. The tap must still be delivered with its own
        // width, not released on the very tick it commits.
        let mut w = SyncWindow::new();
        assert_eq!(w.process(LP, AM, AM, 0, W, true), 0);
        assert_eq!(w.process(0, AM, AM, 1_000, W, true), 0); // 1 ms tap
        assert_eq!(w.process(0, AM, AM, 7_000, W, true), LP, "the tap was deleted");
        assert_eq!(w.process(0, AM, AM, 7_999, W, true), LP, "width cut short");
        assert_eq!(w.process(0, AM, AM, 8_000, W, true), 0);
    }

    #[test]
    fn a_long_gap_cannot_create_a_giant_release_debt() {
        // The Windows loops skip process() entirely while the source is absent,
        // so a window can be open across a multi-second USB outage. The delay a
        // press "owes" its release is by contract at most the window.
        let mut w = SyncWindow::new();
        w.process(LP, AM, AM, 0, W, true);
        assert_eq!(w.process(LP, AM, AM, 5_000_000, W, true), LP);
        w.process(0, AM, AM, 5_001_000, W, true);
        assert_eq!(
            w.process(0, AM, AM, 5_011_000, W, true),
            0,
            "the button hung far past its release"
        );
    }

    #[test]
    fn toggling_sync_off_clears_the_release_debt() {
        const WW: u32 = 16_000;
        let mut w = SyncWindow::new();
        w.process(LP, AM, AM, 0, WW, true);
        assert_eq!(w.process(LP, AM, AM, 16_000, WW, true), LP); // 16 ms delay recorded
        assert_eq!(w.process(LP, AM, AM, 17_000, WW, false), LP); // off: passthrough
        assert_eq!(w.process(LP, AM, AM, 30_000, WW, true), LP); // on again, no delay earned
        assert_eq!(
            w.process(0, AM, AM, 40_000, WW, true),
            0,
            "an unearned debt from before the toggle held the button down"
        );
    }

    #[test]
    fn a_clock_that_steps_backwards_does_not_strand_a_bit() {
        // The Linux bulk path translates a firmware timestamp and can hand us a
        // `now_us` slightly EARLIER than the previous call.
        let mut w = SyncWindow::new();
        w.process(LP, AM, AM, 1_000_500, W, true);
        w.process(LP, AM, AM, 1_000_300, W, true); // 200 us backwards
        w.process(LP, AM, AM, 1_010_000, W, true); // commit
        w.process(0, AM, AM, 1_011_000, W, true); // release
        assert_eq!(
            w.process(0, AM, AM, 1_021_000, W, true),
            0,
            "a backwards clock step wrapped the debt into a huge hold"
        );
    }

    #[test]
    fn next_deadline_covers_an_outstanding_release() {
        // After a delayed release is scheduled, an event-driven caller must have
        // something to wake on even though no window is open.
        let mut w = SyncWindow::new();
        w.process(LP, AM, AM, 0, W, true);
        w.process(LP, AM, AM, 5_000, W, true); // commit, delay = 5 ms
        assert!(w.pending_until(W).is_none(), "window closed");
        w.process(0, AM, AM, 20_000, W, true); // release -> owes 5 ms
        assert_eq!(
            w.next_deadline_us(W),
            Some(25_000),
            "the delayed release must be wakeable"
        );
    }
}
