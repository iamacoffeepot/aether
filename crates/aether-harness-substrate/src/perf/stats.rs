//! Robust K-trial statistics (ADR-0085 §2).
//!
//! Run-to-run perf noise is heavy-tailed — one trial can hit a multi-millisecond
//! outlier — so a replicated measurement is centred on the **median of the
//! per-trial values** and banded with the **IQR**, never a mean and never a
//! standard deviation. These are the primitives the perf lanes reduce their
//! trials with: the paired comparison ([`super::report::compare`]) over
//! base-vs-candidate deltas, and the registry replication
//! ([`super::registry::band`]) over one configuration's repeated sweeps.

use serde::{Deserialize, Serialize};

pub(crate) fn sorted(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(f64::total_cmp);
    v
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn quantile_sorted(s: &[f64], q: f64) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let idx = ((s.len() - 1) as f64 * q).round() as usize;
    s[idx.min(s.len() - 1)]
}

pub(crate) fn median_sorted(s: &[f64]) -> f64 {
    quantile_sorted(s, 0.5)
}

pub(crate) fn iqr_sorted(s: &[f64]) -> f64 {
    quantile_sorted(s, 0.75) - quantile_sorted(s, 0.25)
}

/// One measurement replicated across K trials, reduced to ADR-0085's robust
/// centre and band.
///
/// The extremes ride along with the quartiles because they answer a question
/// the band alone cannot: a wide IQR means the measurement is genuinely
/// dispersed, whereas a tight IQR next to a far-off `max` means one trial went
/// wrong. Reading a band without them invites both mistakes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandStats {
    /// Trials that contributed a value. Below the requested K when a trial
    /// failed, or when the measurement is one a trial does not always produce.
    pub trials: u64,
    /// Median of the per-trial values — the centre (§2, never the mean).
    pub median: f64,
    pub p25: f64,
    pub p75: f64,
    /// `p75 - p25` — the band (§2).
    pub iqr: f64,
    pub min: f64,
    pub max: f64,
}

impl BandStats {
    /// Reduce K per-trial values. An empty set yields a zero band at zero
    /// trials rather than `None`, so a cell that no trial produced still has a
    /// place in the report and reads as "no trials" instead of "measured zero".
    #[must_use]
    pub fn of(values: &[f64]) -> Self {
        let s = sorted(values.to_vec());
        Self {
            trials: s.len() as u64,
            median: median_sorted(&s),
            p25: quantile_sorted(&s, 0.25),
            p75: quantile_sorted(&s, 0.75),
            iqr: iqr_sorted(&s),
            min: s.first().copied().unwrap_or(0.0),
            max: s.last().copied().unwrap_or(0.0),
        }
    }

    /// Where the whole band sits relative to `reference`.
    ///
    /// This is a statement about the interval, not a verdict: a band that
    /// clears the reference on every trial is a different claim from one whose
    /// quartiles straddle it, and saying which is the honest end of a
    /// replication. Classification of a *change* stays with the paired
    /// comparison, whose band is on the delta rather than on either absolute
    /// number (ADR-0085 §3).
    #[must_use]
    pub fn position_against(&self, reference: f64) -> BandPosition {
        if self.trials == 0 {
            return BandPosition::Straddles;
        }
        if self.p75 < reference {
            BandPosition::Below
        } else if self.p25 > reference {
            BandPosition::Above
        } else {
            BandPosition::Straddles
        }
    }
}

/// Where a [`BandStats`] interquartile band sits relative to a reference value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandPosition {
    /// The whole band is under the reference.
    Below,
    /// The band contains the reference — the replication does not separate them.
    Straddles,
    /// The whole band is over the reference.
    Above,
}

impl BandPosition {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Below => "below",
            Self::Straddles => "straddles",
            Self::Above => "above",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire (ADR-0085 §2): the centre is the median and the band the IQR,
    /// so a single wild trial moves neither. A mean-based centre would move to
    /// ~2100 on this input, which is exactly the substitution the ADR rejects —
    /// and which nothing else in the pipeline would catch, because a mean is a
    /// perfectly plausible-looking number.
    #[test]
    fn one_outlier_moves_neither_the_centre_nor_the_band() {
        let clean = BandStats::of(&[100.0, 101.0, 102.0, 103.0, 104.0]);
        let spiked = BandStats::of(&[100.0, 101.0, 102.0, 103.0, 10_000.0]);
        assert_eq!(spiked.median, clean.median, "the median must ignore the outlier");
        assert_eq!(spiked.p25, clean.p25);
        assert_eq!(spiked.max, 10_000.0, "the outlier must still be visible in `max`");
    }

    /// Tripwire: a band positioned against a reference reports `Straddles`
    /// whenever the quartiles contain it. Collapsing the three-way answer into
    /// a two-way "is the median above or below" would turn every noisy cell
    /// into a confident claim.
    #[test]
    fn a_band_containing_the_reference_straddles_it() {
        let spread = BandStats::of(&[0.8, 0.95, 1.0, 1.05, 1.2]);
        assert_eq!(spread.position_against(1.0), BandPosition::Straddles);

        let under = BandStats::of(&[0.18, 0.19, 0.20, 0.21, 0.22]);
        assert_eq!(under.position_against(1.0), BandPosition::Below);

        let over = BandStats::of(&[2.9, 3.0, 3.1, 3.2, 3.3]);
        assert_eq!(over.position_against(1.0), BandPosition::Above);
    }

    /// A measurement no trial produced must not read as a measured zero that
    /// happens to sit below every reference.
    #[test]
    fn an_empty_band_claims_nothing() {
        let empty = BandStats::of(&[]);
        assert_eq!(empty.trials, 0);
        assert_eq!(empty.position_against(1.0), BandPosition::Straddles);
    }
}
