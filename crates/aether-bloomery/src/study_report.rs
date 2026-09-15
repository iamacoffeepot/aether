//! The forecast-grading study report (ADR-0151 / ADR-0180, issues #3525, #3666).
//!
//! [`grade`] is a pure read over a [`Snapshot`]: per bloom, it folds the
//! admitted [`EvidenceKind::StudyRecord`] entries in the bloom's evidence log
//! into actual tokens and worker seconds, reads the retry axis off the bloom's
//! dispatch ledger, and grades all three against the sealed [`Forecast`]. It
//! resolves each study evidence's `detail` digest to its [`StudyRecord`] bytes
//! through a read-only resolver — the evidence log holds digests, not the cost
//! columns, so a snapshot-only signature could not read actuals; resolving
//! content-addressed bytes is a read, inside ADR-0151's "pure read / no side
//! effects" envelope.
//!
//! The two sources are deliberately separate (ADR-0180). Study records are
//! per-attempt artifacts, so counting them conflates independent members and
//! independent stages with retries; the ledger counts dispatches per execution
//! slot, so a clean multi-member bloom grades zero. Keeping retries off the
//! resolver also means a study artifact the resolver cannot read costs the
//! grade its token and worker-second columns and nothing else.
//!
//! It mutates nothing and opens no port: the report is a projection any
//! consumer (the REST control surface, the outward mirror, the study stage's
//! own attempt) computes from the journal-derived snapshot.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::ids::{BloomId, StageId};
use crate::reduce::Snapshot;
use crate::values::{DispatchKey, EvidenceKind, Forecast, StudyRecord, WithdrawalCause};

/// A forecast grade for one bloom: the token and worker-second actuals summed
/// from its admitted study records, the retry actual read off its dispatch ledger,
/// the sealed forecast they are graded against, and the per-axis over/under
/// delta (`actual − predicted`, so a positive delta overshot the forecast).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BloomGrade {
    /// The graded bloom.
    pub bloom: BloomId,
    /// The bloom's sealed forecast — the promise the actuals are graded against.
    pub forecast: Forecast,
    /// Actual total tokens, summed over every resolved study record that grades
    /// its evidence's subject and belongs to this bloom (uncached input +
    /// cache-write + cache-read + output; the cache-write TTL splits are already
    /// summed in cache-write and are not re-added).
    pub actual_tokens: u64,
    /// Actual worker time in whole seconds, summed over the durations of the
    /// study records that grade their evidence's subject and belong to this
    /// bloom — not the bloom's elapsed wall-clock time, which concurrent members
    /// make a different quantity.
    pub actual_worker_secs: u64,
    /// Actual retries — dispatches of an execution slot beyond its first,
    /// summed over the bloom's dispatch ledger (ADR-0180). A slot dispatched
    /// once contributes zero, and independent slots never contribute to one
    /// another: two members each constructing once is zero retries, and so is
    /// one member walking `Construct → Verify` cleanly. The axis a
    /// [`StageBinding::retry_budget`](crate::StageBinding::retry_budget) caps
    /// re-dispatch on, counted from the far side.
    ///
    /// A granted attempt counts, because the operator bought real execution. A
    /// parked attempt's release does not, because replaying the held work order
    /// mints no dispatch. Read off the journal-derived ledger rather than the
    /// study records, so no artifact the resolver cannot read can move it.
    pub actual_retries: u32,
    /// `actual_tokens − predicted_tokens` (positive overshot the forecast).
    pub token_delta: i64,
    /// `actual_worker_secs − predicted_worker_secs` (positive overshot the
    /// forecast).
    pub worker_secs_delta: i64,
    /// `actual_retries − predicted_retries` (positive overshot the forecast).
    pub retries_delta: i64,
    /// The bloom's sealed members whose work was actually judged — the
    /// denominator of the send-back rate.
    ///
    /// Counted off the sealed member list rather than off the dispatch ledger,
    /// so a member that never dispatched at all still sits in the denominator; a
    /// member withdrawn for a cause other than its own verdict is dropped from
    /// it, because its work was never judged. The synthetic composition
    /// workpiece is not a sealed member and is on neither side. See
    /// `send_back_counts`.
    #[serde(default)]
    pub graded_members: u32,
    /// How many of [`graded_members`](Self::graded_members) had their work
    /// refused — sent back for a repair lap under the `Refine` disposition, or
    /// withdrawn on their own red verdict under `Eject`. See
    /// `send_back_counts` for how each is recognized.
    ///
    /// The send-back rate is this over `graded_members`. Both halves are counts
    /// rather than a ratio because a [`BloomGrade`] is `Eq` and
    /// content-addressable, which a float is not — and because the numerator and
    /// the denominator answer different questions on their own.
    #[serde(default)]
    pub sent_back_members: u32,
}

/// The signed over/under delta `actual − predicted`, computed on the unsigned
/// magnitudes so no `u64 → i64` cast can wrap: a positive result overshot the
/// forecast, a negative one came in under. The saturating fallbacks are
/// unreachable for real token/second counts (they never approach `i64::MAX`) and
/// keep the function total.
fn delta(actual: u64, predicted: u64) -> i64 {
    if actual >= predicted {
        i64::try_from(actual - predicted).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(predicted - actual).unwrap_or(i64::MAX)
    }
}

/// `(graded_members, sent_back_members)` for one bloom — the two halves of its
/// send-back rate.
///
/// Both halves walk the sealed member list, so the composition workpiece is on
/// neither side: it is not a member, and a seam repair is not a member's work
/// coming back. The denominator drops a member withdrawn for a cause other than
/// its own verdict — an operator's call, or an ancestor's withdrawal stranding
/// it — because that member's work was never judged, and counting it would read
/// a cancelled bloom as a bloom whose work was good.
///
/// The numerator has to recognize both sealed dispositions of a red verify
/// (`RedVerify`), because they leave different traces and a fold that knew only
/// one would report zero for every bloom sealed on the other:
///
/// - Under `Refine` the member re-enters that stage, which it reaches by no
///   other road, so its slot appears in the dispatch ledger keyed at `Refine`.
///   The ledger is the right witness rather than the member's own cursor: it is
///   journal-derived, nothing inside a bloom's life clears it, and a grant or a
///   stage advance rewinds the cursor (ADR-0180).
/// - Under `Eject` — the default — there is no repair lap at all: the member
///   withdraws carrying `WithdrawalCause::Verify` (ADR-0218 §Amendment: low
///   tolerance).
///
/// A member counts once either way, however many laps it then spends.
/// [`actual_retries`](BloomGrade::actual_retries) already measures how often
/// work came back; this measures how much of it did.
fn send_back_counts(record: &crate::BloomRecord) -> (u32, u32) {
    let judged = || {
        record.spec.members().iter().filter(|member| {
            record.withdrawn.get(&member.workpiece).is_none_or(|withdrawal| withdrawal.cause == WithdrawalCause::Verify)
        })
    };
    let sent_back = judged()
        .filter(|member| {
            record
                .dispatches
                .contains_key(&DispatchKey::Member { workpiece: member.workpiece.clone(), stage: StageId::Refine })
                || record
                    .withdrawn
                    .get(&member.workpiece)
                    .is_some_and(|withdrawal| withdrawal.cause == WithdrawalCause::Verify)
        })
        .count();

    (u32::try_from(judged().count()).unwrap_or(u32::MAX), u32::try_from(sent_back).unwrap_or(u32::MAX))
}

/// The per-bloom forecast grade for every bloom the snapshot knows.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct StudyReport {
    /// One grade per bloom, in the snapshot's bloom-id order.
    pub blooms: Vec<BloomGrade>,
}

/// Grade every bloom's actuals against its sealed forecast (ADR-0151,
/// ADR-0180). Pure: reads the snapshot and resolves study-record bytes through
/// `source`, mutates nothing.
///
/// `source` resolves a study evidence's `detail` digest to its [`StudyRecord`]
/// bytes, returning `None` when the artifact is unavailable — an unresolvable
/// record contributes no cost or time and cannot touch the retry axis, which is
/// the dispatch ledger's. A resolved record that does not grade its evidence's
/// subject, or that names a different bloom, takes the same posture: it
/// contributes no cost or time either. A bloom with no study evidence grades to
/// zero cost and time; its retries are whatever it dispatched.
#[must_use]
pub fn grade(snapshot: &Snapshot, source: impl Fn(&Digest) -> Option<StudyRecord>) -> StudyReport {
    let blooms = snapshot
        .blooms
        .iter()
        .map(|(id, record)| {
            let forecast = record.spec.forecast();
            let mut actual_tokens = 0u64;
            let mut actual_millis = 0u64;
            for evidence in &record.evidence {
                if evidence.kind != EvidenceKind::StudyRecord {
                    continue;
                }
                // A resolved record that does not grade this evidence's subject, or
                // that names a different bloom, is skipped rather than summed:
                // ADR-0151's pure read has no refusal channel to reject a mismatched
                // artifact, and ADR-0180 already spends an unreadable record's cost
                // and time columns and nothing else, so an unbound record takes the
                // same posture as an unresolvable one.
                if let Some(study) = source(&evidence.detail)
                    && study.grades(&evidence.subject)
                    && study.bloom == *id
                {
                    let cost = &study.cost;
                    actual_tokens = actual_tokens
                        .saturating_add(cost.input_tokens)
                        .saturating_add(cost.cache_write_tokens)
                        .saturating_add(cost.cache_read_tokens)
                        .saturating_add(cost.output_tokens);
                    actual_millis = actual_millis.saturating_add(cost.duration_millis);
                }
            }
            let actual_worker_secs = actual_millis / 1000;
            // One retry is one dispatch of a slot beyond its first, so each
            // ledger entry contributes its count minus one and a slot dispatched
            // once contributes nothing (ADR-0180).
            let actual_retries =
                record.dispatches.values().fold(0u32, |retries, count| retries.saturating_add(count.saturating_sub(1)));
            let (graded_members, sent_back_members) = send_back_counts(record);
            BloomGrade {
                bloom: *id,
                forecast,
                actual_tokens,
                actual_worker_secs,
                actual_retries,
                token_delta: delta(actual_tokens, forecast.predicted_tokens),
                worker_secs_delta: delta(actual_worker_secs, forecast.predicted_worker_secs),
                retries_delta: i64::from(actual_retries) - i64::from(forecast.predicted_retries),
                graded_members,
                sent_back_members,
            }
        })
        .collect();
    StudyReport { blooms }
}
