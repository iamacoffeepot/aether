//! Why one bloom is not advancing (#5281).
//!
//! When the reducer emits no decision for a transition, exactly one guard
//! returned early, and on 2026-08-19 recovering which one took an hour of
//! reading the reducer, the reactors, and the journal by hand. This is the
//! request that replaces the hour.
//!
//! # It reads; it does not re-derive
//!
//! ADR-0206 settled the shape: a served explanation reads **stored facts**, and
//! only a hypothetical question re-runs a decision. So every rung of the chain
//! below is a projection of record fields the reducer already wrote, plus the
//! two pure predicates the rest of the projection already calls —
//! [`blocking_ancestor`] for a declared edge and
//! [`at_park_ceiling`] for a spent
//! aggregate budget. Nothing here evaluates a guard, and nothing here restates
//! one.
//!
//! That distinction is load-bearing rather than pedantic. A hand-written
//! account of *why a guard would refuse* is a second description of the
//! decision path: it stops matching the code and then answers confidently and
//! wrongly, which is worse for an operator than silence. What a `because`
//! sentence here says is only ever what a stored field holds — "no integration
//! is recorded", "member `wp-a` is wedged at Verify" — so it cannot drift
//! away from the truth without the field drifting with it.
//!
//! Where a boundary has recorded an ADR-0206 refusal, that refusal rides the
//! rung verbatim — gate, guard, and the values the guard read. Every boundary
//! in the chain is converted (#5289), and each carries its own stored refusal
//! when it holds one. A rung with none reports its stored state and
//! `refusal: None`; no rung is ever given a fabricated stand-in for a refusal
//! it does not hold, which is the whole distinction above.
//!
//! # The answer nests
//!
//! Not landing because no integration is recorded; no integration because the
//! fold refused; the fold refused because adoption found no candidate ref for
//! this member. [`WhyDocument::chain`] runs outermost-first and each rung names
//! the one below it in `waiting_on`, so the reader follows it down instead of
//! re-assembling a flat list. A stalled member and a stalled composition have
//! different causes, so [`WhyDocument::members`] answers per member beside it.

use alloc::borrow::ToOwned as _;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::aggregate_verify::at_park_ceiling;
use super::readiness::blocking_ancestor;
use super::{BloomRecord, BloomStatus, Snapshot};
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::port::{MemberWhy, TransitionWhy, WhyDocument, WhyState};

/// The transition names, spelled once. They are the machinery's own words for
/// these boundaries — the same five ADR-0206 names as its operator-visible set
/// — so a reader can grep a rung's name and land on the code.
const LAND: &str = super::gate::LAND_GATE;
const AGGREGATE_REVIEW: &str = super::gate::AGGREGATE_REVIEW_GATE;
const AGGREGATE_VERIFY: &str = super::gate::AGGREGATE_VERIFY_GATE;
const FOLD: &str = super::gate::FOLD_GATE;
const DISPATCH_MEMBER: &str = super::gate::DISPATCH_MEMBER_GATE;

/// Why `bloom` is not advancing, or `None` when no such bloom is known.
#[must_use]
pub fn why_of(snapshot: &Snapshot, bloom: &BloomId) -> Option<WhyDocument> {
    let record = snapshot.blooms.get(bloom)?;
    let members = member_answers(snapshot, record, *bloom);

    let dispatch = dispatch_rung(record, &members);
    let fold = fold_rung(snapshot, record, *bloom, &dispatch);
    let verify = stored_refusal(
        aggregate_rung(record, AGGREGATE_VERIFY, StageId::AggregateVerify, record.aggregate_verify_rolls, &fold),
        snapshot,
        bloom,
    );
    let review = stored_refusal(
        aggregate_rung(record, AGGREGATE_REVIEW, StageId::AggregateReview, record.aggregate_rolls, &fold),
        snapshot,
        bloom,
    );
    let land = stored_refusal(land_rung(record, &verify, &review), snapshot, bloom);

    Some(WhyDocument {
        bloom: *bloom,
        status: record.status,
        chain: vec![land, review, verify, fold, dispatch],
        members,
    })
}

/// One answer per sealed member, in sealed order.
///
/// The ladder is ordered by how terminal the stop is, so a member carrying two
/// of them is reported by the one an operator would act on: a withdrawal and a
/// wedge are over, a park or an amendment needs a person, an eviction needs a
/// sibling, and a declared edge needs an ancestor. A member with none of them
/// and a cursor is simply working.
fn member_answers(snapshot: &Snapshot, record: &BloomRecord, bloom: BloomId) -> Vec<MemberWhy> {
    record
        .spec
        .members()
        .iter()
        .map(|member| {
            let workpiece = &member.workpiece;
            let (state, because) = member_state(snapshot, record, bloom, workpiece);
            MemberWhy {
                workpiece: workpiece.clone(),
                state,
                because,
                blocked_by: blocking_ancestor(record, workpiece),
                refusal: snapshot.member_refusal(&bloom, workpiece).cloned(),
            }
        })
        .collect()
}

fn member_state(
    snapshot: &Snapshot,
    record: &BloomRecord,
    bloom: BloomId,
    workpiece: &WorkpieceId,
) -> (WhyState, String) {
    if record.claims.contains_key(workpiece) {
        return (WhyState::Done, "integrated; its resolution claim is recorded".to_owned());
    }
    if let Some(withdrawal) = record.withdrawn.get(workpiece) {
        return (WhyState::Blocked, format!("withdrawn from the line by {}", withdrawal.operator));
    }
    if let Some(wedge) = record.wedged.get(workpiece) {
        return (WhyState::Blocked, format!("wedged at {:?}; its retry budget is spent", wedge.stage));
    }
    if record.host_faults.contains_key(workpiece) {
        return (WhyState::Blocked, "held at Verify because the host could not run the gates".to_owned());
    }
    if let Some(park) = snapshot.member_park(&bloom, workpiece) {
        return (WhyState::Blocked, format!("its {:?} declined without a candidate", park.stage));
    }
    if snapshot.awaiting_surface(&bloom, workpiece).is_some() {
        return (WhyState::Blocked, "waiting on an operator to widen its declared surface".to_owned());
    }
    if let Some(eviction) = snapshot.lease_eviction(&bloom, workpiece) {
        return (
            WhyState::Blocked,
            format!("evicted off {} by {}; it re-dispatches when that member integrates", eviction.path, eviction.by.0),
        );
    }
    if record.deferred_dispatches.contains(workpiece) {
        return (WhyState::Blocked, "its dispatch is deferred by the operator brake".to_owned());
    }
    if let Some(ancestor) = blocking_ancestor(record, workpiece) {
        return (WhyState::Blocked, format!("waiting on the declared dependency {}", ancestor.0));
    }
    if let Some(cursor) = record.progress.get(workpiece) {
        return (WhyState::InFlight, format!("attempt {} of {:?} is out", cursor.attempts, cursor.stage));
    }
    (WhyState::Blocked, "never entered the line and no ancestor explains it".to_owned())
}

/// Member dispatch: done once every member that has not left the line carries a
/// claim, in flight while any member's attempt is out, blocked otherwise.
///
/// The blocked case names the *first* blocked member in sealed order rather
/// than all of them, because the chain is what an operator reads first and the
/// per-member list beside it already holds the rest.
fn dispatch_rung(record: &BloomRecord, members: &[MemberWhy]) -> TransitionWhy {
    if let Some(hold) = operator_hold(record) {
        return rung(DISPATCH_MEMBER, WhyState::Blocked, hold, None);
    }
    let live = || members.iter().filter(|member| !record.withdrawn.contains_key(&member.workpiece));
    if live().all(|member| member.state == WhyState::Done) {
        return rung(DISPATCH_MEMBER, WhyState::Done, "every member carries a resolution claim".to_owned(), None);
    }
    if let Some(blocked) = live().find(|member| member.state == WhyState::Blocked) {
        return rung(
            DISPATCH_MEMBER,
            WhyState::Blocked,
            format!("member {} is {}", blocked.workpiece.0, blocked.because),
            None,
        );
    }
    rung(DISPATCH_MEMBER, WhyState::InFlight, "one or more member attempts are out".to_owned(), None)
}

/// The fold: done once an integration is recorded, refused when the fold's own
/// ADR-0206 gate stopped it, otherwise waiting on member dispatch.
///
/// This is the one rung that can carry a real [`super::RecordedRefusal`] today, and it
/// is the exact answer the 2026-08-19 hour was spent recovering by hand.
fn fold_rung(snapshot: &Snapshot, record: &BloomRecord, bloom: BloomId, dispatch: &TransitionWhy) -> TransitionWhy {
    if record.integration.is_some() {
        return rung(FOLD, WhyState::Done, "an integration is recorded for this bloom".to_owned(), None);
    }
    if let Some(refusal) = snapshot.fold_refusal(&bloom) {
        let mut refused = rung(FOLD, WhyState::Refused, format!("{} refused at {}", refusal.gate, refusal.guard), None);
        refused.refusal = Some(refusal.clone());
        return refused;
    }
    if dispatch.state == WhyState::Done {
        return rung(FOLD, WhyState::InFlight, "the integration is dispatched and has not reported".to_owned(), None);
    }
    rung(
        FOLD,
        WhyState::Blocked,
        "no integration is recorded and not every member has resolved".to_owned(),
        Some(DISPATCH_MEMBER.to_owned()),
    )
}

/// One aggregate gate: done once it has passed on the held fold, blocked at its
/// park ceiling, otherwise waiting on the fold.
///
/// `rolls` is the gate's own roll counter — the two gates keep separate ones,
/// and reading the wrong one is how a bloom parked at the verify ceiling
/// reports a healthy review.
fn aggregate_rung(
    record: &BloomRecord,
    name: &'static str,
    stage: StageId,
    rolls: u32,
    fold: &TransitionWhy,
) -> TransitionWhy {
    if record.aggregate_passed.contains(&stage) {
        return rung(name, WhyState::Done, "this gate has passed on the fold currently held".to_owned(), None);
    }
    if let Some(hold) = operator_hold(record) {
        return rung(name, WhyState::Blocked, hold, None);
    }
    if record.review_park.is_some() && stage == StageId::AggregateReview {
        return rung(
            name,
            WhyState::Blocked,
            "the review parked on a question an operator must settle".to_owned(),
            None,
        );
    }
    if at_park_ceiling(record, stage, rolls) {
        return rung(name, WhyState::Blocked, format!("{rolls} rolls have spent this gate's sealed budget"), None);
    }
    if fold.state == WhyState::Done {
        return rung(name, WhyState::InFlight, format!("dispatched against the held fold after {rolls} rolls"), None);
    }
    rung(name, WhyState::Blocked, "there is no folded tree to judge".to_owned(), Some(FOLD.to_owned()))
}

/// The land: done once the bloom is landed, in flight once it is resolved,
/// otherwise waiting on whichever aggregate gate has not passed.
fn land_rung(record: &BloomRecord, verify: &TransitionWhy, review: &TransitionWhy) -> TransitionWhy {
    if record.status == BloomStatus::Landed {
        return rung(LAND, WhyState::Done, "mainline advanced onto this bloom's head".to_owned(), None);
    }
    if record.landing_rolls > 0 {
        return rung(
            LAND,
            WhyState::Blocked,
            format!("{} landing attempts have been refused", record.landing_rolls),
            None,
        );
    }
    if record.status == BloomStatus::Resolved {
        return rung(LAND, WhyState::InFlight, "the bloom is resolved and its land is dispatched".to_owned(), None);
    }
    if let Some(gate) = [verify, review].into_iter().find(|gate| gate.state != WhyState::Done) {
        return rung(
            LAND,
            WhyState::Blocked,
            format!("the bloom is not resolved; {} has not passed", gate.transition),
            Some(gate.transition.clone()),
        );
    }
    rung(LAND, WhyState::Blocked, "both aggregate gates passed but the bloom is not resolved".to_owned(), None)
}

/// The operator brake, worded once. It stops dispatch at every rung, so the
/// sentence has to read the same wherever it surfaces.
fn operator_hold(record: &BloomRecord) -> Option<String> {
    record.operator_hold.as_ref().map(|hold| format!("the bloom is on the operator brake, held by {}", hold.operator))
}

fn rung(transition: &'static str, state: WhyState, because: String, waiting_on: Option<String>) -> TransitionWhy {
    TransitionWhy { transition: transition.to_owned(), state, because, refusal: None, waiting_on }
}

/// Attach the ADR-0206 refusal this boundary recorded, when one still stands.
///
/// A stored refusal is stronger evidence than the rung's own reading of the
/// record: the rung infers state from stored fields, and the refusal *is* the
/// decision that produced that state, with the values the guard consulted. So
/// it overrides the state to [`WhyState::Refused`] and speaks in the gate's own
/// words rather than the projection's paraphrase.
///
/// It cannot contradict a completed transition, because the fold that records a
/// boundary's dispatch drops that boundary's refusal in the same breath — the
/// rung and the refusal are two readings of one write.
fn stored_refusal(rung: TransitionWhy, snapshot: &Snapshot, bloom: &BloomId) -> TransitionWhy {
    let Some(refusal) = snapshot.refusal(bloom, &rung.transition) else {
        return rung;
    };
    TransitionWhy {
        state: WhyState::Refused,
        because: format!("{} refused at {}", refusal.gate, refusal.guard),
        refusal: Some(refusal.clone()),
        ..rung
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use alloc::string::ToString as _;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{FOLD, why_of};
    use crate::digest::Digest;
    use crate::ids::{BloomId, IdempotencyKey, WorkpieceId};
    use crate::port::{TransitionWhy, WhyState};
    use crate::reduce::gate::{RecordedRead, RecordedRefusal};
    use crate::reduce::{Event, Fact, Snapshot, reduce};
    use crate::testing::{draft, membership};
    use crate::values::{MemberDependency, ResolvedConfigs, SpendWindow};

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn sealed(edges: &[MemberDependency]) -> (Snapshot, BloomId) {
        let spec = draft(0, vec![membership("wp-a", 1), membership("wp-b", 2)]).seal();
        let bloom = spec.id();
        let snapshot = Snapshot::new(digest(0)).with_green_base(digest(0));
        let seal = Event {
            idempotency_key: IdempotencyKey("seal".into()),
            fact: Fact::GraphSeal { predecessor: None, spec, edges: edges.to_vec() },
        };
        let decided = reduce(&snapshot, &seal, &ResolvedConfigs::default(), &SpendWindow::default());
        (snapshot.apply(&seal, &decided, &ResolvedConfigs::default()), bloom)
    }

    fn rung<'a>(chain: &'a [TransitionWhy], name: &str) -> &'a TransitionWhy {
        chain.iter().find(|rung| rung.transition == name).expect("every chain carries every rung")
    }

    #[test]
    fn a_refused_fold_reports_the_refusal_and_the_values_it_read() {
        // The 2026-08-19 defect exactly: the bloom reads Sealed with every
        // blocker field null, and the sentence naming the member and the reason
        // had been constructed, matched against, and dropped.
        let (snapshot, bloom) = sealed(&[]);
        let refusal = RecordedRefusal {
            gate: "fold".to_string(),
            guard: "candidate_ref_present".to_string(),
            reads: vec![RecordedRead { field: "member".to_string(), value: "wp-a".to_string() }],
        };
        let refused = Event {
            idempotency_key: IdempotencyKey("refused".into()),
            fact: Fact::FoldRefused { bloom, refusal: refusal.clone() },
        };
        let decided = reduce(&snapshot, &refused, &ResolvedConfigs::default(), &SpendWindow::default());
        let snapshot = snapshot.apply(&refused, &decided, &ResolvedConfigs::default());

        let document = why_of(&snapshot, &bloom).expect("the bloom is known");
        let fold = rung(&document.chain, FOLD);

        assert_eq!(fold.state, WhyState::Refused);
        assert_eq!(fold.refusal.as_ref().expect("the stored refusal rides the rung"), &refusal);
        assert!(fold.because.contains("candidate_ref_present"), "{}", fold.because);
    }

    #[test]
    fn a_member_waiting_on_a_declared_edge_names_the_dependency() {
        let (snapshot, bloom) =
            sealed(&[MemberDependency { member: WorkpieceId("wp-b".into()), depends_on: WorkpieceId("wp-a".into()) }]);

        let document = why_of(&snapshot, &bloom).expect("the bloom is known");
        let waiting =
            document.members.iter().find(|member| member.workpiece.0 == "wp-b").expect("the dependent is a member");

        assert_eq!(waiting.blocked_by.as_ref().map(|id| id.0.as_str()), Some("wp-a"));
        assert!(waiting.because.contains("wp-a"), "{}", waiting.because);
    }

    #[test]
    fn a_bloom_that_is_advancing_reports_the_transition_in_flight_rather_than_a_refusal() {
        // A freshly sealed bloom has both members dispatched and nothing
        // refused. Reporting a blocker here would be the confident lie the
        // whole design exists to avoid.
        let (snapshot, bloom) = sealed(&[]);

        let document = why_of(&snapshot, &bloom).expect("the bloom is known");

        assert_eq!(rung(&document.chain, super::DISPATCH_MEMBER).state, WhyState::InFlight);
        assert!(document.chain.iter().all(|rung| rung.refusal.is_none()), "nothing refused yet");
        assert!(document.members.iter().all(|member| member.state == WhyState::InFlight));
    }

    #[test]
    fn the_chain_names_the_rung_below_it_rather_than_repeating_the_cause() {
        // The nesting is the point: land -> the gate that has not passed ->
        // the fold -> member dispatch. A flat list makes the reader re-derive
        // the order, which is the hour this route replaces.
        let (snapshot, bloom) = sealed(&[]);

        let document = why_of(&snapshot, &bloom).expect("the bloom is known");
        let names: Vec<&str> = document.chain.iter().map(|rung| rung.transition.as_str()).collect();

        assert_eq!(names, vec!["land", "aggregate_review", "aggregate_verify", "fold", "dispatch_member"]);
        assert_eq!(rung(&document.chain, FOLD).waiting_on.as_deref(), Some(super::DISPATCH_MEMBER));
        assert_eq!(rung(&document.chain, super::AGGREGATE_VERIFY).waiting_on.as_deref(), Some(FOLD));
        assert!(rung(&document.chain, "land").waiting_on.is_some());
    }

    #[test]
    fn an_unknown_bloom_answers_nothing_rather_than_an_empty_chain() {
        let (snapshot, _) = sealed(&[]);

        assert!(why_of(&snapshot, &BloomId(digest(0xAB))).is_none());
    }
}
