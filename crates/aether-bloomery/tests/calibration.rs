//! The capability ledger (ADR-0184): a pure fold over admitted `(event,
//! decisions)` pairs into per-`(harness, model, effort) × stage` counts.
//!
//! These cases exercise the fold this crate owns — which agent a dispatch is
//! keyed under, which lane a failing verdict is charged to, and how a study
//! record reaches the stage that spent it — driven through `reduce` rather than
//! spliced, because what the ledger counts is exactly what the reducer decided.

mod common;

use std::collections::BTreeMap;

use aether_data::Kind;
use aether_data::wire::to_vec;

use aether_bloomery::{
    AgentSelection, BloomId, CalibrationLedger, CandidateRef, CapabilityCell, CapabilityLedger, ConfigKind, Decision,
    Decisions, Digest, Event, Evidence, EvidenceKind, Fact, Harness, ModelOverride, Outcome, ReasoningEffort,
    ResolvedConfigs, SealError, Snapshot, SpendWindow, StageCatalog, StageId, StageOverride, StudyCost, StudyRecord,
    Unproducible, VerifyFailure, VerifyFailureSet, reduce,
};
use common::{claim, digest, draft_with_member_override, event, membership, workpiece};

/// The member every case drives, and the digests it runs against.
const MEMBER: &str = "wp-a";
/// The member's sealed scope revision — the digest a Construct dispatch
/// displays, and so the join key of its study record.
const REVISION: u8 = 10;
/// The candidate tree the member's Construct captures — the digest every later
/// lane displays.
const TREE: u8 = 100;

/// A snapshot and the ledger folded beside it, both driven by the same admitted
/// events — the pairing the control core holds.
struct Journal {
    snapshot: Snapshot,
    ledger: CalibrationLedger,
    configs: ResolvedConfigs,
    bloom: BloomId,
}

impl Journal {
    /// Seal a one-member bloom whose member seals `override_`, and fold the seal.
    fn sealed(override_: &ModelOverride) -> Self {
        let (draft, configs) = draft_with_member_override(1, membership(MEMBER, REVISION), override_);
        let spec = draft.seal();
        let bloom = spec.id();
        let mut journal = Self {
            snapshot: Snapshot::new(digest(1)).with_green_base(digest(1)),
            ledger: CalibrationLedger::default(),
            configs,
            bloom,
        };
        journal.admit(&event("seal", Fact::Seal(spec)));
        journal
    }

    /// Reduce one event, fold it into both projections, and hand back what was
    /// decided.
    fn admit(&mut self, event: &Event) -> Decisions {
        let decisions = reduce(&self.snapshot, event, &self.configs, &SpendWindow::default());
        self.ledger.observe(event, &decisions, &self.configs);
        self.snapshot = self.snapshot.apply(event, &decisions, &self.configs);
        decisions
    }

    /// Complete the member's attempt at `stage`, capturing `candidate`.
    fn completed(&mut self, key: &str, stage: StageId, candidate: Option<CandidateRef>) -> Decisions {
        self.admit(&event(
            key,
            Fact::AttemptCompleted {
                bloom: self.bloom,
                workpiece: workpiece(MEMBER),
                stage,
                passed: true,
                evidence: Evidence {
                    subject: digest(TREE),
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(90),
                },
                candidate,
            },
        ))
    }

    /// Return a failing terminal-Verify verdict naming `failed`.
    fn verify_failed(&mut self, key: &str, failed: &[VerifyFailure]) -> Decisions {
        self.admit(&event(
            key,
            Fact::VerifyFailed {
                bloom: self.bloom,
                workpiece: workpiece(MEMBER),
                evidence: Evidence {
                    subject: digest(TREE),
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(91),
                },
                failed_verifiers: failed.iter().copied().collect(),
            },
        ))
    }

    /// Admit one study evidence naming `subject`'s attempt and the artifact
    /// `detail` addresses.
    fn study(&mut self, key: &str, subject: u8, detail: u8) -> Decisions {
        self.admit(&event(
            key,
            Fact::AdmitEvidence {
                bloom: self.bloom,
                evidence: Evidence {
                    subject: digest(subject),
                    kind: EvidenceKind::StudyRecord,
                    detail: digest(detail),
                },
            },
        ))
    }
}

/// An override that runs the member's lanes under one agent and escalates its
/// Refine re-entry onto another — the motivating shape (#4601), and the one a
/// cell key has to keep apart.
fn escalating() -> ModelOverride {
    ModelOverride {
        agent: Some(AgentSelection { harness: Harness::Claude, model: "claude-opus-5".into() }),
        reasoning_effort: None,
        per_stage: BTreeMap::from([(
            StageId::Refine,
            StageOverride {
                agent: Some(AgentSelection { harness: Harness::Grok, model: "grok-build-1".into() }),
                reasoning_effort: Some(ReasoningEffort::Max),
            },
        )]),
    }
}

/// A study record costing `cost_micro_usd` and taking `duration_millis`, bound
/// to the attempt it grades.
fn record(bloom: BloomId, subject: u8, cost_micro_usd: u64, duration_millis: u64) -> StudyRecord {
    StudyRecord {
        bloom,
        subject: digest(subject),
        cost: StudyCost { cost_micro_usd, duration_millis, ..StudyCost::default() },
    }
}

/// The cell for `stage`, or `None` when the ledger measured none.
fn cell(ledger: &CapabilityLedger, stage: StageId) -> Option<&CapabilityCell> {
    ledger.cells.iter().find(|cell| cell.stage == stage)
}

/// How many verdicts a cell charged to one verifier identity.
fn verdicts(cell: &CapabilityCell, verifier: VerifyFailure) -> u64 {
    cell.failures.iter().find(|failures| failures.verifier == verifier).map_or(0, |failures| failures.verdicts)
}

/// Walk the member Construct → Verify-fails → Refine, one lap per verdict, and
/// hand back the journal.
fn walked(verdicts: &[&[VerifyFailure]]) -> Journal {
    let mut journal = Journal::sealed(&escalating());
    let captured = CandidateRef { tree: digest(TREE), checkout: digest(TREE + 1) };
    journal.completed("construct", StageId::Construct, Some(captured));
    for (lap, failed) in verdicts.iter().enumerate() {
        journal.verify_failed(&format!("verify-failed-{lap}"), failed);
        journal.completed(&format!("refine-{lap}"), StageId::Refine, None);
    }
    journal
}

// Tripwire (ADR-0184, the work-order pin): the cell key is *recomputed* from the
// sealed override and the dispatch's catalog profile, never read off
// `Transformation.model`. Every journaled dispatch carries that field as `None`
// — the reducer authors it so and the host fills it downstream of the journal —
// so a fold that joined on it would produce an empty table that still passes a
// naive "the ledger is a projection" test. Asserting the `None` beside the cells
// keeps the premise pinned too: the day a dispatch starts carrying a model, this
// says so rather than silently agreeing.
#[test]
fn the_agent_is_recomputed_from_the_sealed_override_not_the_dispatchs_empty_model() {
    let mut journal = Journal::sealed(&escalating());
    let captured = CandidateRef { tree: digest(TREE), checkout: digest(TREE + 1) };
    journal.completed("construct", StageId::Construct, Some(captured));
    let refine = journal.verify_failed("verify-failed", &[VerifyFailure::Clippy]);

    for decision in &refine.effects {
        if let Decision::DispatchAttempt { transformation, .. } = decision {
            assert!(transformation.model.is_none(), "the journal never carries a resolved model");
        }
    }

    let ledger = journal.ledger.report(|_| None);
    let construct = cell(&ledger, StageId::Construct).expect("the Construct lane is measured");
    let repair = cell(&ledger, StageId::Refine).expect("the Refine lane is measured");

    assert_eq!(
        (construct.agent.harness, construct.agent.model.as_str(), construct.agent.effort),
        (Harness::Claude, "claude-opus-5", ReasoningEffort::High),
        "Construct takes the member-wide agent over the catalog's, at the catalog's effort",
    );
    assert_eq!(
        (repair.agent.harness, repair.agent.model.as_str(), repair.agent.effort),
        (Harness::Grok, "grok-build-1", ReasoningEffort::Max),
        "the Refine entry escalates onto its own agent, so the two lanes are two cells",
    );
}

// Tripwire (ADR-0178 / ADR-0181): a failing terminal Verify is charged to the
// model lane that *wrote* the refused candidate, per verdict.
//
// Two ways to get this wrong, both of which read as plausible. Charging the
// Verify stage puts the failure mix on a compiler — and since a mechanical lane
// is no cell at all, the whole column silently disappears. Reading the member
// cursor's `seen_verify_failures` instead reports a union, so a lane that failed
// `verify.clippy` on every lap of a bloom looks exactly like one that failed it
// once, which is the difference the suppression column exists to show.
#[test]
fn a_failing_verify_charges_verdicts_to_the_lane_that_wrote_the_candidate() {
    let ledger = walked(&[
        &[VerifyFailure::Clippy],
        &[VerifyFailure::Clippy, VerifyFailure::Suppress],
        &[VerifyFailure::Clippy],
    ])
    .ledger
    .report(|_| None);

    let construct = cell(&ledger, StageId::Construct).expect("the Construct lane is measured");
    let repair = cell(&ledger, StageId::Refine).expect("the Refine lane is measured");

    assert_eq!(verdicts(construct, VerifyFailure::Clippy), 1, "the first verdict refused what Construct wrote");
    assert_eq!(verdicts(repair, VerifyFailure::Clippy), 2, "the two later verdicts refused what Refine wrote");
    assert_eq!(verdicts(repair, VerifyFailure::Suppress), 1, "suppression pressure is its own count, not a flag");
    assert_eq!(verdicts(construct, VerifyFailure::Suppress), 0, "and it is not smeared across the member's lanes");
    assert!(cell(&ledger, StageId::Verify).is_none(), "the mechanical fan-out ran a compiler, so it calibrates nobody");
}

#[test]
fn a_containment_refusal_lands_in_the_ledger() {
    // ADR-0209: without an arm for the appended fact the ninth identity is
    // never counted — which is the entire point of journaling it.
    let mut journal = Journal::sealed(&escalating());
    let captured = CandidateRef { tree: digest(TREE), checkout: digest(TREE + 1) };
    journal.completed("construct", StageId::Construct, Some(captured));
    journal.admit(&event(
        "containment",
        Fact::ContainmentRefused {
            bloom: journal.bloom,
            workpiece: workpiece(MEMBER),
            evidence: Evidence { subject: digest(TREE), kind: EvidenceKind::VerificationResult, detail: digest(91) },
            failed_verifiers: VerifyFailureSet::one(VerifyFailure::Containment),
            violating_paths: vec!["crates/other/src/lib.rs".into()],
        },
    ));

    let ledger = journal.ledger.report(|_| None);
    let construct = cell(&ledger, StageId::Construct).expect("the Construct lane is measured");
    assert_eq!(verdicts(construct, VerifyFailure::Containment), 1);
}

// Tripwire (ADR-0184): a study record reaches the stage that spent it, through
// the digest that attempt displayed — and an artifact that will not resolve
// costs its cell the cost, time, and sample columns and nothing else.
//
// The join is the fragile part: Construct displays the member's scope revision
// and every later lane displays its captured tree, so a fold that summed a
// bloom's records into all of its cells (or into whichever it saw first) would
// report the repair's spend against the clean first draft. The sample count is
// what keeps that honest — a cell whose artifacts did not resolve says so
// instead of reporting a cheap agent.
#[test]
fn a_study_record_reaches_the_stage_that_spent_it_and_an_unreadable_one_costs_only_its_columns() {
    let mut journal = walked(&[&[VerifyFailure::Clippy]]);
    let bloom = journal.bloom;
    journal.study("study-construct", REVISION, 80);
    journal.study("study-refine", TREE, 81);
    journal.study("study-refine-unreadable", TREE, 82);
    journal.admit(&event("integrate", Fact::Integrate { bloom, claim: claim(MEMBER, REVISION, TREE) }));

    let records = BTreeMap::from([
        (digest(80), record(bloom, REVISION, 5_000, 2_500)),
        (digest(81), record(bloom, TREE, 9_000, 4_000)),
    ]);
    let ledger = journal.ledger.report(|detail: &Digest| records.get(detail).copied());

    let construct = cell(&ledger, StageId::Construct).expect("the Construct lane is measured");
    let repair = cell(&ledger, StageId::Refine).expect("the Refine lane is measured");

    assert_eq!((construct.cost_micro_usd, construct.worker_secs, construct.samples), (5_000, 2, 1));
    assert_eq!(
        (repair.cost_micro_usd, repair.worker_secs, repair.samples),
        (9_000, 4, 1),
        "the unreadable third record costs its cell a sample, not a wrong number",
    );
    assert_eq!(
        (construct.attempts, construct.rolls_to_green, construct.resolved_members),
        (1, 1, 1),
        "the member resolved, so the lap Construct spent on it is a lap to green",
    );
    assert_eq!(repair.cost_per_resolved_member(), Some(9_000));
}

// Tripwire (ADR-0190): the failing-verifier set is the one axis with no recorded
// decision to read it off, so it comes off the fact — which makes it the one
// place a *refused* event could still move a column. A verdict the reducer threw
// out never happened, and charging it would let anyone with the admit door
// inflate a model's failure mix without a single lane running.
#[test]
fn a_refused_verify_verdict_charges_nothing() {
    let mut journal = Journal::sealed(&escalating());
    // The member is still at Construct, so a terminal-Verify verdict is a stage
    // mismatch the reducer refuses.
    let refused = journal.verify_failed("verify-failed-too-early", &[VerifyFailure::Clippy]);
    assert!(refused.effects.is_empty(), "the reducer refused it");

    let ledger = journal.ledger.report(|_| None);
    let construct = cell(&ledger, StageId::Construct).expect("the seal dispatched Construct");

    assert_eq!(construct.failures, Vec::new(), "a refused verdict is not an observation of anything");
    assert_eq!(verdicts(construct, VerifyFailure::Clippy), 0);
}

// Tripwire: the honesty boundary is carried on the document, not left to a
// renderer to remember. A ledger read without it invites "this model is clean"
// off a table that can only say "this model's failures were the ones a gate can
// see" (ADR-0184).
#[test]
fn a_rendered_ledger_carries_its_caveat() {
    assert_eq!(CalibrationLedger::default().report(|_| None).caveat, aether_bloomery::LEDGER_CAVEAT);
}

/// One history folded two ways: live (each commit observes against the config
/// table as of that moment) and boot (the final table is already full, the
/// way `on_load_configs_result` then `on_replay_result` sequence it).
struct History {
    snapshot: Snapshot,
    live: CalibrationLedger,
    configs: ResolvedConfigs,
    rows: Vec<(Event, Decisions)>,
}

impl History {
    fn new() -> Self {
        Self {
            snapshot: Snapshot::new(digest(1)).with_green_base(digest(1)),
            live: CalibrationLedger::default(),
            configs: ResolvedConfigs::default(),
            rows: Vec::new(),
        }
    }

    fn publish(&mut self, override_: &ModelOverride) {
        self.configs.insert(override_.address(), ModelOverride::NAME, to_vec(override_).expect("override encodes"));
    }

    fn admit(&mut self, event: Event) -> Decisions {
        let decisions = reduce(&self.snapshot, &event, &self.configs, &SpendWindow::default());
        self.live.observe(&event, &decisions, &self.configs);
        self.snapshot = self.snapshot.apply(&event, &decisions, &self.configs);
        self.rows.push((event, decisions.clone()));
        decisions
    }

    fn replay(&self) -> CalibrationLedger {
        let mut ledger = CalibrationLedger::default();
        for (event, decisions) in &self.rows {
            ledger.observe(event, decisions, &self.configs);
        }
        ledger
    }
}

fn override_for(harness: Harness, model: &str) -> ModelOverride {
    ModelOverride { agent: Some(AgentSelection { harness, model: model.into() }), ..ModelOverride::default() }
}

// Tripwire (ADR-0184): boot replay rebuilds the ledger exactly as the live
// commits built it, even when config admissions interleave with dispatch-bearing
// commits. The boot path front-loads the final table; the live path observes
// against the table as of each commit. Those two views agree because both
// admission doors (seal and supersede) refuse a registry that names an address
// the reducer cannot produce — so a recorded `DispatchAttempt` never names a
// config that was absent when it folded, and a later-admitted entry sitting in
// the replay table is invisible to earlier rows. A future door that lets a
// reference to an absent entry through would make the live fold fall back to
// the catalog profile (`observe` treats `Missing` as unsealed) while replay
// resolves the now-present override, and this equality would fail.
#[test]
fn boot_replay_rebuilds_the_ledger_exactly_as_live_commits_built_it_because_doors_refuse_absent_config_refs() {
    let first = override_for(Harness::Claude, "claude-opus-5");
    let second = override_for(Harness::Grok, "grok-build-1");
    let (draft_a, _) = draft_with_member_override(1, membership(MEMBER, REVISION), &first);
    let (draft_b, _) = draft_with_member_override(1, membership(MEMBER, REVISION), &second);
    let spec_a = draft_a.seal();
    let spec_b = draft_b.seal();

    let mut history = History::new();

    let refused = reduce(
        &history.snapshot,
        &event("seal-too-early", Fact::Seal(spec_a.clone())),
        &history.configs,
        &SpendWindow::default(),
    );
    assert!(
        matches!(
            refused.outcome,
            Outcome::SealRejected(SealError::UnproducibleConfig { ref kind, reason: Unproducible::Absent, .. })
                if kind == ModelOverride::NAME
        ),
        "a seal that names an absent override is refused at the door: {refused:?}",
    );

    history.publish(&first);
    let Outcome::Sealed(predecessor) = history.admit(event("seal-a", Fact::Seal(spec_a))).outcome else {
        panic!("the first override is present, so its seal admits");
    };

    history.publish(&second);
    let superseded = history.admit(event("sup-b", Fact::Supersede { predecessor, successor: spec_b }));
    assert!(
        matches!(superseded.outcome, Outcome::Superseded { .. }),
        "the second override is present, so the successor admits: {superseded:?}",
    );

    let live = history.live.report(|_| None);
    let replayed = history.replay().report(|_| None);
    assert_eq!(live, replayed, "front-loading the final table rebuilds the live fold");

    let profile = StageCatalog::profile_of(StageId::Construct);
    let want = vec![first.resolve(StageId::Construct, &profile), second.resolve(StageId::Construct, &profile)];
    let got: Vec<_> = live.cells.iter().map(|cell| cell.agent.clone()).collect();
    assert_eq!(got, want, "each override keyed its own Construct cell, so equality is not both paths ignoring configs");
}
