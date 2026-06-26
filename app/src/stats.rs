use gilrs::Button;
use std::collections::{HashMap, VecDeque};

/// One video frame at 60 fps — the boundary that matters for MvC2.
pub const FRAME_MS: f64 = 1000.0 / 60.0; // 16.667 ms

/// Default number of recent chords the verdict is judged over.
pub const DEFAULT_WINDOW: usize = 12;
/// Tunable bounds for the decision window.
pub const MIN_WINDOW: usize = 6;
pub const MAX_WINDOW: usize = 40;
/// Chords needed in the window before we'll commit to a verdict.
const MIN_SAMPLES: usize = 8;

/// Inferred read on whether presses are being grouped upstream (a NOBD sync
/// window, an OBD/turbo macro, or SOCD cleaning) vs. natural finger timing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Grouping {
    /// Gaps reflect your fingers — nothing is grouping presses.
    Natural,
    /// A few same-frame pairs — light grouping or very fast hands.
    Hint,
    /// A sync window looks active: grouped within a window, real gaps beyond it.
    Window,
    /// Almost everything is same-frame — looks like an always-on macro/turbo/OBD.
    AlwaysOn,
}

/// One recent input kept in the sliding window: a chord (≥2 buttons, with its
/// spread gap and canonical button-set) or a solo (single attack button alone).
struct Sample {
    gap_ms: f64,
    is_solo: bool,
    sig: String,
    /// First-press time (ms since session epoch) for the game-frame simulation.
    t0_ms: f64,
}

/// Would these two presses straddle a 60 fps game-poll boundary? Uses a
/// FREE-RUNNING clock (`floor(t / period)`), never reset on input, so the press
/// phase is random relative to the game's poll — exactly like a real game.
/// `true` = the game reads the two buttons on different frames (a dropped chord).
pub fn game_frame_split(t0_ms: f64, gap_ms: f64) -> bool {
    let p = FRAME_MS;
    (t0_ms / p).floor() != ((t0_ms + gap_ms) / p).floor()
}

/// A rolling window of the most recent inputs. Everything — the verdict, the
/// same-frame rate, the histogram — is computed over only the last `window`
/// chords, so the decision keeps re-deciding live and flips when you toggle the
/// firmware mid-session (no Reset needed).
pub struct GapStats {
    samples: VecDeque<Sample>,
    window: usize,
    /// Measured USB report interval (ms). One "frame" for bucketing. Default 1.0.
    frame_ms: f64,
}

impl GapStats {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            window: DEFAULT_WINDOW,
            frame_ms: 1.0,
        }
    }

    /// Resize the decision window (clamped), dropping the oldest samples if it
    /// shrank below the current fill.
    pub fn set_window(&mut self, n: usize) {
        self.window = n.clamp(MIN_WINDOW, MAX_WINDOW);
        self.trim();
    }

    pub fn window(&self) -> usize {
        self.window
    }

    fn trim(&mut self) {
        while self.samples.len() > self.window {
            self.samples.pop_front();
        }
    }

    /// Update the USB frame size from the measured device report interval. We
    /// clamp to a sane 0.2–8 ms so a bad early reading can't wreck bucketing.
    pub fn set_frame_ms(&mut self, ms: f64) {
        if ms.is_finite() && ms > 0.0 {
            self.frame_ms = ms.clamp(0.2, 8.0);
        }
    }

    pub fn frame_ms(&self) -> f64 {
        self.frame_ms
    }

    /// Record a detected chord: its spread gap, the buttons, and the first-press
    /// time (ms since session epoch) for the game-frame simulation.
    pub fn record_chord(&mut self, gap_ms: f64, buttons: &[Button], t0_ms: f64) {
        let mut names: Vec<String> = buttons.iter().map(|b| format!("{:?}", b)).collect();
        names.sort();
        names.dedup();
        self.samples.push_back(Sample {
            gap_ms,
            is_solo: false,
            sig: names.join("+"),
            t0_ms,
        });
        self.trim();
    }

    /// Record a single attack button that registered on its own (a stray/solo).
    pub fn record_solo(&mut self) {
        self.samples.push_back(Sample {
            gap_ms: 0.0,
            is_solo: true,
            sig: String::new(),
            t0_ms: 0.0,
        });
        self.trim();
    }

    /// Chord gaps in the current window (excludes solos).
    fn gaps(&self) -> Vec<f64> {
        self.samples
            .iter()
            .filter(|s| !s.is_solo)
            .map(|s| s.gap_ms)
            .collect()
    }

    /// Chords in the window.
    pub fn count(&self) -> usize {
        self.samples.iter().filter(|s| !s.is_solo).count()
    }

    /// Solo presses in the window.
    pub fn solo_count(&self) -> usize {
        self.samples.iter().filter(|s| s.is_solo).count()
    }

    /// Distinct chord compositions in the window.
    pub fn distinct_chords(&self) -> usize {
        let mut set = std::collections::HashSet::new();
        for s in self.samples.iter().filter(|s| !s.is_solo) {
            set.insert(s.sig.as_str());
        }
        set.len()
    }

    pub fn average(&self) -> f64 {
        let g = self.gaps();
        if g.is_empty() {
            return 0.0;
        }
        g.iter().sum::<f64>() / g.len() as f64
    }

    fn sorted(&self) -> Vec<f64> {
        let mut s = self.gaps();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s
    }

    /// p-th percentile of the gaps (p in 0.0..=1.0), nearest-rank.
    pub fn percentile(&self, p: f64) -> f64 {
        let s = self.sorted();
        if s.is_empty() {
            return 0.0;
        }
        let idx = (((s.len() - 1) as f64) * p).round() as usize;
        s[idx.min(s.len() - 1)]
    }

    pub fn min(&self) -> f64 {
        self.gaps().iter().cloned().fold(f64::INFINITY, f64::min)
    }

    pub fn max(&self) -> f64 {
        self.gaps().iter().cloned().fold(0.0f64, f64::max)
    }

    /// Recommended NOBD window: covers your worst realistic attempt (p95) + 1 ms
    /// headroom, clamped to 3..=16 ms. p95 (not the average) so it catches ~95%
    /// of your dashes.
    pub fn recommended_nobd(&self) -> u32 {
        if self.count() == 0 {
            return 0;
        }
        (self.percentile(0.95).ceil() as u32 + 1).clamp(3, 16)
    }

    /// Expected drop rate WITHOUT NOBD (0..1): for each pair the chance a frame
    /// boundary splits it is `gap / FRAME_MS` (clamped to 1); averaged over pairs.
    /// This is the analytical expectation under uniform-random poll phase.
    pub fn split_probability(&self) -> f64 {
        let g = self.gaps();
        if g.is_empty() {
            return 0.0;
        }
        let sum: f64 = g.iter().map(|&x| (x / FRAME_MS).min(1.0)).sum();
        sum / g.len() as f64
    }

    /// How many chords in the window WOULD HAVE split across a 60 fps game-poll
    /// boundary, simulated against the free-running clock with each chord's real
    /// phase. This is the realized (not expected) count — what a 60 fps game
    /// would actually have dropped.
    pub fn simulated_split_count(&self) -> usize {
        self.samples
            .iter()
            .filter(|s| !s.is_solo && game_frame_split(s.t0_ms, s.gap_ms))
            .count()
    }

    /// Realized 60 fps split rate (0..1) over the window.
    pub fn simulated_split_rate(&self) -> f64 {
        let n = self.count();
        if n == 0 {
            0.0
        } else {
            self.simulated_split_count() as f64 / n as f64
        }
    }

    /// Threshold (ms) below which a gap counts as "same USB frame".
    fn same_frame_thresh(&self) -> f64 {
        0.5 * self.frame_ms
    }

    /// % of pairs that landed within one USB report (≈ 0 gap). A human can't
    /// press two buttons inside one ~1 ms USB frame, so a high value means
    /// presses are being GROUPED upstream — a NOBD sync window, an OBD/turbo
    /// macro, or SOCD cleaning — not your natural finger timing.
    pub fn same_frame_pct(&self) -> f64 {
        let g = self.gaps();
        if g.is_empty() {
            return 0.0;
        }
        let t = self.same_frame_thresh();
        let n = g.iter().filter(|&&x| x < t).count();
        n as f64 / g.len() as f64 * 100.0
    }

    /// Histogram of gaps bucketed by USB frame: `(frame_index, count)` for every
    /// index from 0 up to the largest seen. Frame 0 = same-frame (grouped).
    pub fn frame_histogram(&self) -> Vec<(u32, usize)> {
        let g = self.gaps();
        if g.is_empty() {
            return Vec::new();
        }
        let mut map: HashMap<u32, usize> = HashMap::new();
        let mut max_idx = 0u32;
        for &x in &g {
            let idx = (x / self.frame_ms).round() as u32;
            max_idx = max_idx.max(idx);
            *map.entry(idx).or_insert(0) += 1;
        }
        (0..=max_idx).map(|i| (i, *map.get(&i).unwrap_or(&0))).collect()
    }

    /// Smallest populated frame index ABOVE 0 — i.e. the shortest real gap that
    /// "escaped" grouping. For a sync window this sits just past the window edge.
    fn first_real_frame(&self) -> Option<u32> {
        self.frame_histogram()
            .into_iter()
            .find(|&(i, c)| i >= 1 && c > 0)
            .map(|(i, _)| i)
    }

    /// Empty frame slots between same-frame (0) and the first surviving gap.
    /// 0 = natural (gaps fill frame 1, 2, 3…); a wide empty band = a sync window
    /// collapsed everything inside it onto frame 0 (the "missing middle").
    pub fn dead_zone_frames(&self) -> u32 {
        match self.first_real_frame() {
            Some(k) => k.saturating_sub(1),
            None => 0, // everything same-frame: no surviving gap to bound a zone
        }
    }

    /// Estimated sync-window width (ms): the shortest gap that still split across
    /// frames sits just beyond the window. `None` until a real gap survives.
    pub fn estimated_window_ms(&self) -> Option<f64> {
        self.first_real_frame().map(|k| k as f64 * self.frame_ms)
    }

    /// Chords required in the window before a verdict locks in.
    fn required(&self) -> usize {
        self.window.min(MIN_SAMPLES)
    }

    /// Inferred grouping mode from the three signatures, over the sliding window:
    ///  A) same-frame rate, B) the dead zone, C) singles + fixed combo.
    /// `None` until there are enough recent chords to judge.
    pub fn grouping(&self) -> Option<Grouping> {
        if self.count() < self.required() {
            return None;
        }
        let sf = self.same_frame_pct();

        // Signature A: barely any same-frame pairs → gaps track your fingers.
        if sf < 15.0 {
            return Some(Grouping::Natural);
        }

        let dz = self.dead_zone_frames();
        let single_combo = self.distinct_chords() <= 1;

        // Signature C: nearly everything same-frame, always the SAME button set,
        // and not one single button ever registered alone → an always-on macro.
        if sf >= 85.0 && self.solo_count() == 0 && single_combo {
            return Some(Grouping::AlwaysOn);
        }

        // Signature B: a dead zone (grouped within a window, real gaps beyond it),
        // OR singles still pass through, OR a solid same-frame majority → a window.
        if dz >= 1 || self.solo_count() > 0 || sf >= 30.0 {
            return Some(Grouping::Window);
        }

        // Some same-frame pairs but no clear signature — fast hands or light noise.
        Some(Grouping::Hint)
    }

    /// True when a grouping/buffering firmware looks active (Window or AlwaysOn).
    pub fn grouping_active(&self) -> bool {
        matches!(
            self.grouping(),
            Some(Grouping::Window) | Some(Grouping::AlwaysOn)
        )
    }

    /// How many more chords until a verdict locks in (0 = ready).
    pub fn samples_until_verdict(&self) -> usize {
        self.required().saturating_sub(self.count())
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }
}
