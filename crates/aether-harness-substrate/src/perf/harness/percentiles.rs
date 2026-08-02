//! Percentile collapse over one cell's raw samples: the [`Stats`] summary and
//! the tail-mass mode indicator that separates a bimodal cell from a merely
//! skewed one.

/// p50 / p90 / p99 / max over a sample set, plus the sample count. All
/// values are nanoseconds.
#[derive(Clone, Copy, Default, Debug)]
pub struct Stats {
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
    pub n: usize,
    /// Fraction of samples above [`TAIL_MASS_MULTIPLE`] times this cell's own
    /// `p50` — the mode indicator (iamacoffeepot/aether#4265).
    ///
    /// The #4177 arc established that the regressed cell is *bistable*: it
    /// boots into a high or a low mode, and in the high mode roughly 17 of 200
    /// frames stall well past the median while the median itself barely moves.
    /// Percentiles cannot separate those, because a cell with 8.5% of samples
    /// in the slow cluster and one with 1% both lift `p99`. A tail *mass* can:
    /// the modes differ in how many samples sit out there, not in where the
    /// centre is.
    ///
    /// Scaled to the cell's own median rather than an absolute nanosecond
    /// threshold, so one number reads across topologies whose medians differ by
    /// an order of magnitude.
    pub tail_mass: f64,
}

/// Multiple of a cell's own `p50` above which a sample counts toward
/// [`Stats::tail_mass`].
///
/// Chosen against the observed separation rather than tuned: #4177's stalls run
/// 2–22 µs against sub-microsecond medians, so they clear this by a wide margin
/// while ordinary jitter does not come close. A value this far out means the
/// indicator does not need per-topology calibration — the point is to separate
/// two populations that are orders apart, not to draw a fine line.
pub const TAIL_MASS_MULTIPLE: u64 = 8;

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn nearest_rank(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Summarise a sample set into [`Stats`] (consumes + sorts the input).
#[must_use]
pub fn summarize(mut samples: Vec<u64>) -> Stats {
    let n = samples.len();
    if n == 0 {
        return Stats::default();
    }
    samples.sort_unstable();
    let p50 = nearest_rank(&samples, 0.50);
    Stats {
        p50,
        p90: nearest_rank(&samples, 0.90),
        p99: nearest_rank(&samples, 0.99),
        max: samples[n - 1],
        n,
        tail_mass: tail_mass(&samples, p50),
    }
}

/// Fraction of `sorted` above `TAIL_MASS_MULTIPLE * p50`.
///
/// Zero for a degenerate cell (a `p50` of zero would put every sample "above"
/// a threshold of zero, which says nothing about modality).
#[allow(
    clippy::cast_precision_loss,
    reason = "a mass fraction is a trend ratio; f64 is exact past any sample count a cell collects"
)]
fn tail_mass(sorted: &[u64], p50: u64) -> f64 {
    if sorted.is_empty() || p50 == 0 {
        return 0.0;
    }
    let threshold = p50.saturating_mul(TAIL_MASS_MULTIPLE);
    // Sorted ascending, so the tail is the suffix past the first sample over
    // the threshold — a partition point, not a scan.
    let below = sorted.partition_point(|sample| *sample <= threshold);
    (sorted.len() - below) as f64 / sorted.len() as f64
}

#[cfg(test)]
mod tests {
    use std::iter::repeat_n;

    use super::*;

    /// Mirrors `report::MODE_PRESENT_TAIL_MASS`, which is private to that
    /// module. Kept beside the test that depends on the two agreeing.
    const MODE_PRESENT_TAIL_MASS_DOC: f64 = 0.01;

    /// Tripwire: the mode indicator separates a bimodal cell from a
    /// merely-skewed one, which is the whole reason it exists
    /// (iamacoffeepot/aether#4265).
    ///
    /// Both cells below share a median and both have a long tail, so `p50` and
    /// `max` cannot tell them apart — the first has one outlier, the second the
    /// ~17-in-200 slow population #4177 measured. The pinned values are
    /// computed from the samples, so they move when `summarize` does.
    #[test]
    fn tail_mass_separates_a_bimodal_cell_from_a_skewed_one() {
        // 199 samples at 100ns, one at 20_000ns: a single outlier.
        let mut skewed = vec![100_u64; 199];
        skewed.push(20_000);
        let skewed = summarize(skewed);

        // 183 at 100ns, 17 at 20_000ns: two populations.
        let mut bimodal = vec![100_u64; 183];
        bimodal.extend(repeat_n(20_000_u64, 17));
        let bimodal = summarize(bimodal);

        assert_eq!(skewed.p50, bimodal.p50, "the two cells are indistinguishable by median");
        assert_eq!(skewed.max, bimodal.max, "and by max");
        assert!(
            (skewed.tail_mass - 0.005).abs() < 1e-9,
            "one sample in 200 past the threshold, got {}",
            skewed.tail_mass
        );
        assert!(
            (bimodal.tail_mass - 0.085).abs() < 1e-9,
            "17 samples in 200 past the threshold, got {}",
            bimodal.tail_mass
        );
        assert!(
            bimodal.tail_mass > MODE_PRESENT_TAIL_MASS_DOC && skewed.tail_mass < MODE_PRESENT_TAIL_MASS_DOC,
            "the comparator's mode threshold has to land between them",
        );
    }

    /// A cell whose samples all sit within the multiple carries no tail, so a
    /// steady workload never reads as having flipped mode.
    #[test]
    fn a_steady_cell_reports_no_tail() {
        let steady = summarize((0..200).map(|i| 100 + i % 7).collect());
        assert!(steady.tail_mass.abs() < 1e-9, "a tight distribution has no tail, got {}", steady.tail_mass);
    }
}
