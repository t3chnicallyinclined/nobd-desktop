// NOBD sync window — pure (raw, now_us) -> grouped transform.
//
// Direct port of the firmware's syncGpioGetAll() and the desktop app's
// src/sync_window.rs. Deliberately has NO time/OS dependency: the caller passes
// the current time in microseconds and the window in microseconds. That makes it
//   - unit-testable in plain user mode (feed synthetic timestamps), and
//   - drop-in for the UMDF filter (pass driver time), with identical behavior.
//
// Only RISING EDGES of "synced" bits are delayed. Held bits and releases pass
// through instantly, so holding a direction while mashing two attacks ships the
// direction immediately and only groups the fresh attacks — exactly like the stick.
//
// IMPORTANT (driver integration): a LONE press is held until the window expires.
// Because HID is report-on-change, the device may send NO further report during
// that window, so the driver MUST re-invoke process() on a timer (see DESIGN.md
// "The hold / injection model") to release the lone press after `window_us`.
#pragma once
#include <cstdint>

class SyncWindow {
public:
    // raw          : current raw button bits this poll
    // attack_mask  : which bits count as "attacks" (>=2 of these => a chord)
    // synced_mask  : which bits are subject to the window (attacks only, or all)
    // now_us       : monotonic time in microseconds
    // window_us    : sync window width in microseconds
    // enabled      : false => raw passthrough (live A/B toggle)
    uint16_t process(uint16_t raw, uint16_t attack_mask, uint16_t synced_mask,
                     uint64_t now_us, uint32_t window_us, bool enabled) {
        if (!enabled) {
            committed_ = raw;
            pending_ = false;
            return raw;
        }

        const uint16_t passthru = static_cast<uint16_t>(raw & ~synced_mask);
        const uint16_t raw_s = static_cast<uint16_t>(raw & synced_mask);
        const uint16_t prev = committed_;

        bool have_start = pending_;
        uint64_t start = pending_ ? start_us_ : 0;
        uint16_t sync_new = pending_ ? sync_new_ : 0;

        const uint16_t just_pressed = static_cast<uint16_t>(raw_s & ~prev & ~sync_new);
        const uint16_t just_released = static_cast<uint16_t>(prev & ~raw_s);

        // Releases are immediate.
        committed_ = static_cast<uint16_t>(committed_ & ~just_released);
        // Drop any pending press released before the window closed (bounce filter).
        sync_new = static_cast<uint16_t>(sync_new & raw_s);

        if (just_pressed) {
            if (!have_start) {
                start = now_us;
                have_start = true;
                sync_new = just_pressed;
            } else {
                sync_new = static_cast<uint16_t>(sync_new | just_pressed);
            }
        }

        if (have_start) {
            const uint64_t held = now_us - start;
            // Commit when the window expires OR we already hold 2+ attacks
            // (deliver-on-grouped: nothing left to wait for => 0 added frames).
            const bool grouped = popcount16(sync_new & attack_mask) >= 2;
            if (grouped || held >= window_us) {
                committed_ = static_cast<uint16_t>(committed_ | sync_new);
                sync_new = 0;
                have_start = false;
            }
        }

        pending_ = have_start;
        if (have_start) {
            start_us_ = start;
            sync_new_ = sync_new;
        }
        return static_cast<uint16_t>(passthru | committed_);
    }

    // True while a press is being held inside an open window — the driver uses
    // this to know it must arm/keep a timer to release a lone press on expiry.
    bool windowOpen() const { return pending_; }

    void reset() { committed_ = 0; sync_new_ = 0; start_us_ = 0; pending_ = false; }

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
};
