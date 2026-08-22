//! The metrics ledger: cost, timing, and throughput folded from the journal.
//!
//! [`MetricsLedger::observe`] is the counterpart to
//! [`CalibrationLedger::observe`](crate::CalibrationLedger::observe) and
//! [`Snapshot::apply`](crate::reduce::Snapshot::apply): a caller folds each
//! admitted `(event, decisions)` pair in journal order and holds the
//! accumulator beside the snapshot. Boot replay rebuilds it; a live admission
//! extends it in O(1). Dollars are never stored — the fold holds study-artifact
//! digests, and a report resolves them through the same seam `grade` /
//! [`measure`](crate::measure) already use. `cost == 0` means unpriced, never
//! free, and is counted apart from any mean.
//!
//! The seat is recomputed from the sealed catalog profile with the member's
//! override resolved over it — never read from
//! [`Transformation::model`](crate::Transformation::model), which the reducer
//! authors as `None` (the trap [`crate::calibration`] documents). Only a model
//! lane mints a seat: the mechanical verify fan-out stays on the dispatch
//! rollup.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::ids::{BloomId, StageId};
use crate::ledger::{SeatDispatch, priced_micro_usd};
use crate::reduce::{Decision, Decisions, Event, Fact};
use crate::values::{DispatchKey, EvidenceKind, ReasoningEffort, ResolvedConfigs, ResolvedModel, StudyRecord};

/// How many timeline spans one bloom read returns before it truncates.
pub const TIMELINE_SPAN_CAP: u64 = 256;
/// How many day rows a days read returns at most.
pub const DAYS_CAP: u64 = 90;
/// Default page size for bloom and dispatch lists.
pub const METRICS_DEFAULT_LIMIT: u64 = 100;
/// Hard ceiling for bloom and dispatch pages.
pub const METRICS_MAX_LIMIT: u64 = 1_000;
/// The day bucket a row with no envelope stamp lands in — never a fabricated
/// civil date.
pub const RECONSTRUCTED_WINDOW: &str = "reconstructed";

/// The host's window label for an envelope stamp: `bloomery/daily/YYYY-MM-DD`
/// in UTC, the same spelling the fleet already uses. The fold does not invent
/// a timezone; it names the host-clock instant's UTC day.
#[must_use]
pub fn window_label(unix_millis: u64) -> String {
    let (year, month, day) = utc_ymd(unix_millis / 1000);
    let mut label = String::from("bloomery/daily/");
    push_u32(&mut label, year, 4);
    label.push('-');
    push_u32(&mut label, month, 2);
    label.push('-');
    push_u32(&mut label, day, 2);
    label
}

/// The journal-derived accumulator a caller folds admitted events through.
///
/// Holds nothing that is not derivable from the journal and its envelope
/// stamps, so a replay rebuilds exactly the ledger the live fold produced.
/// Evidence-only scalars (session reuse, peak resident bytes, per-call arrays)
/// are not here — they are written at intake onto the rollup row keyed by
/// nonce, because they never enter the journal.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct MetricsLedger {
    dispatches: BTreeMap<DispatchId, DispatchAcc>,
    blooms: BTreeMap<BloomId, BloomAcc>,
    days: BTreeMap<String, DayAcc>,
    studies: Vec<Study>,
    /// Highest journal sequence observed. `0` means nothing has been folded.
    through_sequence: u64,
}

/// One dispatch as the fold keys it — bloom, slot, and the digest the attempt
/// displayed. The host's nonce is a later join, not a journal fact.
type DispatchId = (BloomId, DispatchKey, Digest);

#[derive(Clone, PartialEq, Eq, Debug)]
struct DispatchAcc {
    bloom: BloomId,
    workpiece: String,
    stage: StageId,
    displayed: Digest,
    sequence: u64,
    recorded_unix_millis: Option<u64>,
    reconstructed: bool,
    agent: ResolvedModel,
    /// Whether the sealed command is a model lane. Mechanical dispatches stay
    /// on the dispatch rollup; they do not mint a seat.
    model_lane: bool,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct BloomAcc {
    seal_sequence: u64,
    members: u64,
    dispatches: u64,
    first_unix_millis: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct DayAcc {
    dispatches: u64,
    landed: u64,
    wedges: u64,
    cycle_sum_millis: u64,
    cycle_samples: u64,
    quiesced: bool,
    reconstructed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Study {
    bloom: BloomId,
    subject: Digest,
    detail: Digest,
}

/// One dispatch row the rollup cache persists.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MetricDispatch {
    /// Deterministic fold identity; the host may also store the dispatch nonce.
    pub id: String,
    pub bloom: BloomId,
    pub workpiece: String,
    pub stage: StageId,
    pub displayed: Digest,
    pub sequence: u64,
    pub recorded_unix_millis: Option<u64>,
    /// True when the envelope stamp was absent and the span is reconstructed.
    pub reconstructed: bool,
    pub agent: ResolvedModel,
    /// Study-artifact digest, when one was admitted against this displayed
    /// attempt. Dollars stay on that artifact.
    pub study: Option<Digest>,
}

/// One bloom rollup row.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MetricBloom {
    pub bloom: BloomId,
    pub seal_sequence: u64,
    pub members: u64,
    pub dispatches: u64,
}

/// One day rollup row, keyed on the host window label.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MetricDay {
    pub label: String,
    pub dispatches: u64,
    /// Priced study records attributed to this day's dispatches. `0` is
    /// unpriced or unresolved, never free.
    pub spend_micro_usd: u64,
    pub landed: u64,
    pub wedges: u64,
    /// Mean first-dispatch-to-landing span of the blooms that landed this day,
    /// or `None` when none did. Never a zero mean.
    pub cycle_time_millis: Option<u64>,
    /// A spend quiesce was recorded on this day.
    pub quiesced: bool,
    /// This is the undated bucket for rows with no envelope stamp — not a
    /// civil day, and not comparable with the dated rows.
    pub reconstructed: bool,
}

/// Fixed-size fleet summary.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct MetricsSummary {
    pub blooms: u64,
    pub dispatches: u64,
    pub unpriced: u64,
    pub reconstructed: u64,
    /// Live in-flight blooms on the snapshot at read time — the 1 Hz / poll
    /// join, not a journal fact.
    pub active_blooms: u64,
}

/// One seat row: a calibration cell plus the token and cache columns a study
/// record actually resolved.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MetricsSeat {
    pub agent: ResolvedModel,
    pub stage: StageId,
    pub attempts: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    /// Sum of priced (`cost > 0`) study records only.
    pub cost_micro_usd: u64,
    /// How many study records contributed to [`cost_micro_usd`](Self::cost_micro_usd).
    pub priced_samples: u64,
    /// Study records whose priced column is zero — counted, never averaged.
    pub unpriced: u64,
}

impl MetricsSeat {
    /// Mean micro-USD of priced samples, or `None` when none were priced.
    ///
    /// `None` is unmeasured, never zero. Flattening it would make a seat that
    /// only ran unpriced attempts look like the cheapest one in the table.
    #[must_use]
    pub fn mean_cost_micro_usd(&self) -> Option<u64> {
        (self.priced_samples > 0).then(|| self.cost_micro_usd / self.priced_samples)
    }
}

/// One per-member stage span on a bloom timeline.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TimelineSpan {
    pub workpiece: String,
    pub stage: StageId,
    pub sequence: u64,
    pub started_unix_millis: Option<u64>,
    /// True when no envelope stamp was present on the dispatch that opened
    /// this span — the reader must not treat the order as wall-clock time.
    pub reconstructed: bool,
}

/// A bloom's stage timeline, truncated at [`TIMELINE_SPAN_CAP`].
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MetricsTimeline {
    pub bloom: BloomId,
    pub spans: Vec<TimelineSpan>,
    pub truncated: bool,
}

impl MetricsLedger {
    /// Highest journal sequence folded into this ledger.
    #[must_use]
    pub fn through_sequence(&self) -> u64 {
        self.through_sequence
    }

    /// Fold one admitted event and its recorded decisions.
    ///
    /// `sequence` is the journal row. `envelope` is that row's host-clock stamp
    /// (`recorded_unix_millis`); `None` is a pre-column row and is marked
    /// reconstructed rather than given an invented time.
    ///
    /// The seat is recomputed from the sealed catalog profile with the member's
    /// override resolved over it.
    pub fn observe(
        &mut self,
        sequence: u64,
        event: &Event,
        decisions: &Decisions,
        configs: &ResolvedConfigs,
        envelope: Option<u64>,
    ) {
        if sequence > self.through_sequence {
            self.through_sequence = sequence;
        }
        if let Fact::Seal(spec) = &event.fact {
            let bloom = spec.id();
            let members = u64::try_from(spec.members().len()).unwrap_or(u64::MAX);
            let acc = self.blooms.entry(bloom).or_default();
            acc.seal_sequence = sequence;
            acc.members = members;
        }
        for effect in &decisions.effects {
            self.observe_effect(sequence, effect, configs, envelope);
        }
    }

    /// Every dispatch row, in (sequence, id) order — the persist surface.
    #[must_use]
    pub fn dispatch_rows(&self) -> Vec<MetricDispatch> {
        let mut rows: Vec<MetricDispatch> = self.dispatches.values().map(|acc| self.dispatch_row(acc)).collect();
        rows.sort_by(|a, b| a.sequence.cmp(&b.sequence).then_with(|| a.id.cmp(&b.id)));
        rows
    }

    /// Every bloom rollup, in seal-sequence order.
    #[must_use]
    pub fn bloom_rows(&self) -> Vec<MetricBloom> {
        let mut rows: Vec<MetricBloom> = self
            .blooms
            .iter()
            .map(|(bloom, acc)| MetricBloom {
                bloom: *bloom,
                seal_sequence: acc.seal_sequence,
                members: acc.members,
                dispatches: acc.dispatches,
            })
            .collect();
        rows.sort_by_key(|row| (row.seal_sequence, row.bloom));
        rows
    }

    /// Every day rollup. The undated reconstructed bucket, when present, is
    /// first; dated days follow in label-ascending order, newest dated day last.
    #[must_use]
    pub fn day_rows(&self, source: impl Fn(&Digest) -> Option<StudyRecord>) -> Vec<MetricDay> {
        let mut spend_by_label: BTreeMap<String, u64> = BTreeMap::new();
        for study in &self.studies {
            let Some(acc) =
                self.dispatches.values().find(|acc| (acc.bloom, acc.displayed) == (study.bloom, study.subject))
            else {
                continue;
            };
            let Some(record) =
                source(&study.detail).filter(|record| record.grades(&study.subject) && record.bloom == study.bloom)
            else {
                continue;
            };
            let Some(cost) = priced_micro_usd(record.cost.cost_micro_usd) else {
                continue;
            };
            let slot = spend_by_label.entry(day_label(acc.recorded_unix_millis)).or_insert(0);
            *slot = slot.saturating_add(cost);
        }

        let mut rows: Vec<MetricDay> = self
            .days
            .iter()
            .map(|(label, acc)| MetricDay {
                label: label.clone(),
                dispatches: acc.dispatches,
                spend_micro_usd: spend_by_label.get(label).copied().unwrap_or(0),
                landed: acc.landed,
                wedges: acc.wedges,
                cycle_time_millis: (acc.cycle_samples > 0).then(|| acc.cycle_sum_millis / acc.cycle_samples),
                quiesced: acc.quiesced,
                reconstructed: acc.reconstructed,
            })
            .collect();
        rows.sort_by(|a, b| b.reconstructed.cmp(&a.reconstructed).then_with(|| a.label.cmp(&b.label)));
        rows
    }

    /// Fixed-size summary. `active_blooms` is the live snapshot join.
    #[must_use]
    pub fn summary(&self, active_blooms: u64, source: impl Fn(&Digest) -> Option<StudyRecord>) -> MetricsSummary {
        let mut unpriced = 0u64;
        for study in &self.studies {
            match source(&study.detail) {
                Some(record)
                    if record.grades(&study.subject)
                        && record.bloom == study.bloom
                        && record.cost.cost_micro_usd == 0 =>
                {
                    unpriced = unpriced.saturating_add(1);
                }
                _ => {}
            }
        }
        MetricsSummary {
            blooms: u64::try_from(self.blooms.len()).unwrap_or(u64::MAX),
            dispatches: u64::try_from(self.dispatches.len()).unwrap_or(u64::MAX),
            unpriced,
            reconstructed: u64::try_from(self.dispatches.values().filter(|row| row.reconstructed).count())
                .unwrap_or(u64::MAX),
            active_blooms,
        }
    }

    /// Seat rows: calibration cells plus token / cache columns. An unpriced
    /// study record increments [`MetricsSeat::unpriced`] and is excluded from
    /// the priced sum and the mean.
    #[must_use]
    pub fn seats(&self, source: impl Fn(&Digest) -> Option<StudyRecord>) -> Vec<MetricsSeat> {
        let mut cells: BTreeMap<SeatKey, MetricsSeat> = BTreeMap::new();
        for acc in self.dispatches.values().filter(|acc| acc.model_lane) {
            let key = SeatKey::of(&acc.agent, acc.stage);
            let cell = cells.entry(key).or_insert_with(|| MetricsSeat {
                agent: acc.agent.clone(),
                stage: acc.stage,
                attempts: 0,
                input_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                cost_micro_usd: 0,
                priced_samples: 0,
                unpriced: 0,
            });
            cell.attempts = cell.attempts.saturating_add(1);
        }
        for study in &self.studies {
            let Some(acc) = self
                .dispatches
                .values()
                .find(|row| row.model_lane && (row.bloom, row.displayed) == (study.bloom, study.subject))
            else {
                continue;
            };
            let Some(record) =
                source(&study.detail).filter(|record| record.grades(&study.subject) && record.bloom == study.bloom)
            else {
                continue;
            };
            let key = SeatKey::of(&acc.agent, acc.stage);
            let cell = cells.entry(key).or_insert_with(|| MetricsSeat {
                agent: acc.agent.clone(),
                stage: acc.stage,
                attempts: 0,
                input_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                cost_micro_usd: 0,
                priced_samples: 0,
                unpriced: 0,
            });
            cell.input_tokens = cell.input_tokens.saturating_add(record.cost.input_tokens);
            cell.cache_read_tokens = cell.cache_read_tokens.saturating_add(record.cost.cache_read_tokens);
            cell.cache_write_tokens = cell.cache_write_tokens.saturating_add(record.cost.cache_write_tokens);
            cell.output_tokens = cell.output_tokens.saturating_add(record.cost.output_tokens);
            if let Some(cost) = priced_micro_usd(record.cost.cost_micro_usd) {
                cell.cost_micro_usd = cell.cost_micro_usd.saturating_add(cost);
                cell.priced_samples = cell.priced_samples.saturating_add(1);
            } else {
                cell.unpriced = cell.unpriced.saturating_add(1);
            }
        }
        cells.into_values().collect()
    }

    /// Per-member stage spans for `bloom`, capped at [`TIMELINE_SPAN_CAP`].
    #[must_use]
    pub fn timeline(&self, bloom: BloomId) -> MetricsTimeline {
        let mut spans: Vec<TimelineSpan> = self
            .dispatches
            .values()
            .filter(|row| row.bloom == bloom)
            .map(|row| TimelineSpan {
                workpiece: row.workpiece.clone(),
                stage: row.stage,
                sequence: row.sequence,
                started_unix_millis: row.recorded_unix_millis,
                reconstructed: row.reconstructed,
            })
            .collect();
        spans.sort_by(|a, b| a.sequence.cmp(&b.sequence).then_with(|| a.workpiece.cmp(&b.workpiece)));
        let cap = usize::try_from(TIMELINE_SPAN_CAP).unwrap_or(usize::MAX);
        let truncated = spans.len() > cap;
        spans.truncate(cap);
        MetricsTimeline { bloom, spans, truncated }
    }

    fn observe_effect(&mut self, sequence: u64, effect: &Decision, configs: &ResolvedConfigs, envelope: Option<u64>) {
        if let Some(dispatched) = SeatDispatch::from_effect(effect) {
            self.dispatch(sequence, dispatched, configs, envelope);
            return;
        }
        match effect {
            Decision::RecordEvidence { bloom, evidence } if evidence.kind == EvidenceKind::StudyRecord => {
                self.studies.push(Study { bloom: *bloom, subject: evidence.subject, detail: evidence.detail });
            }
            Decision::EmitReceipt(projected) => {
                let first = self.blooms.get(&projected.receipt.bloom).and_then(|acc| acc.first_unix_millis);
                let day = self.day(envelope);
                day.landed = day.landed.saturating_add(1);
                if let (Some(landing_millis), Some(first)) = (envelope, first) {
                    day.cycle_sum_millis = day.cycle_sum_millis.saturating_add(landing_millis.saturating_sub(first));
                    day.cycle_samples = day.cycle_samples.saturating_add(1);
                }
            }
            Decision::RecordWedge { .. } => {
                let day = self.day(envelope);
                day.wedges = day.wedges.saturating_add(1);
            }
            Decision::RecordSpendQuiesce { quiesce: Some(_) } => {
                self.day(envelope).quiesced = true;
            }
            _ => {}
        }
    }

    fn dispatch(
        &mut self,
        sequence: u64,
        dispatched: SeatDispatch<'_>,
        configs: &ResolvedConfigs,
        envelope: Option<u64>,
    ) {
        let agent = dispatched.agent(configs);
        let model_lane = dispatched.is_model_lane();
        let SeatDispatch { bloom, key, stage, workpiece, displayed, .. } = dispatched;

        let id = (bloom, key, displayed);
        let is_new = !self.dispatches.contains_key(&id);
        self.dispatches.entry(id).or_insert_with(|| DispatchAcc {
            bloom,
            workpiece,
            stage,
            displayed,
            sequence,
            recorded_unix_millis: envelope,
            reconstructed: envelope.is_none(),
            agent,
            model_lane,
        });
        if is_new {
            {
                let bloom_acc = self.blooms.entry(bloom).or_default();
                bloom_acc.dispatches = bloom_acc.dispatches.saturating_add(1);
                // Keep the earliest present stamp; `Option::min` treats `None` as
                // smallest, so an unstamped later dispatch must not wipe one.
                if envelope.is_some() {
                    bloom_acc.first_unix_millis = bloom_acc.first_unix_millis.min(envelope).or(envelope);
                }
            }
            let day = self.day(envelope);
            day.dispatches = day.dispatches.saturating_add(1);
        }
    }

    fn day(&mut self, envelope: Option<u64>) -> &mut DayAcc {
        let reconstructed = envelope.is_none();
        let acc = self.days.entry(day_label(envelope)).or_default();
        acc.reconstructed = reconstructed;
        acc
    }

    fn dispatch_row(&self, acc: &DispatchAcc) -> MetricDispatch {
        let study = self
            .studies
            .iter()
            .find(|study| (study.bloom, study.subject) == (acc.bloom, acc.displayed))
            .map(|study| study.detail);
        MetricDispatch {
            id: dispatch_id(acc),
            bloom: acc.bloom,
            workpiece: acc.workpiece.clone(),
            stage: acc.stage,
            displayed: acc.displayed,
            sequence: acc.sequence,
            recorded_unix_millis: acc.recorded_unix_millis,
            reconstructed: acc.reconstructed,
            agent: acc.agent.clone(),
            study,
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct SeatKey {
    harness: &'static str,
    model: String,
    effort: ReasoningEffort,
    stage: StageId,
}

impl SeatKey {
    fn of(agent: &ResolvedModel, stage: StageId) -> Self {
        Self { harness: agent.harness.as_str(), model: agent.model.clone(), effort: agent.effort, stage }
    }
}

fn day_label(envelope: Option<u64>) -> String {
    envelope.map_or_else(|| String::from(RECONSTRUCTED_WINDOW), window_label)
}

fn dispatch_id(acc: &DispatchAcc) -> String {
    let mut id = String::from("fold:");
    id.push_str(&acc.bloom.0.to_hex());
    id.push(':');
    id.push_str(&acc.workpiece);
    id.push(':');
    id.push_str(stage_slug(acc.stage));
    id.push(':');
    id.push_str(&acc.displayed.to_hex());
    id
}

fn stage_slug(stage: StageId) -> &'static str {
    match stage {
        StageId::Sketch => "sketch",
        StageId::Scope => "scope",
        StageId::Approve => "approve",
        StageId::Construct => "construct",
        StageId::Verify => "verify",
        StageId::Refine => "refine",
        StageId::Review => "review",
        StageId::Integrate => "integrate",
        StageId::AggregateVerify => "aggregate-verify",
        StageId::AggregateReview => "aggregate-review",
        StageId::Land => "land",
        StageId::Study => "study",
        StageId::Reconcile => "reconcile",
        StageId::BaseVerify => "base-verify",
    }
}

fn push_u32(into: &mut String, value: u32, width: usize) {
    let mut digits = [b'0'; 10];
    let mut n = value;
    let mut i = 10;
    while i > 0 {
        i -= 1;
        digits[i] = b'0' + u8::try_from(n % 10).unwrap_or(0);
        n /= 10;
        if n == 0 && 10 - i >= width {
            break;
        }
    }
    for digit in &digits[i..] {
        into.push(char::from(*digit));
    }
}

/// UTC civil date from Unix seconds. Howard Hinnant's `civil_from_days`.
fn utc_ymd(unix_secs: u64) -> (u32, u32, u32) {
    let z = i64::try_from(unix_secs / 86_400).unwrap_or(i64::MAX) + 719_468;
    let era = if z >= 0 {
        z
    } else {
        z - 146_096
    } / 146_097;
    let doe = u64::try_from(z - era * 146_097).unwrap_or(0);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = i64::try_from(yoe).unwrap_or(0) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 {
        mp + 3
    } else {
        mp - 9
    };
    let y = if m <= 2 {
        y + 1
    } else {
        y
    };
    (u32::try_from(y).unwrap_or(1970), u32::try_from(m).unwrap_or(1), u32::try_from(d).unwrap_or(1))
}

#[cfg(test)]
mod window_tests {
    use super::{RECONSTRUCTED_WINDOW, window_label};

    #[test]
    fn window_label_names_the_utc_day_of_the_envelope() {
        assert_eq!(window_label(0), "bloomery/daily/1970-01-01");
        assert_eq!(RECONSTRUCTED_WINDOW, "reconstructed");
    }
}
