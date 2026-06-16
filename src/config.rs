//! Thin wrapper over the cross-process shared state (nobd_shared). The tray and
//! hooks call these; the standalone nobd.exe writes the same memory, so config
//! changes from the app apply live and stats flow back to it.

use std::sync::atomic::Ordering;
use nobd_shared::state;

#[inline] pub fn enabled() -> bool { state().enabled.load(Ordering::Relaxed) != 0 }
#[inline] pub fn set_enabled(v: bool) { state().enabled.store(v as u32, Ordering::Relaxed); }

#[inline] pub fn window_ms() -> u128 { state().window_ms.load(Ordering::Relaxed) as u128 }
#[inline] pub fn window_ms_u32() -> u32 { state().window_ms.load(Ordering::Relaxed) }
#[inline] pub fn set_window(ms: u32) { state().window_ms.store(ms, Ordering::Relaxed); }

#[inline] pub fn block_in_frame() -> bool { state().block_in_frame.load(Ordering::Relaxed) != 0 }
#[inline] pub fn set_block(v: bool) { state().block_in_frame.store(v as u32, Ordering::Relaxed); }

#[inline] pub fn directions_windowed() -> bool { state().directions_windowed.load(Ordering::Relaxed) != 0 }
#[inline] pub fn set_directions(v: bool) { state().directions_windowed.store(v as u32, Ordering::Relaxed); }

#[inline] pub fn settle_ms() -> u64 { state().settle_ms.load(Ordering::Relaxed) as u64 }

#[inline] pub fn groups() -> u64 { state().groups.load(Ordering::Relaxed) }
#[inline] pub fn singles() -> u64 { state().singles.load(Ordering::Relaxed) }

#[inline]
pub fn record_delivery(committed_bits: u16) {
    let s = state();
    match committed_bits.count_ones() {
        0 => {}
        1 => {
            let c = s.singles.fetch_add(1, Ordering::Relaxed) + 1;
            if c <= 200 {
                crate::log::log(&format!("DELIVER single 0x{committed_bits:04X} (singles={c})"));
            }
        }
        _ => {
            let c = s.groups.fetch_add(1, Ordering::Relaxed) + 1;
            crate::log::log(&format!("DELIVER GROUP  0x{committed_bits:04X} (groups={c})"));
        }
    }
}

#[inline]
pub fn record_latency(us: u64) {
    let s = state();
    s.lat_last_us.store(us, Ordering::Relaxed);
    s.lat_max_us.fetch_max(us, Ordering::Relaxed);
    s.lat_sum_us.fetch_add(us, Ordering::Relaxed);
    let n = s.lat_count.fetch_add(1, Ordering::Relaxed) + 1;
    if n % 10 == 0 && n <= 2000 {
        let (avg, max) = s.latency_ms();
        let mode = if block_in_frame() { "BLOCK" } else { "defer" };
        crate::log::log(&format!(
            "LATENCY[{mode}] this={:.1}ms  avg={avg:.1}ms  max={max:.1}ms  (n={n}, window={}ms)",
            us as f64 / 1000.0, window_ms_u32(),
        ));
    }
}

#[inline]
pub fn record_gap(us: u64) {
    let s = state();
    s.gap_sum_us.fetch_add(us, Ordering::Relaxed);
    s.gap_max_us.fetch_max(us, Ordering::Relaxed);
    s.gap_count.fetch_add(1, Ordering::Relaxed);
}

pub fn latency_ms() -> (f64, f64, f64) {
    let s = state();
    let (avg, max) = s.latency_ms();
    (s.lat_last_us.load(Ordering::Relaxed) as f64 / 1000.0, avg, max)
}

pub fn finger_gap_ms() -> (f64, f64) { state().finger_gap_ms() }
pub fn recommended_window_ms() -> u64 { state().recommended_window_ms() as u64 }
pub fn reset_stats() { state().reset_stats(); }

#[inline] pub fn heartbeat() { state().dll_heartbeat.fetch_add(1, Ordering::Relaxed); }

#[inline] pub fn record_save() { state().saves.fetch_add(1, Ordering::Relaxed); }
#[inline] pub fn saves() -> u64 { state().saves.load(Ordering::Relaxed) }

/// Record the latest inter-poll interval (µs) as a smoothed game frame time.
#[inline]
pub fn record_frame_us(delta_us: u64) {
    // Ignore implausible gaps (pauses, first sample): 4ms..40ms is the sane band.
    if !(4_000..=40_000).contains(&delta_us) {
        return;
    }
    let s = state();
    let prev = s.frame_us.load(Ordering::Relaxed);
    let smoothed = if prev == 0 { delta_us } else { (prev * 7 + delta_us) / 8 };
    s.frame_us.store(smoothed, Ordering::Relaxed);
}
