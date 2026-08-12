//! The forecast-grading study report (ADR-0151, ADR-0180): `grade` is a pure
//! read over a snapshot that sums a bloom's admitted study records into actual
//! tokens and worker seconds, reads its retry actual off the journal-derived
//! dispatch ledger, and grades all three against the sealed forecast.
//!
//! These cases exercise the owned fold logic — the token/duration summation, the
//! ledger's beyond-the-first retry sum, and the per-axis over/under deltas — not
//! the serde/wire machinery the value types derive. Every retry case drives real
//! dispatches through `reduce` rather than splicing state, because what the
//! ledger counts is exactly what the reducer decided to dispatch.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeMap;

use aether_bloomery::{
    BloomId, Digest, Evidence, EvidenceKind, Fact, Forecast, Membership, Snapshot, StageId, StudyCost, StudyRecord,
    grade,
};
use common::{digest, draft, event, membership, step, workpiece};

/// A study record carrying the graded cost columns (the `bloom` / `subject`
/// fields are immaterial to the grade — it reads only `cost`).
fn study(cost: StudyCost) -> StudyRecord {
    StudyRecord { bloom: BloomId(digest(0)), subject: digest(0), cost }
}

/// A cost with the token columns and duration the grade sums; the cache-write
/// TTL splits are set so a double-count would be visible.
fn cost(input: u64, cache_write: u64, cache_read: u64, output: u64, duration_millis: u64) -> StudyCost {
    StudyCost {
        cost_micro_usd: 0,
        turns: 0,
        duration_millis,
        input_tokens: input,
        cache_write_tokens: cache_write,
        cache_write_1h_tokens: cache_write,
        cache_write_5m_tokens: 0,
        cache_read_tokens: cache_read,
        output_tokens: output,
    }
}

/// Seal a bloom on a fresh snapshot carrying the given forecast. The seal
/// dispatches every member at the entry stage, so the returned snapshot already
/// holds one ledger entry per member.
fn sealed_with(forecast: Forecast, members: Vec<Membership>) -> (Snapshot, BloomId) {
    let mut sealing = draft(1, members);
    sealing.forecast = forecast;
    let spec = sealing.seal();
    let bloom = spec.id();
    let (snapshot, _) = step(&Snapshot::new(digest(1)), &event("seal", Fact::Seal(spec)));
    (snapshot, bloom)
}

/// Complete one member's attempt at `stage` — the fact that advances or
/// re-dispatches the member, and therefore the fact that moves its ledger slot.
fn completed(bloom: BloomId, member: &str, stage: StageId, passed: bool, detail: u8) -> Fact {
    Fact::AttemptCompleted {
        bloom,
        workpiece: workpiece(member),
        stage,
        passed,
        evidence: Evidence { subject: digest(70), kind: EvidenceKind::VerificationResult, detail: digest(detail) },
        candidate: None,
    }
}

/// Admit one study record against the bloom, naming its artifact by digest.
fn study_admitted(bloom: BloomId, subject: u8, detail: u8) -> Fact {
    Fact::AdmitEvidence {
        bloom,
        evidence: Evidence { subject: digest(subject), kind: EvidenceKind::StudyRecord, detail: digest(detail) },
    }
}

// ADR-0180 — the retry axis counts dispatches per execution slot, so a bloom
// whose two members each construct once cleanly grades zero retries even though
// it produced a study record per member and walked each member on to Verify. The
// token and worker-second axes still sum every member's record.
//
// Tripwire for the defect this replaced: counting study records bloom-wide and
// subtracting one reported one phantom retry here, and the same fold reported a
// phantom retry for any single member that walked two stages.
#[test]
fn independent_members_and_stages_are_not_retries() {
    // The forecast overshoots on tokens and undershoots on time and retries, so
    // both delta directions are exercised.
    let (snapshot, bloom) = sealed_with(
        Forecast { predicted_tokens: 500, predicted_worker_secs: 3, predicted_retries: 2 },
        vec![membership("wp-a", 10), membership("wp-b", 11)],
    );

    let mut records = BTreeMap::new();
    records.insert(digest(80), study(cost(100, 50, 200, 30, 1500))); // 380 tokens, 1500 ms
    records.insert(digest(81), study(cost(200, 0, 100, 20, 800))); //   320 tokens,  800 ms

    let mut snapshot = snapshot;
    for (member, index) in [("wp-a", 0u8), ("wp-b", 1u8)] {
        snapshot = step(
            &snapshot,
            &event(&format!("construct-{member}"), completed(bloom, member, StageId::Construct, true, 90 + index)),
        )
        .0;
        snapshot = step(&snapshot, &event(&format!("study-{member}"), study_admitted(bloom, 70 + index, 80 + index))).0;
    }

    let report = grade(&snapshot, |d: &Digest| records.get(d).copied());
    assert_eq!(report.blooms.len(), 1);
    let graded = report.blooms[0];
    assert_eq!(graded.bloom, bloom);
    assert_eq!(graded.actual_tokens, 700, "380 + 320 tokens, TTL splits not double-counted");
    assert_eq!(graded.actual_worker_secs, 2, "2300 ms floors to 2 whole seconds");
    assert_eq!(graded.actual_retries, 0, "four slots dispatched once each is no retry");
    assert_eq!(graded.token_delta, 200, "700 actual over 500 predicted");
    assert_eq!(graded.worker_secs_delta, -1, "2 actual under 3 predicted");
    assert_eq!(graded.retries_delta, -2, "0 actual under 2 predicted");
}

// ADR-0180 — a member re-dispatched at one stage spends exactly one retry: the
// slot's second dispatch, and nothing else in the bloom. `Construct`'s budget is
// 2, so one failing completion re-dispatches the same stage in place.
#[test]
fn a_re_dispatched_stage_grades_exactly_one_retry() {
    let (snapshot, bloom) = sealed_with(Forecast::default(), vec![membership("wp", 10)]);

    let (snapshot, _) =
        step(&snapshot, &event("construct-fail", completed(bloom, "wp", StageId::Construct, false, 90)));

    let graded = grade(&snapshot, |_: &Digest| None).blooms[0];
    assert_eq!(graded.actual_retries, 1, "one slot dispatched twice is one retry");
    assert_eq!(graded.retries_delta, 1, "1 actual over 0 predicted");
}

// ADR-0180 — the retry axis is the ledger's, so a study artifact the resolver
// cannot read costs the grade its token and worker-second columns and nothing
// else.
//
// Tripwire: reading retries back out of the evidence log. Three unresolvable
// records against a member that was dispatched twice would report two retries
// under the old fold, and the count would move again with a fourth record that
// no dispatch caused.
#[test]
fn unresolvable_study_artifacts_leave_the_ledger_retries_standing() {
    let (snapshot, bloom) = sealed_with(Forecast::default(), vec![membership("wp", 10)]);

    let (mut snapshot, _) =
        step(&snapshot, &event("construct-fail", completed(bloom, "wp", StageId::Construct, false, 90)));
    for index in 0..3u8 {
        snapshot = step(&snapshot, &event(&format!("study-{index}"), study_admitted(bloom, 70 + index, 80 + index))).0;
    }

    let graded = grade(&snapshot, |_: &Digest| None).blooms[0];
    assert_eq!(
        (graded.actual_tokens, graded.actual_worker_secs),
        (0, 0),
        "nothing resolved, so no tokens or worker seconds"
    );
    assert_eq!(graded.actual_retries, 1, "the one re-dispatch stands, independent of how many records were logged");
}
