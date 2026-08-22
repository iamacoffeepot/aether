//! Withdrawing one member from a walking bloom (#5327).
//!
//! `supersede --eject` is a draft edit, not a member-removal mechanism: the
//! coordinator learns only that a whole bloom was replaced, so shedding one
//! wrong member costs a new bloom id, a re-adoption of every resolved sibling,
//! a fresh claim transfer, and a new work order. This door is the narrow move
//! that was missing.
//!
//! It is modelled on the operator brake rather than on the wedge, because a
//! wedge is something a member *earns* by exhausting a budget and this is an
//! act no verdict produced. So it carries a reason and an operator, both
//! refused blank, and the record of who decided is its whole product.
//!
//! What a withdrawal does:
//!
//! - **It removes the member from the folds.** The completeness scan, the
//!   candidate list, and the resolve gate are otherwise total over the sealed
//!   member list, which is exactly why one member that will never produce a
//!   claim pins the bloom, its siblings' finished work, and the mainline
//!   behind it.
//! - **It kills the lane and frees the ref.** [`Decision::CancelDispatch`]
//!   reaches the executor and [`Decision::ReleaseMemberClaimRef`] frees
//!   `refs/bloomery/claims/<workpiece>` alone — never the bloom's admission
//!   ref, which the still-walking bloom keeps.
//! - **It touches nothing else.** No sibling's claim is revoked, no budget is
//!   spent or handed back, no cursor of anyone else's moves, and the bloom
//!   does not un-resolve.
//!
//! And it is one-way. A member wrongly withdrawn is re-scoped and sealed into
//! a later bloom, which is precisely what releasing its claim ref makes
//! possible.
//!
//! **Dependents cascade or the door refuses.** A dependent of a withdrawn
//! member can never enter the line — its construct base will never exist — and
//! parking it would leave the bloom pinned by the very member the withdrawal
//! was meant to free. So a withdrawal that would strand dependents is refused
//! fail-closed, naming them, unless the request opts into the cascade; a
//! cascaded dependent leaves with a derived
//! [`WithdrawalCause::Dependency`], which is the visible reason rather than a
//! nameless wait.

use alloc::borrow::ToOwned as _;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::vec::Vec;

use super::integrate::adoption_source;
use super::readiness::dependents_of;
use super::{BloomRecord, BloomStatus, Decision, Decisions, Outcome, Snapshot, WithdrawError};
use crate::ids::{BloomId, WorkpieceId};
use crate::values::{MemberCandidate, Membership, Withdrawal, WithdrawalCause};

/// Reduce an operator withdrawal ([`Fact::Withdraw`](crate::Fact::Withdraw)).
///
/// The refusal ladder is checked in [`WithdrawError`]'s declaration order, so
/// the first thing wrong with a request is the thing the operator is told
/// about, and nothing is emitted until every named member has passed it — a
/// refusal journals no effects at all.
pub(super) fn reduce_withdraw(
    snapshot: &Snapshot,
    bloom: &BloomId,
    withdrawals: &[Withdrawal],
    cascade: bool,
) -> Decisions {
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Decisions::rejected(Outcome::WithdrawRejected(WithdrawError::UnknownOrInactiveBloom));
    };
    // Only a `Sealed` bloom is still running a line a member can leave. A
    // resolved bloom's members all carry claims, so every withdrawal against
    // one is already `AlreadyResolved`; naming the status refusal first keeps
    // the answer honest for a landed or superseded id too.
    if record.status != BloomStatus::Sealed {
        return Decisions::rejected(Outcome::WithdrawRejected(WithdrawError::UnknownOrInactiveBloom));
    }
    if withdrawals.is_empty() {
        return Decisions::rejected(Outcome::WithdrawRejected(WithdrawError::NoMembersNamed));
    }
    if let Some(error) = named_refusal(record, withdrawals) {
        return Decisions::rejected(Outcome::WithdrawRejected(error));
    }

    let roots: Vec<WorkpieceId> = withdrawals.iter().map(|withdrawal| withdrawal.workpiece.clone()).collect();
    let stranded = dependents_of(record, &roots);
    if !cascade && !stranded.is_empty() {
        let names = stranded.into_iter().map(|(dependent, _)| dependent).collect();
        return Decisions::rejected(Outcome::WithdrawRejected(WithdrawError::DependentsWouldStrand(names)));
    }

    // The operator-named members first, then each cascaded dependent in sealed
    // member order, so the journal reads in the order the decision was made
    // and a dependent's cause always sits behind the withdrawal that caused it.
    let mut leaving: Vec<Withdrawal> = withdrawals.to_vec();
    leaving.extend(stranded.into_iter().map(|(dependent, ancestor)| cascaded(withdrawals, dependent, &ancestor)));

    let departed: BTreeSet<&WorkpieceId> = leaving.iter().map(|withdrawal| &withdrawal.workpiece).collect();
    let mut effects = Vec::new();
    for withdrawal in &leaving {
        let workpiece = withdrawal.workpiece.clone();
        effects.push(Decision::RecordWithdrawal { bloom: *bloom, withdrawal: withdrawal.clone() });
        effects.push(Decision::CancelDispatch { bloom: *bloom, workpiece: workpiece.clone() });
        effects.push(Decision::ReleaseMemberClaimRef { bloom: *bloom, workpiece: workpiece.clone() });
        // Frees the workpiece in the in-journal `active` map so a later bloom
        // can seal it, exactly as `reduce_supersede` frees a dropped member.
        effects.push(Decision::ReleaseMembership { workpiece, bloom: *bloom });
    }

    // What is still in the line: the sealed list minus the members leaving in
    // this event, and minus the ones the record already shows gone. Both
    // subtractions, because a withdrawal is one member's exit and an operator
    // shedding a bloom sends them one request at a time. Filtering by
    // `departed` alone meant such a bloom never saw an empty remainder: it was
    // never marked terminal, kept reading `Sealed` with every member gone, and
    // went on holding the one-active-bloom-per-mainline slot — refusing the
    // next seal with `ActiveBloomExists` on behalf of a bloom with nothing left
    // in it (#5409). `record.withdrawn` is the same field `named_refusal`
    // reads to recognize an already-withdrawn member.
    let remaining: Vec<&Membership> = record
        .spec
        .members()
        .iter()
        .filter(|member| !departed.contains(&member.workpiece) && !record.withdrawn.contains_key(&member.workpiece))
        .collect();
    let terminal = remaining.is_empty();
    if terminal {
        // A bloom with no remaining member has no artifact to land, so it
        // stops holding the one-active-bloom-per-mainline slot rather than
        // dispatching a fold over an empty membership.
        effects.push(Decision::MarkBloomWithdrawn { bloom: *bloom });
    } else if remaining.iter().all(|member| record.claims.contains_key(&member.workpiece)) {
        // The withdrawal completed the claim set. Nothing later runs
        // `claim_effects` for this bloom, so without this the bloom would sit
        // on a full claim set that is never folded.
        let members: Vec<MemberCandidate> = remaining
            .iter()
            .filter_map(|member| {
                record
                    .claims
                    .get(&member.workpiece)
                    .map(|claim| MemberCandidate { workpiece: member.workpiece.clone(), candidate: claim.candidate })
            })
            .collect();
        let adopt_from = adoption_source(snapshot, *bloom, &members);
        effects.push(Decision::DispatchIntegration { bloom: *bloom, base: record.spec.base(), members, adopt_from });
    }

    Decisions {
        outcome: Outcome::MembersWithdrawn {
            bloom: *bloom,
            withdrawn: leaving.into_iter().map(|withdrawal| withdrawal.workpiece).collect(),
            terminal,
        },
        effects,
    }
}

/// The refusal the operator-named set earns, or `None` when every entry is
/// withdrawable. Checked over the whole set before anything is emitted, so a
/// request naming one bad member journals nothing for the good ones.
fn named_refusal(record: &BloomRecord, withdrawals: &[Withdrawal]) -> Option<WithdrawError> {
    let mut named: BTreeSet<&WorkpieceId> = BTreeSet::new();
    for withdrawal in withdrawals {
        let workpiece = &withdrawal.workpiece;
        if !record.spec.members().iter().any(|member| member.workpiece == *workpiece) {
            return Some(WithdrawError::NotAMember(workpiece.clone()));
        }
        if withdrawal.reason.trim().is_empty() {
            return Some(WithdrawError::BlankReason);
        }
        if withdrawal.operator.trim().is_empty() {
            return Some(WithdrawError::BlankOperator);
        }
        // A repeat inside one request is the same fact twice, and is refused
        // the way a second withdrawal of an already-withdrawn member is.
        if record.withdrawn.contains_key(workpiece) || !named.insert(workpiece) {
            return Some(WithdrawError::AlreadyWithdrawn(workpiece.clone()));
        }
        // A reviewed member is immutable (ADR-0191 §4): withdrawing it would
        // pull verified work out from under a fold that already counted it.
        if record.claims.contains_key(workpiece) {
            return Some(WithdrawError::AlreadyResolved(workpiece.clone()));
        }
    }
    None
}

/// The withdrawal a cascaded dependent leaves under: the ancestor that
/// stranded it, and the operator and words of the request that named that
/// ancestor.
///
/// The derived reason names the ancestor rather than restating the operator's
/// sentence alone, because what a reader of this member needs to know first is
/// that nobody decided anything about *it*.
fn cascaded(named: &[Withdrawal], dependent: WorkpieceId, ancestor: &WorkpieceId) -> Withdrawal {
    let cause = named.iter().find(|withdrawal| withdrawal.workpiece == *ancestor).or_else(|| named.first());
    let reason = cause.map_or_else(
        || format!("its dependency {} was withdrawn", ancestor.0),
        |cause| format!("its dependency {} was withdrawn: {}", ancestor.0, cause.reason),
    );
    Withdrawal {
        workpiece: dependent,
        cause: WithdrawalCause::Dependency { on: ancestor.clone() },
        reason,
        operator: cause.map_or_else(|| "operator".to_owned(), |cause| cause.operator.clone()),
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString as _};
    use alloc::vec;
    use alloc::vec::Vec;

    use super::reduce_withdraw;
    use crate::digest::Digest;
    use crate::ids::{BloomId, WorkpieceId};
    use crate::reduce::{Decision, Decisions, Fact, Outcome, Snapshot, WithdrawError, is_active_unlanded};
    use crate::testing::{claim, draft, event, membership, step};
    use crate::values::{MemberDependency, Withdrawal, WithdrawalCause};

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn workpiece(name: &str) -> WorkpieceId {
        WorkpieceId(name.into())
    }

    /// A sealed three-member bloom (`wp-a` at revision 1, `wp-b` at 2, `wp-c`
    /// at 3), optionally carrying declared `(member, depends_on)` edges.
    fn sealed(edges: &[(&str, &str)]) -> (Snapshot, BloomId) {
        let spec = draft(0, vec![membership("wp-a", 1), membership("wp-b", 2), membership("wp-c", 3)]).seal();
        let bloom = spec.id();
        let graph: Vec<MemberDependency> = edges
            .iter()
            .map(|(member, depends_on)| MemberDependency {
                member: workpiece(member),
                depends_on: workpiece(depends_on),
            })
            .collect();
        let fact = if graph.is_empty() {
            Fact::Seal(spec)
        } else {
            Fact::GraphSeal { predecessor: None, spec, edges: graph }
        };
        (step(&Snapshot::new(digest(0)).with_green_base(digest(0)), &event("seal", fact)).0, bloom)
    }

    /// Land a resolution claim on `name`, the way a passing terminal Verify does.
    fn integrate(snapshot: &Snapshot, bloom: BloomId, name: &str, revision: u8, candidate: u8) -> Snapshot {
        step(snapshot, &event(name, Fact::Integrate { bloom, claim: claim(name, revision, candidate) })).0
    }

    /// One member's withdrawal as its own fact — the shape an operator
    /// shedding a bloom a member at a time produces.
    fn withdraw(bloom: BloomId, name: &str) -> Fact {
        Fact::Withdraw { bloom, withdrawals: vec![withdrawal(name)], cascade: false }
    }

    fn withdrawal(name: &str) -> Withdrawal {
        Withdrawal {
            workpiece: workpiece(name),
            cause: WithdrawalCause::Operator,
            reason: "the scope was wrong".to_string(),
            operator: "ops".to_string(),
        }
    }

    fn decide(snapshot: &Snapshot, bloom: BloomId, withdrawals: &[Withdrawal], cascade: bool) -> Decisions {
        reduce_withdraw(snapshot, &bloom, withdrawals, cascade)
    }

    fn integration_members(decisions: &Decisions) -> Option<Vec<String>> {
        decisions.effects.iter().find_map(|effect| match effect {
            Decision::DispatchIntegration { members, .. } => {
                Some(members.iter().map(|member| member.workpiece.0.clone()).collect())
            }
            _ => None,
        })
    }

    #[test]
    fn a_withdrawal_that_completes_the_claim_set_dispatches_the_fold() {
        // The plausible bug: nothing later runs `claim_effects` for this bloom,
        // so a bloom whose last unresolved member is withdrawn would sit on a
        // full claim set that is never folded and never lands.
        let (snapshot, bloom) = sealed(&[]);
        let snapshot = integrate(&snapshot, bloom, "wp-a", 1, 10);
        let snapshot = integrate(&snapshot, bloom, "wp-b", 2, 11);

        let decisions = decide(&snapshot, bloom, &[withdrawal("wp-c")], false);
        assert!(
            matches!(decisions.outcome, Outcome::MembersWithdrawn { terminal: false, .. }),
            "{:?}",
            decisions.outcome
        );
        assert_eq!(
            integration_members(&decisions),
            Some(vec!["wp-a".to_string(), "wp-b".to_string()]),
            "the remaining claim set folds without the withdrawn member"
        );
    }

    #[test]
    fn a_withdrawn_members_candidate_is_absent_from_the_dispatched_fold() {
        // The plausible bug: the completeness scan can be taught about
        // withdrawal while the candidate list still merges the withdrawn
        // member's half-finished tree into the artifact.
        let (snapshot, bloom) = sealed(&[]);
        let snapshot = integrate(&snapshot, bloom, "wp-a", 1, 10);
        let snapshot = integrate(&snapshot, bloom, "wp-c", 3, 12);

        let decisions = decide(&snapshot, bloom, &[withdrawal("wp-b")], false);
        assert_eq!(integration_members(&decisions), Some(vec!["wp-a".to_string(), "wp-c".to_string()]));
    }

    #[test]
    fn a_withdrawal_that_would_strand_a_dependent_is_refused_and_journals_nothing() {
        // The plausible bug: a refusal that has already emitted effects, so the
        // named member leaves while the door reports it did not.
        let (snapshot, bloom) = sealed(&[("wp-c", "wp-b")]);

        let decisions = decide(&snapshot, bloom, &[withdrawal("wp-b")], false);
        assert_eq!(
            decisions.outcome,
            Outcome::WithdrawRejected(WithdrawError::DependentsWouldStrand(vec![workpiece("wp-c")]))
        );
        assert!(decisions.effects.is_empty(), "a refusal emits nothing: {:?}", decisions.effects);
    }

    #[test]
    fn a_cascade_withdraws_the_dependent_naming_its_ancestor() {
        let (snapshot, bloom) = sealed(&[("wp-c", "wp-b")]);

        let decisions = decide(&snapshot, bloom, &[withdrawal("wp-b")], true);
        let causes: Vec<(String, WithdrawalCause)> = decisions
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Decision::RecordWithdrawal { withdrawal, .. } => {
                    Some((withdrawal.workpiece.0.clone(), withdrawal.cause.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            causes,
            vec![
                ("wp-b".to_string(), WithdrawalCause::Operator),
                ("wp-c".to_string(), WithdrawalCause::Dependency { on: workpiece("wp-b") }),
            ],
            "the named member leads and the dependent names the ancestor that stranded it"
        );
    }

    #[test]
    fn a_resolved_member_cannot_be_withdrawn() {
        // The plausible bug: the ADR-0191 section 4 immutability hole —
        // withdrawing reviewed work out from under a fold that counted it.
        let (snapshot, bloom) = sealed(&[]);
        let snapshot = integrate(&snapshot, bloom, "wp-a", 1, 10);

        let decisions = decide(&snapshot, bloom, &[withdrawal("wp-a")], false);
        assert_eq!(decisions.outcome, Outcome::WithdrawRejected(WithdrawError::AlreadyResolved(workpiece("wp-a"))));
        assert!(decisions.effects.is_empty());
    }

    #[test]
    fn a_blank_reason_and_a_blank_operator_each_refuse() {
        // The plausible bug: the door defaults the audit trail, which is the
        // whole product of an act no verdict produced.
        let (snapshot, bloom) = sealed(&[]);

        let blank_reason = Withdrawal { reason: "   ".to_string(), ..withdrawal("wp-a") };
        assert_eq!(
            decide(&snapshot, bloom, &[blank_reason], false).outcome,
            Outcome::WithdrawRejected(WithdrawError::BlankReason)
        );

        let blank_operator = Withdrawal { operator: String::new(), ..withdrawal("wp-a") };
        assert_eq!(
            decide(&snapshot, bloom, &[blank_operator], false).outcome,
            Outcome::WithdrawRejected(WithdrawError::BlankOperator)
        );
    }

    #[test]
    fn withdrawing_every_member_marks_the_bloom_terminal_and_dispatches_no_fold() {
        // The plausible bug: a terminal bloom that still dispatches a fold (and
        // then a land) against an empty membership.
        let (snapshot, bloom) = sealed(&[]);
        let all = [withdrawal("wp-a"), withdrawal("wp-b"), withdrawal("wp-c")];

        let decisions = decide(&snapshot, bloom, &all, false);
        assert!(
            matches!(decisions.outcome, Outcome::MembersWithdrawn { terminal: true, .. }),
            "{:?}",
            decisions.outcome
        );
        assert!(
            decisions.effects.iter().any(|effect| matches!(effect, Decision::MarkBloomWithdrawn { .. })),
            "an emptied bloom is marked terminal"
        );
        assert_eq!(integration_members(&decisions), None, "there is no artifact left to fold");

        let withdrawn = event("withdraw", Fact::Withdraw { bloom, withdrawals: all.to_vec(), cascade: false });
        let next = step(&snapshot, &withdrawn).0;
        assert!(
            !is_active_unlanded(next.blooms[&bloom].status),
            "a fully-withdrawn bloom frees the one-active-bloom slot"
        );
        assert!(
            next.blooms[&bloom].progress.is_empty(),
            "every withdrawn member's cursor is dropped, so the doctor's non-terminal walk skips it"
        );
    }

    #[test]
    fn a_bloom_withdrawn_one_member_at_a_time_still_reaches_terminal() {
        // The plausible bug, and the one that was live: `remaining` filtered
        // the sealed member list by the withdrawals in *this* event alone, so a
        // bloom emptied one member per request never saw an empty remainder. It
        // never got `MarkBloomWithdrawn`, kept reading `Sealed` with every
        // member gone, and went on holding the one-active-bloom-per-mainline
        // slot — refusing the next seal with `ActiveBloomExists` on behalf of a
        // bloom with nothing left in it. Live on bloom 4360e7e4a081: all six
        // members withdrawn, still reading `Sealed` (#5409).
        let (mut snapshot, bloom) = sealed(&[]);

        for name in ["wp-a", "wp-b"] {
            let (next, decided) = step(&snapshot, &event(name, withdraw(bloom, name)));
            assert!(
                matches!(decided.outcome, Outcome::MembersWithdrawn { terminal: false, .. }),
                "{name} leaves siblings behind: {:?}",
                decided.outcome
            );
            snapshot = next;
        }

        let (next, decided) = step(&snapshot, &event("wp-c", withdraw(bloom, "wp-c")));
        assert!(
            matches!(decided.outcome, Outcome::MembersWithdrawn { terminal: true, .. }),
            "the last member out empties the bloom: {:?}",
            decided.outcome
        );
        assert!(
            decided.effects.iter().any(|effect| matches!(effect, Decision::MarkBloomWithdrawn { .. })),
            "an emptied bloom is marked terminal however many events emptied it"
        );
        assert_eq!(integration_members(&decided), None, "there is no artifact left to fold");
        assert!(
            !is_active_unlanded(next.blooms[&bloom].status),
            "and the slot is freed, so the next seal is not refused by a bloom with no members"
        );
    }

    #[test]
    fn a_withdrawal_spends_no_attempt_and_wedges_nothing() {
        // Tripwire: a withdrawal must never reach the attempt vocabulary. An
        // AdvanceStage or a DispatchAttempt here would make it a retry grant
        // wearing a different name, and a RecordWedge would spend a budget the
        // member is no longer running against (#5327).
        let (snapshot, bloom) = sealed(&[]);
        let decisions = decide(&snapshot, bloom, &[withdrawal("wp-a")], false);
        assert!(
            !decisions.effects.iter().any(|effect| matches!(
                effect,
                Decision::AdvanceStage { .. } | Decision::DispatchAttempt { .. } | Decision::RecordWedge { .. }
            )),
            "{:?}",
            decisions.effects
        );
    }
}
