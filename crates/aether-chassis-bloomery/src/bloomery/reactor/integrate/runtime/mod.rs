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
//!    the next fold's expected checkpoint. The bootstrap checkpoint also carries
//!    resume position: a reactor restarted mid-fold reads the branch at the last
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

use std::sync::Arc;
use std::time::Duration;

use aether_actor::Addressable;
use aether_actor::runtime;
use aether_bloomery::{
    Admit, BloomId, Checkpoint, Digest, Event, Fact, IdempotencyKey, IntegrateOutcome, IntegratePayload, Topic,
};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::IntegrateReactorCapability;
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
            control_mailbox: <ControlCore as Addressable>::resolve(0, ()),
            mailer,
            self_mailbox,
            _timer: None,
        }
    }
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
    use core::fmt::Write;
    let mut key = String::with_capacity(24 + 64 + 1 + 64);
    key.push_str("aether.bloomery.resolve:");
    for byte in bloom.as_bytes() {
        let _ = write!(key, "{byte:02x}");
    }
    key.push(':');
    for byte in tree.as_bytes() {
        let _ = write!(key, "{byte:02x}");
    }
    IdempotencyKey(key)
}

/// One payload's fold outcome.
enum FoldOutcome {
    /// Every candidate integrated (or was already on the branch); the resolve
    /// fact to admit.
    Resolved(Box<Event>),
    /// A definitive refusal — the branch carries a tree that matches neither
    /// the bootstrap position nor any candidate (a foreign advance). Acked with
    /// the carried reason, never re-driven.
    Refused(String),
    /// A transient stop — a stale checkpoint mid-fold or a transport fault; the
    /// entry re-drains next tick and the bootstrap checkpoint re-resumes it.
    Stopped(String),
}

/// Fold one payload's candidates onto the bloom's integration branch and build
/// the resolve event. The bootstrap checkpoint decides the resume position:
/// at the base → fold from the first candidate; at candidate `k` → resume after
/// it; anywhere else → refuse (a foreign advance on the single-writer branch).
fn fold_integration(source: &SourceShell, payload: &IntegratePayload) -> FoldOutcome {
    let bloom = BloomId(payload.bloom);
    if payload.members.is_empty() {
        return FoldOutcome::Refused("integration dispatched with zero candidates".to_owned());
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
        for member in &payload.members {
            match source.adopt_candidate(&predecessor, &bloom, &member.workpiece.0) {
                // A member with a ref in neither namespace has no work to fold.
                // Refuse rather than fold a partial set: the resolve would claim
                // an artifact that never carried that member's changes.
                Ok(false) => {
                    return FoldOutcome::Refused(format!(
                        "member `{}` carries a claim but no candidate ref under this bloom or its predecessor; \
                         refusing to fold a set missing a member's work",
                        member.workpiece.0
                    ));
                }
                Ok(true) => {}
                Err(error) => return FoldOutcome::Stopped(format!("adopting a candidate ref failed: {error}")),
            }
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
    for member in &payload.members[start.min(payload.members.len())..] {
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
            // A cross-member collision. Refused rather than stopped: re-driving
            // on a timer cannot resolve it — the two candidates conflict until
            // something changes one of them, which is a decision above this
            // reactor, not a retry.
            Ok(IntegrateOutcome::Conflict { .. }) => {
                return FoldOutcome::Refused(format!(
                    "member `{}` conflicts with the members already folded; the collision needs a decision, \
                     not a re-drive",
                    member.workpiece.0
                ));
            }
            Ok(other) => {
                return FoldOutcome::Stopped(format!(
                    "integration fold stopped by a concurrent branch advance: {other:?}"
                ));
            }
            Err(error) => return FoldOutcome::Stopped(format!("integrate transport failed: {error}")),
        }
    }
    let Some(head) = head else {
        // Nothing folded this drain and the branch position recovered no head —
        // the branch sits at a candidate tree but its commit has no recorded
        // head correspondence (a corrupt or externally-written branch). Refuse
        // loudly rather than admit a resolve with a fabricated head.
        return FoldOutcome::Refused(
            "integration branch already at a candidate tree but its commit reverse-resolves no landable head; \
             refusing to fabricate one"
                .to_owned(),
        );
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
            FoldOutcome::Refused(reason) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::integrate",
                    sequence = entry.sequence,
                    %reason,
                    "integration refused definitively; acking the entry instead of re-driving",
                );
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

        match drain_and_integrate(store, &source) {
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
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
