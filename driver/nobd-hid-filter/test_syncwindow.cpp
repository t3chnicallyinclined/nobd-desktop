// Standalone test for SyncWindow.h — mirrors the Rust unit tests in
// shared/src/sync_window.rs so we can prove the C++ port is behavior-identical.
// Build: cl /EHsc /std:c++17 test_syncwindow.cpp   (or: g++ -std=c++17)
#include "SyncWindow.h"
#include <cstdio>

static const uint16_t LP = 1u << 2; // button 2 = LP
static const uint16_t HP = 1u << 0; // button 0 = HP
static const uint16_t AM = 0x00FF;  // attack mask
static const uint32_t W  = 5000;    // 5 ms window (us)

static int failures = 0;
#define CHECK(expr, label) do { if (!(expr)) { printf("FAIL: %s\n", label); ++failures; } \
                                else { printf("ok:   %s\n", label); } } while (0)

// Drive a 1 kHz poll loop over a script of {ms, raw} change points and report
// the ms at which `bit` turned on, and how many ms it stayed on. Mirrors the
// `visible()` helper in the Rust tests.
struct Point { uint64_t ms; uint16_t raw; };
static bool visible(const Point* script, int n, uint32_t window_ms, uint64_t until_ms,
                    uint16_t bit, uint64_t* on_ms, uint64_t* width_ms) {
    SyncWindow w;
    bool have_on = false, have_off = false;
    uint64_t on = 0, off = until_ms;
    uint16_t raw = 0;
    for (uint64_t ms = 0; ms <= until_ms; ++ms) {
        for (int i = n - 1; i >= 0; --i) {
            if (script[i].ms <= ms) { raw = script[i].raw; break; }
        }
        const uint16_t out = w.process(raw, AM, AM, ms * 1000, window_ms * 1000, true);
        if ((out & bit) && !have_on) { on = ms; have_on = true; }
        if (have_on && !(out & bit) && !have_off) { off = ms; have_off = true; }
    }
    if (!have_on) return false;
    *on_ms = on;
    *width_ms = off - on;
    return true;
}

int main() {
    // solo_attack_delayed_then_committed
    { SyncWindow w; uint64_t t = 0;
      CHECK(w.process(LP, AM, AM, t, W, true) == 0, "solo: lone LP held");
      t += W + 1000;
      CHECK(w.process(LP, AM, AM, t, W, true) == LP, "solo: committed after window"); }

    // two_attacks_grouped (deliver-on-grouped commits immediately)
    { SyncWindow w; uint64_t t = 0;
      CHECK(w.process(LP, AM, AM, t, W, true) == 0, "group: lone LP held");
      t += 1000; // 1ms later, partner arrives
      CHECK(w.process(LP | HP, AM, AM, t, W, true) == (LP | HP), "group: pair committed now"); }

    // simultaneous_pair_immediate
    { SyncWindow w;
      CHECK(w.process(LP | HP, AM, AM, 0, W, true) == (LP | HP), "simul: same-poll pair immediate"); }

    // early_released_press_is_DELIVERED (it used to be silently dropped)
    { SyncWindow w; uint64_t t = 0;
      CHECK(w.process(LP, AM, AM, t, W, true) == 0, "early: LP edge held");
      t = 1000;
      CHECK(w.process(HP, AM, AM, t, W, true) == 0, "early: LP released, HP held");
      t = 7000; // LP's window (opened at 0) expires at 5000; both commit
      CHECK(w.process(HP, AM, AM, t, W, true) == (LP | HP), "early: LP still delivered");
      t = 8000; // LP owes its 7 ms delay from a 1 ms release -> leaves at 8000
      CHECK(w.process(HP, AM, AM, t, W, true) == HP, "early: LP release lands late"); }

    // held_button_passes_through_after_commit, with the release delayed by the
    // same amount the press was (pulse width preserved).
    { SyncWindow w; uint64_t t = 0;
      CHECK(w.process(LP, AM, AM, t, W, true) == 0, "held: LP edge held");
      t = W + 1000; // 6000: commit, delay = 6 ms
      CHECK(w.process(LP, AM, AM, t, W, true) == LP, "held: committed");
      t = 7000;
      CHECK(w.process(LP, AM, AM, t, W, true) == LP, "held: still held immediate");
      t = 8000; // physical release; owes 6 ms
      CHECK(w.process(0, AM, AM, t, W, true) == LP, "held: release owes the delay");
      t = 14000;
      CHECK(w.process(0, AM, AM, t, W, true) == 0, "held: release lands at +delay"); }

    // directions_bypass_by_default (bits 8,9 outside attack/synced mask)
    { SyncWindow w; uint64_t t = 0;
      uint16_t dirs = (1u << 8) | (1u << 9);
      CHECK(w.process(LP | dirs, AM, AM, t, W, true) == dirs, "dirs: immediate, LP held");
      t += 1000;
      CHECK(w.process(LP | dirs, AM, AM, t, W, true) == dirs, "dirs: still within window");
      t += W + 1000;
      CHECK(w.process(LP | dirs, AM, AM, t, W, true) == (LP | dirs), "dirs: LP now committed"); }

    // disabled => raw passthrough
    { SyncWindow w;
      CHECK(w.process(LP, AM, AM, 0, W, false) == LP, "disabled: raw passthrough"); }

    // --- regressions: the window used to eat and shorten presses ------------

    // short_tap_is_delivered_not_swallowed: a 2 ms tap under a 5 ms window used
    // to produce nothing at all.
    { const Point s[] = { {0, LP}, {2, 0} };
      uint64_t on = 0, wd = 0;
      const bool got = visible(s, 2, 5, 40, LP, &on, &wd);
      CHECK(got && on == 5 && wd == 2, "tap: 2 ms tap delivered, width intact"); }

    // short_tap_is_delivered_at_the_widest_window: a 10 ms jab used to vanish.
    { const Point s[] = { {0, LP}, {10, 0} };
      uint64_t on = 0, wd = 0;
      const bool got = visible(s, 2, 16, 60, LP, &on, &wd);
      CHECK(got && on == 16 && wd == 10, "tap: 10 ms jab survives a 16 ms window"); }

    // pulse_width_is_preserved: 40 ms hold at a 16 ms window must reach the game
    // as 40 ms, not as 40 - 16.
    { const Point s[] = { {0, LP}, {40, 0} };
      uint64_t on = 0, wd = 0;
      const bool got = visible(s, 2, 16, 90, LP, &on, &wd);
      CHECK(got && on == 16 && wd == 40, "width: 40 ms hold stays 40 ms"); }

    // bounce_on_the_lead_button_still_groups_the_chord
    { const Point s[] = { {0, LP}, {1, 0}, {2, LP}, {3, LP | HP}, {40, 0} };
      uint64_t lp_on = 0, lp_w = 0, hp_on = 0, hp_w = 0;
      const bool a = visible(s, 5, 5, 90, LP, &lp_on, &lp_w);
      const bool b = visible(s, 5, 5, 90, HP, &hp_on, &hp_w);
      CHECK(a && b && lp_on == hp_on && lp_on == 3, "bounce: lead bounce still groups"); }

    // nextDeadlineUs must cover an outstanding delayed release, or an
    // event-driven driver has nothing to wake on.
    { SyncWindow w; uint64_t d = 0;
      w.process(LP, AM, AM, 0, W, true);
      w.process(LP, AM, AM, 5000, W, true);          // commit, delay = 5 ms
      CHECK(!w.windowOpen(), "deadline: window closed");
      w.process(0, AM, AM, 20000, W, true);          // release -> owes 5 ms
      CHECK(w.busy() && w.nextDeadlineUs(W, &d) && d == 25000,
            "deadline: delayed release is wakeable"); }

    printf("\n%s (%d failures)\n", failures ? "TESTS FAILED" : "ALL TESTS PASSED", failures);
    return failures ? 1 : 0;
}
