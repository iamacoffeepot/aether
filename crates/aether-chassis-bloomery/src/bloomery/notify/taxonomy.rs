//! What counts as loud, what counts as a milestone, and what each one says
//! (#5166, #5457).
//!
//! # This is a second copy of the war room's taxonomy, and that is a known cost
//!
//! The console computes the same set — `warroom::alerts` and
//! `warroom::interrupts` — and the issue asks for one definition of "loud" so
//! the unattended channel and the attended board cannot drift. They are two
//! definitions here, because the console's functions take the *console's* own
//! dto mirror of the view document, not [`ViewDocument`], and the coordinator
//! chassis cannot depend on `aether-bloomery-console` — that is a manifest
//! change on a crate whose approval tier is `human`, not something a slice
//! decides.
//!
//! So this walk is written against the port types the console's dto mirrors
//! field-for-field, and the drift risk is real: a field added to
//! [`MemberView`] that the console starts shouting about will stay silent here
//! until someone adds it below. The honest fix is to lift the taxonomy into
//! `aether-bloomery` over the port types and have the console call it — one
//! definition, both readers — and that is its own piece of work, not this one.
//!
//! The milestone half below has no console counterpart at all: the board shows
//! progress by *being* a board, so the war room never had to name "this member
//! just resolved" as an event. Lifting the taxonomy therefore has to carry two
//! volumes, not one — which is a reason to do it deliberately rather than as a
//! side effect of this walk.
//!
//! # Loud is a *set*, not a stream
//!
//! Every function here is pure in the document: it answers "what is true right
//! now", and the reactor turns that into transitions by differencing against
//! what it has already said. That is why nothing here needs an event stream, a
//! topic, or a producer — the projection already carries the whole answer, and
//! the same document produces the same keys forever.
//!
//! # Two volumes over one walk
//!
//! A **loud** event is an unanswered condition: something has stopped and a
//! person is owed the news. A **milestone** is the line working — a bloom
//! entering the line, a member integrating. Both are read off the same
//! document in the same pass and both carry the same stable-key discipline;
//! they differ only in whether an unconfigured operator hears them, which is
//! [`NotifyConfig::milestones`](super::NotifyConfig)'s single job.
//!
//! Bloom lifecycle (sealed, landed, superseded, withdrawn) stays [`Volume::Loud`]
//! even though it reads like a milestone: those four are the terminal facts
//! about a bloom's existence, they arrive at most once each, and an operator
//! who has turned milestones off still wants to be told that the thing he
//! sealed has landed.

use aether_bloomery::{BloomStatus, BloomView, MemberView, SpendQuiesce, ViewDocument};
use aether_bloomery_github::short_hex;

/// How much of the taxonomy an event belongs to — the axis the operator's
/// milestone knob selects on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Volume {
    /// An unanswered condition: something has stopped and a person is owed the
    /// news. Posted whatever the configuration says.
    Loud,
    /// The line working. Posted only when the operator has asked for
    /// milestones.
    Milestone,
}

/// One reportable fact, with the key that identifies it across polls, the
/// message an operator reads, and how loud it is.
///
/// The key is what makes the channel idempotent, so it must name the
/// *condition* and nothing else — no timestamp, no poll counter, no rendering
/// detail. Where a condition genuinely recurs with a count (a landing refused
/// twice, a fault series rolling), the count is part of the key: each refusal
/// is its own transition, and an operator who saw the first is owed the second.
/// A count that is *progress* rather than identity — how many siblings have
/// resolved alongside this one — belongs in the message and never in the key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NotifyEvent {
    /// The stable identity of this condition.
    pub key: String,
    /// The plain-text message posted for it.
    pub message: String,
    /// Whether an operator who has not asked for milestones hears it.
    pub volume: Volume,
}

impl NotifyEvent {
    fn loud(key: String, message: String) -> Self {
        Self { key, message, volume: Volume::Loud }
    }

    fn milestone(key: String, message: String) -> Self {
        Self { key, message, volume: Volume::Milestone }
    }
}

/// Every reportable fact in `view`, document then bloom then member, in
/// document order, at both volumes.
///
/// The order is the order messages are posted in, so a reader sees the
/// document-wide stop before the bloom it stopped and the bloom before its
/// members. Filtering by [`Volume`] is the caller's job — the ledger differences
/// the whole set, so flipping the knob changes what is *said*, never what is
/// *known*.
#[must_use]
pub fn notify_events(view: &ViewDocument) -> Vec<NotifyEvent> {
    let mut events = Vec::new();
    if let Some(quiesce) = &view.spend_quiesce {
        events.push(quiesce_event(quiesce));
    }
    for bloom in &view.blooms {
        push_bloom_events(&mut events, bloom);
    }
    events
}

/// The spend door closing is document-wide: nothing seals or dispatches until
/// a person raises the ceiling, so it leads the message order.
fn quiesce_event(quiesce: &SpendQuiesce) -> NotifyEvent {
    match quiesce {
        SpendQuiesce::Window { window, spent_micro_usd, ceiling_micro_usd } => NotifyEvent::loud(
            format!("quiesce:window:{window}"),
            format!("quiesce  window {window} spent {spent_micro_usd} of {ceiling_micro_usd} micro-usd"),
        ),
        SpendQuiesce::Bloom { window, bloom, spent_micro_usd, ceiling_micro_usd } => NotifyEvent::loud(
            format!("quiesce:bloom:{}", short_hex(&bloom.0)),
            format!(
                "quiesce  bloom {} in window {window} spent {spent_micro_usd} of {ceiling_micro_usd} micro-usd",
                short_hex(&bloom.0)
            ),
        ),
    }
}

fn push_bloom_events(events: &mut Vec<NotifyEvent>, bloom: &BloomView) {
    let id = short_hex(&bloom.id.0);
    if let Some(status) = lifecycle_line(bloom, &id) {
        events.push(NotifyEvent::loud(format!("status:{id}:{:?}", bloom.status), status));
    }
    if let Some(park) = &bloom.review_park {
        events.push(NotifyEvent::loud(
            format!("park:{id}"),
            format!("park  bloom {id} is held on aggregate-review question {}", short_hex(&park.question)),
        ));
    }
    if let Some(block) = &bloom.landing_blocked {
        // The roll count is in the key: each refusal is its own transition, and
        // an operator who was told about the first is owed the one that spent
        // the budget.
        events.push(NotifyEvent::loud(
            format!("landing:{id}:{}", block.rolls),
            format!("landing  bloom {id} refused {} of {} landing attempts", block.rolls, block.budget),
        ));
    }
    if let Some(fault) = &bloom.executor_fault {
        let terminal = if fault.terminal {
            "; terminal, recovery is a successor"
        } else {
            ""
        };
        events.push(NotifyEvent::loud(
            format!("fault:{id}:{}", fault.rolls),
            format!(
                "host fault  bloom {id} could not judge its fold, {} of {} rolls{terminal}",
                fault.rolls, fault.budget
            ),
        ));
    }
    if let Some(hold) = &bloom.operator_hold {
        events.push(NotifyEvent::loud(format!("hold:{id}"), format!("hold  bloom {id} is frozen: {}", hold.reason)));
    }
    if let Some(composition) = &bloom.composition {
        if !composition.findings.is_empty() {
            events.push(NotifyEvent::loud(
                format!("findings:{id}:{}", composition.findings.len()),
                format!("findings  bloom {id}'s composition has {} open finding(s)", composition.findings.len()),
            ));
        }
        if let Some(wedge) = &composition.wedge {
            events.push(NotifyEvent::loud(
                format!("composition_wedge:{id}"),
                format!(
                    "wedge  bloom {id}'s composition wedged at {:?}; evidence {}",
                    wedge.stage,
                    short_hex(&wedge.evidence)
                ),
            ));
        }
    }
    // The first dispatch is one transition per bloom, so the key names the
    // bloom alone: an operator is owed "it started", not a running count of
    // who is in flight, which the board already renders better than a line of
    // text could.
    if bloom.members.iter().any(|member| member.cursor.is_some()) {
        events.push(NotifyEvent::milestone(format!("dispatch:{id}"), format!("dispatch  bloom {id} entered the line")));
    }

    // Withdrawn members are out of both counts. They left the line and will
    // never resolve, so counting them in the denominator would make the
    // progress line stall short of its own total forever — the one way a
    // progress report can be worse than no progress report.
    let in_line = bloom.members.iter().filter(|member| member.withdrawn.is_none());
    let progress = Progress {
        resolved: in_line.clone().filter(|member| member.resolution.is_some()).count(),
        total: in_line.count(),
    };
    for member in &bloom.members {
        push_member_events(events, &id, member, progress);
    }
}

/// How far a bloom's membership has got, carried into each resolution
/// milestone so one line answers "and how many are left" without a second
/// batched message that would need a key of its own.
#[derive(Clone, Copy)]
struct Progress {
    /// Members carrying a resolution claim.
    resolved: usize,
    /// Members admitted to the bloom.
    total: usize,
}

/// The one-line lifecycle report for a bloom that has reached a status worth
/// waking someone for, or `None` for one that is simply working.
///
/// `Resolved` is deliberately quiet: it means the artifact exists and the land
/// is next, which is the line working rather than the line stopping. The land
/// itself is the message.
///
/// The landed head digest the issue asks for is **not** here, because
/// [`BloomView`] does not carry one — the document names the bloom and its
/// membership, and the head lives on the landing receipt the mirror reactor
/// routes. Reporting the member count without inventing a head is the honest
/// half.
fn lifecycle_line(bloom: &BloomView, id: &str) -> Option<String> {
    let members = bloom.members.len();
    match bloom.status {
        BloomStatus::Sealed => Some(format!("sealed  bloom {id} with {members} member(s)")),
        BloomStatus::Landed => Some(format!("landed  bloom {id} with {members} member(s)")),
        BloomStatus::Superseded => Some(format!(
            "superseded  bloom {id} by {}",
            bloom.superseded_by.as_ref().map_or_else(|| "an unnamed successor".to_owned(), |next| short_hex(&next.0))
        )),
        BloomStatus::Withdrawn => Some(format!("withdrawn  bloom {id}; every member left the line")),
        BloomStatus::Resolved => None,
    }
}

fn push_member_events(events: &mut Vec<NotifyEvent>, bloom: &str, member: &MemberView, progress: Progress) {
    // A withdrawn member raises nothing, exactly as the war room has it: an
    // operator decided it, so there is no unanswered condition, and a wedge it
    // carried on the way out is history rather than a live stop.
    if member.withdrawn.is_some() {
        return;
    }
    let workpiece = &member.workpiece.0;
    if let Some(wedge) = &member.wedge {
        let cause = member.wedge_cause.map_or_else(String::new, |cause| format!(" ({cause:?})"));
        events.push(NotifyEvent::loud(
            format!("wedge:{bloom}:{workpiece}"),
            format!(
                "wedge  {workpiece} in bloom {bloom} wedged at {:?}{cause}; evidence {}",
                wedge.stage,
                short_hex(&wedge.evidence)
            ),
        ));
    }
    if let Some(fault) = &member.host_fault {
        events.push(NotifyEvent::loud(
            format!("host_fault:{bloom}:{workpiece}"),
            format!("host fault  {workpiece} in bloom {bloom} cannot run its gates: {}", fault.findings),
        ));
    }
    if let Some(pending) = &member.pending_decision {
        events.push(NotifyEvent::loud(
            format!("decision:{bloom}:{workpiece}"),
            format!("decision  {workpiece} in bloom {bloom} is held at {:?}: {}", pending.stage, pending.prompt),
        ));
    }
    if let Some(park) = &member.park {
        events.push(NotifyEvent::loud(
            format!("member_park:{bloom}:{workpiece}"),
            format!(
                "park  {workpiece} in bloom {bloom} declined at {:?}; evidence {}",
                park.stage,
                short_hex(&park.evidence)
            ),
        ));
    }
    if let Some(awaiting) = &member.awaiting_surface {
        events.push(NotifyEvent::loud(
            format!("surface:{bloom}:{workpiece}"),
            format!(
                "surface  {workpiece} in bloom {bloom} needs {} more path(s) declared: {}",
                awaiting.paths.len(),
                awaiting.summary
            ),
        ));
    }

    // Integration is the one member transition the loud set has no reason to
    // carry and an operator most wants on a healthy night. The sibling count
    // rides in the message rather than the key, so the line reads as progress
    // while still posting exactly once per member.
    if member.resolution.is_some() {
        events.push(NotifyEvent::milestone(
            format!("resolved:{bloom}:{workpiece}"),
            format!(
                "resolved  {workpiece} in bloom {bloom} integrated ({} of {} member(s) resolved)",
                progress.resolved, progress.total
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery::testing::digest;
    use aether_bloomery::{
        BloomId, BloomStatus, BloomView, HostFaultView, LandingBlock, MemberPark, MemberView, StageId,
        VerifyFailureSet, ViewDocument, Wedge, WedgeCause, WithdrawnView, WorkpieceId,
    };

    use super::notify_events;

    fn member(workpiece: &str) -> MemberView {
        MemberView { workpiece: WorkpieceId(workpiece.to_owned()), ..MemberView::default() }
    }

    fn wedge() -> Wedge {
        Wedge { stage: StageId::Verify, evidence: digest(9), repeated_verifiers: VerifyFailureSet::default() }
    }

    #[test]
    fn every_loud_field_reaches_a_keyed_event() {
        // The plausible bug: the walk only looks at bloom-level fields, so a
        // wedged or host-faulted member never wakes anyone — the exact silence
        // the unattended channel exists to close.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: BloomId(digest(0xab)),
                status: BloomStatus::Sealed,
                landing_blocked: Some(LandingBlock { rolls: 2, budget: 3 }),
                members: vec![
                    MemberView { wedge: Some(wedge()), wedge_cause: Some(WedgeCause::Work), ..member("issue-1") },
                    MemberView {
                        host_fault: Some(HostFaultView { findings: "no cargo".to_owned() }),
                        ..member("issue-2")
                    },
                    MemberView {
                        park: Some(MemberPark { stage: StageId::Construct, evidence: digest(3) }),
                        ..member("issue-3")
                    },
                ],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let keys: Vec<_> = notify_events(&view).into_iter().map(|event| event.key).collect();
        assert_eq!(
            keys,
            [
                "status:abababababab:Sealed",
                "landing:abababababab:2",
                "wedge:abababababab:issue-1",
                "host_fault:abababababab:issue-2",
                "member_park:abababababab:issue-3",
            ]
        );
    }

    #[test]
    fn a_withdrawn_member_wakes_nobody() {
        // Tripwire: the war room deliberately stays silent about a member an
        // operator already removed (#5327). A channel that shouts where the
        // board does not is the drift this taxonomy exists to avoid — and the
        // bloom itself is Resolved, which is the one status that is quiet.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: BloomId(digest(1)),
                status: BloomStatus::Resolved,
                members: vec![MemberView {
                    wedge: Some(wedge()),
                    withdrawn: Some(WithdrawnView {
                        cause: "operator".to_owned(),
                        depends_on: None,
                        reason: "superseded by hand".to_owned(),
                        operator: "owner".to_owned(),
                    }),
                    ..member("issue-1")
                }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        assert!(notify_events(&view).is_empty());
    }

    #[test]
    fn a_key_is_stable_across_two_identical_documents() {
        // Tripwire: the whole dedupe rests on the key being a function of the
        // condition alone. A timestamp, a counter, or an iteration order
        // sneaking into a key makes every poll re-post the same message.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: BloomId(digest(7)),
                status: BloomStatus::Sealed,
                members: vec![MemberView { wedge: Some(wedge()), ..member("issue-1") }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        assert_eq!(notify_events(&view), notify_events(&view.clone()));
    }
}
