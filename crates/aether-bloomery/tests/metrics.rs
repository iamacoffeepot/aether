//! The metrics ledger: journal-order fold of dispatches, seats, and timelines.
//!
//! These cases exercise the fold this crate owns — seat recomputation, envelope
//! stamps vs reconstruction, and the unpriced-vs-mean distinction — driven
//! through `reduce` rather than spliced.

mod common;

use std::collections::BTreeMap;

use aether_data::wire::to_vec;

use aether_bloomery::{
    AgentSelection, BloomId, BloomStatus, CandidateRef, Decision, Decisions, Event, Evidence, EvidenceKind, Fact,
    Harness, MemberDependency, MetricBloom, MetricDispatch, MetricsLedger, ModelOverride, Outcome, ReasoningEffort,
    ResolvedConfigs, SealError, Snapshot, SpendWindow, StageId, StageOverride, StudyCost, StudyRecord, SupersedeError,
    WorkpieceId, reduce,
};
use common::{claim, compiled_resolved, digest, draft, draft_with_member_override, event, membership, workpiece};

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
    fn fresh() -> Self {
        Self {
            snapshot: Snapshot::new(digest(1)).with_green_base(digest(1)),
            ledger: MetricsLedger::default(),
            configs: compiled_resolved(),
            bloom: BloomId(digest(0)),
            next_sequence: 1,
        }
    }

    fn sealed(override_: &ModelOverride) -> Self {
        let (draft, configs) = draft_with_member_override(1, membership(MEMBER, REVISION), override_);
        let spec = draft.seal();
        let bloom = spec.id();
        let mut journal = Self {
            snapshot: Snapshot::new(digest(1)).with_green_base(digest(1)),
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

fn encoded_rows(ledger: &MetricsLedger, bloom: BloomId) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let records = BTreeMap::from([(digest(40), study_record(bloom, REVISION, 1_000_000))]);
    (
        to_vec(&ledger.dispatch_rows()).expect("dispatches encode"),
        to_vec(&ledger.bloom_rows()).expect("blooms encode"),
        to_vec(&ledger.day_rows(|asked| records.get(asked).copied())).expect("days encode"),
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

    let first = encoded_rows(&live.ledger, live.bloom);

    let mut refold = MetricsLedger::default();
    let mut replayed = Vec::new();
    {
        let (draft, configs) = draft_with_member_override(1, membership(MEMBER, REVISION), &escalating());
        let spec = draft.seal();
        let mut snapshot = Snapshot::new(digest(1)).with_green_base(digest(1));
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
    assert_eq!(encoded_rows(&refold, live.bloom), first, "a full refold reproduces the live rows");

    let mut resumed = MetricsLedger::default();
    let (draft, configs) = draft_with_member_override(1, membership(MEMBER, REVISION), &escalating());
    let spec = draft.seal();
    let mut snapshot = Snapshot::new(digest(1)).with_green_base(digest(1));
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
    assert_eq!(encoded_rows(&resumed, live.bloom), first, "resuming from the cursor matches a full fold");
}

/// The plausible bug: `AggregateReview` folds as an empty bloom-level workpiece,
/// so the timeline paints a second tail beside the composition cursor for the
/// same integration subject.
#[test]
fn aggregate_gate_spans_belong_to_the_composition_workpiece() {
    let mut journal = Journal::sealed(&escalating());
    journal.admit(
        &event("integrate", Fact::Integrate { bloom: journal.bloom, claim: claim(MEMBER, REVISION, TREE) }),
        Some(3_000),
    );
    let decisions = journal.admit(
        &event(
            "resolve",
            Fact::Resolve { bloom: journal.bloom, tree: digest(30), head: digest(40), lineage: Vec::new() },
        ),
        Some(4_000),
    );
    assert!(
        decisions.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateReview { .. })),
        "resolve dispatches the critic: {decisions:?}"
    );
    assert!(
        decisions.effects.iter().any(|effect| matches!(effect, Decision::DispatchAggregateVerify { .. })),
        "resolve still dispatches the mechanical gate: {decisions:?}"
    );

    let timeline = journal.ledger.timeline(journal.bloom);
    let span = timeline
        .spans
        .iter()
        .find(|span| span.stage == StageId::AggregateReview)
        .unwrap_or_else(|| panic!("AggregateReview must appear on the timeline: {:?}", timeline.spans));
    assert_eq!(
        span.workpiece,
        WorkpieceId::COMPOSITION,
        "AggregateReview is a composition span, not a bloom-level empty workpiece: {span:?}"
    );
    assert!(
        timeline.spans.iter().all(|span| span.stage != StageId::AggregateVerify),
        "the mechanical gate mints no dispatch row: {:?}",
        timeline.spans
    );
    // Seal of the one-member draft dispatches Construct (the entry stage).
    // Integrate records the claim and `DispatchIntegration` (not a SeatDispatch).
    // Resolve emits `DispatchAggregateReview` (folded) and `DispatchAggregateVerify`
    // (not a SeatDispatch). Main's count for this journal is therefore 2.
    let bloom = journal
        .ledger
        .bloom_rows()
        .into_iter()
        .find(|row| row.bloom == journal.bloom)
        .expect("the seal minted a bloom rollup");
    assert_eq!(
        bloom.dispatches, 2,
        "Construct entry plus AggregateReview; AggregateVerify must not increment: {bloom:?}"
    );
}

/// Tripwire: the `metric_dispatch` store key is stable across a display-column
/// change, because it is a primary key under an upsert. Moving
/// `MetricDispatch.workpiece` to the composition must not rewrite `id`.
#[test]
fn aggregate_review_dispatch_id_keeps_the_empty_bloom_workpiece_segment() {
    let mut journal = Journal::sealed(&escalating());
    journal.admit(
        &event("integrate", Fact::Integrate { bloom: journal.bloom, claim: claim(MEMBER, REVISION, TREE) }),
        Some(3_000),
    );
    journal.admit(
        &event(
            "resolve",
            Fact::Resolve { bloom: journal.bloom, tree: digest(30), head: digest(40), lineage: Vec::new() },
        ),
        Some(4_000),
    );

    let review = journal
        .ledger
        .dispatch_rows()
        .into_iter()
        .find(|row| row.stage == StageId::AggregateReview)
        .expect("resolve folded the critic");
    assert_eq!(review.workpiece, WorkpieceId::COMPOSITION, "the display column moved: {review:?}");
    assert_eq!(
        review.id,
        format!("fold:{}::aggregate-review:{}", journal.bloom.0.to_hex(), review.displayed.to_hex()),
        "the persisted id keeps the empty bloom-level workpiece segment: {review:?}"
    );
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

/// Tripwire: a consumer reading the tail as today gets a bucket that is not a day.
#[test]
fn the_undated_bucket_never_occupies_the_newest_day_slot() {
    let mut journal = Journal::sealed(&escalating());
    let captured = CandidateRef { tree: digest(TREE), checkout: digest(TREE + 1) };
    journal.completed("construct", StageId::Construct, Some(captured), None);

    let rows = journal.ledger.day_rows(|_| None);
    assert!(
        rows.first().is_some_and(|row| row.reconstructed),
        "the undated bucket leads so it cannot be mistaken for a civil day: {rows:?}"
    );
    assert!(
        rows.last().is_some_and(|row| !row.reconstructed && row.label.starts_with("bloomery/daily/")),
        "the newest slot is a dated day: {rows:?}"
    );
}

/// The plausible bug: a column is wired to the wrong accumulator, or dollars
/// are attributed to the read day instead of the dispatch's day.
#[test]
fn a_day_carries_its_priced_dollars_landings_wedges_and_cycle_mean() {
    let mut journal = Journal::sealed(&escalating());
    let first_dispatch_millis = 1_000;
    let land_millis = 5_000;
    let miss = |key: &str, bloom| {
        event(
            key,
            Fact::AttemptCompleted {
                bloom,
                workpiece: workpiece(MEMBER),
                stage: StageId::Construct,
                passed: false,
                evidence: Evidence {
                    subject: digest(TREE),
                    kind: EvidenceKind::VerificationResult,
                    detail: digest(91),
                },
                candidate: None,
            },
        )
    };
    journal.admit(&miss("fail-1", journal.bloom), Some(2_000));
    let wedged = journal.admit(&miss("fail-2", journal.bloom), Some(2_000));
    assert!(
        wedged.effects.iter().any(|effect| matches!(effect, Decision::RecordWedge { .. })),
        "the second Construct miss wedges: {wedged:?}"
    );
    journal.study("study", REVISION, 40);
    journal.snapshot.blooms.get_mut(&journal.bloom).expect("the seal recorded the bloom").status =
        BloomStatus::Resolved;
    let landed =
        journal.admit(&event("land", Fact::Land { bloom: journal.bloom, new_head: digest(40) }), Some(land_millis));
    assert!(
        landed.effects.iter().any(|effect| matches!(effect, Decision::EmitReceipt(_))),
        "a resolved bloom on its sealed base lands: {landed:?}"
    );

    let bloom = journal.bloom;
    let priced =
        journal.ledger.day_rows(|asked| (*asked == digest(40)).then_some(study_record(bloom, REVISION, 900_000)));
    let day = priced.last().expect("a stamped fold produces a dated day");
    assert!(!day.reconstructed, "the newest row is the civil day: {day:?}");
    assert_eq!(day.spend_micro_usd, 900_000, "priced dollars land on the dispatch's day: {day:?}");
    assert_eq!(day.landed, 1, "EmitReceipt is one landing: {day:?}");
    assert_eq!(day.wedges, 1, "RecordWedge is one stop: {day:?}");
    assert_eq!(
        day.cycle_time_millis,
        Some(land_millis - first_dispatch_millis),
        "cycle mean is first dispatch to land: {day:?}"
    );

    let unpriced = journal.ledger.day_rows(|asked| (*asked == digest(40)).then_some(study_record(bloom, REVISION, 0)));
    assert_eq!(
        unpriced.last().expect("the dated day remains").spend_micro_usd,
        0,
        "an unpriced record is unresolved, never averaged in as free"
    );
}

fn edge(member: &str, depends_on: &str) -> MemberDependency {
    MemberDependency { member: workpiece(member), depends_on: workpiece(depends_on) }
}

fn bloom_row(ledger: &MetricsLedger, bloom: BloomId) -> MetricBloom {
    ledger
        .bloom_rows()
        .into_iter()
        .find(|row| row.bloom == bloom)
        .unwrap_or_else(|| panic!("expected a rollup for {bloom:?}, got {:?}", ledger.bloom_rows()))
}

/// The plausible bug: observe matches only `Fact::Seal`, so a real graph bloom
/// (and a graph successor) land in the rollup via dispatch with members=0 and
/// `seal_sequence=0`.
#[test]
fn a_graph_seal_and_its_graph_successor_carry_members_and_seal_sequence() {
    let predecessor_spec = draft(1, vec![membership("wp-a", 10), membership("wp-b", 11)]).seal();
    let successor_spec = draft(1, vec![membership("wp-a", 10), membership("wp-b", 11), membership("wp-c", 12)]).seal();
    let successor = successor_spec.id();
    let seal = event(
        "graph-seal",
        Fact::GraphSeal { predecessor: None, spec: predecessor_spec, edges: vec![edge("wp-b", "wp-a")] },
    );
    let mut journal = Journal::fresh();
    let sealed = journal.admit(&seal, Some(1_000));
    let Outcome::Sealed(predecessor) = sealed.outcome else {
        panic!("a declared graph admits: {sealed:?}");
    };
    let superseded = journal.admit(
        &event(
            "graph-sup",
            Fact::GraphSeal {
                predecessor: Some(predecessor),
                spec: successor_spec,
                edges: vec![edge("wp-b", "wp-a"), edge("wp-c", "wp-a")],
            },
        ),
        Some(2_000),
    );
    assert!(
        matches!(superseded.outcome, Outcome::Superseded { successor: id, .. } if id == successor),
        "a graph successor admits: {superseded:?}"
    );

    let first = bloom_row(&journal.ledger, predecessor);
    assert_eq!(
        first.members, 2,
        "the graph bloom's membership is the admitted spec, not the dispatch default: {first:?}"
    );
    assert_eq!(first.seal_sequence, 1, "the graph bloom's sequence is the admitting row: {first:?}");

    let successor_row = bloom_row(&journal.ledger, successor);
    assert_eq!(
        successor_row.members, 3,
        "the graph successor's membership is the admitted spec, not members=0: {successor_row:?}"
    );
    assert_eq!(
        successor_row.seal_sequence, 2,
        "the graph successor's sequence is the admitting row: {successor_row:?}"
    );

    let replayed = journal.admit(&seal, Some(3_000));
    assert!(matches!(replayed.outcome, Outcome::Duplicate), "the same graph-seal key is a duplicate: {replayed:?}");
    assert_eq!(
        bloom_row(&journal.ledger, predecessor).seal_sequence,
        1,
        "a duplicate graph seal must not overwrite the admitting sequence"
    );
}

/// The plausible bug: observe never matches `Fact::Supersede`, so a successor
/// bloom reports members=0 and `seal_sequence=0` (or is missing until a dispatch
/// creates the default row).
#[test]
fn a_supersede_records_the_successor_members_and_seal_sequence() {
    let predecessor_spec = draft(1, vec![membership("wp-a", 10)]).seal();
    let successor_spec = draft(1, vec![membership("wp-a", 10), membership("wp-b", 11)]).seal();
    let successor = successor_spec.id();
    let mut journal = Journal::fresh();
    let sealed = journal.admit(&event("seal", Fact::Seal(predecessor_spec)), Some(1_000));
    let Outcome::Sealed(predecessor) = sealed.outcome else {
        panic!("the predecessor admits: {sealed:?}");
    };
    let superseded =
        journal.admit(&event("sup", Fact::Supersede { predecessor, successor: successor_spec }), Some(2_000));
    assert!(
        matches!(superseded.outcome, Outcome::Superseded { successor: id, .. } if id == successor),
        "the successor admits: {superseded:?}"
    );

    let first = bloom_row(&journal.ledger, predecessor);
    assert_eq!(first.members, 1, "the predecessor keeps its admitted membership: {first:?}");
    assert_eq!(first.seal_sequence, 1, "the predecessor keeps its admitting sequence: {first:?}");

    let successor_row = bloom_row(&journal.ledger, successor);
    assert_eq!(successor_row.members, 2, "the successor's membership is the admitted spec: {successor_row:?}");
    assert_eq!(successor_row.seal_sequence, 2, "the successor's sequence is the admitting row: {successor_row:?}");
}

/// The plausible bug: observe keys off the Seal fact, so a refused empty seal
/// mints a ghost rollup and a duplicate Seal overwrites the real sequence.
#[test]
fn a_refused_or_duplicate_seal_does_not_mint_or_overwrite_a_bloom_rollup() {
    let spec = draft(1, vec![membership(MEMBER, REVISION)]).seal();
    let seal = event("seal", Fact::Seal(spec));
    let mut journal = Journal::fresh();
    let sealed = journal.admit(&seal, Some(1_000));
    let Outcome::Sealed(bloom) = sealed.outcome else {
        panic!("a valid seal admits: {sealed:?}");
    };
    assert_eq!(bloom_row(&journal.ledger, bloom).seal_sequence, 1);
    assert_eq!(journal.ledger.bloom_rows().len(), 1);

    let duplicate = journal.admit(&seal, Some(2_000));
    assert!(matches!(duplicate.outcome, Outcome::Duplicate), "the same key is a duplicate: {duplicate:?}");
    assert_eq!(
        bloom_row(&journal.ledger, bloom).seal_sequence,
        1,
        "a duplicate must not overwrite the admitting sequence"
    );
    assert_eq!(journal.ledger.through_sequence(), 2, "the journal cursor still consumes the duplicate row");
    assert_eq!(journal.ledger.bloom_rows().len(), 1, "a duplicate must not mint a second rollup");

    let empty = draft(1, vec![]).seal();
    let refused = journal.admit(&event("empty", Fact::Seal(empty.clone())), Some(3_000));
    assert!(
        matches!(refused.outcome, Outcome::SealRejected(SealError::EmptyMembership)),
        "an empty seal is refused: {refused:?}"
    );
    assert_eq!(
        journal.ledger.bloom_rows().len(),
        1,
        "a refused seal must not mint a ghost rollup: {:?}",
        journal.ledger.bloom_rows()
    );
    assert_eq!(journal.ledger.summary(0, |_| None).blooms, 1, "the summary must not count a refused seal as a bloom");

    let refused_sup =
        journal.admit(&event("empty-sup", Fact::Supersede { predecessor: bloom, successor: empty }), Some(4_000));
    assert!(
        matches!(
            refused_sup.outcome,
            Outcome::SupersedeRejected(SupersedeError::InvalidMember(SealError::EmptyMembership))
        ),
        "an empty successor is refused: {refused_sup:?}"
    );
    assert_eq!(
        journal.ledger.bloom_rows().len(),
        1,
        "a refused supersede must not mint a successor rollup: {:?}",
        journal.ledger.bloom_rows()
    );
    assert_eq!(bloom_row(&journal.ledger, bloom).seal_sequence, 1);
}
