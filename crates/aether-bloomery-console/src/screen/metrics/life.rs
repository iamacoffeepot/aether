//! Per-member wall time from a bloom timeline: stages, nested substages, bloom rollup.
//!
//! Durations use each span's own end. A missing end is unmeasured — never the
//! next span's start.

use aether_bloomery::{
    SPAN_OUTCOME_FAILED, SPAN_OUTCOME_INTEGRATED, SPAN_OUTCOME_PROBE, SPAN_OUTCOME_RETIRED, SPAN_SUBSTAGE_PREPARE,
    WorkpieceId,
};

use crate::dto::{StageId, TimelineSpan};

use super::bucket::format_duration;
use super::glyph::{CellKind, family_of, glyph};

type StageTotals = [(LifeStage, u64); 7];

/// Operator-facing buckets a member's wall time folds into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifeStage {
    Queued,
    Construct,
    Verify,
    IntegrationWait,
    Review,
    Fold,
    Land,
}

impl LifeStage {
    const ALL: [Self; 7] =
        [Self::Queued, Self::Construct, Self::Verify, Self::IntegrationWait, Self::Review, Self::Fold, Self::Land];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Construct => "construct",
            Self::Verify => "verify",
            Self::IntegrationWait => "integration wait",
            Self::Review => "review",
            Self::Fold => "fold",
            Self::Land => "land",
        }
    }
}

/// One selectable row in the member table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifeRow {
    pub key: String,
    pub label: String,
    pub duration_millis: u64,
    pub share: u64,
    pub depth: u8,
}

/// One bloom-level stage rollup row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RollupRow {
    pub stage: LifeStage,
    pub sum_millis: u64,
    pub min_millis: u64,
    pub max_millis: u64,
    pub holder: String,
}

/// One member's life plus the bloom rollup painted beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberLife {
    pub workpiece: String,
    pub total_millis: u64,
    pub bar: String,
    pub rows: Vec<LifeRow>,
    pub rollup: Vec<RollupRow>,
}

/// Fold `spans` into the selected member's life. Substage spans nest; they do
/// not add to the parent stage's total.
#[must_use]
pub fn compose(spans: &[TimelineSpan], workpiece: &str) -> MemberLife {
    let totals = stage_totals(spans, workpiece);
    let total_millis = totals.iter().map(|(_, millis)| *millis).sum();
    let rows = member_rows(spans, workpiece, &totals, total_millis);
    MemberLife {
        workpiece: workpiece.to_owned(),
        total_millis,
        bar: labelled_bar(&totals, total_millis),
        rows,
        rollup: bloom_rollup(spans),
    }
}

fn member_rows(
    spans: &[TimelineSpan],
    workpiece: &str,
    totals: &[(LifeStage, u64)],
    total_millis: u64,
) -> Vec<LifeRow> {
    let mut rows = Vec::new();
    for &(stage, millis) in totals {
        if millis == 0 {
            continue;
        }
        rows.push(stage_row(stage, millis, total_millis, workpiece, spans));
        match stage {
            LifeStage::Construct => rows.extend(construct_sub_rows(spans, workpiece, total_millis)),
            LifeStage::Verify => rows.extend(verify_run_rows(spans, workpiece, total_millis)),
            LifeStage::Queued | LifeStage::IntegrationWait | LifeStage::Review | LifeStage::Fold | LifeStage::Land => {}
        }
    }
    rows
}

fn stage_row(stage: LifeStage, millis: u64, total_millis: u64, workpiece: &str, spans: &[TimelineSpan]) -> LifeRow {
    let label = match stage {
        LifeStage::Queued => queued_label(spans, workpiece),
        _ => stage.label().to_owned(),
    };
    LifeRow {
        key: format!("stage:{}", stage.label()),
        label,
        duration_millis: millis,
        share: share(millis, total_millis),
        depth: 0,
    }
}

fn construct_sub_rows(spans: &[TimelineSpan], workpiece: &str, total_millis: u64) -> Vec<LifeRow> {
    spans
        .iter()
        .filter(|span| span.workpiece == workpiece && bucket_of(span.stage) == Some(LifeStage::Construct))
        .filter_map(|span| {
            let name = span.substage.as_deref()?;
            Some(nested_row(format!("construct:{name}"), substage_label(name), wall_millis(span), total_millis, 1))
        })
        .collect()
}

fn verify_run_rows(spans: &[TimelineSpan], workpiece: &str, total_millis: u64) -> Vec<LifeRow> {
    let mut rows = Vec::new();
    for parent in parents(spans, workpiece).into_iter().filter(|span| span.stage == StageId::Verify) {
        let millis = wall_millis(parent);
        let key = run_key(parent);
        rows.push(nested_row(key.clone(), run_label(parent), millis, total_millis, 1));
        for child in substages_of(spans, parent) {
            let Some(name) = child.substage.as_deref() else {
                continue;
            };
            rows.push(nested_row(format!("{key}:{name}"), substage_label(name), wall_millis(child), total_millis, 2));
        }
    }
    rows
}

fn nested_row(key: String, label: impl Into<String>, millis: u64, total_millis: u64, depth: u8) -> LifeRow {
    LifeRow { key, label: label.into(), duration_millis: millis, share: share(millis, total_millis), depth }
}

fn bloom_rollup(spans: &[TimelineSpan]) -> Vec<RollupRow> {
    let members = member_names(spans);
    let per_member: Vec<(String, StageTotals)> = members
        .into_iter()
        .map(|name| {
            let totals = stage_totals(spans, &name);
            (name, totals)
        })
        .collect();
    LifeStage::ALL.into_iter().filter_map(|stage| rollup_of(&per_member, stage)).collect()
}

fn rollup_of(per_member: &[(String, StageTotals)], stage: LifeStage) -> Option<RollupRow> {
    let mut sum: u64 = 0;
    let mut min = u64::MAX;
    let mut max: u64 = 0;
    let mut holder = String::new();
    for (name, totals) in per_member {
        let millis = totals.iter().find(|(bucket, _)| *bucket == stage).map_or(0, |(_, value)| *value);
        if millis == 0 {
            continue;
        }
        sum = sum.saturating_add(millis);
        min = min.min(millis);
        if millis > max {
            max = millis;
            holder.clone_from(name);
        }
    }
    (sum > 0).then_some(RollupRow { stage, sum_millis: sum, min_millis: min, max_millis: max, holder })
}

fn stage_totals(spans: &[TimelineSpan], workpiece: &str) -> StageTotals {
    let mut totals = LifeStage::ALL.map(|stage| (stage, 0));
    for span in parents(spans, workpiece) {
        let Some(bucket) = bucket_of(span.stage) else {
            continue;
        };
        add_millis(&mut totals, bucket, wall_millis(span));
    }
    add_millis(&mut totals, LifeStage::Queued, queued_millis(spans, workpiece));
    add_millis(&mut totals, LifeStage::IntegrationWait, integration_wait_millis(spans, workpiece));
    totals
}

fn add_millis(totals: &mut [(LifeStage, u64)], stage: LifeStage, millis: u64) {
    if let Some((_, slot)) = totals.iter_mut().find(|(bucket, _)| *bucket == stage) {
        *slot = slot.saturating_add(millis);
    }
}

fn queued_millis(spans: &[TimelineSpan], workpiece: &str) -> u64 {
    let Some(construct_start) = first_construct_start(spans, workpiece) else {
        return 0;
    };
    let bloom_start = spans.iter().filter_map(|span| span.started_unix_millis).min().unwrap_or(construct_start);
    construct_start.saturating_sub(bloom_start)
}

fn queued_label(spans: &[TimelineSpan], workpiece: &str) -> String {
    let Some(construct_start) = first_construct_start(spans, workpiece) else {
        return LifeStage::Queued.label().to_owned();
    };
    let bloom_start = spans.iter().filter_map(|span| span.started_unix_millis).min().unwrap_or(construct_start);
    let mut waited = None;
    for span in spans.iter().filter(|span| span.workpiece != workpiece && span.substage.is_none()) {
        if !overlaps(span, bloom_start, construct_start) {
            continue;
        }
        if span.stage == StageId::BaseVerify {
            waited = Some(span.stage);
            break;
        }
        if waited.is_none() {
            waited = Some(span.stage);
        }
    }
    waited.map_or_else(|| LifeStage::Queued.label().to_owned(), |stage| format!("queued ({stage})"))
}

fn first_construct_start(spans: &[TimelineSpan], workpiece: &str) -> Option<u64> {
    parents(spans, workpiece)
        .into_iter()
        .filter(|span| bucket_of(span.stage) == Some(LifeStage::Construct))
        .filter_map(|span| span.started_unix_millis)
        .min()
}

fn integration_wait_millis(spans: &[TimelineSpan], workpiece: &str) -> u64 {
    let parents = parents(spans, workpiece);
    let integrated_end = parents
        .iter()
        .filter(|span| span.stage == StageId::Verify && span.outcome.as_deref() == Some(SPAN_OUTCOME_INTEGRATED))
        .filter_map(|span| span.ended_unix_millis)
        .max();
    let Some(end) = integrated_end else {
        return 0;
    };
    let next = parents
        .iter()
        .filter(|span| {
            matches!(
                span.stage,
                StageId::Review
                    | StageId::Integrate
                    | StageId::AggregateReview
                    | StageId::AggregateVerify
                    | StageId::Land
            )
        })
        .filter_map(|span| span.started_unix_millis)
        .filter(|&start| start >= end)
        .min();
    next.unwrap_or(end).saturating_sub(end)
}

fn overlaps(span: &TimelineSpan, window_start: u64, window_end: u64) -> bool {
    let start = span.started_unix_millis.unwrap_or(0);
    let end = span.ended_unix_millis.unwrap_or(start);
    start < window_end && end > window_start
}

fn labelled_bar(totals: &[(LifeStage, u64)], total_millis: u64) -> String {
    totals
        .iter()
        .filter(|(_, millis)| *millis > 0)
        .map(|(stage, millis)| {
            let mark = glyph(CellKind::Stage(family_of(stage_id(*stage))));
            format!("{mark} {} {} {}%", stage.label(), format_duration(*millis), share(*millis, total_millis))
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn stage_id(stage: LifeStage) -> StageId {
    match stage {
        LifeStage::Queued | LifeStage::IntegrationWait | LifeStage::Fold | LifeStage::Land => StageId::Land,
        LifeStage::Construct => StageId::Construct,
        LifeStage::Verify => StageId::Verify,
        LifeStage::Review => StageId::Review,
    }
}

fn bucket_of(stage: StageId) -> Option<LifeStage> {
    match stage {
        StageId::Construct | StageId::Refine | StageId::Reconcile => Some(LifeStage::Construct),
        StageId::Verify => Some(LifeStage::Verify),
        StageId::Review => Some(LifeStage::Review),
        StageId::Integrate | StageId::AggregateVerify | StageId::AggregateReview => Some(LifeStage::Fold),
        StageId::Land => Some(LifeStage::Land),
        StageId::Sketch
        | StageId::Scope
        | StageId::Approve
        | StageId::Study
        | StageId::BaseVerify
        | StageId::Unknown => None,
    }
}

fn parents<'a>(spans: &'a [TimelineSpan], workpiece: &str) -> Vec<&'a TimelineSpan> {
    spans.iter().filter(|span| span.workpiece == workpiece && span.substage.is_none()).collect()
}

fn substages_of<'a>(spans: &'a [TimelineSpan], parent: &TimelineSpan) -> Vec<&'a TimelineSpan> {
    spans
        .iter()
        .filter(|span| {
            span.workpiece == parent.workpiece
                && span.stage == parent.stage
                && span.substage.is_some()
                && same_run(span, parent)
        })
        .collect()
}

fn same_run(span: &TimelineSpan, parent: &TimelineSpan) -> bool {
    match (span.run, parent.run) {
        (Some(left), Some(right)) => left == right,
        _ => span.sequence == parent.sequence,
    }
}

fn member_names(spans: &[TimelineSpan]) -> Vec<String> {
    let mut names = Vec::new();
    for span in spans {
        if span.workpiece.is_empty() || span.workpiece == WorkpieceId::COMPOSITION {
            continue;
        }
        if !names.contains(&span.workpiece) {
            names.push(span.workpiece.clone());
        }
    }
    names
}

fn wall_millis(span: &TimelineSpan) -> u64 {
    let Some(start) = span.started_unix_millis else {
        return 0;
    };
    span.ended_unix_millis.unwrap_or(start).saturating_sub(start)
}

fn share(part: u64, total: u64) -> u64 {
    if total == 0 {
        0
    } else {
        (u128::from(part) * 100 / u128::from(total)).try_into().unwrap_or(100)
    }
}

fn run_key(span: &TimelineSpan) -> String {
    span.run.map_or_else(|| format!("run:{}", span.sequence), |run| format!("run:{}", run.as_hex()))
}

fn run_label(span: &TimelineSpan) -> String {
    let outcome = span.outcome.as_deref().map(outcome_label);
    let prefix = span.run.map(|run| run.prefix());
    match (outcome, prefix) {
        (Some(outcome), Some(prefix)) => format!("{outcome}  {prefix}"),
        (Some(outcome), None) => outcome,
        (None, Some(prefix)) => format!("run {prefix}"),
        (None, None) => "run".to_owned(),
    }
}

fn outcome_label(outcome: &str) -> String {
    match outcome {
        SPAN_OUTCOME_RETIRED => "retired by head move".to_owned(),
        SPAN_OUTCOME_INTEGRATED => "integrated".to_owned(),
        SPAN_OUTCOME_FAILED => "failed test".to_owned(),
        SPAN_OUTCOME_PROBE => "probe".to_owned(),
        other => other.to_owned(),
    }
}

fn substage_label(name: &str) -> String {
    if name == SPAN_SUBSTAGE_PREPARE {
        return "prepare".to_owned();
    }
    name.strip_prefix("verify.").unwrap_or(name).to_owned()
}

/// In-row bar scaled to `max_millis`, empty when the row has no duration.
#[must_use]
pub fn duration_bar(millis: u64, max_millis: u64, width: usize) -> String {
    let width = width.max(1);
    if millis == 0 || max_millis == 0 {
        return " ".repeat(width);
    }
    let filled = usize::try_from((u128::from(millis) * u128::from(width as u64)) / u128::from(max_millis))
        .unwrap_or(width)
        .min(width)
        .max(1);
    format!("{}{}", "█".repeat(filled), " ".repeat(width.saturating_sub(filled)))
}
