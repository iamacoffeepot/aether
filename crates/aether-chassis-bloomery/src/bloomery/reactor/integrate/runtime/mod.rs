//! The runtime for the integrate-reactor capability (ADR-0152 §Resolution drives
//! integration — issue #3650).
//!
//! A poll-driven loop that turns the reducer's integration decisions into the
//! git-side fold that produces the landable head:
//!
//! 1. **Drain.** Each tick drains the store's
//!    [`Topic::Integrate`] outbox
//!    topic (its own connection, mirroring the land reactor's store ownership) and
//!    decodes each [`IntegratePayload`] — the
//!    bloom whose members all carry claims, its sealed base, and every member's
//!    claimed candidate tree in member order.
//! 2. **Fold.** It bootstraps the integration namespace
//!    ([`SourceShell::integration_checkpoint`], idempotent) and folds each
//!    candidate onto the branch through the CAS-guarded
//!    [`SourceShell::integrate`], chaining each `Integrated` outcome's tree into
//!    the next fold's expected checkpoint. A candidate that collides stops
//!    itself, not the pass: the rest fold on, and the tree the pass ends at is
//!    the settled checkpoint every conflicted member is then sent to reconcile
//!    against (ADR-0189, #4952). The bootstrap checkpoint also carries resume
//!    position: a reactor restarted mid-fold reads the branch at the last
//!    integrated candidate and continues after it rather than re-folding.
//! 3. **Admit.** After the last candidate integrates it admits a
//!    [`Fact::Resolve`] carrying the final tree, the landable head, and the
//!    candidate lineage — where `reduce_resolve` verifies every member's claim
//!    and emits the `DispatchLand` the existing land reactor consumes.
//!
//! A stale checkpoint mid-fold (a concurrent writer on the single-writer branch)
//! stops the ack prefix so the entry re-drains and re-resumes; a branch tree that
//! matches neither the base nor any candidate is a foreign advance — a definitive
//! refusal, acked with a loud warn rather than re-driven forever. Config-gated
//! exactly like the mirror / executor / land reactors.

use std::mem::take;
use std::sync::Arc;
use std::time::Duration;

use aether_actor::Addressable;
use aether_actor::runtime;
use aether_bloomery::{
    Admit, AdmitResult, BloomId, Checkpoint, Digest, Event, Evidence, EvidenceKind, Fact, Gate, IdempotencyKey,
    IntegrateOutcome, IntegratePayload, MemberCandidate, Read, Refusal, SplicePayload, Topic, WorkpieceId,
};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::IntegrateReactorCapability;
use crate::artifacts::{ArtifactsCapabilityState, PutResult, resolve_root};
use crate::bloomery::IntegrateReactorSetup;
use crate::bloomery::SourceShell;
use crate::bloomery::outbox::TopicOutbox;
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::control::ControlCore;
use crate::store::{SqliteStore, StoreBackend};
use aether_bloomery_github::candidate_ref_name;

// The autoloaded control-core component's lineage mailbox — where an admitted
// `Fact::Resolve` is sent. Resolved from the lineage path, mirroring the land
// reactor's `control_mailbox`. The one exported spelling (#3668).

/// The self-addressed wake the poll timer fires each interval; its handler
/// drains the integrate topic and folds each entry. Zero-field — the timer
/// carries only the schedule.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.integrate.integrate_tick")]
pub struct IntegrateTick {}

/// Runtime state for [`IntegrateReactorCapability`]. The shell + store are `Some`
/// only when configured; a disabled reactor holds neither and spawns no timer.
pub struct IntegrateReactorState {
    source: Option<SourceShell>,
    store: Option<SqliteStore>,
    // Where a fold-conflict overlay is filed so a wedge's evidence detail
    // resolves against the artifacts store (ADR-0189). `None` when the store
    // would not open; the fact still admits, the bytes are unretrievable.
    artifacts: Option<ArtifactsCapabilityState>,
    control_mailbox: MailboxId,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    // The poll timer sidecar; `None` when disabled. Held for its `Drop`, which
    // stops + joins the thread on teardown.
    _timer: Option<TimerHandle>,
}

impl IntegrateReactorState {
    /// Build state over an explicit shell + store — the seam the runtime tests
    /// drive with a fake-GitHub-backed shell and an in-memory store, bypassing
    /// `init`. Spawns no timer; a test drives the loop by feeding an
    /// [`IntegrateTick`] into the handler directly.
    #[must_use]
    pub fn with_parts(
        source: Option<SourceShell>,
        store: Option<SqliteStore>,
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
    ) -> Self {
        Self {
            source,
            store,
            artifacts: None,
            control_mailbox: <ControlCore as Addressable>::resolve(0, ()),
            mailer,
            self_mailbox,
            _timer: None,
        }
    }
}

/// The idempotency key a bloom's fold-conflict admits under.
///
/// Keyed by the conflicted candidate tree *and* the checkout commit offered
/// to the merge, as well as `(bloom, workpiece, checkpoint)`. The checkpoint
/// is the folded tree of the *earlier* members and does not change between
/// laps. A member that reconciles, verifies, and re-collides with the same
/// fold produces a new candidate tree; without that tree in the key the
/// second collision reduces to `AppendOutcome::Duplicate` and the bloom
/// stops with the entry acked. An ancestry repair can replace the checkout
/// while preserving the tree; without that vehicle in the key the repaired
/// merge is discarded as a replay of the earlier collision (#5083). The
/// sibling [`resolve_key`] carries the tree for the same reason (#4722).
fn fold_conflict_key(
    bloom: &Digest,
    workpiece: &str,
    checkpoint: &Digest,
    candidate: &Digest,
    attempted: &Digest,
) -> IdempotencyKey {
    let mut key = String::with_capacity(32 + 64 + 1 + workpiece.len() + 1 + 64 + 1 + 64 + 1 + 64);
    key.push_str("aether.bloomery.fold-conflict:");
    key.push_str(&bloom.to_hex());
    key.push(':');
    key.push_str(workpiece);
    key.push(':');
    key.push_str(&checkpoint.to_hex());
    key.push(':');
    key.push_str(&candidate.to_hex());
    key.push(':');
    key.push_str(&attempted.to_hex());
    IdempotencyKey(key)
}

/// The reconcile work-order overlay: the standing contract, the colliding
/// paths, and the conflicted candidate's own diff. The lane checks out the
/// folded head, so the member's work lives here rather than in the tree.
fn fold_conflict_overlay(paths: &[String], diff: &str) -> String {
    use core::fmt::Write;
    let mut overlay = String::from(
        "## Fold conflict\n\nReproduce this member's intent on top of what the fold now contains; stay inside the \
         declared surface.\n",
    );
    if !paths.is_empty() {
        overlay.push_str("\n## Conflicting paths\n\n");
        for path in paths {
            let _ = writeln!(overlay, "- {path}");
        }
    }
    let trimmed = diff.trim();
    if !trimmed.is_empty() {
        overlay.push_str("\n## Conflicted candidate\n\n```diff\n");
        overlay.push_str(trimmed);
        if !trimmed.ends_with('\n') {
            overlay.push('\n');
        }
        overlay.push_str("```\n");
    }
    overlay
}

/// The idempotency key a bloom's resolve admits under.
///
/// Keyed by the integrated tree as well as the bloom, because the tree is what
/// the resolve asserts: a re-drain of the *same* fold (before the ack lands, or
/// after a crash-and-replay) re-derives the same tree and reduces to a
/// duplicate, while a later lap that folds a genuinely different tree admits a
/// second, distinct fact. A bloom resolves once per integration it folds: an
/// aggregate-review finding routes a member back through Refine → Verify, and
/// the lap that follows integrates a different tree under the same bloom
/// (#4722). Under the bloom-only key that second resolve was swallowed as a
/// replay and the run stopped dead.
fn resolve_key(bloom: &Digest, tree: &Digest) -> IdempotencyKey {
    let mut key = String::with_capacity(24 + 64 + 1 + 64);
    key.push_str("aether.bloomery.resolve:");
    key.push_str(&bloom.to_hex());
    key.push(':');
    key.push_str(&tree.to_hex());
    IdempotencyKey(key)
}

/// One member's collision, the tree the branch stood at when the merge that
/// produced it was attempted, and the checkout commit that merge offered.
///
/// `at` is what decides whether the evidence still describes the settled
/// checkpoint: a member folding behind this one moves the branch past `at`, and
/// a collision measured against a tree the fold has left is evidence about a
/// checkpoint nothing will reconcile onto. `attempted` is the vehicle actually
/// merged — not the candidate tree — so an ancestry-corrected retry cannot
/// share the previous collision's admission key.
struct Collision<'a> {
    member: &'a MemberCandidate,
    at: Digest,
    paths: Vec<String>,
    diff: String,
    attempted: Digest,
}

/// One conflicted member's admission: the fact, the overlay bytes its Reconcile
/// work order reads, and the workpiece whose lane resolves the seam.
struct Conflicted {
    event: Box<Event>,
    overlay: String,
    workpiece: WorkpieceId,
    /// The settled checkpoint the evidence is bound to — the artifacts-store
    /// parent the overlay is filed under.
    checkpoint: Digest,
}

/// One payload's fold outcome.
enum FoldOutcome {
    /// Every candidate integrated (or was already on the branch); the resolve
    /// fact to admit.
    Resolved(Box<Event>),
    /// One or more cross-member collisions (ADR-0189): admit a `FoldConflict`
    /// per conflicted member so each reconciles against the settled checkpoint.
    Conflicted(Vec<Conflicted>),
    /// A definitive refusal — a named guard stopped the fold, or the branch
    /// carries a tree that matches neither the bootstrap position nor any
    /// candidate (a foreign advance). Acked with the carried reason, never
    /// re-driven; a named-guard refusal is admitted so the served view can
    /// show it (ADR-0206).
    Refused(Refusal),
    /// A transient stop — a stale checkpoint mid-fold or a transport fault; the
    /// entry re-drains next tick and the bootstrap checkpoint re-resumes it.
    Stopped(String),
}

fn fold_refusal(gate: &'static str, guard: &'static str, reads: Vec<Read>) -> Refusal {
    Gate::new(gate)
        .require(guard, || false, move || reads)
        .decide(|| ())
        .into_result()
        .err()
        .unwrap_or_else(|| Refusal { gate, guard, reads: Vec::new() })
}

fn require_members(gate: &'static str, count: usize) -> Result<(), FoldOutcome> {
    Gate::new(gate)
        .require("members_present", || count > 0, || aether_bloomery::reads![members: count])
        .decide(|| ())
        .into_result()
        .map_err(FoldOutcome::Refused)
}

fn adopt_inherited(
    source: &SourceShell,
    bloom: &BloomId,
    predecessor: BloomId,
    members: &[MemberCandidate],
    gate: &'static str,
    transport: &str,
) -> Result<(), FoldOutcome> {
    for member in members {
        match source.adopt_candidate(&predecessor, bloom, &member.workpiece.0) {
            Ok(true) => {}
            Ok(false) => {
                return Err(FoldOutcome::Refused(fold_refusal(
                    gate,
                    "candidate_ref_present",
                    aether_bloomery::reads![member: member.workpiece.0, predecessor: predecessor.0.to_hex()],
                )));
            }
            Err(error) => return Err(FoldOutcome::Stopped(format!("{transport}: {error}"))),
        }
    }
    Ok(())
}

fn fold_refused_key(bloom: &Digest, guard: &str, member: &str) -> IdempotencyKey {
    let mut key = String::with_capacity(32 + 64 + 1 + guard.len() + 1 + member.len());
    key.push_str("aether.bloomery.fold-refused:");
    key.push_str(&bloom.to_hex());
    key.push(':');
    key.push_str(guard);
    key.push(':');
    key.push_str(member);
    IdempotencyKey(key)
}

fn fold_refused_event(bloom: BloomId, refusal: &Refusal) -> Event {
    let member = refusal.reads.iter().find(|read| read.field == "member").map_or("", |read| read.value.as_str());
    Event {
        idempotency_key: fold_refused_key(&bloom.0, refusal.guard, member),
        fact: Fact::FoldRefused { bloom, refusal: refusal.recorded() },
    }
}

/// Fold one payload's candidates onto the bloom's integration branch and build
/// the resolve event. The bootstrap checkpoint decides the resume position:
/// at the base → fold from the first candidate; at candidate `k` → resume after
/// it; anywhere else → refuse (a foreign advance on the single-writer branch).
fn fold_integration(source: &SourceShell, payload: &IntegratePayload) -> FoldOutcome {
    let bloom = BloomId(payload.bloom);
    if let Err(outcome) = require_members("fold", payload.members.len()) {
        return outcome;
    }
    // How to fold. One member's candidate was built against the bloom's own
    // base, which is where the branch starts, so stating its tree is exact —
    // and cheaper, since it needs no ancestry read. More than one has to be
    // *combined*: the second candidate knows nothing of the first, so replacing
    // the tree would keep only the last member's work (#3653). Merging reads
    // each member's candidate ref, whose commit carries the ancestry a tree
    // cannot.
    let combining = payload.members.len() > 1 || payload.adopt_from.is_some();

    // An inherited claim arrives with no ref of its own: a candidate ref is
    // addressed under the bloom that produced it, and a successor is a different
    // bloom. Adopt into this bloom's namespace before folding, so the fold reads
    // only its own refs and a retired predecessor's namespace is never
    // load-bearing for a live bloom. Every member is offered, and adoption is
    // adopt-if-absent: a re-drained fold re-adopts harmlessly, and a member that
    // re-ran under the successor — the mixed set, where the claim completing the
    // set is a fresh one and the predecessor is named for the inherited members
    // beside it — keeps the capture it produced.
    if let Some(predecessor) = payload.adopt_from {
        let predecessor = BloomId(predecessor);
        if let Err(outcome) =
            adopt_inherited(source, &bloom, predecessor, &payload.members, "fold", "adopting a candidate ref failed")
        {
            return outcome;
        }
    }

    let position = match source.integration_checkpoint(&bloom, &payload.base) {
        Ok(position) => position,
        Err(error) => return FoldOutcome::Stopped(format!("integration bootstrap failed: {error}")),
    };
    // Resume position for the stating fold: a branch already at candidate k
    // continues after it. The freshly-bootstrapped branch's minted base tree
    // matches no candidate (capture refuses empty diffs, so no candidate tree
    // equals the base tree) and starts at the first. A combining fold cannot
    // resume this way — its branch carries a *merged* tree, which equals no
    // member's candidate — so it re-offers every member from the start and lets
    // the merge answer: one the branch already contains reports "nothing to do"
    // and the fold moves past it. Ancestry is the record of what is folded, and
    // the branch already holds it.
    let start = if combining {
        0
    } else {
        payload.members.iter().position(|member| member.candidate == position.checkpoint.tree).map_or(0, |i| i + 1)
    };
    // The landable head of the last fold: seeded from the branch position
    // (recovering a fold interrupted after its final write), overwritten below.
    let mut expected = position.checkpoint;
    let mut head = position.head;

    // Settle the fold before anything reconciles (#4952). A collision stops that
    // member, not the pass: the merge endpoint leaves the branch untouched on a
    // 409, so the members behind it fold onto the same expected tree and the
    // branch ends where it would have ended had nothing collided. That tree is
    // the settled checkpoint, and every conflicted member is sent to reconcile
    // against it. Reconciliation is not commutative — a member reconciled
    // against an intermediate tree is correct about a checkpoint the rest of the
    // fold then moves, and pays a second round for a collision it never had,
    // which is the N² cascade this replaces.
    let mut pending: Vec<&MemberCandidate> = payload.members[start.min(payload.members.len())..].iter().collect();
    let mut collisions: Vec<Collision<'_>> = Vec::new();
    loop {
        let round = take(&mut pending);
        collisions.clear();
        for member in round {
            let folded = if combining {
                source.integrate_merge(&bloom, &candidate_ref_name(&bloom, &member.workpiece.0), &expected)
            } else {
                source.integrate(&bloom, &member.candidate, &expected)
            };
            match folded {
                Ok(IntegrateOutcome::Integrated { tree, head: new_head }) => {
                    expected = Checkpoint { bloom, tree };
                    head = Some(new_head);
                }
                // A cross-member collision. Journaled as FoldConflict (ADR-0189)
                // rather than refused in prose: the member reconciles against
                // the folded checkpoint. Re-driving the same trees cannot
                // resolve it; the dispatched lane can.
                Ok(IntegrateOutcome::Conflict { paths, diff, attempted, .. }) => {
                    collisions.push(Collision { member, at: expected.tree, paths, diff, attempted });
                    pending.push(member);
                }
                Ok(other) => {
                    return FoldOutcome::Stopped(format!(
                        "integration fold stopped by a concurrent branch advance: {other:?}"
                    ));
                }
                Err(error) => return FoldOutcome::Stopped(format!("integrate transport failed: {error}")),
            }
        }
        // Every outstanding collision measured against the tree the pass ended
        // on already describes the settled checkpoint. Anything measured
        // earlier was moved past by a member folding behind it, so the deferred
        // set is re-offered against the settled tree — which is also the only
        // way a collision a sibling's fold happened to absorb stops being one.
        // Terminating: a pass that does not break has moved the tree, so it
        // folded a member and `pending` is strictly shorter.
        if collisions.iter().all(|collision| collision.at == expected.tree) {
            break;
        }
    }

    if !collisions.is_empty() {
        let checkout = head.unwrap_or(payload.base);
        return FoldOutcome::Conflicted(
            collisions
                .iter()
                .map(|collision| {
                    conflicted_member(&bloom, &collision.member.workpiece, collision, expected.tree, checkout)
                })
                .collect(),
        );
    }

    let Some(head) = head else {
        // Nothing folded this drain and the branch position recovered no head —
        // the branch sits at a candidate tree but its commit has no recorded
        // head correspondence (a corrupt or externally-written branch). Refuse
        // loudly rather than admit a resolve with a fabricated head.
        return FoldOutcome::Refused(fold_refusal(
            "fold",
            "landable_head",
            aether_bloomery::reads![branch_head: "absent"],
        ));
    };
    let tree = expected.tree;
    let event = Event {
        idempotency_key: resolve_key(&payload.bloom, &tree),
        fact: Fact::Resolve {
            bloom,
            tree,
            head,
            lineage: payload.members.iter().map(|member| member.candidate).collect(),
        },
    };
    FoldOutcome::Resolved(Box::new(event))
}

/// The scratch bloom id a member splice folds onto, so the weave's
/// integration branch is never the destination.
fn splice_namespace(bloom: &BloomId, workpiece: &WorkpieceId) -> BloomId {
    let mut bytes = Vec::from(*b"aether.bloomery.splice:");
    bytes.extend_from_slice(bloom.0.as_bytes());
    bytes.push(b':');
    bytes.extend_from_slice(workpiece.0.as_bytes());
    BloomId(Digest::of_wire_bytes(&bytes))
}

fn splice_assembled_key(bloom: &Digest, workpiece: &str, tree: &Digest, head: &Digest) -> IdempotencyKey {
    let mut key = String::with_capacity(32 + 64 + 1 + workpiece.len() + 1 + 64 + 1 + 64);
    key.push_str("aether.bloomery.splice-assembled:");
    key.push_str(&bloom.to_hex());
    key.push(':');
    key.push_str(workpiece);
    key.push(':');
    key.push_str(&tree.to_hex());
    key.push(':');
    key.push_str(&head.to_hex());
    IdempotencyKey(key)
}

/// Fold one dependent's independent tips onto a scratch branch.
///
/// Candidate refs stay in the real bloom's namespace (adoption fills inherited
/// ones). The destination is a per-member integration id so this merge cannot
/// advance the weave branch. A residual collision is the dependent's
/// [`Fact::FoldConflict`], not a tip's — the tips are already resolved.
fn fold_splice(source: &SourceShell, payload: &SplicePayload) -> FoldOutcome {
    let bloom = BloomId(payload.bloom);
    if let Err(outcome) = require_members("splice", payload.members.len()) {
        return outcome;
    }
    if let Some(predecessor) = payload.adopt_from {
        let predecessor = BloomId(predecessor);
        if let Err(outcome) =
            adopt_inherited(source, &bloom, predecessor, &payload.members, "splice", "adopting a splice tip failed")
        {
            return outcome;
        }
    }

    let namespace = splice_namespace(&bloom, &payload.workpiece);
    let position = match source.integration_checkpoint(&namespace, &payload.base) {
        Ok(position) => position,
        Err(error) => return FoldOutcome::Stopped(format!("splice bootstrap failed: {error}")),
    };
    let mut expected = Checkpoint { bloom: namespace, tree: position.checkpoint.tree };
    let mut head = position.head;
    let mut collisions: Vec<Collision<'_>> = Vec::new();
    for member in &payload.members {
        match source.integrate_merge(&namespace, &candidate_ref_name(&bloom, &member.workpiece.0), &expected) {
            Ok(IntegrateOutcome::Integrated { tree, head: new_head }) => {
                expected = Checkpoint { bloom: namespace, tree };
                head = Some(new_head);
            }
            Ok(IntegrateOutcome::Conflict { paths, diff, attempted, .. }) => {
                collisions.push(Collision { member, at: expected.tree, paths, diff, attempted });
            }
            Ok(other) => {
                return FoldOutcome::Stopped(format!("splice fold stopped by a concurrent branch advance: {other:?}"));
            }
            Err(error) => return FoldOutcome::Stopped(format!("splice merge failed: {error}")),
        }
    }

    if !collisions.is_empty() {
        let checkout = head.unwrap_or(payload.base);
        return FoldOutcome::Conflicted(vec![conflicted_member(
            &bloom,
            &payload.workpiece,
            &collisions[0],
            expected.tree,
            checkout,
        )]);
    }

    let Some(head) = head else {
        return FoldOutcome::Refused(fold_refusal(
            "splice",
            "landable_head",
            aether_bloomery::reads![branch_head: "absent"],
        ));
    };
    let tree = expected.tree;
    let event = Event {
        idempotency_key: splice_assembled_key(&payload.bloom, &payload.workpiece.0, &tree, &head),
        fact: Fact::SpliceAssembled { bloom, workpiece: payload.workpiece.clone(), tree, head },
    };
    FoldOutcome::Resolved(Box::new(event))
}

/// Build one member's collision admission against the settled checkpoint.
///
/// `owner` is the workpiece whose Reconcile lane resolves this seam, passed
/// rather than read off the collision because it is the one thing ADR-0191 §1
/// moves: re-homing the reconcile onto the composition workpiece (#4964) changes
/// this argument at its single call site and nothing else in the fold.
fn conflicted_member(
    bloom: &BloomId,
    owner: &WorkpieceId,
    collision: &Collision<'_>,
    checkpoint: Digest,
    checkout: Digest,
) -> Conflicted {
    let overlay = fold_conflict_overlay(&collision.paths, &collision.diff);
    let evidence = Evidence {
        subject: checkpoint,
        kind: EvidenceKind::FoldConflict,
        detail: Digest::of_wire_bytes(overlay.as_bytes()),
    };
    let event = Event {
        idempotency_key: fold_conflict_key(
            &bloom.0,
            &owner.0,
            &checkpoint,
            &collision.member.candidate,
            &collision.attempted,
        ),
        fact: Fact::FoldConflict { bloom: *bloom, workpiece: owner.clone(), checkpoint, head: checkout, evidence },
    };

    Conflicted { event: Box::new(event), overlay, workpiece: owner.clone(), checkpoint }
}

/// Persist each conflicted member's overlay and encode its fact, returning the
/// [`Admit`]s to forward. `None` is a transient failure: the caller stops the
/// ack prefix, the entry re-drains, and the same settled checkpoint re-derives
/// the same keys.
fn admit_conflicts(
    store: &mut dyn StoreBackend,
    mut artifacts: Option<&mut ArtifactsCapabilityState>,
    bloom: &Digest,
    sequence: u64,
    conflicts: &[Conflicted],
) -> Option<Vec<Admit>> {
    let mut admits = Vec::with_capacity(conflicts.len());
    for conflicted in conflicts {
        let workpiece = &conflicted.workpiece.0;
        if let Err(error) = store.record_fold_conflict(bloom.as_bytes(), workpiece, &conflicted.overlay) {
            tracing::warn!(
                target: "aether_chassis_bloomery::integrate",
                sequence,
                %workpiece,
                %error,
                "fold-conflict overlay did not persist; stopping the ack prefix to re-drive",
            );
            return None;
        }
        if !store_fold_conflict_overlay(artifacts.as_deref_mut(), &conflicted.overlay, &conflicted.checkpoint) {
            tracing::warn!(
                target: "aether_chassis_bloomery::integrate",
                sequence,
                %workpiece,
                "fold-conflict overlay was not stored; stopping the ack prefix to re-drive",
            );
            return None;
        }
        let Ok(bytes) = to_vec(&*conflicted.event) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::integrate",
                sequence,
                %workpiece,
                "fold-conflict event did not encode; stopping the ack prefix to re-drive",
            );
            return None;
        };
        admits.push(Admit { event: bytes });
    }

    Some(admits)
}

/// Drain the integrate topic and fold each entry, returning the [`Admit`]s to
/// forward to the control core (one per resolved bloom) and the highest
/// contiguously-processed outbox sequence to ack (`None` when nothing
/// processed). A decode failure, an encode failure, or a transient fold stop
/// halts the ack prefix so the entry re-drains; a definitive refusal is a
/// processed entry (acked, no admit). The factored-out network side,
/// unit-testable against a `SqliteStore` + a fake-GitHub-backed shell without
/// the mail harness.
fn drain_and_integrate(
    store: &mut dyn StoreBackend,
    source: &SourceShell,
    mut artifacts: Option<&mut ArtifactsCapabilityState>,
) -> rusqlite::Result<(Vec<Admit>, Option<u64>)> {
    let entries = store.drain_topic(Topic::Integrate)?;
    let mut admits = Vec::new();
    let mut ack_through = None;
    for entry in entries {
        let Ok(payload) = from_bytes::<IntegratePayload>(&entry.payload) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::integrate",
                sequence = entry.sequence,
                "integrate outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        match fold_integration(source, &payload) {
            FoldOutcome::Resolved(event) => {
                let Ok(bytes) = to_vec(&*event) else {
                    tracing::warn!(
                        target: "aether_chassis_bloomery::integrate",
                        sequence = entry.sequence,
                        "resolve event did not encode; stopping the ack prefix to re-drive",
                    );
                    break;
                };
                admits.push(Admit { event: bytes });
                ack_through = Some(entry.sequence);
            }
            FoldOutcome::Conflicted(conflicts) => {
                let Some(conflicted) =
                    admit_conflicts(store, artifacts.as_deref_mut(), &payload.bloom, entry.sequence, &conflicts)
                else {
                    break;
                };
                admits.extend(conflicted);
                ack_through = Some(entry.sequence);
            }
            FoldOutcome::Refused(refusal) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::integrate",
                    sequence = entry.sequence,
                    %refusal,
                    "integration refused definitively; admitting the refusal and acking the entry instead of re-driving",
                );
                let Ok(bytes) = to_vec(&fold_refused_event(BloomId(payload.bloom), &refusal)) else {
                    tracing::warn!(
                        target: "aether_chassis_bloomery::integrate",
                        sequence = entry.sequence,
                        "fold-refused event did not encode; stopping the ack prefix to re-drive",
                    );
                    break;
                };
                admits.push(Admit { event: bytes });
                ack_through = Some(entry.sequence);
            }
            FoldOutcome::Stopped(reason) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::integrate",
                    sequence = entry.sequence,
                    %reason,
                    "integration stopped; the entry re-drains and resumes next tick",
                );
                break;
            }
        }
    }
    Ok((admits, ack_through))
}

fn drain_and_splice(
    store: &mut dyn StoreBackend,
    source: &SourceShell,
    mut artifacts: Option<&mut ArtifactsCapabilityState>,
) -> rusqlite::Result<(Vec<Admit>, Option<u64>)> {
    let entries = store.drain_topic(Topic::Splice)?;
    let mut admits = Vec::new();
    let mut ack_through = None;
    for entry in entries {
        let Ok(payload) = from_bytes::<SplicePayload>(&entry.payload) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::integrate",
                sequence = entry.sequence,
                "splice outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        match fold_splice(source, &payload) {
            FoldOutcome::Resolved(event) => {
                let Ok(bytes) = to_vec(&*event) else {
                    tracing::warn!(
                        target: "aether_chassis_bloomery::integrate",
                        sequence = entry.sequence,
                        "splice-assembled event did not encode; stopping the ack prefix to re-drive",
                    );
                    break;
                };
                admits.push(Admit { event: bytes });
                ack_through = Some(entry.sequence);
            }
            FoldOutcome::Conflicted(conflicts) => {
                let Some(conflicted) =
                    admit_conflicts(store, artifacts.as_deref_mut(), &payload.bloom, entry.sequence, &conflicts)
                else {
                    break;
                };
                admits.extend(conflicted);
                ack_through = Some(entry.sequence);
            }
            FoldOutcome::Refused(refusal) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::integrate",
                    sequence = entry.sequence,
                    %refusal,
                    "splice refused definitively; admitting the refusal and acking the entry instead of re-driving",
                );
                let Ok(bytes) = to_vec(&fold_refused_event(BloomId(payload.bloom), &refusal)) else {
                    tracing::warn!(
                        target: "aether_chassis_bloomery::integrate",
                        sequence = entry.sequence,
                        "fold-refused event did not encode; stopping the ack prefix to re-drive",
                    );
                    break;
                };
                admits.push(Admit { event: bytes });
                ack_through = Some(entry.sequence);
            }
            FoldOutcome::Stopped(reason) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::integrate",
                    sequence = entry.sequence,
                    %reason,
                    "splice stopped; the entry re-drains and resumes next tick",
                );
                break;
            }
        }
    }
    Ok((admits, ack_through))
}

fn digest_hex(digest: &Digest) -> String {
    digest.to_hex()
}

/// File the overlay under the same sha256 `detail` the fold-conflict evidence
/// names, so a wedge later resolves against the artifacts store. A missing
/// store still admits (the address is a pure function of the bytes); a store
/// that faults is transient and must not ack.
fn store_fold_conflict_overlay(
    artifacts: Option<&mut ArtifactsCapabilityState>,
    overlay: &str,
    parent: &Digest,
) -> bool {
    let Some(artifacts) = artifacts else {
        tracing::warn!(
            target: "aether_chassis_bloomery::integrate",
            "no artifacts store configured; the fold-conflict overlay is unretrievable",
        );
        return true;
    };
    match artifacts.put(overlay.as_bytes(), &[digest_hex(parent)]) {
        PutResult::Ok { .. } => true,
        PutResult::Err { error } => {
            tracing::warn!(
                target: "aether_chassis_bloomery::integrate",
                ?error,
                "fold-conflict overlay was not stored",
            );
            false
        }
    }
}

fn open_artifacts(configured: Option<&str>) -> Option<ArtifactsCapabilityState> {
    let root = resolve_root(configured);
    match ArtifactsCapabilityState::open(&root) {
        Ok(artifacts) => Some(artifacts),
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::integrate",
                root = %root.display(),
                %error,
                "artifacts store did not open; fold-conflict evidence will be unretrievable this session",
            );
            None
        }
    }
}

#[runtime]
impl NativeActor for IntegrateReactorCapability {
    type State = IntegrateReactorState;
    type Config = ();
    type Params = IntegrateReactorSetup;

    const NAMESPACE: &'static str = "aether.bloomery.integrate";

    fn init(
        (): (),
        config: IntegrateReactorSetup,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<IntegrateReactorState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let control_mailbox = <ControlCore as Addressable>::resolve(0, ());

        // Unconfigured → disabled: no shell, no store, no timer. The integrate
        // outbox accumulates and drains once a token/owner/repo is supplied,
        // unless the `fake` backend is selected (#4732).
        let Some(source) = config.source else {
            tracing::info!(
                target: "aether_chassis_bloomery::integrate",
                "integrate reactor mounted disabled (unconfigured token/owner/repo); integrate outbox will accumulate",
            );
            return Ok(IntegrateReactorState {
                source: None,
                store: None,
                artifacts: None,
                control_mailbox,
                mailer,
                self_mailbox,
                _timer: None,
            });
        };

        let store = SqliteStore::open(&config.store_path).map_err(|e| BootError::Other(Box::new(e)))?;
        let interval = Duration::from_secs(config.poll_interval_secs.max(1));
        let timer = spawn_timer(
            Arc::clone(&mailer),
            self_mailbox,
            IntegrateTick::ID,
            IntegrateTick::default().encode_into_bytes(),
            "aether-bloomery-integrate",
            interval,
        );
        tracing::info!(
            target: "aether_chassis_bloomery::integrate",
            repository = ?config.repository,
            poll_interval_secs = config.poll_interval_secs,
            "integrate reactor mounted; polling the store for integration decisions",
        );
        Ok(IntegrateReactorState {
            source: Some(source),
            store: Some(store),
            artifacts: open_artifacts(config.artifacts_root.as_deref()),
            control_mailbox,
            mailer,
            self_mailbox,
            _timer: Some(timer),
        })
    }

    /// Fire an immediate boot tick so an integration left undrained by a prior
    /// crash folds without waiting a full poll interval. Disabled reactors push
    /// nothing.
    fn wire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        if state.source.is_some() {
            state.mailer.push(Mail::new(
                state.self_mailbox,
                IntegrateTick::ID,
                IntegrateTick::default().encode_into_bytes(),
                1,
            ));
        }
    }

    /// Poll wake: drain + fold the integrate topic, acking the processed prefix
    /// and forwarding each resolved bloom's `Fact::Resolve` to the control core.
    /// The GitHub calls run inline on the dispatcher (the poll cadence spaces
    /// them).
    #[handler::single]
    fn on_integrate_tick(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: IntegrateTick) {
        let Some(source) = state.source.clone() else {
            return;
        };
        let control_mailbox = state.control_mailbox;
        let Some(store) = state.store.as_mut() else {
            return;
        };

        match drain_and_integrate(store, &source, state.artifacts.as_mut()) {
            Ok((admits, ack_through)) => {
                if let Some(sequence) = ack_through
                    && let Err(error) = store.ack_topic(Topic::Integrate, sequence)
                {
                    tracing::warn!(target: "aether_chassis_bloomery::integrate", %error, "integrate ack failed; entries re-drive");
                }
                for admit in admits {
                    // Fire-and-forget: the control actor's on_admit is reliable
                    // local mail, and the reducer's idempotency key dedups a
                    // resend, so no settlement handle is needed here.
                    let _ = ctx.send_envelope_detached(control_mailbox, Admit::ID, &admit.encode_into_bytes());
                }
            }
            Err(error) => {
                tracing::warn!(target: "aether_chassis_bloomery::integrate", %error, "integrate drain failed");
            }
        }
        match drain_and_splice(store, &source, state.artifacts.as_mut()) {
            Ok((admits, ack_through)) => {
                if let Some(sequence) = ack_through
                    && let Err(error) = store.ack_topic(Topic::Splice, sequence)
                {
                    tracing::warn!(target: "aether_chassis_bloomery::integrate", %error, "splice ack failed; entries re-drive");
                }
                for admit in admits {
                    let _ = ctx.send_envelope_detached(control_mailbox, Admit::ID, &admit.encode_into_bytes());
                }
            }
            Err(error) => {
                tracing::warn!(target: "aether_chassis_bloomery::integrate", %error, "splice drain failed");
            }
        }
    }

    /// Control's reply to a fire-and-forget admit. Ok is a no-op; Err is the
    /// refused-admission event that used to miss dispatch.
    #[handler::single]
    fn on_admit_result(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: AdmitResult) {
        if let AdmitResult::Err { error } = mail {
            tracing::error!(target: "aether_chassis_bloomery::integrate", %error, "admit refused");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
