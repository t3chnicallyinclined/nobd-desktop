/// One video frame at 60 fps — the boundary that matters for MvC2. A two-button
/// press whose gap exceeds this can't land on the same frame even with a maxed
/// (16 ms) NOBD window.
pub const FRAME_MS: f64 = 1000.0 / 60.0; // 16.667 ms

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
    /// Almost everything is same-frame — an always-on macro/turbo/OBD.
    AlwaysOn,
}

pub struct GapStats {
    gaps: Vec<f64>,
}

impl GapStats {
    pub fn new() -> Self {
        Self { gaps: Vec::new() }
    }

    pub fn record(&mut self, gap_ms: f64) {
        self.gaps.push(gap_ms);
    }

    pub fn count(&self) -> usize {
        self.gaps.len()
    }

    pub fn average(&self) -> f64 {
        if self.gaps.is_empty() {
            return 0.0;
        }
        self.gaps.iter().sum::<f64>() / self.gaps.len() as f64
    }

    fn sorted(&self) -> Vec<f64> {
        let mut s = self.gaps.clone();
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
        self.gaps.iter().cloned().fold(f64::INFINITY, f64::min)
    }

    pub fn max(&self) -> f64 {
        self.gaps.iter().cloned().fold(0.0f64, f64::max)
    }

    /// Recommended NOBD window: covers your worst realistic attempt (p95 of your
    /// gaps) + 1 ms headroom, clamped to 3..=16 ms (16 ms = one frame, the honest
    /// maximum). p95 (not the average) so the window catches ~95% of your dashes.
    pub fn recommended_nobd(&self) -> u32 {
        if self.gaps.is_empty() {
            return 0;
        }
        (self.percentile(0.95).ceil() as u32 + 1).clamp(3, 16)
    }

    /// Expected drop rate WITHOUT NOBD (0..1). For each pair, the chance a frame
    /// boundary falls between the two presses is `gap / FRAME_MS` (clamped to 1),
    /// i.e. how often that input would split. Averaged across pairs = the share
    /// of your dashes that would drop on a random frame alignment.
    pub fn split_probability(&self) -> f64 {
        if self.gaps.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.gaps.iter().map(|&g| (g / FRAME_MS).min(1.0)).sum();
        sum / self.gaps.len() as f64
    }

    /// % of pairs that landed within one USB report (gap < 0.5 ms ≈ 0). A human
    /// can't press two buttons inside one ~1 ms USB frame, so a high value means
    /// presses are being GROUPED upstream — a NOBD sync window, an OBD/turbo
    /// macro, or SOCD cleaning — not your natural finger timing.
    pub fn same_frame_pct(&self) -> f64 {
        if self.gaps.is_empty() {
            return 0.0;
        }
        let n = self.gaps.iter().filter(|&&g| g < 0.5).count();
        n as f64 / self.gaps.len() as f64 * 100.0
    }

    /// Inferred grouping mode from the same-frame rate. `None` until there are
    /// enough pairs (8) to judge.
    pub fn grouping(&self) -> Option<Grouping> {
        if self.gaps.len() < 8 {
            return None;
        }
        let p = self.same_frame_pct();
        Some(if p >= 80.0 {
            Grouping::AlwaysOn
        } else if p >= 30.0 {
            Grouping::Window
        } else if p >= 10.0 {
            Grouping::Hint
        } else {
            Grouping::Natural
        })
    }
}
