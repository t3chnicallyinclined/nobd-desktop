// NOBD sync window — pure (raw, now_us) -> grouped transform.
//
// Direct port of the firmware's syncGpioGetAll() and the desktop app's
// shared/src/sync_window.rs. Deliberately has NO time/OS dependency: the caller
// passes the current time in microseconds and the window in microseconds. That
// makes it
//   - unit-testable in plain user mode (feed synthetic timestamps), and
//   - drop-in for the UMDF filter (pass driver time), with identical behavior.
//
// THE CONTRACT
// NOBD changes WHEN an edge reports, never WHICH buttons report and never HOW
// LONG you held them:
//   * Only RISING EDGES of "synced" bits are delayed. Held bits outside
//     synced_mask (directions) pass through instantly, so holding a direction
//     while mashing two attacks ships the direction immediately.
//   * A press is held for at most window_us, and is delivered early the moment
//     2+ attacks are held (deliver-on-grouped => 0 added frames).
//   * A PRESS IS NEVER DELETED. A tap shorter than the window is still
//     delivered, shifted later.
//   * PULSE WIDTH IS PRESERVED. A release is delayed by exactly the same amount
//     its own press was, so a 40 ms hold reaches the game as a 40 ms hold and
//     not as (40 - window).
//
// The last two points are why this is a delay line and not a plain latch: an
// implementation that lets releases through immediately silently shortens every
// press by up to window_us and erases any press shorter than that outright.
//
// IMPORTANT (driver integration): both a lone press and a delayed release land
// on a deadline with no accompanying HID report. Because HID is report-on-change
// the device may send nothing at all in that time, so the driver MUST re-invoke
// process() on a timer at nextDeadlineUs() (see DESIGN.md "The hold / injection
// model"). Arming only on the window expiry is not enough — a delayed release
// would then hang until the next unrelated report.
#pragma once
#include <cstdint>

class SyncWindow {
public:
    static const int NBITS = 16;
    // rel_us_ sentinel: this pending bit is still physically held.
    static const uint64_t HELD = UINT64_MAX;

    // raw          : current raw button bits this poll
    // attack_mask  : which bits count as "attacks" (>=2 of these => a chord)
    // synced_mask  : which bits are subject to the window (attacks only, or all)
    // now_us       : monotonic time in microseconds
    // window_us    : sync window width in microseconds
    // enabled      : false => raw passthrough (live A/B toggle)
    uint16_t process(uint16_t raw, uint16_t attack_mask, uint16_t synced_mask,
                     uint64_t now_us, uint32_t window_us, bool enabled) {
        if (!enabled) {
            // Live A/B toggle: drop every debt so re-enabling starts clean.
            // delay_us_ IS the debt: leaving it behind charged an old delay to
            // a press that was never held back, lengthening the pulse instead of
            // preserving it.
            committed_ = raw;
            sync_new_ = 0;
            pending_ = false;
            releasing_ = 0;
            resumed_ = 0;
            for (int b = 0; b < NBITS; ++b) delay_us_[b] = 0;
            return raw;
        }

        const uint16_t passthru = static_cast<uint16_t>(raw & ~synced_mask);
        const uint16_t raw_s = static_cast<uint16_t>(raw & synced_mask);

        // ---- 1. presses ---------------------------------------------------
        // Re-pressed while its release debt was still running: keep it down and
        // cancel the release, so a bounce reads as one continuous press. Remember
        // it, because to the player this IS a fresh press and must count toward
        // deliver-on-grouped; masking by raw_s drops a bit that goes up again.
        resumed_ = static_cast<uint16_t>((resumed_ | (releasing_ & raw_s)) & raw_s);
        releasing_ = static_cast<uint16_t>(releasing_ & ~raw_s);

        // New = held, not already visible, not already waiting in the window.
        const uint16_t just_pressed =
            static_cast<uint16_t>(raw_s & ~committed_ & ~sync_new_);
        if (just_pressed) {
            if (!pending_) {
                start_us_ = now_us;
                pending_ = true;
            }
            sync_new_ = static_cast<uint16_t>(sync_new_ | just_pressed);
            for (int b = 0; b < NBITS; ++b) {
                if (just_pressed & (1u << b)) {
                    press_us_[b] = now_us;
                    rel_us_[b] = HELD;
                }
            }
        }

        // ---- 2. physical releases -----------------------------------------
        // A pending bit that goes up is NOT dropped: note when it went up and
        // let it commit normally, so a tap shorter than the window still lands.
        // A pending bit that is back down had a bounce — forget the release.
        for (int b = 0; b < NBITS; ++b) {
            const uint16_t m = static_cast<uint16_t>(1u << b);
            if (!(sync_new_ & m)) continue;
            if (raw_s & m) {
                rel_us_[b] = HELD;
            } else if (rel_us_[b] == HELD) {
                rel_us_[b] = now_us;
            }
        }

        // A committed bit that goes up owes exactly the delay its press was held
        // by — that is what preserves the pulse width the player actually made.
        const uint16_t just_released =
            static_cast<uint16_t>(committed_ & ~raw_s & ~releasing_);
        for (int b = 0; b < NBITS; ++b) {
            if (just_released & (1u << b)) {
                release_at_us_[b] = now_us + static_cast<uint64_t>(delay_us_[b]);
            }
        }
        releasing_ = static_cast<uint16_t>(releasing_ | just_released);

        // ---- 3. close the window ------------------------------------------
        if (pending_) {
            // Saturating: a caller can hand us a now_us that stepped backwards,
            // and unsigned wrap here instantly "expires" the window and poisons
            // the debt below.
            const uint64_t held = now_us >= start_us_ ? now_us - start_us_ : 0;
            // Commit when the window expires OR we already hold 2+ attacks
            // (deliver-on-grouped: nothing left to wait for => 0 added frames).
            const bool grouped = popcount16(static_cast<uint16_t>(
                                     (sync_new_ | resumed_) & raw_s & attack_mask)) >= 2;
            if (grouped || held >= window_us) {
                committed_ = static_cast<uint16_t>(committed_ | sync_new_);
                for (int b = 0; b < NBITS; ++b) {
                    if (!(sync_new_ & (1u << b))) continue;
                    // Clamped to the window by contract: without it a long
                    // source outage or a backwards clock produced a debt of
                    // seconds, and the button hung down for all of it.
                    uint64_t raw_d = now_us >= press_us_[b] ? now_us - press_us_[b] : 0;
                    if (raw_d > window_us) raw_d = window_us;
                    const uint32_t d = static_cast<uint32_t>(raw_d);
                    delay_us_[b] = d;
                    // Released before it ever went out: deliver it now and
                    // schedule the release its own delay later, so the game sees
                    // the same pulse width, just shifted.
                    if (rel_us_[b] != HELD) {
                        // Width = commit time + how long it was ACTUALLY held.
                        // `rel + delay` put the deadline in the PAST when the
                        // commit landed late, re-deleting the tap.
                        const uint64_t hold =
                            rel_us_[b] >= press_us_[b] ? rel_us_[b] - press_us_[b] : 0;
                        release_at_us_[b] = now_us + hold;
                        releasing_ = static_cast<uint16_t>(releasing_ | (1u << b));
                    }
                }
                sync_new_ = 0;
                resumed_ = 0;
                pending_ = false;
            }
        }

        // ---- 4. apply releases whose debt has run out ----------------------
        for (int b = 0; b < NBITS; ++b) {
            const uint16_t m = static_cast<uint16_t>(1u << b);
            if ((releasing_ & m) && now_us >= release_at_us_[b]) {
                committed_ = static_cast<uint16_t>(committed_ & ~m);
                releasing_ = static_cast<uint16_t>(releasing_ & ~m);
            }
        }

        return static_cast<uint16_t>(passthru | committed_);
    }

    // True while a press is being held inside an open window.
    bool windowOpen() const { return pending_; }

    // True while anything is owed — an open window OR an outstanding delayed
    // release. The driver must keep a timer armed whenever this is true.
    bool busy() const { return pending_ || releasing_ != 0; }

    // The next absolute time process() must be re-entered: the earlier of the
    // open window's expiry and any outstanding delayed release. Returns false
    // when nothing is owed.
    bool nextDeadlineUs(uint32_t window_us, uint64_t* out) const {
        bool have = false;
        uint64_t next = 0;
        if (pending_) {
            next = start_us_ + window_us;
            have = true;
        }
        for (int b = 0; b < NBITS; ++b) {
            if (!(releasing_ & (1u << b))) continue;
            const uint64_t d = release_at_us_[b];
            if (!have || d < next) {
                next = d;
                have = true;
            }
        }
        if (have && out) *out = next;
        return have;
    }

    void reset() {
        committed_ = 0;
        sync_new_ = 0;
        start_us_ = 0;
        pending_ = false;
        releasing_ = 0;
        resumed_ = 0;
        for (int b = 0; b < NBITS; ++b) {
            press_us_[b] = 0;
            rel_us_[b] = HELD;
            delay_us_[b] = 0;
            release_at_us_[b] = 0;
        }
    }

private:
    static int popcount16(uint16_t x) {
        int c = 0;
        while (x) { c += (x & 1); x >>= 1; }
        return c;
    }

    uint16_t committed_ = 0;  // == debouncedGpio: bits the game is allowed to see
    uint16_t sync_new_ = 0;   // rising edges held inside the open window
    uint64_t start_us_ = 0;   // window open time
    bool pending_ = false;    // is a window currently open?

    // Pending-phase bookkeeping (valid for bits set in sync_new_).
    uint64_t press_us_[NBITS] = {};        // when each pending bit was pressed
    uint64_t rel_us_[NBITS] = {            // physical release time, or HELD
        HELD, HELD, HELD, HELD, HELD, HELD, HELD, HELD,
        HELD, HELD, HELD, HELD, HELD, HELD, HELD, HELD };

    // Committed-phase bookkeeping (valid for bits set in committed_).
    uint32_t delay_us_[NBITS] = {};        // delay its press was held back by
    uint16_t releasing_ = 0;               // physically up, still waiting it out
    uint16_t resumed_ = 0;                 // re-pressed out of a release debt
    uint64_t release_at_us_[NBITS] = {};   // when each may leave the output
};
