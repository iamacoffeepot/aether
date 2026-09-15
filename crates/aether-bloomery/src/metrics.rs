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

use aether_data::wire::{Error as WireError, from_bytes};
use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::ledger::{SeatDispatch, priced_micro_usd};
use crate::reduce::{Decision, Decisions, Event, Fact, Outcome};
use crate::values::{
    BloomSpec, DispatchKey, EvidenceKind, MemberPin, MemberVerifyOutcome, ReasoningEffort, ResolvedConfigs,
    ResolvedModel, SharedRunCompletion, SharedRunPlan, StudyRecord,
};

/// How many timeline spans one bloom read returns before it truncates.
pub const TIMELINE_SPAN_CAP: u64 = 256;
/// A shared-run verify span whose candidate is on the selected head.
pub const SPAN_OUTCOME_INTEGRATED: &str = "integrated";
/// A shared-run verify span retired by a closure-intersecting head move.
pub const SPAN_OUTCOME_RETIRED: &str = "retired";
/// A shared-run verify span whose member was attributed a test failure.
pub const SPAN_OUTCOME_FAILED: &str = "failed";
/// A shared-run substage that ran an attribution probe.
pub const SPAN_OUTCOME_PROBE: &str = "probe";
/// Source-preparation substage under a shared-run verify span.
pub const SPAN_SUBSTAGE_PREPARE: &str = "prepare";
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
    /// Shared-run plans remembered so a later start/complete can name members.
    plans: BTreeMap<Digest, PlanAcc>,
    /// Per-member verify spans attributed from shared-run facts, keyed by run then workpiece.
    verify_spans: BTreeMap<(Digest, String), VerifySpanAcc>,
    /// Source-preparation substages keyed by plan then workpiece.
    prepare_spans: BTreeMap<(Digest, String), PrepareSpanAcc>,
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
    ended_unix_millis: Option<u64>,
    reconstructed: bool,
    agent: ResolvedModel,
    /// Whether the sealed command is a model lane. Mechanical dispatches stay
    /// on the dispatch rollup; they do not mint a seat.
    model_lane: bool,
    /// Members this one execution proves besides the workpiece it is keyed on.
    covers: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct PlanAcc {
    bloom: BloomId,
    members: Vec<(Digest, String)>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct VerifySpanAcc {
    bloom: BloomId,
    workpiece: String,
    run: Digest,
    plan: Digest,
    sequence: u64,
    started_unix_millis: Option<u64>,
    ended_unix_millis: Option<u64>,
    reconstructed: bool,
    outcome: Option<String>,
    evidence: Option<Digest>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct PrepareSpanAcc {
    bloom: BloomId,
    workpiece: String,
    plan: Digest,
    run: Option<Digest>,
    sequence: u64,
    started_unix_millis: Option<u64>,
    ended_unix_millis: Option<u64>,
    reconstructed: bool,
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
    /// Every member workpiece this one execution proves, when it proves more
    /// than the workpiece it is keyed on.
    ///
    /// A contextual shared run (ADR-0218) runs one combined gate over the
    /// composed node and is keyed on the composition, so without this list a
    /// covered member's dispatch history is empty while the run that is
    /// actually proving it is on screen under another name. Empty means the row
    /// proves only its own [`workpiece`](Self::workpiece) — a serial shared run
    /// already folds one row per member.
    pub covers: Vec<String>,
}

impl MetricDispatch {
    /// Decode one persisted rollup payload, tolerating a row written before
    /// [`covers`](Self::covers) existed.
    ///
    /// The wire format is positional and unversioned (ADR-0118), so a payload
    /// from an older fold ends where `covers` begins. The metrics cache is
    /// rebuilt from the journal whenever it falls behind, but a row persisted
    /// by the previous binary stays as it was written until that rebuild — and
    /// a hard decode failure there would empty a bloom's dispatch list rather
    /// than lose one field.
    pub fn decode_payload(bytes: &[u8]) -> Result<Self, WireError> {
        from_bytes::<Self>(bytes).or_else(|error| {
            from_bytes::<PreCoverageMetricDispatch>(bytes)
                .map(PreCoverageMetricDispatch::into_current)
                .map_err(|_| error)
        })
    }
}

/// [`MetricDispatch`] as the fold wrote it before coverage was recorded.
#[derive(Deserialize)]
struct PreCoverageMetricDispatch {
    id: String,
    bloom: BloomId,
    workpiece: String,
    stage: StageId,
    displayed: Digest,
    sequence: u64,
    recorded_unix_millis: Option<u64>,
    reconstructed: bool,
    agent: ResolvedModel,
    study: Option<Digest>,
}

impl PreCoverageMetricDispatch {
    fn into_current(self) -> MetricDispatch {
        MetricDispatch {
            id: self.id,
            bloom: self.bloom,
            workpiece: self.workpiece,
            stage: self.stage,
            displayed: self.displayed,
            sequence: self.sequence,
            recorded_unix_millis: self.recorded_unix_millis,
            reconstructed: self.reconstructed,
            agent: self.agent,
            study: self.study,
            covers: Vec::new(),
        }
    }
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

/// One stage span on a bloom timeline — a member lane, a composition tail,
/// or a substage nested under a shared-run verify.
///
/// Trailing fields are optional so a consumer that only knows the original
/// five still decodes; a missing end must not be inferred from the next start.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TimelineSpan {
    pub workpiece: String,
    pub stage: StageId,
    pub sequence: u64,
    pub started_unix_millis: Option<u64>,
    /// True when no envelope stamp was present on the dispatch that opened
    /// this span — the reader must not treat the order as wall-clock time.
    pub reconstructed: bool,
    pub ended_unix_millis: Option<u64>,
    /// Physical shared-run identity, when this span is a member verify or a
    /// substage of one.
    pub run: Option<Digest>,
    /// `integrated`, `retired`, `failed`, or `probe` when the fold has one.
    pub outcome: Option<String>,
    /// `prepare`, a gate identity, or `probe` under a verify span.
    pub substage: Option<String>,
}

/// Wall-clock receipts for one evidence artifact, used to emit gate substages.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TimelineTimings {
    pub duration_millis: u64,
    pub gates: Vec<TimelineGateTiming>,
}

/// One umbrella member's wall-clock share from `evidence.json`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TimelineGateTiming {
    pub command: String,
    pub duration_millis: u64,
    pub prepare_millis: Option<u64>,
}

/// A bloom's stage timeline. Only top-level spans count toward
/// [`TIMELINE_SPAN_CAP`]; substages ride under their kept parent.
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
    /// Bloom rollups initialize only from an admitted [`Fact::Seal`],
    /// [`Fact::GraphSeal`], or [`Fact::Supersede`] — the same set
    /// [`Snapshot::apply`](crate::reduce::Snapshot::apply) registers. A refused
    /// or duplicate seal must not mint a ghost row or overwrite the sequence
    /// that actually admitted the bloom.
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
        if let Some((spec, bloom)) = admitted_bloom(&event.fact, &decisions.outcome) {
            let acc = self.blooms.entry(bloom).or_default();
            acc.seal_sequence = sequence;
            acc.members = u64::try_from(spec.members().len()).unwrap_or(u64::MAX);
        }
        self.observe_shared_run(sequence, event, decisions, envelope);
        for effect in &decisions.effects {
            self.observe_effect(sequence, effect, configs, envelope);
        }
    }

    /// Every dispatch row, in (sequence, id) order — the persist surface.
    #[must_use]
    pub fn dispatch_rows(&self) -> Vec<MetricDispatch> {
        let mut rows: Vec<MetricDispatch> =
            self.dispatches.iter().map(|((_, key, _), acc)| self.dispatch_row(key, acc)).collect();
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

    /// Stage spans for `bloom`, capped at [`TIMELINE_SPAN_CAP`]. Only
    /// top-level spans count toward the cap — member lanes, the composition's
    /// aggregate-gate identities, and shared-run verify spans — while prepare
    /// and gate substages ride under their kept parent and never push a
    /// top-level span out.
    #[must_use]
    pub fn timeline(&self, bloom: BloomId) -> MetricsTimeline {
        self.timeline_with(bloom, |_| None)
    }

    /// [`timeline`](Self::timeline), with gate substages from evidence receipts.
    ///
    /// `timings` is keyed by the evidence digest a completed verify span
    /// retained. A miss emits the run span without gates.
    #[must_use]
    pub fn timeline_with(
        &self,
        bloom: BloomId,
        mut timings: impl FnMut(&Digest) -> Option<TimelineTimings>,
    ) -> MetricsTimeline {
        let attributed =
            |workpiece: &str| self.verify_spans.values().any(|span| span.bloom == bloom && span.workpiece == workpiece);
        let has_member_verify = self.verify_spans.values().any(|span| span.bloom == bloom);
        let mut parents: Vec<TimelineSpan> = self
            .dispatches
            .values()
            .filter(|row| row.bloom == bloom)
            .filter(|row| {
                row.stage != StageId::Verify
                    || !(attributed(&row.workpiece) || has_member_verify && row.workpiece == WorkpieceId::COMPOSITION)
            })
            .map(|row| TimelineSpan {
                workpiece: row.workpiece.clone(),
                stage: row.stage,
                sequence: row.sequence,
                started_unix_millis: row.recorded_unix_millis,
                reconstructed: row.reconstructed,
                ended_unix_millis: row.ended_unix_millis,
                run: None,
                outcome: None,
                substage: None,
            })
            .collect();
        parents.extend(self.verify_spans.values().filter(|span| span.bloom == bloom).map(|span| TimelineSpan {
            workpiece: span.workpiece.clone(),
            stage: StageId::Verify,
            sequence: span.sequence,
            started_unix_millis: span.started_unix_millis,
            reconstructed: span.reconstructed,
            ended_unix_millis: span.ended_unix_millis,
            run: Some(span.run),
            outcome: span.outcome.clone(),
            substage: None,
        }));
        parents.sort_by(|a, b| {
            a.sequence
                .cmp(&b.sequence)
                .then_with(|| a.workpiece.cmp(&b.workpiece))
                .then_with(|| a.substage.is_some().cmp(&b.substage.is_some()))
                .then_with(|| a.substage.cmp(&b.substage))
        });
        let cap = usize::try_from(TIMELINE_SPAN_CAP).unwrap_or(usize::MAX);
        let truncated = parents.len() > cap;
        parents.truncate(cap);
        let kept: Vec<(Digest, String)> =
            parents.iter().filter_map(|span| span.run.map(|run| (run, span.workpiece.clone()))).collect();
        let mut spans = parents;
        spans.extend(
            self.prepare_spans
                .values()
                .filter(|span| span.bloom == bloom)
                .filter(|span| {
                    span.run.is_none() || kept.contains(&(span.run.unwrap_or_default(), span.workpiece.clone()))
                })
                .map(|span| TimelineSpan {
                    workpiece: span.workpiece.clone(),
                    stage: StageId::Verify,
                    sequence: span.sequence,
                    started_unix_millis: span.started_unix_millis,
                    reconstructed: span.reconstructed,
                    ended_unix_millis: span.ended_unix_millis,
                    run: span.run,
                    outcome: None,
                    substage: Some(String::from(SPAN_SUBSTAGE_PREPARE)),
                }),
        );
        for span in self
            .verify_spans
            .values()
            .filter(|span| span.bloom == bloom && kept.contains(&(span.run, span.workpiece.clone())))
        {
            let Some(evidence) = span.evidence else {
                continue;
            };
            let Some(receipt) = timings(&evidence) else {
                continue;
            };
            spans.extend(gate_substages(span, &receipt));
        }
        spans.sort_by(|a, b| {
            a.sequence
                .cmp(&b.sequence)
                .then_with(|| a.workpiece.cmp(&b.workpiece))
                .then_with(|| a.substage.is_some().cmp(&b.substage.is_some()))
                .then_with(|| a.substage.cmp(&b.substage))
        });
        MetricsTimeline { bloom, spans, truncated }
    }

    fn observe_effect(&mut self, sequence: u64, effect: &Decision, configs: &ResolvedConfigs, envelope: Option<u64>) {
        let dispatched = SeatDispatch::from_effect(effect);
        if !dispatched.is_empty() {
            let covers = shared_run_coverage(effect);
            for seat in dispatched {
                self.dispatch(sequence, seat, &covers, configs, envelope);
            }
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
        covers: &[String],
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
            ended_unix_millis: None,
            reconstructed: envelope.is_none(),
            agent,
            model_lane,
            covers: covers.to_vec(),
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

    fn observe_shared_run(&mut self, sequence: u64, event: &Event, decisions: &Decisions, envelope: Option<u64>) {
        for effect in &decisions.effects {
            match effect {
                Decision::DispatchSharedRun { dispatch } => self.remember_plan(&dispatch.plan),
                Decision::DispatchSharedRunPreparation { plan } => {
                    self.remember_plan(plan);
                    self.open_prepare_spans(sequence, plan, envelope);
                }
                Decision::CancelSharedRun { plan } => self.retire_plan(*plan, envelope),
                _ => {}
            }
        }
        match &event.fact {
            Fact::ProposeSharedRun { plan, .. } => self.remember_plan(plan),
            Fact::SharedRunPrepared { plan, .. } => self.close_prepare_spans(*plan, envelope),
            Fact::SharedRunStarted { plan, run, .. } => self.open_verify_spans(sequence, *plan, *run, envelope),
            Fact::SharedRunCompleted { completion, .. } => self.close_verify_spans(completion, envelope),
            Fact::IntegrationAdvanced { bloom, head, .. } => {
                self.mark_integrated(*bloom, &head.coverage, envelope);
            }
            Fact::AttemptCompleted { bloom, workpiece, stage, .. } => {
                self.close_dispatch(*bloom, &workpiece.0, *stage, envelope);
            }
            _ => {}
        }
    }

    fn remember_plan(&mut self, plan: &SharedRunPlan) {
        self.plans.insert(
            plan.digest(),
            PlanAcc {
                bloom: plan_bloom(plan),
                members: plan
                    .requests
                    .iter()
                    .map(|request| (request.digest(), request.member.workpiece.0.clone()))
                    .collect(),
            },
        );
    }

    fn open_prepare_spans(&mut self, sequence: u64, plan: &SharedRunPlan, envelope: Option<u64>) {
        let bloom = plan_bloom(plan);
        for request in &plan.requests {
            self.prepare_spans.entry((plan.digest(), request.member.workpiece.0.clone())).or_insert_with(|| {
                PrepareSpanAcc {
                    bloom,
                    workpiece: request.member.workpiece.0.clone(),
                    plan: plan.digest(),
                    run: None,
                    sequence,
                    started_unix_millis: envelope,
                    ended_unix_millis: None,
                    reconstructed: envelope.is_none(),
                }
            });
        }
    }

    fn close_prepare_spans(&mut self, plan: Digest, envelope: Option<u64>) {
        for span in self.prepare_spans.values_mut().filter(|span| span.plan == plan && span.ended_unix_millis.is_none())
        {
            span.ended_unix_millis = envelope;
        }
    }

    fn open_verify_spans(&mut self, sequence: u64, plan: Digest, run: Digest, envelope: Option<u64>) {
        let Some(remembered) = self.plans.get(&plan).cloned() else {
            return;
        };
        for (_, member) in &remembered.members {
            if let Some(prepare) = self.prepare_spans.get_mut(&(plan, member.clone())) {
                prepare.run = Some(run);
            }
            self.verify_spans.entry((run, member.clone())).or_insert_with(|| VerifySpanAcc {
                bloom: remembered.bloom,
                workpiece: member.clone(),
                run,
                plan,
                sequence,
                started_unix_millis: envelope,
                ended_unix_millis: None,
                reconstructed: envelope.is_none(),
                outcome: None,
                evidence: None,
            });
        }
    }

    fn close_verify_spans(&mut self, completion: &SharedRunCompletion, envelope: Option<u64>) {
        let members = self.plans.get(&completion.plan).map(|plan| plan.members.clone()).unwrap_or_default();
        for outcome in &completion.outcomes {
            let Some(workpiece) = members
                .iter()
                .find_map(|(request, workpiece)| (*request == outcome.request()).then_some(workpiece.clone()))
            else {
                continue;
            };
            let Some(span) = self.verify_spans.get_mut(&(completion.run, workpiece)) else {
                continue;
            };
            span.ended_unix_millis = envelope.or(span.ended_unix_millis);
            span.evidence = outcome_evidence(outcome).or(span.evidence);
            if span.outcome.is_none() {
                span.outcome = outcome_label(outcome).map(str::to_owned);
            }
        }
        for span in
            self.verify_spans.values_mut().filter(|span| span.run == completion.run && span.ended_unix_millis.is_none())
        {
            span.ended_unix_millis = envelope;
        }
    }

    fn retire_plan(&mut self, plan: Digest, envelope: Option<u64>) {
        for span in self.verify_spans.values_mut().filter(|span| span.plan == plan && span.ended_unix_millis.is_none())
        {
            span.ended_unix_millis = envelope;
            if span.outcome.is_none() {
                span.outcome = Some(String::from(SPAN_OUTCOME_RETIRED));
            }
        }
        self.close_prepare_spans(plan, envelope);
    }

    fn mark_integrated(&mut self, bloom: BloomId, coverage: &[MemberPin], envelope: Option<u64>) {
        for pin in coverage {
            let Some(span) = self
                .verify_spans
                .values_mut()
                .filter(|span| {
                    span.bloom == bloom
                        && span.workpiece == pin.workpiece.0
                        && span.outcome.as_deref() != Some(SPAN_OUTCOME_RETIRED)
                        && span.outcome.as_deref() != Some(SPAN_OUTCOME_FAILED)
                })
                .max_by_key(|span| span.sequence)
            else {
                continue;
            };
            span.ended_unix_millis = span.ended_unix_millis.or(envelope);
            if span.outcome.as_deref() != Some(SPAN_OUTCOME_FAILED) {
                span.outcome = Some(String::from(SPAN_OUTCOME_INTEGRATED));
            }
        }
    }

    fn close_dispatch(&mut self, bloom: BloomId, workpiece: &str, stage: StageId, envelope: Option<u64>) {
        let Some(acc) = self
            .dispatches
            .values_mut()
            .filter(|row| row.bloom == bloom && row.workpiece == workpiece && row.stage == stage)
            .max_by_key(|row| row.sequence)
        else {
            return;
        };
        acc.ended_unix_millis = envelope.or(acc.ended_unix_millis);
    }

    fn day(&mut self, envelope: Option<u64>) -> &mut DayAcc {
        let reconstructed = envelope.is_none();
        let acc = self.days.entry(day_label(envelope)).or_default();
        acc.reconstructed = reconstructed;
        acc
    }

    fn dispatch_row(&self, key: &DispatchKey, acc: &DispatchAcc) -> MetricDispatch {
        let study = self
            .studies
            .iter()
            .find(|study| (study.bloom, study.subject) == (acc.bloom, acc.displayed))
            .map(|study| study.detail);
        MetricDispatch {
            id: dispatch_id(key, acc),
            bloom: acc.bloom,
            workpiece: acc.workpiece.clone(),
            stage: acc.stage,
            displayed: acc.displayed,
            sequence: acc.sequence,
            recorded_unix_millis: acc.recorded_unix_millis,
            reconstructed: acc.reconstructed,
            agent: acc.agent.clone(),
            study,
            covers: acc.covers.clone(),
        }
    }
}

/// The member workpieces one dispatched effect proves beyond its own row.
fn shared_run_coverage(effect: &Decision) -> Vec<String> {
    match effect {
        Decision::DispatchSharedRun { dispatch } => dispatch.covered_members(),
        _ => Vec::new(),
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

/// The spec a successful seal or supersede just admitted, so a graph or
/// successor bloom is counted with its members and journal sequence.
///
/// Refused and duplicate outcomes are `None`: they still advance
/// [`MetricsLedger::through_sequence`] because the row was journaled, but they
/// must not mint or clobber a bloom rollup.
fn admitted_bloom<'a>(fact: &'a Fact, outcome: &Outcome) -> Option<(&'a BloomSpec, BloomId)> {
    match (fact, outcome) {
        (Fact::Seal(spec), Outcome::Sealed(id)) => Some((spec, *id)),
        (Fact::Supersede { successor, .. }, Outcome::Superseded { successor: id, .. }) => Some((successor, *id)),
        (Fact::GraphSeal { spec, .. }, Outcome::Sealed(id) | Outcome::Superseded { successor: id, .. }) => {
            Some((spec, *id))
        }
        _ => None,
    }
}

fn outcome_label(outcome: &MemberVerifyOutcome) -> Option<&'static str> {
    match outcome {
        MemberVerifyOutcome::Failed { .. } => Some(SPAN_OUTCOME_FAILED),
        MemberVerifyOutcome::PassedIn { .. }
        | MemberVerifyOutcome::PassedStandalone { .. }
        | MemberVerifyOutcome::HostFault { .. }
        | MemberVerifyOutcome::Survived { .. }
        | MemberVerifyOutcome::Pending { .. } => None,
    }
}

fn plan_bloom(plan: &SharedRunPlan) -> BloomId {
    plan.requests.first().map_or_else(|| BloomId(Digest::default()), |request| request.bloom)
}

fn outcome_evidence(outcome: &MemberVerifyOutcome) -> Option<Digest> {
    match outcome {
        MemberVerifyOutcome::PassedStandalone { proof, .. } => Some(proof.evidence.detail),
        MemberVerifyOutcome::PassedIn { receipt, .. }
        | MemberVerifyOutcome::Failed { evidence: receipt, .. }
        | MemberVerifyOutcome::HostFault { evidence: receipt, .. } => Some(receipt.detail),
        MemberVerifyOutcome::Survived { .. } | MemberVerifyOutcome::Pending { .. } => None,
    }
}

fn gate_substages(span: &VerifySpanAcc, timings: &TimelineTimings) -> Vec<TimelineSpan> {
    let mut cursor = span.started_unix_millis.unwrap_or(0);
    let mut substages = Vec::new();
    for gate in &timings.gates {
        let duration = gate.duration_millis.saturating_add(gate.prepare_millis.unwrap_or(0));
        let end = cursor.saturating_add(duration);
        substages.push(TimelineSpan {
            workpiece: span.workpiece.clone(),
            stage: StageId::Verify,
            sequence: span.sequence,
            started_unix_millis: Some(cursor),
            reconstructed: span.reconstructed,
            ended_unix_millis: Some(end),
            run: Some(span.run),
            outcome: None,
            substage: Some(gate.command.clone()),
        });
        cursor = end;
    }
    substages
}

fn day_label(envelope: Option<u64>) -> String {
    envelope.map_or_else(|| String::from(RECONSTRUCTED_WINDOW), window_label)
}

fn dispatch_id(key: &DispatchKey, acc: &DispatchAcc) -> String {
    let workpiece = match key {
        DispatchKey::Member { .. } => acc.workpiece.as_str(),
        DispatchKey::Bloom { .. } => "",
    };
    let mut id = String::from("fold:");
    id.push_str(&acc.bloom.0.to_hex());
    id.push(':');
    id.push_str(workpiece);
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

#[cfg(test)]
mod timeline_cap_tests {
    use super::{
        DispatchAcc, MetricsLedger, PrepareSpanAcc, SPAN_SUBSTAGE_PREPARE, TIMELINE_SPAN_CAP, TimelineGateTiming,
        TimelineTimings, VerifySpanAcc,
    };
    use crate::digest::Digest;
    use crate::ids::{BloomId, StageId, WorkpieceId};
    use crate::values::{DispatchKey, Harness, ReasoningEffort, ResolvedModel};
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn digest_of(index: usize) -> Digest {
        let mut bytes = [0xA0; 32];
        bytes[0..8].copy_from_slice(&u64::try_from(index).unwrap_or(u64::MAX).to_le_bytes());
        Digest::from_bytes(bytes)
    }

    fn agent() -> ResolvedModel {
        ResolvedModel { harness: Harness::Claude, model: String::from("test"), effort: ReasoningEffort::Medium }
    }

    fn push_dispatch(ledger: &mut MetricsLedger, bloom: BloomId, workpiece: &str, stage: StageId, index: usize) {
        let sequence = u64::try_from(index).unwrap_or(u64::MAX);
        let key = DispatchKey::Member { workpiece: WorkpieceId(String::from(workpiece)), stage };
        ledger.dispatches.insert(
            (bloom, key, digest_of(index)),
            DispatchAcc {
                bloom,
                workpiece: String::from(workpiece),
                stage,
                displayed: digest_of(index),
                sequence,
                recorded_unix_millis: Some(sequence.saturating_mul(1_000)),
                ended_unix_millis: Some(sequence.saturating_mul(1_000).saturating_add(500)),
                reconstructed: false,
                agent: agent(),
                model_lane: true,
                covers: Vec::new(),
            },
        );
    }

    #[test]
    fn gate_substages_do_not_push_the_land_span_out() {
        // The plausible bug: every gate substage counted toward
        // TIMELINE_SPAN_CAP, so a bloom with a few shared runs and several
        // gates each truncated away its own latest spans — integration,
        // review, fold, land — while reporting them as a complete timeline.
        let mut ledger = MetricsLedger::default();
        let bloom = BloomId(digest_of(999_999));
        for index in 1..=9usize {
            push_dispatch(&mut ledger, bloom, "wp-a", StageId::Construct, index);
        }
        push_dispatch(&mut ledger, bloom, "wp-a", StageId::Land, 10);
        let run = digest_of(500_001);
        let plan = digest_of(500_002);
        let evidence = digest_of(500_003);
        ledger.verify_spans.insert(
            (run, String::from("wp-a")),
            VerifySpanAcc {
                bloom,
                workpiece: String::from("wp-a"),
                run,
                plan,
                sequence: 5,
                started_unix_millis: Some(5_000),
                ended_unix_millis: Some(6_000),
                reconstructed: false,
                outcome: None,
                evidence: Some(evidence),
            },
        );
        ledger.prepare_spans.insert(
            (plan, String::from("wp-a")),
            PrepareSpanAcc {
                bloom,
                workpiece: String::from("wp-a"),
                plan,
                run: Some(run),
                sequence: 4,
                started_unix_millis: Some(4_000),
                ended_unix_millis: Some(5_000),
                reconstructed: false,
            },
        );
        let gates: Vec<TimelineGateTiming> = (0..300)
            .map(|index| TimelineGateTiming {
                command: format!("verify.gate-{index:03}"),
                duration_millis: 1,
                prepare_millis: None,
            })
            .collect();
        let timings = TimelineTimings { duration_millis: 300, gates };
        let timeline = ledger.timeline_with(bloom, |detail| (*detail == evidence).then_some(timings.clone()));
        assert!(
            !timeline.truncated,
            "eleven top-level spans are under the cap however many gates they carry: {}",
            timeline.spans.len()
        );
        assert!(
            timeline.spans.iter().any(|span| span.stage == StageId::Land && span.substage.is_none()),
            "the land span survives its bloom's own gate detail: {:?}",
            timeline.spans.iter().map(|span| (span.stage, span.substage.clone())).collect::<Vec<_>>()
        );
        assert!(
            timeline.spans.iter().any(|span| span.substage.as_deref() == Some(SPAN_SUBSTAGE_PREPARE)),
            "the prepare substage rides under its kept run: {:?}",
            timeline.spans.len()
        );
        assert_eq!(
            timeline
                .spans
                .iter()
                .filter(|span| span.substage.as_deref().is_some_and(|name| name.starts_with("verify.gate-")))
                .count(),
            300,
            "no gate substage is cut: {:?}",
            timeline.spans.len()
        );
    }

    #[test]
    fn the_cap_still_binds_top_level_spans() {
        // The companion tripwire: excluding substages must not remove the cap
        // itself — a bloom with more member spans than the cap still truncates.
        let mut ledger = MetricsLedger::default();
        let bloom = BloomId(digest_of(999_998));
        let parents = usize::try_from(TIMELINE_SPAN_CAP).unwrap_or(usize::MAX).saturating_add(5);
        for index in 1..=parents {
            push_dispatch(&mut ledger, bloom, "wp-a", StageId::Construct, index);
        }
        let timeline = ledger.timeline(bloom);
        assert!(timeline.truncated, "parents past the cap still truncate");
        assert_eq!(
            timeline.spans.len(),
            usize::try_from(TIMELINE_SPAN_CAP).unwrap_or(usize::MAX),
            "only the top-level cap binds: {:?}",
            timeline.spans.len()
        );
    }
}
