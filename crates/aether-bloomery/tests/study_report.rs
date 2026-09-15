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
use std::iter::once;

use aether_bloomery::{
    BloomDraft, BloomId, CandidateRef, CoordinationPolicy, Digest, DispatchKey, Event, Evidence, EvidenceKind, Fact,
    Forecast, Membership, RedVerify, ResolvedConfigs, Snapshot, SpendWindow, StageId, StudyCost, StudyRecord,
    VerifyFailure, WithdrawalCause, config_address, grade, reduce,
};
use aether_data::Kind;
use aether_data::wire::to_vec;
use common::{compiled_manifest, digest, draft, event, membership, step, workpiece};

/// A study record carrying the graded cost columns, bound to the given bloom
/// and subject.
fn study(bloom: BloomId, subject: Digest, cost: StudyCost) -> StudyRecord {
    StudyRecord { bloom, subject, cost }
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
    let (snapshot, _) = step(&Snapshot::new(digest(1)).with_green_base(digest(1)), &event("seal", Fact::Seal(spec)));
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
    records.insert(digest(80), study(bloom, digest(70), cost(100, 50, 200, 30, 1500))); // 380 tokens, 1500 ms
    records.insert(digest(81), study(bloom, digest(71), cost(200, 0, 100, 20, 800))); //   320 tokens,  800 ms

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

// ADR-0151 / ADR-0180 — a resolved study record that does not grade this
// evidence's subject, or that names a different bloom, must contribute nothing:
// the fold used to sum a record's cost as soon as `source` returned `Some`,
// with no check that the resolved bytes were actually bound to the evidence
// that named them. An artifact store keyed only by content digest can return
// bytes for an unrelated attempt or an unrelated bloom under a colliding or
// stale `detail` digest, and that must take the same zero-contribution posture
// as an unresolvable record, not be silently summed in.
//
// Tripwire for the defect this closes: the two failure modes are pinned
// separately (wrong `subject`, wrong `bloom`), each with a cost two orders of
// magnitude past the honest record's, so either one leaking through the guard
// is unmistakable in `actual_tokens` / `actual_worker_secs`.
#[test]
fn an_unbound_study_record_is_not_summed_into_the_grade() {
    let (snapshot, bloom) = sealed_with(Forecast::default(), vec![membership("wp", 10)]);

    let mut snapshot = snapshot;
    for (subject, detail) in [(70u8, 80u8), (71, 81), (72, 82)] {
        snapshot = step(&snapshot, &event(&format!("study-{detail}"), study_admitted(bloom, subject, detail))).0;
    }

    let mut records = BTreeMap::new();
    // Honest: correct bloom, correct subject, a small cost.
    records.insert(digest(80), study(bloom, digest(70), cost(100, 50, 200, 30, 1200)));
    // Wrong subject: correct bloom, but bound to an unrelated attempt digest.
    records.insert(digest(81), study(bloom, digest(200), cost(10_000, 10_000, 10_000, 10_000, 60_000)));
    // Wrong bloom: correct subject, but bound to an unrelated bloom.
    records.insert(digest(82), study(BloomId(digest(201)), digest(72), cost(10_000, 10_000, 10_000, 10_000, 60_000)));

    let graded = grade(&snapshot, |d: &Digest| records.get(d).copied()).blooms[0];
    assert_eq!(
        graded.actual_tokens, 380,
        "only the honest record's 380 tokens count; the subject- and bloom-mismatched records must not be summed"
    );
    assert_eq!(
        graded.actual_worker_secs, 1,
        "only the honest record's 1200ms counts; the unbound records' 60s each must not be summed"
    );
    assert_eq!(graded.actual_retries, 0, "the ledger's retry axis is untouched by the resolver's records");
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

/// Complete one member's attempt at `stage`, capturing `candidate` — the
/// terminal-Verify cursor a failing verdict needs a tree to bind against.
fn captured(bloom: BloomId, member: &str, stage: StageId, tree: u8, detail: u8) -> Fact {
    Fact::AttemptCompleted {
        bloom,
        workpiece: workpiece(member),
        stage,
        passed: true,
        evidence: Evidence { subject: digest(tree), kind: EvidenceKind::VerificationResult, detail: digest(detail) },
        candidate: Some(CandidateRef { tree: digest(tree), checkout: digest(tree + 1) }),
    }
}

/// A failing terminal-Verify verdict over `member`'s captured tree.
fn verify_failed(bloom: BloomId, member: &str, tree: u8, detail: u8) -> Fact {
    Fact::VerifyFailed {
        bloom,
        workpiece: workpiece(member),
        evidence: Evidence { subject: digest(tree), kind: EvidenceKind::VerificationResult, detail: digest(detail) },
        failed_verifiers: once(VerifyFailure::Clippy).collect(),
        findings: String::new(),
    }
}

/// Seal a bloom whose sealed [`CoordinationPolicy`] repairs a red verify rather
/// than ejecting on it, with the [`ResolvedConfigs`] that produce it.
///
/// `Eject` is the default disposition (ADR-0218 §Amendment: low tolerance), so
/// a bloom that re-enters `Refine` at all is one that sealed the policy — which
/// means the resolved registry has to carry it, and `step` cannot: it resolves
/// the compiled vocabulary and nothing else.
fn sealed_refining(members: Vec<Membership>) -> (Snapshot, BloomId, ResolvedConfigs) {
    let (mut configs, mut resolved) = compiled_manifest();
    let policy = CoordinationPolicy {
        red_verify: RedVerify::Refine,
        max_run_members: 1,
        max_serial_requests: 1,
        host_class: String::from("study-report-test"),
        ..CoordinationPolicy::default()
    };
    let bytes = to_vec(&policy).expect("the policy encodes");
    let address = config_address(CoordinationPolicy::NAME, &bytes);
    configs.insert::<CoordinationPolicy>(address);
    resolved.insert(address, CoordinationPolicy::NAME, bytes, None);

    let spec = BloomDraft { proposals: members, base: digest(1), configs, ..BloomDraft::default() }.seal();
    let bloom = spec.id();
    let snapshot =
        step_with(&Snapshot::new(digest(1)).with_green_base(digest(1)), &event("seal", Fact::Seal(spec)), &resolved);
    (snapshot, bloom, resolved)
}

/// `step`, over a registry this test sealed rather than the compiled one.
fn step_with(snapshot: &Snapshot, event: &Event, resolved: &ResolvedConfigs) -> Snapshot {
    let decisions = reduce(snapshot, event, resolved, &SpendWindow::default());
    snapshot.apply(event, &decisions, resolved)
}

// The send-back axis (#6068) under the default `Eject` disposition: a member
// whose terminal Verify comes back red withdraws carrying
// `WithdrawalCause::Verify`, and that withdrawal is what marks it as sent back.
// The member that captured a candidate and was never refused is not.
//
// The denominator is the members whose work was *judged*, which is why the
// ejected member stays in it: dropping every withdrawn member — the filter the
// three resolution folds in `reduce::integrate` use, and the obvious one to
// copy — makes the numerator and the denominator move together and reports this
// bloom as 0 of 1 clean.
#[test]
fn an_ejected_member_is_counted_as_sent_back_and_stays_in_the_denominator() {
    let (snapshot, bloom) = sealed_with(Forecast::default(), vec![membership("wp-a", 10), membership("wp-b", 11)]);

    let mut snapshot = step(&snapshot, &event("construct-a", captured(bloom, "wp-a", StageId::Construct, 100, 90))).0;
    snapshot = step(&snapshot, &event("verify-a-red", verify_failed(bloom, "wp-a", 100, 91))).0;
    snapshot = step(&snapshot, &event("construct-b", captured(bloom, "wp-b", StageId::Construct, 110, 94))).0;

    let record = snapshot.blooms.get(&bloom).expect("the sealed bloom is in the snapshot");
    assert_eq!(
        record.withdrawn.get(&workpiece("wp-a")).map(|withdrawal| withdrawal.cause.clone()),
        Some(WithdrawalCause::Verify),
        "the default disposition ejects on red, so this is the trace the numerator has to read",
    );

    let graded = grade(&snapshot, |_: &Digest| None).blooms[0];
    assert_eq!(graded.graded_members, 2, "the ejected member's work was judged, so it stays in the denominator");
    assert_eq!(graded.sent_back_members, 1, "the ejection is the send-back; the untouched member is not one");
}

// The same axis under the `Refine` disposition, which leaves an entirely
// different trace: no withdrawal at all, and a `Refine` slot in the dispatch
// ledger instead.
//
// Tripwire for the shape this axis must not take. `actual_retries`, computed
// three lines above it in the same fold, sums the ledger's values — and summing
// them here reports 2 for the one member that spent two repair laps. The second
// red verify is in the fixture for exactly that reason: the numerator counts
// members whose work came back, never the laps they then spent, which is the
// axis `actual_retries` already owns.
#[test]
fn a_refined_member_counts_once_however_many_repair_laps_it_spends() {
    let (snapshot, bloom, resolved) = sealed_refining(vec![membership("wp-a", 10), membership("wp-b", 11)]);

    let mut snapshot =
        step_with(&snapshot, &event("construct-a", captured(bloom, "wp-a", StageId::Construct, 100, 90)), &resolved);
    snapshot = step_with(&snapshot, &event("verify-a-red", verify_failed(bloom, "wp-a", 100, 91)), &resolved);
    snapshot = step_with(&snapshot, &event("refine-a", captured(bloom, "wp-a", StageId::Refine, 102, 92)), &resolved);
    snapshot = step_with(&snapshot, &event("verify-a-red-2", verify_failed(bloom, "wp-a", 102, 93)), &resolved);
    snapshot =
        step_with(&snapshot, &event("construct-b", captured(bloom, "wp-b", StageId::Construct, 110, 94)), &resolved);

    let record = snapshot.blooms.get(&bloom).expect("the sealed bloom is in the snapshot");
    assert!(record.withdrawn.is_empty(), "the Refine disposition repairs rather than ejects, so nothing withdrew");
    assert!(
        record
            .dispatches
            .get(&DispatchKey::Member { workpiece: workpiece("wp-a"), stage: StageId::Refine })
            .is_some_and(|laps| *laps > 1),
        "the fixture has to spend more than one repair lap or it cannot tell a member count from a lap count: {:?}",
        record.dispatches,
    );

    let graded = grade(&snapshot, |_: &Digest| None).blooms[0];
    assert_eq!(graded.graded_members, 2, "both members are live and judged");
    assert_eq!(
        graded.sent_back_members, 1,
        "one member's work came back, twice over; the numerator counts members refused, not the laps they spent",
    );
}
