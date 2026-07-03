// Standalone test for SyncWindow.h — mirrors the Rust unit tests in
// src/sync_window.rs so we can prove the C++ port is behavior-identical.
// Build: cl /EHsc /std:c++17 test_syncwindow.cpp
#include "SyncWindow.h"
#include <cstdio>

static const uint16_t LP = 1u << 2; // button 2 = LP
static const uint16_t HP = 1u << 0; // button 0 = HP
static const uint16_t AM = 0x00FF;  // attack mask
static const uint32_t W  = 5000;    // 5 ms window (us)

static int failures = 0;
#define CHECK(expr, label) do { if (!(expr)) { printf("FAIL: %s\n", label); ++failures; } \
                                else { printf("ok:   %s\n", label); } } while (0)

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

    // early_released_press_is_dropped
    { SyncWindow w; uint64_t t = 0;
      CHECK(w.process(LP, AM, AM, t, W, true) == 0, "drop: LP edge held");
      t += 1000;
      CHECK(w.process(HP, AM, AM, t, W, true) == 0, "drop: LP released, HP held");
      t += W + 1000;
      CHECK(w.process(HP, AM, AM, t, W, true) == HP, "drop: LP dropped, HP committed"); }

    // held_button_passes_through_after_commit
    { SyncWindow w; uint64_t t = 0;
      CHECK(w.process(LP, AM, AM, t, W, true) == 0, "held: LP edge held");
      t += W + 1000;
      CHECK(w.process(LP, AM, AM, t, W, true) == LP, "held: committed");
      t += 1000;
      CHECK(w.process(LP, AM, AM, t, W, true) == LP, "held: still held immediate");
      t += 1000;
      CHECK(w.process(0,  AM, AM, t, W, true) == 0,  "held: release immediate"); }

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

    printf("\n%s (%d failures)\n", failures ? "TESTS FAILED" : "ALL TESTS PASSED", failures);
    return failures ? 1 : 0;
}
