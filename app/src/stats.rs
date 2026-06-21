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

    pub fn median(&self) -> f64 {
        if self.gaps.is_empty() {
            return 0.0;
        }
        let mut sorted = self.gaps.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        sorted[sorted.len() / 2]
    }

    pub fn min(&self) -> f64 {
        self.gaps.iter().cloned().fold(f64::INFINITY, f64::min)
    }

    pub fn max(&self) -> f64 {
        self.gaps.iter().cloned().fold(0.0f64, f64::max)
    }

    /// Recommended NOBD slider value: ceil(average) + 1, clamped to 3..=16 ms
    /// (16 ms = one frame, the honest maximum).
    pub fn recommended_nobd(&self) -> u32 {
        if self.gaps.is_empty() {
            return 0;
        }
        let avg = self.average();
        (avg.ceil() as u32 + 1).clamp(3, 16)
    }

    /// Percentage of gaps that are effectively zero (< 0.1ms).
    /// High percentage suggests OBD or a macro button is active.
    pub fn zero_gap_pct(&self) -> f64 {
        if self.gaps.is_empty() {
            return 0.0;
        }
        let zero_count = self.gaps.iter().filter(|&&g| g < 0.1).count();
        zero_count as f64 / self.gaps.len() as f64 * 100.0
    }

    /// Count of pairs where gap >= 1ms (first button was solo for 1+ USB frames).
    pub fn pre_fire_count(&self) -> usize {
        self.gaps.iter().filter(|&&g| g >= 1.0).count()
    }
}
