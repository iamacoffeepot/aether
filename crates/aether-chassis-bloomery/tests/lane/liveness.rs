//! The property every lane-boundary scenario checks, whether or not it thought
//! to.
//!
//! A harness that only asserts "scenario X reaches outcome Y" is the same
//! whack-a-mole with a faster loop: it holds the failures someone already paid
//! for and says nothing about the next one. What generalizes is the property the
//! live coordinator violated —
//!
//! > **Quiescence with work outstanding is a failure.** A bloom is advancing, in
//! > a declared terminal state, or wedged with recorded evidence. Sealed, no
//! > wedge, empty outbox, nothing outstanding — the state a coordinator sat in
//! > for five hours — is a failure by construction.
//!
//! and its sibling, from the orders that sat undispatched all morning —
//!
//! > **Every dispatched order terminates.** An order left in
//! > `outstanding_orders` with no completion advances no counter, so no ceiling
//! > is ever reached and no wedge is ever recorded.
//!
//! Neither names a bug. That is the point: they fail on the next one too. They
//! are checked on every poll of every scenario's settle loop, so a scenario does
//! not opt in and cannot forget.

#![allow(dead_code, reason = "each test binary compiles the whole module and uses only the fixtures it needs")]

use std::fmt::Write as _;

use aether_bloomery::{BloomStatus, ViewDocument};

/// Everything a poll observes about whether the coordinator is doing anything.
///
/// Compared for equality between polls: unequal means the world moved, which is
/// what "advancing" means here. Deliberately coarse — it does not care *what*
/// moved, only that something did, so a step nobody predicted still counts as
/// progress and a new kind of stall still counts as a stall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// The rendered projection: bloom statuses, member resolutions, wedges,
    /// pending decisions.
    pub projection: String,
    /// The nonces the store still holds as outstanding orders.
    pub outstanding: Vec<String>,
    /// How many lane runs the mock has recorded.
    pub runs: usize,
}

impl Progress {
    /// Fold a poll's observations into a comparable fingerprint.
    #[must_use]
    pub fn observe(document: &ViewDocument, outstanding: Vec<String>, runs: usize) -> Self {
        Self { projection: render(document), outstanding, runs }
    }
}

/// The projection as a stable string — `Debug` over the view types, which
/// changes exactly when a field a scenario could care about changes, and needs
/// no per-field maintenance as the projection grows.
fn render(document: &ViewDocument) -> String {
    let mut rendered = String::new();
    for bloom in &document.blooms {
        let _ = write!(rendered, "{bloom:?}|");
    }
    rendered
}

/// Why a quiescent coordinator is allowed to be quiescent — or is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Quiescence {
    /// A declared terminal state: nothing further is owed.
    Terminal(String),
    /// Every unresolved member is wedged, and each wedge names the evidence that
    /// produced it. Stopped, but accountably.
    Wedged(String),
    /// Stopped with work still owed. The failure this harness exists for.
    Stalled(String),
}

/// Classify a coordinator that has stopped moving.
///
/// The order matters: an outstanding order is a stall *whatever* the projection
/// says, because a dispatched order with no completion is precisely the state
/// that advances no counter — a bloom can look wedged, or even resolved, while
/// an order it forgot sits in the table forever.
#[must_use]
pub fn classify(document: &ViewDocument, outstanding: &[String]) -> Quiescence {
    if !outstanding.is_empty() {
        return Quiescence::Stalled(format!(
            "{} dispatched order(s) never completed: {}. An order left outstanding advances no wedge counter, so no \
             ceiling is ever reached",
            outstanding.len(),
            outstanding.join(", "),
        ));
    }

    if document.blooms.is_empty() {
        return Quiescence::Stalled("no bloom reached the projection at all".to_owned());
    }

    let mut unresolved = Vec::new();
    for bloom in &document.blooms {
        match bloom.status {
            // Resolved is terminal for the lane boundary: every member has
            // passed its stages. Landing is the integrate/land reactors' step
            // and needs the GitHub side this tier deliberately does not mount.
            BloomStatus::Resolved | BloomStatus::Landed | BloomStatus::Superseded => continue,
            BloomStatus::Sealed => {}
        }
        for member in &bloom.members {
            if member.resolution.is_none() && member.wedge.is_none() {
                unresolved.push(format!("{:?}/{}", bloom.id, member.workpiece.0));
            }
        }
    }

    if unresolved.is_empty() {
        let statuses: Vec<String> = document.blooms.iter().map(|bloom| format!("{:?}", bloom.status)).collect();
        // A sealed bloom whose every member carries a wedge has stopped for a
        // recorded reason; a bloom that reached a terminal status has stopped
        // because it is done. A bloom at its aggregate-review executor-fault
        // ceiling (ADR-0176) is the first stop of the recorded kind that is not
        // a *member's*: every member resolved and the fold is still held, so
        // without this it reads as a bloom that finished.
        let wedged = document.blooms.iter().any(|bloom| {
            bloom.members.iter().any(|member| member.wedge.is_some())
                || bloom.executor_fault.is_some_and(|fault| fault.terminal)
        });
        let summary = statuses.join(", ");
        return if wedged {
            Quiescence::Wedged(summary)
        } else {
            Quiescence::Terminal(summary)
        };
    }

    Quiescence::Stalled(format!(
        "sealed with {} member(s) neither resolved nor wedged and nothing in flight: {}",
        unresolved.len(),
        unresolved.join(", "),
    ))
}

#[cfg(test)]
mod tests {
    use aether_bloomery::{
        BloomId, BloomStatus, BloomView, Digest, Evidence, EvidenceKind, ExecutorFaultView, MemberView, ViewDocument,
        Wedge, WorkpieceId,
    };

    use super::{Quiescence, classify};

    fn member(resolved: bool, wedge: Option<Wedge>) -> MemberView {
        MemberView {
            workpiece: WorkpieceId("wp".to_owned()),
            scope_revision: Digest::from_bytes([1; 32]),
            approval: Evidence { subject: Digest::default(), kind: EvidenceKind::Approval, detail: Digest::default() },
            resolution: resolved.then(|| aether_bloomery::ResolutionClaim {
                workpiece: WorkpieceId("wp".to_owned()),
                scope_revision: Digest::from_bytes([1; 32]),
                candidate: Digest::from_bytes([2; 32]),
                evidence: Evidence {
                    subject: Digest::default(),
                    kind: EvidenceKind::ResolutionClaim,
                    detail: Digest::default(),
                },
            }),
            pending_decision: None,
            wedge,
        }
    }

    fn document(status: BloomStatus, members: Vec<MemberView>) -> ViewDocument {
        ViewDocument {
            mainline: Digest::default(),
            observed: Digest::default(),
            spend_quiesce: None,
            blooms: vec![BloomView {
                id: BloomId(Digest::from_bytes([7; 32])),
                status,
                superseded_by: None,
                members,
                landing_blocked: None,
                executor_fault: None,
            }],
        }
    }

    #[test]
    fn a_sealed_bloom_with_nothing_in_flight_is_a_failure() {
        // Tripwire: this is the exact state the live coordinator sat in for five
        // hours — sealed, no wedge, empty outbox, nothing outstanding. If it
        // ever classifies as anything but a stall, every scenario in this tier
        // goes quietly green on a dead coordinator.
        let stalled = classify(&document(BloomStatus::Sealed, vec![member(false, None)]), &[]);

        assert!(matches!(stalled, Quiescence::Stalled(_)), "quiescence with work owed must fail: {stalled:?}");
    }

    #[test]
    fn an_order_that_never_completed_is_a_failure_however_the_bloom_reads() {
        // Tripwire: the second invariant, and the reason it is checked before
        // the projection. An order left in `outstanding_orders` advances no
        // counter, so a bloom can read perfectly resolved while a lane it
        // forgot is owed forever.
        let stalled = classify(&document(BloomStatus::Resolved, vec![member(true, None)]), &["n-lost".to_owned()]);

        assert!(matches!(stalled, Quiescence::Stalled(_)), "an outstanding order outranks a clean projection");
    }

    #[test]
    fn a_terminal_executor_fault_is_an_accountable_stop_not_a_finished_bloom() {
        // Tripwire (ADR-0176): a bloom at its executor-fault ceiling has every
        // member resolved and its fold still held, so the member-shaped tests
        // above all pass on it. Classifying that as `Terminal` would let a bloom
        // stopped dead on a broken host read as one that finished its work.
        let faulted = ViewDocument {
            mainline: Digest::default(),
            observed: Digest::default(),
            spend_quiesce: None,
            blooms: vec![BloomView {
                id: BloomId(Digest::from_bytes([7; 32])),
                status: BloomStatus::Sealed,
                superseded_by: None,
                members: vec![member(true, None)],
                landing_blocked: None,
                executor_fault: Some(ExecutorFaultView {
                    subject: Digest::from_bytes([3; 32]),
                    rolls: 2,
                    budget: 2,
                    evidence: Digest::from_bytes([9; 32]),
                    terminal: true,
                }),
            }],
        };

        assert!(matches!(classify(&faulted, &[]), Quiescence::Wedged(_)));
    }

    #[test]
    fn a_wedge_is_a_legitimate_stop_and_a_resolution_is_a_terminal_one() {
        let wedge = Wedge {
            stage: aether_bloomery::StageId::Verify,
            evidence: Digest::from_bytes([9; 32]),
            repeated_verifiers: aether_bloomery::VerifyFailureSet::EMPTY,
        };

        assert!(matches!(
            classify(&document(BloomStatus::Sealed, vec![member(false, Some(wedge))]), &[]),
            Quiescence::Wedged(_)
        ));
        assert!(matches!(
            classify(&document(BloomStatus::Resolved, vec![member(true, None)]), &[]),
            Quiescence::Terminal(_)
        ));
    }
}
