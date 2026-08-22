//! Numbers the five-region dashboard paints from the store.

use crate::dto::{BloomStatus, DigestHex, MetricDay, SpendQuiesce, ViewDocument};
use crate::screen::partition::live_blooms;
use crate::store::Store;

use super::bucket::format_duration;
use super::cost::format_micro_usd;
use super::sparkline::{dated, day_spark};

/// Header / today / footer strips the shell chrome paints.
#[derive(Clone, Debug, Default)]
pub struct Dashboard {
    pub spend_spark: String,
    pub landed_spark: String,
    pub wedge_spark: String,
    pub today: String,
    pub footer: String,
}

/// Compose the dashboard strips. Missing resources stay empty rather than
/// inventing a series.
#[must_use]
pub fn compose(store: &Store) -> Dashboard {
    let days = store.days().value.as_ref().map_or(&[][..], Vec::as_slice);
    let summary = store.summary().value.as_ref();
    let view = store.view().value.as_ref();

    let today = today_line(store, days);
    let footer = footer_line(store, days, summary.map(|summary| summary.active_blooms), view);

    Dashboard {
        spend_spark: day_spark(days, |day| day.spend_micro_usd),
        landed_spark: day_spark(days, |day| day.landed),
        wedge_spark: day_spark(days, |day| day.wedges),
        today,
        footer,
    }
}

fn today_line(store: &Store, days: &[MetricDay]) -> String {
    let spend = store.spend().value.as_ref();
    if spend.is_none() && days.is_empty() {
        return String::new();
    }
    let ceiling = store.view().value.as_ref().and_then(|view| match &view.spend_quiesce {
        Some(SpendQuiesce::Window { ceiling_micro_usd, .. } | SpendQuiesce::Bloom { ceiling_micro_usd, .. }) => {
            Some(*ceiling_micro_usd)
        }
        _ => None,
    });
    let total = spend.map_or(0, |window| window.total_micro_usd);
    let unpriced = spend.map_or(0, |window| window.unpriced_records);
    let unaccounted = spend.map_or(0, |window| window.unaccounted_dispatches);
    let gauge = spend_gauge(total, ceiling, 10);
    let ceiling_label = ceiling.map_or_else(|| "uncapped".to_owned(), format_micro_usd);
    let dated = dated(days);
    let window = spend
        .map(|window| window.label.as_str())
        .or_else(|| dated.last().map(|day| day.label.as_str()))
        .unwrap_or("today");
    format!(
        "today  {window}  {}/{ceiling_label}  {gauge}  unpriced {unpriced}  unaccounted {unaccounted}",
        format_micro_usd(total)
    )
}

fn footer_line(store: &Store, days: &[MetricDay], active: Option<u64>, view: Option<&ViewDocument>) -> String {
    let dated = dated(days);
    let landed_today = dated.last().map_or_else(
        || {
            view.map_or(0, |view| {
                view.blooms.iter().filter(|bloom| bloom.status == Some(BloomStatus::Landed)).count() as u64
            })
        },
        |day| day.landed,
    );
    let cycle = dated.iter().rev().find_map(|day| day.cycle_time_millis).or_else(|| mean_cycle(store));
    let cycle_label = cycle.map_or_else(|| "—".to_owned(), format_duration);
    let flight = active.unwrap_or_else(|| view.map_or(0, |view| live_blooms(view).count() as u64));
    let (busy, total) = occupancy(view);
    format!("landed {landed_today}  cycle {cycle_label}  flight {flight}  lanes {busy}/{total}")
}

fn occupancy(view: Option<&ViewDocument>) -> (usize, usize) {
    let Some(view) = view else {
        return (0, 0);
    };
    let members: Vec<_> = live_blooms(view).flat_map(|bloom| bloom.members.iter()).collect();
    let busy = members
        .iter()
        .filter(|member| {
            member.wedge.is_some()
                || member.pending_decision.is_some()
                || member.blocked_by.as_deref().is_some_and(|name| !name.is_empty())
        })
        .count();
    (busy, members.len())
}

fn mean_cycle(store: &Store) -> Option<u64> {
    let dispatches = store.dispatches().value.as_ref()?;
    let mut ranges: Vec<(DigestHex, u64, u64)> = Vec::new();
    for row in dispatches {
        let Some(stamp) = row.recorded_unix_millis else {
            continue;
        };
        if let Some((_, first, last)) = ranges.iter_mut().find(|(bloom, _, _)| *bloom == row.bloom) {
            *first = (*first).min(stamp);
            *last = (*last).max(stamp);
        } else {
            ranges.push((row.bloom, stamp, stamp));
        }
    }
    let samples: Vec<u64> =
        ranges.into_iter().map(|(_, first, last)| last.saturating_sub(first)).filter(|span| *span > 0).collect();
    if samples.is_empty() {
        None
    } else {
        Some(samples.iter().sum::<u64>() / u64::try_from(samples.len()).unwrap_or(1))
    }
}

fn spend_gauge(spent: u64, ceiling: Option<u64>, width: usize) -> String {
    let width = width.max(1);
    let Some(ceiling) = ceiling.filter(|ceiling| *ceiling > 0) else {
        return format!("{}{}", "░".repeat(width.min(1)), " ".repeat(width.saturating_sub(1)));
    };
    let filled = usize::try_from((u128::from(spent.min(ceiling)) * u128::from(width as u64)) / u128::from(ceiling))
        .unwrap_or(width)
        .min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width.saturating_sub(filled)))
}

#[cfg(test)]
mod tests {
    use super::{compose, format_duration};
    use crate::dto::MetricDay;
    use crate::store::Store;
    use std::time::Duration;

    #[test]
    fn the_footer_reads_the_newest_dated_day_not_the_undated_bucket() {
        // The plausible bug: today's numbers read off a bucket that is not a day.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_days(Ok(vec![
            MetricDay {
                label: "bloomery/daily/2026-08-19".into(),
                landed: 3,
                cycle_time_millis: Some(7_200_000),
                reconstructed: false,
                ..MetricDay::default()
            },
            MetricDay { label: "reconstructed".into(), landed: 0, reconstructed: true, ..MetricDay::default() },
        ]));
        let footer = compose(&store).footer;
        assert!(footer.contains("landed 3"), "{footer}");
        assert!(footer.contains(&format!("cycle {}", format_duration(7_200_000))), "{footer}");
    }

    #[test]
    fn a_real_series_paints_shape_and_an_absent_one_says_so() {
        // The plausible bug: the header reporting an invented flat series as data.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_days(Ok((1..=14)
            .map(|spend| MetricDay {
                label: format!("bloomery/daily/2026-08-{spend:02}"),
                spend_micro_usd: spend,
                ..MetricDay::default()
            })
            .collect()));
        let spark = compose(&store).spend_spark;
        assert_ne!(spark.chars().min(), spark.chars().max(), "{spark}");

        let empty = Store::new(Duration::from_secs(1));
        let missing = compose(&empty);
        assert_eq!(missing.spend_spark, "—");
        assert_eq!(missing.landed_spark, "—");
        assert_eq!(missing.wedge_spark, "—");
        assert!(!missing.spend_spark.chars().all(|ch| ch == '▁'));
    }
}
