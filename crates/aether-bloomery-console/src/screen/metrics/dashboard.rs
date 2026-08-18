//! Numbers the five-region dashboard paints from the store.

use crate::dto::{BloomStatus, DigestHex, MetricDay, SpendQuiesce, ViewDocument};
use crate::screen::partition::live_blooms;
use crate::store::Store;

use super::bucket::format_duration;
use super::cost::format_micro_usd;
use super::sparkline::{last_days, sparkline};

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
    let spend = store.spend().value.as_ref();
    let summary = store.summary().value.as_ref();
    let view = store.view().value.as_ref();

    let mut spend_days = last_days(days, |day| day.spend_micro_usd);
    let mut landed_days = last_days(days, |day| day.landed);
    let mut wedge_days = last_days(days, |day| day.wedges);
    if let Some(window) = spend
        && let Some(last) = spend_days.last_mut()
    {
        *last = window.total_micro_usd;
    }
    if let Some(view) = view {
        if let Some(last) = landed_days.last_mut() {
            *last = view.blooms.iter().filter(|bloom| bloom.status == Some(BloomStatus::Landed)).count() as u64;
        }
        if let Some(last) = wedge_days.last_mut() {
            *last = view
                .blooms
                .iter()
                .flat_map(|bloom| bloom.members.iter())
                .filter(|member| member.wedge.is_some())
                .count() as u64;
        }
    }

    let today = today_line(store, days);
    let footer = footer_line(store, days, summary.map(|summary| summary.active_blooms), view);

    Dashboard {
        spend_spark: sparkline(&spend_days),
        landed_spark: sparkline(&landed_days),
        wedge_spark: sparkline(&wedge_days),
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
    let window = spend
        .map(|window| window.label.as_str())
        .or_else(|| days.last().map(|day| day.label.as_str()))
        .unwrap_or("today");
    format!(
        "today  {window}  {}/{ceiling_label}  {gauge}  unpriced {unpriced}  unaccounted {unaccounted}",
        format_micro_usd(total)
    )
}

fn footer_line(store: &Store, days: &[MetricDay], active: Option<u64>, view: Option<&ViewDocument>) -> String {
    let landed_today = days.last().map_or_else(
        || {
            view.map_or(0, |view| {
                view.blooms.iter().filter(|bloom| bloom.status == Some(BloomStatus::Landed)).count() as u64
            })
        },
        |day| day.landed,
    );
    let cycle = days.iter().rev().find_map(|day| day.cycle_time_millis).or_else(|| mean_cycle(store));
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
