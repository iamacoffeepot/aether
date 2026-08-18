//! 14-day sparkline over a day series.

use crate::dto::MetricDay;

const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const WINDOW: usize = 14;

/// Last `WINDOW` values of `pick`, padded on the left.
#[must_use]
pub fn last_days(days: &[MetricDay], pick: impl Fn(&MetricDay) -> u64) -> Vec<u64> {
    let values: Vec<u64> = days.iter().map(pick).collect();
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

#[cfg(test)]
mod tests {
    use super::{last_days, sparkline};
    use crate::dto::MetricDay;

    #[test]
    fn last_days_pads_a_short_series_on_the_left() {
        let days = [MetricDay { label: "a".into(), dispatches: 3, ..MetricDay::default() }];
        assert_eq!(last_days(&days, |day| day.dispatches), vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
        assert_eq!(sparkline(&[0, 0, 4]).chars().count(), 3);
        assert!(sparkline(&[0, 0, 0]).chars().all(|ch| ch == '▁'));
    }
}
