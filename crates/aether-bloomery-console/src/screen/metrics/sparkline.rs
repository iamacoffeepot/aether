//! 14-day sparkline over a day series.

use crate::dto::MetricDay;

const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const WINDOW: usize = 14;
const NO_SERIES: &str = "—";

/// Dated civil days only — the undated reconstructed bucket is not a day.
#[must_use]
pub fn dated(days: &[MetricDay]) -> Vec<&MetricDay> {
    days.iter().filter(|day| !day.reconstructed).collect()
}

/// Last `WINDOW` values of `pick`, padded on the left.
#[must_use]
pub fn last_days(days: &[&MetricDay], pick: impl Fn(&MetricDay) -> u64) -> Vec<u64> {
    let values: Vec<u64> = days.iter().copied().map(pick).collect();
    let skip = values.len().saturating_sub(WINDOW);
    let mut out = vec![0; WINDOW.saturating_sub(values.len())];
    out.extend(values.into_iter().skip(skip));
    out
}

/// One-character-per-sample sparkline. All-zero is a flat floor, not blank.
#[must_use]
pub fn sparkline(values: &[u64]) -> String {
    let max = values.iter().copied().max().unwrap_or(0);
    values
        .iter()
        .map(|&value| {
            if max == 0 {
                BARS[0]
            } else {
                let index =
                    usize::try_from((u128::from(value) * u128::from((BARS.len() - 1) as u64)) / u128::from(max))
                        .unwrap_or(BARS.len() - 1);
                BARS[index.min(BARS.len() - 1)]
            }
        })
        .collect()
}

/// Sparkline over dated days.
///
/// No dated row is missing data and gets the `—` placeholder; an all-zero
/// dated series is a real idle fortnight and keeps its floor.
#[must_use]
pub fn day_spark(days: &[MetricDay], pick: impl Fn(&MetricDay) -> u64) -> String {
    let dated = dated(days);
    if dated.is_empty() {
        NO_SERIES.to_owned()
    } else {
        sparkline(&last_days(&dated, pick))
    }
}

#[cfg(test)]
mod tests {
    use super::{NO_SERIES, dated, day_spark, last_days, sparkline};
    use crate::dto::MetricDay;

    #[test]
    fn last_days_pads_a_short_series_on_the_left() {
        let days = [MetricDay { label: "a".into(), dispatches: 3, ..MetricDay::default() }];
        assert_eq!(last_days(&[&days[0]], |day| day.dispatches), vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
        assert_eq!(sparkline(&[0, 0, 4]).chars().count(), 3);
        assert!(sparkline(&[0, 0, 0]).chars().all(|ch| ch == '▁'));
    }

    #[test]
    fn the_undated_bucket_is_not_a_day_in_the_window() {
        // The plausible bug: the undated bucket consumes one of the fourteen
        // slots, so the oldest real day silently falls off.
        let days = [
            MetricDay { label: "bloomery/daily/2026-08-19".into(), spend_micro_usd: 7, ..MetricDay::default() },
            MetricDay {
                label: "reconstructed".into(),
                reconstructed: true,
                spend_micro_usd: 99,
                ..MetricDay::default()
            },
        ];
        assert_eq!(last_days(&dated(&days), |day| day.spend_micro_usd), vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7]);
        assert_eq!(day_spark(&days[1..], |day| day.spend_micro_usd), NO_SERIES);
    }
}
