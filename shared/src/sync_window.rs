//! Pure NOBD sync window — `(raw, now_us) -> grouped`, no telemetry, no OS deps.
//!
//! Direct port of the firmware's syncGpioGetAll() and the verified C++
//! `driver/nobd-hid-filter/SyncWindow.h` (16/16 parity tests). The caller passes
//! the current time and window in microseconds, so this is testable in isolation
//! and reusable anywhere: the ViGEm system-wide sync, the HID filter, or the DLL.
//!
//! Only RISING EDGES of `synced` bits are delayed; held bits and releases pass
//! through instantly. When driven by a continuous ~1 kHz poll loop, a lone press
//! is released automatically on the tick where the window expires (no injection
//! needed) — exactly like the stick firmware.

pub struct SyncWindow {
    committed: u16, // bits the consumer is allowed to see (== debouncedGpio)
    sync_new: u16,  // rising edges held inside the open window
    start_us: u64,  // when the window opened
    pending: bool,  // is a window currently open?
}

impl Default for SyncWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncWindow {
    pub fn new() -> Self {
        Self { committed: 0, sync_new: 0, start_us: 0, pending: false }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Absolute time (same clock as `process`'s `now_us`) at which an open
    /// window expires, or `None` when nothing is pending.
    ///
    /// A poll-driven caller doesn't need this — it just calls `process` every
    /// tick and the window closes on whichever tick follows expiry. An
    /// event-driven caller does: the Linux daemon arms a `timerfd` on exactly
    /// this deadline so the commit lands at the window edge rather than at the
    /// next poll slot, which is the difference between honouring a 5 ms window
    /// to ~20 µs and to ~1 ms.
    pub fn pending_until(&self, window_us: u32) -> Option<u64> {
        if self.pending {
            Some(self.start_us + window_us as u64)
        } else {
            None
        }
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
            self.committed = raw;
            self.pending = false;
            return raw;
        }

        let passthru = raw & !synced_mask;
        let raw_s = raw & synced_mask;
        let prev = self.committed;

        let mut have_start = self.pending;
        let mut start = if self.pending { self.start_us } else { 0 };
        let mut sync_new = if self.pending { self.sync_new } else { 0 };

        let just_pressed = raw_s & !prev & !sync_new;
        let just_released = prev & !raw_s;

        // Releases are immediate.
        self.committed &= !just_released;
        // Drop any pending press released before the window closed (bounce filter).
        sync_new &= raw_s;

        if just_pressed != 0 {
            if !have_start {
                start = now_us;
                have_start = true;
                sync_new = just_pressed;
            } else {
                sync_new |= just_pressed;
            }
        }

        if have_start {
            let held = now_us - start;
            // Commit on window expiry OR once 2+ attacks are held (deliver-on-grouped).
            let grouped = (sync_new & attack_mask).count_ones() >= 2;
            if grouped || held >= window_us as u64 {
                self.committed |= sync_new;
                sync_new = 0;
                have_start = false;
            }
        }

        self.pending = have_start;
        if have_start {
            self.start_us = start;
            self.sync_new = sync_new;
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
}
