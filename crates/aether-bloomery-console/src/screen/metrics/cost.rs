//! Cost breakdown groups. Entirely-unpriced groups render `—` and stay
//! out of the mean.

use crate::dto::{MetricDispatch, MetricsSeat, SpendWindowView};

/// Which axis the cost table is grouped on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CostAxis {
    #[default]
    Bloom,
    Member,
    Stage,
    Seat,
}

impl CostAxis {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Bloom => "bloom",
            Self::Member => "member",
            Self::Stage => "stage",
            Self::Seat => "seat",
        }
    }

    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Bloom => Self::Member,
            Self::Member => Self::Stage,
            Self::Stage => Self::Seat,
            Self::Seat => Self::Bloom,
        }
    }
}

/// One grouped cost row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CostGroup {
    pub label: String,
    pub cost_micro_usd: u64,
    pub priced_samples: u64,
    pub unpriced: u64,
}

impl CostGroup {
    /// `—` when nothing in the group was priced; otherwise the sum.
    #[must_use]
    pub fn cost_label(&self) -> String {
        if self.is_unpriced() {
            "—".to_owned()
        } else {
            format_micro_usd(self.cost_micro_usd)
        }
    }

    /// `—` when nothing in the group was priced; otherwise the mean.
    #[must_use]
    pub fn mean_label(&self) -> String {
        self.mean_micro_usd().map_or_else(|| "—".to_owned(), format_micro_usd)
    }

    #[must_use]
    pub fn is_unpriced(&self) -> bool {
        self.priced_samples == 0
    }

    #[must_use]
    pub fn mean_micro_usd(&self) -> Option<u64> {
        (self.priced_samples > 0).then(|| self.cost_micro_usd / self.priced_samples)
    }

    /// In-row bar, empty for an unpriced group so it cannot look cheapest.
    #[must_use]
    pub fn bar(&self, width: usize, max_cost: u64) -> String {
        let width = width.max(1);
        if self.is_unpriced() || max_cost == 0 {
            return " ".repeat(width);
        }
        let filled =
            usize::try_from((u128::from(self.cost_micro_usd) * u128::from(width as u64)) / u128::from(max_cost))
                .unwrap_or(width)
                .min(width)
                .max(1);
        format!("{}{}", "█".repeat(filled), " ".repeat(width.saturating_sub(filled)))
    }
}

/// Mean of priced groups only. Entirely-unpriced groups do not enter.
#[must_use]
pub fn mean_of(groups: &[CostGroup]) -> Option<u64> {
    let (sum, samples) =
        groups.iter().filter(|group| !group.is_unpriced()).fold((0u64, 0u64), |(sum, samples), group| {
            (sum.saturating_add(group.cost_micro_usd), samples.saturating_add(group.priced_samples))
        });
    (samples > 0).then(|| sum / samples)
}

/// Seat and stage axes fold [`MetricsSeat`] rows.
#[must_use]
pub fn groups_from_seats(seats: &[MetricsSeat], axis: CostAxis) -> Vec<CostGroup> {
    match axis {
        CostAxis::Seat => seats
            .iter()
            .map(|seat| CostGroup {
                label: format!("{} {} {}", seat.agent.harness, seat.agent.model, seat.stage),
                cost_micro_usd: seat.cost_micro_usd,
                priced_samples: seat.priced_samples,
                unpriced: seat.unpriced,
            })
            .collect(),
        CostAxis::Stage => fold_seats(seats, |seat| seat.stage.to_string()),
        CostAxis::Bloom | CostAxis::Member => Vec::new(),
    }
}

/// Member axis: dispatch rows have no dollar column, so a member with
/// only unjoined attempts is unpriced rather than free.
#[must_use]
pub fn groups_from_members(dispatches: &[MetricDispatch]) -> Vec<CostGroup> {
    let mut groups: Vec<CostGroup> = Vec::new();
    for dispatch in dispatches {
        let label = if dispatch.workpiece.is_empty() {
            "(bloom)".to_owned()
        } else {
            dispatch.workpiece.clone()
        };
        if let Some(group) = groups.iter_mut().find(|group| group.label == label) {
            group.unpriced = group.unpriced.saturating_add(1);
        } else {
            groups.push(CostGroup { label, cost_micro_usd: 0, priced_samples: 0, unpriced: 1 });
        }
    }
    groups
}

/// Bloom axis from the spend window. A zero-cost bloom with unpriced
/// records in the window is unpriced, not free.
#[must_use]
pub fn groups_from_spend(spend: &SpendWindowView, axis: CostAxis) -> Vec<CostGroup> {
    if axis != CostAxis::Bloom {
        return Vec::new();
    }
    spend
        .per_bloom
        .iter()
        .map(|(bloom, cost)| {
            let priced = u64::from(*cost > 0);
            CostGroup {
                label: bloom.get(..8).unwrap_or(bloom).to_owned(),
                cost_micro_usd: *cost,
                priced_samples: priced,
                unpriced: u64::from(*cost == 0 && spend.unpriced_records > 0),
            }
        })
        .collect()
}

fn fold_seats(seats: &[MetricsSeat], key: impl Fn(&MetricsSeat) -> String) -> Vec<CostGroup> {
    let mut groups: Vec<CostGroup> = Vec::new();
    for seat in seats {
        let label = key(seat);
        if let Some(group) = groups.iter_mut().find(|group| group.label == label) {
            group.cost_micro_usd = group.cost_micro_usd.saturating_add(seat.cost_micro_usd);
            group.priced_samples = group.priced_samples.saturating_add(seat.priced_samples);
            group.unpriced = group.unpriced.saturating_add(seat.unpriced);
        } else {
            groups.push(CostGroup {
                label,
                cost_micro_usd: seat.cost_micro_usd,
                priced_samples: seat.priced_samples,
                unpriced: seat.unpriced,
            });
        }
    }
    groups
}

#[must_use]
pub fn format_micro_usd(micro_usd: u64) -> String {
    let dollars = micro_usd / 1_000_000;
    let rest = micro_usd % 1_000_000;
    if rest == 0 {
        format!("${dollars}")
    } else {
        format!("${dollars}.{:02}", rest / 10_000)
    }
}

#[cfg(test)]
mod tests {
    use super::{CostGroup, mean_of};

    fn priced(label: &str, cost: u64, samples: u64) -> CostGroup {
        CostGroup { label: label.to_owned(), cost_micro_usd: cost, priced_samples: samples, unpriced: 0 }
    }

    fn unpriced(label: &str, count: u64) -> CostGroup {
        CostGroup { label: label.to_owned(), cost_micro_usd: 0, priced_samples: 0, unpriced: count }
    }

    #[test]
    fn an_unpriced_group_renders_an_em_dash_and_is_excluded_from_means() {
        // The plausible bug: cost == 0 is treated as free, so an unpriced
        // seat pulls the mean down and paints $0 instead of —.
        let groups = [priced("construct", 2_000_000, 2), unpriced("review", 3)];
        assert_eq!(groups[1].cost_label(), "—");
        assert_eq!(groups[1].mean_label(), "—");
        assert!(groups[1].is_unpriced());
        assert_eq!(mean_of(&groups), Some(1_000_000), "the unpriced group must not enter the mean");
        assert_eq!(mean_of(&[unpriced("only", 1)]), None);
    }
}
