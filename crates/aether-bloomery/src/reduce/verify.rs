//! Arm of [`super::reduce`]'s fact dispatch (`Fact::VerifyFailed`); wiring
//! lives in `mod.rs`.
//!
//! Typed terminal-Verify failure accounting (ADR-0178).

use alloc::vec::Vec;

use super::attempt::{DispatchTargets, SealedLine, move_effects, wedged};
use super::operator_hold::owed_resume_dispatch;
use super::{
    BloomRecord, BloomStatus, Decision, Decisions, HostFaultError, Outcome, Snapshot, StageProgress, VerifyFailedError,
};
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::values::{Evidence, Membership, VerifyFailure, VerifyFailureSet, Wedge};

/// Reduce one admitted failing member-Verify verdict.
///
/// For current failures `F` and the member's seen set `S`, `R = F ∩ S`
/// decides whether this verdict spends one repair roll, while `S ∪ F` becomes
/// the durable cursor history. A verdict spends at most one roll however many
/// repeated identities it contains.
///
/// `F` empty is not that shape at all — it is the *absence* of a verdict, and
/// routes to [`unjudged_verify`] instead of into the repair accounting above.
pub(super) fn reduce_verify_failed(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    evidence: &Evidence,
    failed_verifiers: VerifyFailureSet,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::VerifyFailedRejected(VerifyFailedError::UnknownOrInactiveBloom));
    };
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::VerifyFailedRejected(VerifyFailedError::UnknownOrInactiveBloom));
    }
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
        return Decisions::rejected(Outcome::VerifyFailedRejected(VerifyFailedError::NotAMember(workpiece.clone())));
    };
    let Some(cursor) = record.progress.get(workpiece).copied() else {
        return Decisions::rejected(Outcome::VerifyFailedRejected(VerifyFailedError::NotDispatched(workpiece.clone())));
    };
    if cursor.stage != StageId::Verify {
        return Decisions::rejected(Outcome::VerifyFailedRejected(VerifyFailedError::StageMismatch {
            expected: cursor.stage,
        }));
    }

    let (subject, checkout) = cursor.candidate.map_or_else(
        || (member.scope_revision, super::splice::member_construct_base(record, workpiece)),
        |candidate| (candidate.tree, candidate.checkout),
    );
    if !evidence.validates(&subject) {
        return Decisions::rejected(Outcome::VerifyFailedRejected(VerifyFailedError::EvidenceNotBound {
            expected: subject,
            got: evidence.subject,
        }));
    }
    let effects = alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }];

    // Binding first, emptiness second: an unjudged verdict still re-dispatches
    // work, so it has to name the subject it was dispatched against before it is
    // allowed to move the member at all.
    if failed_verifiers.is_empty() {
        return unjudged_verify(
            record,
            *bloom,
            member,
            &cursor,
            evidence,
            DispatchTargets { subject, checkout },
            effects,
        );
    }

    // A preflight-only set is a host that could not run the gates, not a
    // candidate the gates judged. Holding here — no Refine, no roll, no
    // attempt increment — is what stops a missing `jscpd` from buying a
    // paid repair lap (#5020). Findings ride the dedicated fact; this
    // safety net keeps a `VerifyFailed` that named only preflight from
    // taking the candidate path either.
    if failed_verifiers == VerifyFailureSet::one(VerifyFailure::Preflight) {
        return host_fault_hold(*bloom, workpiece, evidence, String::new(), effects);
    }

    let line =
        VerifyLine { record, bloom: *bloom, member, cursor: &cursor, targets: DispatchTargets { subject, checkout } };

    // A repeat over an unchanged tree is evidence about the machinery, not the
    // model: nothing new was judged between the two verdicts, so `R = F ∩ S`
    // is measuring the same work twice. Keyed here rather than folded into the
    // intersection because the two mean different things — the seen set records
    // which identities this member has ever failed, and the series records
    // which *generation* of its work the roll count is about.
    if snapshot.member_verify_series(bloom, workpiece) == Some(subject) {
        return repeated_over_one_tree(snapshot, &line, evidence, effects);
    }
    counted_verdict(&line, failed_verifiers, evidence, effects)
}

/// The member-line context every arm of [`reduce_verify_failed`] decides
/// against: the sealed bloom, which member, where its cursor stands, and the
/// two digests its next dispatch aims at.
///
/// Grouped because the arms take all five or none — and because a
/// [`DispatchTargets`] re-derived per arm is a transposed pair waiting to
/// happen (ADR-0152).
struct VerifyLine<'a> {
    record: &'a BloomRecord,
    bloom: BloomId,
    member: &'a Membership,
    cursor: &'a StageProgress,
    targets: DispatchTargets,
}

/// Reduce a failing Verify over a generation the member really produced: the
/// ADR-0178 accounting, and the wedge at its ceiling.
///
/// For current failures `F` and the member's seen set `S`, `R = F ∩ S` decides
/// whether this verdict spends one repair roll, while `S ∪ F` becomes the
/// durable cursor history.
fn counted_verdict(
    line: &VerifyLine<'_>,
    failed_verifiers: VerifyFailureSet,
    evidence: &Evidence,
    mut effects: Vec<Decision>,
) -> Decisions {
    let VerifyLine { record, bloom, member, cursor, targets } = *line;
    let workpiece = &member.workpiece;
    let repeated_verifiers = failed_verifiers.intersection(cursor.seen_verify_failures);
    let seen_verify_failures = cursor.seen_verify_failures.union(failed_verifiers);
    let rolls = cursor.repair_rolls + u32::from(!repeated_verifiers.is_empty());

    // The loop is bounded by N + B: V1 has N = 9 identities, so at most nine
    // failed verdicts can add a new identity without spending a roll; at most B
    // later verdicts can spend the sealed Verify budget before this member wedges.
    if !repeated_verifiers.is_empty() && rolls >= record.stage_catalog.retry_budget_of(StageId::Verify).unwrap_or(1) {
        let progress = StageProgress {
            stage: StageId::Verify,
            attempts: cursor.attempts,
            candidate: cursor.candidate,
            repair_rolls: rolls,
            seen_verify_failures,
            fold_checkpoint: cursor.fold_checkpoint,
            fold_conflict_evidence: None,
            reconcile_assembles_base: false,
        };
        // Persist the union even on the terminal verdict. This cursor write is
        // intentionally not paired with a dispatch; the following RecordWedge
        // restores the terminal marker after AdvanceStage clears any stale one.
        effects.push(Decision::AdvanceStage { bloom, workpiece: workpiece.clone(), progress });
        effects.push(Decision::RecordWedge {
            bloom,
            workpiece: workpiece.clone(),
            wedge: Wedge { stage: StageId::Verify, evidence: evidence.detail, repeated_verifiers },
        });
        return Decisions {
            outcome: Outcome::AttemptWedged {
                bloom,
                workpiece: workpiece.clone(),
                stage: StageId::Verify,
                repeated_verifiers,
            },
            effects,
        };
    }

    // The member is still in whatever fold round it was in (#4952): a repair lap
    // changes the candidate, not which folded tree the candidate has to land on.
    let progress = StageProgress {
        stage: StageId::Refine,
        attempts: 1,
        candidate: cursor.candidate,
        repair_rolls: rolls,
        seen_verify_failures,
        fold_checkpoint: cursor.fold_checkpoint,
        fold_conflict_evidence: None,
        reconcile_assembles_base: false,
    };
    effects.extend(move_effects(
        bloom,
        workpiece,
        member.scope_revision,
        progress,
        targets,
        SealedLine::of(record, member),
    ));
    Decisions { outcome: Outcome::RefineReentered { bloom, workpiece: workpiece.clone(), rolls }, effects }
}

/// Reduce a failing Verify whose judged tree is the one the member's series is
/// already counting — the same effective content failing twice.
///
/// The repeated-verifiers ceiling exists to say "the model keeps producing work
/// that fails the same way". Between two verdicts over one tree the model
/// produced nothing at all, so the second says nothing about the model and buys
/// no repair roll and no verifier identity. What it does say is that the
/// machinery served the same candidate to the gate twice, which is the
/// ADR-0195 series' subject, so the anomaly is recorded there — and at that
/// series' ceiling the member wedges naming machinery rather than Work, so a
/// coordinator stuck re-serving one tree stops without ever being read as a
/// member that could not do its job.
///
/// The member still re-enters `Refine`. The verdict is a real one about a real
/// tree; re-running the gate over that tree would only reproduce it, while a
/// repair lap is the move that can change the tree the next verdict judges.
///
/// On 2026-08-26 this was the second half of the wedge that stopped
/// `retention-archive-tier`: a stale checkout meant the gate judged one tree
/// twice, and the identical failure pair tripped the ceiling with `wedge_cause`
/// Work though no new model work had been judged since the first. The sibling
/// checkout binding makes that particular route unreachable; this is the
/// defence behind it.
fn repeated_over_one_tree(
    snapshot: &Snapshot,
    line: &VerifyLine<'_>,
    evidence: &Evidence,
    mut effects: Vec<Decision>,
) -> Decisions {
    let VerifyLine { record, bloom, member, cursor, targets } = *line;
    let workpiece = &member.workpiece;
    let rolls = match snapshot.member_machinery(&bloom, workpiece) {
        Some(fault) if fault.stage == StageId::Verify => fault.rolls.saturating_add(1),
        _ => 1,
    };
    let budget = record.stage_catalog.retry_budget_of(StageId::Verify).unwrap_or(1);
    let recorded = Decision::RecordMemberMachinery {
        bloom,
        workpiece: workpiece.clone(),
        stage: StageId::Verify,
        rolls,
        evidence: evidence.detail,
    };

    if rolls >= budget {
        effects.push(recorded);
        effects.push(Decision::RecordWedge {
            bloom,
            workpiece: workpiece.clone(),
            wedge: Wedge {
                stage: StageId::Verify,
                evidence: evidence.detail,
                repeated_verifiers: VerifyFailureSet::EMPTY,
            },
        });
        return Decisions {
            outcome: Outcome::MachineryWedged {
                bloom,
                workpiece: workpiece.clone(),
                stage: StageId::Verify,
                rolls,
                budget,
            },
            effects,
        };
    }

    // Every ADR-0178 counter passes through untouched — the roll ledger and the
    // seen set both describe judged generations, and this verdict judged none.
    let progress = StageProgress {
        stage: StageId::Refine,
        attempts: 1,
        candidate: cursor.candidate,
        repair_rolls: cursor.repair_rolls,
        seen_verify_failures: cursor.seen_verify_failures,
        fold_checkpoint: cursor.fold_checkpoint,
        fold_conflict_evidence: None,
        reconcile_assembles_base: false,
    };
    effects.extend(move_effects(
        bloom,
        workpiece,
        member.scope_revision,
        progress,
        targets,
        SealedLine::of(record, member),
    ));
    // Ordered after the move, and it has to be: a cursor leaving the stage its
    // machinery series is against retires that series (ADR-0195), and this
    // cursor is leaving `Verify` for the repair lap. Recorded behind the
    // advance, the anomaly survives the excursion and the next repeat counts
    // from it — the same ordering the terminal repeat wedge above relies on to
    // restore its marker. A lap that produces a genuinely new tree takes the
    // ordinary path, whose advance retires the series and starts the member
    // over with a clean one.
    effects.push(recorded);

    Decisions {
        outcome: Outcome::RefineReentered { bloom, workpiece: workpiece.clone(), rolls: cursor.repair_rolls },
        effects,
    }
}

/// Re-run the gate over a candidate no gate ever judged, or wedge once the
/// member's `Verify` budget is spent.
///
/// A failing member `Verify` that names no verifier is not a failure — ADR-0178
/// gives every real one at least one identity, so an empty set is a lane that
/// died before the umbrella rendered a verdict: killed mid-build, cancelled on
/// its execution limit, gone without leaving readable evidence. The candidate it
/// was dispatched against is untouched.
///
/// That is why this is not a repair. `Refine` hands a model a worktree checked
/// out *at* the candidate and asks it to fix the failures the gate named; with
/// no failures to name, a lane that correctly answers "this work order is
/// already satisfied" changes nothing, captures no diff, and is recorded as one
/// more failed attempt — three laps of that spends a member's whole repair
/// budget on a gate that never ran. What has to happen again is the
/// *verification*, so the retry is `Verify`'s own: its budget, its `attempts`
/// counter, the same targets the failed dispatch aimed at.
///
/// Nothing is charged to the ADR-0178 accounting either. `repair_rolls` and
/// `seen_verify_failures` both pass through unchanged — an attempt that rendered
/// no verdict saw no verifier fail, and folding it into the seen set would let
/// the *next* honest failure read as a repeat and wedge the member for a defect
/// it never had.
///
/// Exhaustion wedges at `Verify` carrying the unjudged attempt's own evidence
/// (the timeout record, the synthesised missing-evidence artifact) and an empty
/// `repeated_verifiers` — which is itself the operator's discriminator, since
/// the repeat rule above always wedges naming the identities that repeated. An
/// empty set on a `Verify` wedge reads as "the gate never answered".
fn unjudged_verify(
    record: &BloomRecord,
    bloom: BloomId,
    member: &Membership,
    cursor: &StageProgress,
    evidence: &Evidence,
    targets: DispatchTargets,
    mut effects: Vec<Decision>,
) -> Decisions {
    let workpiece = &member.workpiece;
    let budget = record.stage_catalog.retry_budget_of(StageId::Verify).unwrap_or(1);
    if cursor.attempts >= budget {
        return wedged(bloom, workpiece, StageId::Verify, evidence, effects);
    }

    let attempt = cursor.attempts + 1;
    let progress = StageProgress {
        stage: StageId::Verify,
        attempts: attempt,
        candidate: cursor.candidate,
        repair_rolls: cursor.repair_rolls,
        seen_verify_failures: cursor.seen_verify_failures,
        fold_checkpoint: cursor.fold_checkpoint,
        fold_conflict_evidence: None,
        reconcile_assembles_base: false,
    };
    effects.extend(move_effects(
        bloom,
        workpiece,
        member.scope_revision,
        progress,
        targets,
        SealedLine::of(record, member),
    ));

    Decisions {
        outcome: Outcome::AttemptRetried { bloom, workpiece: workpiece.clone(), stage: StageId::Verify, attempt },
        effects,
    }
}

/// Reduce a preflight-only Verify: the host could not run the gates (#5020).
pub(super) fn reduce_verify_host_fault(
    snapshot: &Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    evidence: &Evidence,
    findings: &str,
) -> Decisions {
    match verify_at(snapshot, bloom, workpiece, Some(evidence)) {
        Ok(_) => {}
        Err(error) => return Decisions::rejected(Outcome::HostFaultRejected(error)),
    }
    host_fault_hold(
        *bloom,
        workpiece,
        evidence,
        findings.to_owned(),
        alloc::vec![Decision::RecordEvidence { bloom: *bloom, evidence: evidence.clone() }],
    )
}

/// Re-dispatch a member held on a host fault (#5020).
///
/// The cursor does not move and no budget is spent: this is the same Verify
/// the preflight never judged, aimed again now that the host may have the
/// tools. An operator hold still swallows the dispatch.
pub(super) fn reduce_resume_host_fault(snapshot: &Snapshot, bloom: &BloomId, workpiece: &WorkpieceId) -> Decisions {
    let record = match verify_at(snapshot, bloom, workpiece, None) {
        Ok(record) => record,
        Err(error) => return Decisions::rejected(Outcome::HostFaultRejected(error)),
    };
    if !record.host_faults.contains_key(workpiece) {
        return Decisions::rejected(Outcome::HostFaultRejected(HostFaultError::NotHeld(workpiece.clone())));
    }

    let mut effects = alloc::vec![Decision::ClearHostFault { bloom: *bloom, workpiece: workpiece.clone() }];
    if let Some(owed) = owed_resume_dispatch(record, *bloom, workpiece, snapshot.member_checkpoint(bloom, workpiece)) {
        effects.extend(owed);
    }
    Decisions { outcome: Outcome::HostFaultResumed { bloom: *bloom, workpiece: workpiece.clone() }, effects }
}

/// Hold the member at Verify and record the host condition. The cursor, the
/// attempt count, and the repair ledger stay where they were — a host gap
/// is not a verdict on the candidate.
fn host_fault_hold(
    bloom: BloomId,
    workpiece: &WorkpieceId,
    evidence: &Evidence,
    findings: String,
    mut effects: Vec<Decision>,
) -> Decisions {
    effects.push(Decision::RecordHostFault {
        bloom,
        workpiece: workpiece.clone(),
        findings,
        evidence: evidence.detail,
    });
    effects.push(Decision::DeferDispatch { bloom, workpiece: workpiece.clone() });
    Decisions { outcome: Outcome::VerifyHostFaultHeld { bloom, workpiece: workpiece.clone() }, effects }
}

/// The bloom record for a member sitting at terminal Verify, or why it is not.
fn verify_at<'a>(
    snapshot: &'a Snapshot,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    evidence: Option<&Evidence>,
) -> Result<&'a BloomRecord, HostFaultError> {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Err(HostFaultError::UnknownOrInactiveBloom);
    };
    if record.status != BloomStatus::Sealed {
        return Err(HostFaultError::UnknownOrInactiveBloom);
    }
    let Some(member) = record.spec.members().iter().find(|member| member.workpiece == *workpiece) else {
        return Err(HostFaultError::NotAMember(workpiece.clone()));
    };
    let Some(cursor) = record.progress.get(workpiece).copied() else {
        return Err(HostFaultError::NotDispatched(workpiece.clone()));
    };
    if cursor.stage != StageId::Verify {
        return Err(HostFaultError::StageMismatch { expected: cursor.stage });
    }
    if let Some(evidence) = evidence {
        let subject = cursor.candidate.map_or(member.scope_revision, |candidate| candidate.tree);
        if !evidence.validates(&subject) {
            return Err(HostFaultError::EvidenceNotBound { expected: subject, got: evidence.subject });
        }
    }
    Ok(record)
}
