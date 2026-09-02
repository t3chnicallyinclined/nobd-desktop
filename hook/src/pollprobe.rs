//! Poll-cadence probe: does the game read input once per frame, and how tightly?
//!
//! Everything downstream of this — committing at the last moment before the game's next
//! read instead of on a wall-clock timeout — rests on one assumption: that
//! `XInputGetState` is called on a stable, once-per-frame cadence. If the game polls
//! several times a frame, or from its own input thread, "next poll" is not "next frame"
//! and the idea does not work. So measure before building.
//!
//! COST ON THE GAME THREAD, deliberately near zero:
//!   * one `fetch_add` and one relaxed `store` per call, into a fixed static array
//!   * no allocation, no lock, no syscall, no formatting, no file I/O
//!   * it stops sampling entirely once the buffer is full — after that it is a single
//!     relaxed load and a compare
//! All arithmetic, percentiles and logging happen on the background thread that already
//! runs. Nothing here can block the game's input read, which is the one thing that could
//! actually disturb rollback netcode.
//!
//! Samples are RAW. The existing `record_frame_us` filters to a 4..40 ms band, which
//! would silently discard exactly the evidence that matters — a game polling at 1 kHz
//! from a separate thread would look like "no samples" rather than like a problem.

use nobd_shared::NUM_PLAYERS;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// ~17 seconds at 60 Hz: long enough for a percentile to mean something, small enough
/// that the array is 4 KB and fills during a single match.
const N: usize = 1024;

/// PER PLAYER. The first cut of this sampled only slot 0 and would have measured
/// nothing on a real setup: a stick can enumerate as P2 with slot 0 empty
/// (XInputGetState idx=0 returning 1167, ERROR_DEVICE_NOT_CONNECTED), which is exactly
/// what the maintainer's rig does. Deltas are per-slot anyway -- LAST_POLL_US is
/// indexed by player -- so interleaving slots into one buffer would also have produced
/// a meaningless cadence.
static BUF: [[AtomicU32; N]; NUM_PLAYERS] = [const { [const { AtomicU32::new(0) }; N] }; NUM_PLAYERS];
static IDX: [AtomicUsize; NUM_PLAYERS] = [const { AtomicUsize::new(0) }; NUM_PLAYERS];
static REPORTED: [AtomicBool; NUM_PLAYERS] = [const { AtomicBool::new(false) }; NUM_PLAYERS];

/// Called from the XInputGetState detour with the raw inter-poll delta in µs.
/// Hot path: keep this to the two atomics it already is.
#[inline(always)]
pub fn sample(p: usize, delta_us: u64) {
    if p >= NUM_PLAYERS || IDX[p].load(Ordering::Relaxed) >= N {
        return; // full or out of range: one load and a compare, forever
    }
    let i = IDX[p].fetch_add(1, Ordering::Relaxed);
    if i < N {
        // Saturate rather than wrap: a paused game produces a huge delta and a wrapped
        // u32 would read as a plausibly small one, which is worse than an obvious outlier.
        BUF[p][i].store(delta_us.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    }
}

/// The first player slot with a full buffer that has not been reported yet.
#[inline]
pub fn ready() -> Option<usize> {
    (0..NUM_PLAYERS).find(|&p| {
        IDX[p].load(Ordering::Relaxed) >= N && !REPORTED[p].load(Ordering::Relaxed)
    })
}

/// Analyse and format. Background thread only — allocates and sorts.
/// Returns None if it has already been reported once.
pub fn report(p: usize) -> Option<String> {
    if p >= NUM_PLAYERS || REPORTED[p].swap(true, Ordering::Relaxed) {
        return None;
    }
    let v: Vec<u32> = BUF[p].iter().map(|a| a.load(Ordering::Relaxed)).collect();
    Some(format!("P{} {}", p + 1, analyse(&v)))
}

/// The whole analysis, as a pure function so it can be tested without the statics.
fn analyse(samples: &[u32]) -> String {
    let mut v = samples.to_vec();
    v.sort_unstable();
    let n = v.len();
    let pct = |p: f64| v[(((n - 1) as f64) * p) as usize];
    let mean = v.iter().map(|&x| x as u64).sum::<u64>() / n as u64;
    let (p01, p50, p99) = (pct(0.01), pct(0.50), pct(0.99));
    let (p10, p90) = (pct(0.10), pct(0.90));

    // Tightness is measured p10..p90, NOT p99-p01.
    //
    // The first cut used p99-p01 and mis-called a perfectly usable cadence as LOOSE. The
    // real MvC2 capture came back p01=9.69 p50=16.69 p99=19.98, so p99-p01 was 10.29ms and
    // failed a 4ms threshold -- while 93% of samples sat inside 15..17ms and the spread
    // that matters was +/-1ms. The tails are frame hitches, a few per thousand, and an
    // outer-percentile spread over-weights exactly those. What the frame-aware window
    // needs to know is where the BULK lands, because that is what the next-poll
    // prediction rides on.
    let spread = p90.saturating_sub(p10);
    // Still reported, because it names the risk a safety margin has to absorb: a poll
    // arriving EARLIER than predicted is the case that costs a frame.
    let outer = p99.saturating_sub(p01);
    let hz = if p50 > 0 { 1_000_000.0 / p50 as f64 } else { 0.0 };

    // 1 ms buckets, 0..24 ms, so a bimodal cadence (two polls per frame) is visible as
    // two humps rather than hidden inside a mean.
    let mut hist = [0u32; 25];
    for &d in &v {
        hist[((d / 1000) as usize).min(24)] += 1;
    }
    let peak = hist.iter().copied().max().unwrap_or(1).max(1);
    let mut bars = String::new();
    for (ms, &c) in hist.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let w = ((c as u64 * 40) / peak as u64) as usize;
        bars.push_str(&format!(
            "\n    {:>2}ms |{:<40}| {:>4}",
            ms,
            "#".repeat(w.max(1)),
            c
        ));
    }

    let verdict = if p50 >= 15_000 && p50 <= 18_000 && spread <= 3_000 {
        "ONE POLL PER FRAME at ~60Hz, tight. Frame-aware committing is viable."
    } else if p50 >= 15_000 && p50 <= 18_000 {
        "~60Hz median but LOOSE even in the bulk. The next-poll prediction needs a \
         generous safety margin."
    } else if p50 < 8_000 {
        "POLLS FASTER THAN ONCE PER FRAME. 'next poll' is not 'next frame' -- the \
         frame-aware design does not apply as written."
    } else {
        "UNEXPECTED cadence. Do not build on this without looking at the histogram."
    };

    format!(
        "pollprobe: n={n} p01={:.2} p10={:.2} p50={:.2} p90={:.2} p99={:.2} mean={:.2} (ms) \
         spread(p10..p90)={:.2}ms outer(p01..p99)={:.2}ms implied={:.1}Hz\n  {verdict}{bars}",
        p01 as f64 / 1000.0,
        p10 as f64 / 1000.0,
        p50 as f64 / 1000.0,
        p90 as f64 / 1000.0,
        p99 as f64 / 1000.0,
        mean as f64 / 1000.0,
        spread as f64 / 1000.0,
        outer as f64 / 1000.0,
        hz,
    )
}

#[cfg(test)]
mod tests {
    use super::analyse;

    /// A cadence with a given median and +/- jitter, deterministic.
    fn cadence(median_us: u32, jitter_us: u32, n: usize) -> Vec<u32> {
        (0..n)
            .map(|i| {
                let w = if jitter_us == 0 { 0 } else { (i as u32 * 2654435761) % (jitter_us + 1) };
                median_us + w - jitter_us / 2
            })
            .collect()
    }

    #[test]
    fn tight_60hz_is_viable() {
        let r = analyse(&cadence(16_667, 1_000, 1024));
        assert!(r.contains("ONE POLL PER FRAME"), "{r}");
    }

    #[test]
    fn loose_60hz_warns_about_margin() {
        let r = analyse(&cadence(16_667, 9_000, 1024));
        assert!(r.contains("LOOSE"), "{r}");
    }

    /// The case that kills the whole design: an input thread polling far faster than
    /// the frame rate, so "next poll" is not "next frame".
    #[test]
    fn kilohertz_polling_is_called_out() {
        let r = analyse(&cadence(1_000, 200, 1024));
        assert!(r.contains("FASTER THAN ONCE PER FRAME"), "{r}");
    }

    /// The REAL capture from MvC2 (2026-09-02): p50 16.69ms, 93% inside 15..17ms, with a
    /// ~3% tail of frame hitches at 3-13ms and 24ms. p99-p01 was 10.29ms and wrongly read
    /// as LOOSE. It must read as viable.
    #[test]
    fn the_real_mvc2_capture_reads_as_viable() {
        let mut v = Vec::new();
        for (ms, n) in [(3,1),(4,1),(5,2),(6,4),(8,1),(9,2),(10,2),(11,1),(12,3),(13,1),
                        (14,14),(15,147),(16,523),(17,285),(18,22),(19,4),(20,1),(22,1),(24,9)] {
            for _ in 0..n { v.push(ms * 1000 + 500); }
        }
        assert_eq!(v.len(), 1024);
        let r = analyse(&v);
        assert!(r.contains("ONE POLL PER FRAME"), "{r}");
    }

    /// Two polls per frame must not hide inside a mean -- it should NOT read as 60Hz.
    #[test]
    fn twice_per_frame_is_not_mistaken_for_60hz() {
        let r = analyse(&cadence(8_333, 500, 1024));
        assert!(!r.contains("ONE POLL PER FRAME"), "{r}");
    }
}
