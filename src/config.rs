//! Thin wrapper over the cross-process shared state (nobd_shared). Config is
//! shared across players; stats are per-player (p = 0 → P1, 1 → P2).

use std::sync::atomic::Ordering;
use nobd_shared::{state, PlayerStats, NUM_PLAYERS};

// ---- config (shared) ----
#[inline] pub fn enabled() -> bool { state().enabled.load(Ordering::Relaxed) != 0 }
#[inline] pub fn window_ms(p: usize) -> u128 { state().window_ms[p.min(NUM_PLAYERS - 1)].load(Ordering::Relaxed) as u128 }
#[inline] pub fn window_ms_u32(p: usize) -> u32 { state().window_ms[p.min(NUM_PLAYERS - 1)].load(Ordering::Relaxed) }
#[inline] pub fn mode() -> u32 { state().mode.load(Ordering::Relaxed) }
#[inline] pub fn directions_windowed() -> bool { state().directions_windowed.load(Ordering::Relaxed) != 0 }
#[inline] pub fn settle_ms() -> u64 { state().settle_ms.load(Ordering::Relaxed) as u64 }
#[inline] pub fn set_poll_hz(hz: u32) { state().poll_hz.store(hz, Ordering::Relaxed); }
#[inline] pub fn heartbeat() { state().dll_heartbeat.fetch_add(1, Ordering::Relaxed); }

// ---- per-player stats ----
#[inline]
fn pstat(p: usize) -> &'static PlayerStats {
    &state().players[p.min(NUM_PLAYERS - 1)]
}

#[inline]
pub fn record_delivery(p: usize, committed_bits: u16) {
    let s = pstat(p);
    match committed_bits.count_ones() {
        0 => {}
        1 => {
            let c = s.singles.fetch_add(1, Ordering::Relaxed) + 1;
            if c <= 200 {
                crate::log::log(&format!("P{} single 0x{committed_bits:04X} (singles={c})", p + 1));
            }
        }
        _ => {
            let c = s.groups.fetch_add(1, Ordering::Relaxed) + 1;
            crate::log::log(&format!("P{} GROUP  0x{committed_bits:04X} (groups={c})", p + 1));
        }
    }
}

#[inline]
pub fn record_latency(p: usize, us: u64) {
    let s = pstat(p);
    s.lat_last_us.store(us, Ordering::Relaxed);
    s.lat_max_us.fetch_max(us, Ordering::Relaxed);
    s.lat_sum_us.fetch_add(us, Ordering::Relaxed);
    let n = s.lat_count.fetch_add(1, Ordering::Relaxed) + 1;
    if n % 10 == 0 && n <= 2000 {
        let (avg, max) = s.latency_ms();
        let label = match mode() { 1 => "BLOCK", 2 => "contin", _ => "defer" };
        crate::log::log(&format!(
            "P{} LATENCY[{label}] this={:.1}ms avg={avg:.1}ms max={max:.1}ms (n={n}, window={}ms)",
            p + 1, us as f64 / 1000.0, window_ms_u32(p),
        ));
    }
}

#[inline]
pub fn record_gap(p: usize, us: u64) {
    let s = pstat(p);
    s.gap_sum_us.fetch_add(us, Ordering::Relaxed);
    s.gap_max_us.fetch_max(us, Ordering::Relaxed);
    s.gap_count.fetch_add(1, Ordering::Relaxed);
}

// Game-perceived input latency: physical press → first game read that sees it.
#[inline]
pub fn record_gp_latency(p: usize, us: u64) {
    let s = pstat(p);
    s.gp_lat_sum_us.fetch_add(us, Ordering::Relaxed);
    s.gp_lat_max_us.fetch_max(us, Ordering::Relaxed);
    s.gp_lat_count.fetch_add(1, Ordering::Relaxed);
}

#[inline] pub fn record_frame_wait(p: usize) { pstat(p).frame_waits.fetch_add(1, Ordering::Relaxed); }
#[inline] pub fn record_save(p: usize) { pstat(p).saves.fetch_add(1, Ordering::Relaxed); }
#[inline] pub fn record_attempt(p: usize) { pstat(p).attempts.fetch_add(1, Ordering::Relaxed); }
#[inline] pub fn record_miss(p: usize) { pstat(p).misses.fetch_add(1, Ordering::Relaxed); }

#[inline] pub fn frame_waits(p: usize) -> u64 { pstat(p).frame_waits.load(Ordering::Relaxed) }
#[inline] pub fn gp_count(p: usize) -> u64 { pstat(p).gp_lat_count.load(Ordering::Relaxed) }

/// Record the latest inter-poll interval (µs) as a smoothed game frame time.
#[inline]
pub fn record_frame_us(p: usize, delta_us: u64) {
    // Ignore implausible gaps (pauses, first sample): 4ms..40ms is the sane band.
    if !(4_000..=40_000).contains(&delta_us) {
        return;
    }
    let s = pstat(p);
    let prev = s.frame_us.load(Ordering::Relaxed);
    let smoothed = if prev == 0 { delta_us } else { (prev * 7 + delta_us) / 8 };
    s.frame_us.store(smoothed, Ordering::Relaxed);
}
