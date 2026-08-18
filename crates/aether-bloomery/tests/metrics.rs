//! The metrics ledger: journal-order fold of dispatches, seats, and timelines.
//!
//! These cases exercise the fold this crate owns — seat recomputation, envelope
//! stamps vs reconstruction, and the unpriced-vs-mean distinction — driven
//! through `reduce` rather than spliced.

mod common;

use std::collections::BTreeMap;

use aether_data::wire::to_vec;

use aether_bloomery::{
    AgentSelection, BloomId, CandidateRef, Decision, Decisions, Event, Evidence, EvidenceKind, Fact, Harness,
    MetricDispatch, MetricsLedger, ModelOverride, ReasoningEffort, ResolvedConfigs, Snapshot, SpendWindow, StageId,
    StageOverride, StudyCost, StudyRecord, reduce,
};
use common::{digest, draft_with_member_override, event, membership, workpiece};

const MEMBER: &str = "wp-a";
const REVISION: u8 = 10;
const TREE: u8 = 100;

struct Journal {
    snapshot: Snapshot,
    ledger: MetricsLedger,
    configs: ResolvedConfigs,
    bloom: BloomId,
    next_sequence: u64,
}

impl Journal {
    fn sealed(override_: &ModelOverride) -> Self {
        let (draft, configs) = draft_with_member_override(1, membership(MEMBER, REVISION), override_);
        let spec = draft.seal();
        let bloom = spec.id();
        let mut journal = Self {
            snapshot: Snapshot::new(digest(1)),
            ledger: MetricsLedger::default(),
            configs,
            bloom,
            next_sequence: 1,
        };
        journal.admit(&event("seal", Fact::Seal(spec)), Some(1_000));
        journal
    }

    fn admit(&mut self, event: &Event, envelope: Option<u64>) -> Decisions {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let decisions = reduce(&self.snapshot, event, &self.configs, &SpendWindow::default());
        self.ledger.observe(sequence, event, &decisions, &self.configs, envelope);
        self.snapshot = self.snapshot.apply(event, &decisions, &self.configs);
        decisions
    }

    fn completed(
        &mut self,
        key: &str,
        stage: StageId,
        candidate: Option<CandidateRef>,
        envelope: Option<u64>,
    ) -> Decisions {
        self.admit(
            &event(
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
            ),
            envelope,
        )
    }

    fn study(&mut self, key: &str, subject: u8, detail: u8) -> Decisions {
        self.admit(
            &event(
                key,
                Fact::AdmitEvidence {
                    bloom: self.bloom,
                    evidence: Evidence {
                        subject: digest(subject),
                        kind: EvidenceKind::StudyRecord,
                        detail: digest(detail),
                    },
                },
            ),
            Some(3_000),
        )
    }
}

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

fn study_record(bloom: BloomId, subject: u8, cost_micro_usd: u64) -> StudyRecord {
    StudyRecord {
        bloom,
        subject: digest(subject),
        cost: StudyCost { cost_micro_usd, input_tokens: 10, output_tokens: 2, ..StudyCost::default() },
    }
}

fn encoded_rows(ledger: &MetricsLedger) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    (
        to_vec(&ledger.dispatch_rows()).expect("dispatches encode"),
        to_vec(&ledger.bloom_rows()).expect("blooms encode"),
        to_vec(&ledger.day_rows()).expect("days encode"),
    )
}

/// Tripwire: delete-and-refold of the same journal fixture is byte-identical,
/// and a cursor resume does not re-read consumed history.
#[test]
fn a_refold_from_the_same_journal_is_byte_identical_and_the_cursor_resumes() {
    let mut live = Journal::sealed(&escalating());
    let captured = CandidateRef { tree: digest(TREE), checkout: digest(TREE + 1) };
    live.completed("construct", StageId::Construct, Some(captured), Some(2_000));
    live.study("study", REVISION, 40);

    let first = encoded_rows(&live.ledger);

    let mut refold = MetricsLedger::default();
    let mut replayed = Vec::new();
    {
        let (draft, configs) = draft_with_member_override(1, membership(MEMBER, REVISION), &escalating());
        let spec = draft.seal();
        let mut snapshot = Snapshot::new(digest(1));
        for (index, fact) in [
            Fact::Seal(spec),
            Fact::AttemptCompleted {
                bloom: live.bloom,
                workpiece: workpiece(MEMBER),
                stage: StageId::Construct,
                passed: true,
                evidence: Evidence {
                    subject: digest(TREE),
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(90),
                },
                candidate: Some(captured),
            },
            Fact::AdmitEvidence {
                bloom: live.bloom,
                evidence: Evidence { subject: digest(REVISION), kind: EvidenceKind::StudyRecord, detail: digest(40) },
            },
        ]
        .into_iter()
        .enumerate()
        {
            let event = event(&format!("row-{index}"), fact);
            let decisions = reduce(&snapshot, &event, &configs, &SpendWindow::default());
            let envelope = match index {
                0 => Some(1_000),
                1 => Some(2_000),
                _ => Some(3_000),
            };
            refold.observe((index as u64) + 1, &event, &decisions, &configs, envelope);
            snapshot = snapshot.apply(&event, &decisions, &configs);
            replayed.push((event, decisions, envelope));
        }
    }
    assert_eq!(encoded_rows(&refold), first, "a full refold reproduces the live rows");

    let mut resumed = MetricsLedger::default();
    let (draft, configs) = draft_with_member_override(1, membership(MEMBER, REVISION), &escalating());
    let spec = draft.seal();
    let mut snapshot = Snapshot::new(digest(1));
    let first_event = event("row-0", Fact::Seal(spec));
    let first_decisions = reduce(&snapshot, &first_event, &configs, &SpendWindow::default());
    resumed.observe(1, &first_event, &first_decisions, &configs, Some(1_000));
    snapshot = snapshot.apply(&first_event, &first_decisions, &configs);
    let cursor = resumed.through_sequence();
    assert_eq!(cursor, 1, "the cursor stops at the last consumed sequence");

    for (index, (event, decisions, envelope)) in replayed.iter().enumerate().skip(1) {
        let sequence = (index as u64) + 1;
        assert!(sequence > cursor, "resume must not re-read sequence {sequence}");
        resumed.observe(sequence, event, decisions, &configs, *envelope);
        snapshot = snapshot.apply(event, decisions, &configs);
    }
    assert_eq!(encoded_rows(&resumed), first, "resuming from the cursor matches a full fold");
}

/// Timeline spans carry the envelope stamp when the journal row has one, and
/// are marked reconstructed when it does not.
#[test]
fn timeline_spans_carry_envelope_stamps_or_are_marked_reconstructed() {
    let mut stamped = Journal::sealed(&escalating());
    let captured = CandidateRef { tree: digest(TREE), checkout: digest(TREE + 1) };
    stamped.completed("construct", StageId::Construct, Some(captured), Some(2_000));
    let timeline = stamped.ledger.timeline(stamped.bloom);
    assert!(
        timeline.spans.iter().any(|span| span.started_unix_millis == Some(2_000) && !span.reconstructed),
        "a stamped dispatch is a wall-clock span: {:?}",
        timeline.spans
    );

    let mut bare = Journal::sealed(&escalating());
    bare.completed("construct", StageId::Construct, Some(captured), None);
    let timeline = bare.ledger.timeline(bare.bloom);
    assert!(
        timeline.spans.iter().any(|span| span.started_unix_millis.is_none() && span.reconstructed),
        "an unstamped dispatch is reconstructed, never given an invented time: {:?}",
        timeline.spans
    );
}

/// The plausible bug: an unpriced attempt (cost == 0) is treated as free and
/// pulled into the mean, so a seat that ran one priced $1 attempt and one
/// unpriced attempt reports $0.50.
#[test]
fn an_unpriced_attempt_is_counted_and_never_summed_into_a_mean() {
    let mut journal = Journal::sealed(&escalating());
    let captured = CandidateRef { tree: digest(TREE), checkout: digest(TREE + 1) };
    journal.completed("construct", StageId::Construct, Some(captured), Some(2_000));
    journal.study("priced", REVISION, 40);
    journal.study("unpriced", REVISION, 41);

    let bloom = journal.bloom;
    let seats = journal.ledger.seats(|asked| {
        if *asked == digest(40) {
            Some(study_record(bloom, REVISION, 1_000_000))
        } else if *asked == digest(41) {
            Some(study_record(bloom, REVISION, 0))
        } else {
            None
        }
    });
    let construct = seats.iter().find(|seat| seat.stage == StageId::Construct).expect("Construct is a seat");
    assert_eq!(construct.unpriced, 1, "the zero-priced record is counted as unpriced");
    assert_eq!(construct.priced_samples, 1, "only the priced record is a mean sample");
    assert_eq!(construct.cost_micro_usd, 1_000_000, "the unpriced record must not enter the sum");
    assert_eq!(
        construct.mean_cost_micro_usd(),
        Some(1_000_000),
        "the mean is the priced sample, not halved by a free-looking zero"
    );
}

#[test]
fn the_seat_is_recomputed_from_the_sealed_override_not_the_dispatchs_empty_model() {
    let mut journal = Journal::sealed(&escalating());
    let captured = CandidateRef { tree: digest(TREE), checkout: digest(TREE + 1) };
    let decisions = journal.completed("construct", StageId::Construct, Some(captured), Some(2_000));
    for decision in &decisions.effects {
        if let Decision::DispatchAttempt { transformation, .. } = decision {
            assert!(transformation.model.is_none(), "the journal never carries a resolved model");
        }
    }
    let seats = journal.ledger.seats(|_| None);
    let construct = seats.iter().find(|seat| seat.stage == StageId::Construct).expect("Construct is a seat");
    assert_eq!(
        (construct.agent.harness, construct.agent.model.as_str(), construct.agent.effort),
        (Harness::Claude, "claude-opus-5", ReasoningEffort::High),
    );
}

#[test]
fn dispatch_rows_name_a_deterministic_fold_id() {
    let mut journal = Journal::sealed(&escalating());
    let captured = CandidateRef { tree: digest(TREE), checkout: digest(TREE + 1) };
    journal.completed("construct", StageId::Construct, Some(captured), Some(2_000));
    let rows: Vec<MetricDispatch> = journal.ledger.dispatch_rows();
    assert!(rows.iter().any(|row| row.id.starts_with("fold:") && row.stage == StageId::Construct));
}
