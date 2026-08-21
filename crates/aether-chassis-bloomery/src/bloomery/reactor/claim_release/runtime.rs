//! The runtime for the claim-release reactor capability (ADR-0179).
//!
//! A poll-driven loop that turns an authorized release request into the source
//! port's expected-holder compare-and-swap, and journals which terminal it
//! reached.
//!
//! 1. **Drain.** Each tick drains the store's `topic:orphan_claim_release` outbox
//!    topic (its own store connection, mirroring the land reactor) and decodes
//!    each [`OrphanClaimReleasePayload`] — the request digest and the signed
//!    target the reducer already admitted.
//! 2. **Release.** It calls [`SourceShell::complete_release`] with
//!    `Some(expected_holder)`, so a ref that has moved off that holder is spared
//!    and reported rather than clobbered.
//! 3. **Admit.** Every clean outcome is terminal and admits
//!    [`Fact::CompleteOrphanClaimRelease`], keyed by the request digest — so a
//!    redrive of the same entry reduces to a duplicate rather than releasing
//!    twice.
//!
//! **The crash window closes on the redrive.** A release whose source mutation
//! landed but whose completion was never admitted leaves its outbox entry
//! unacked; the next tick re-drains it, the source reports
//! [`AlreadyAbsent`](aether_bloomery::OrphanClaimReleaseCompletion::AlreadyAbsent)
//! because the ref is genuinely gone, and the request completes idempotently.
//! That is exactly why absence is a success rather than an error: making it an
//! error would leave the same authorized request permanently uncompletable —
//! the shape of bug this whole ADR exists to retire.
//!
//! An operational source fault stops the ack prefix instead, leaving the request
//! pending for the next tick. A fault is not a terminal result: journaling one
//! would burn the operator's authorization on a transient network blip.

use std::sync::Arc;
use std::time::Duration;

use aether_actor::Addressable;
use aether_actor::runtime;
use aether_bloomery::{
    Admit, AdmitResult, BloomId, ClaimRefKind, ClaimReleaseOutcome, Digest, Event, Fact, IdempotencyKey,
    MemberClaimReleasePayload, OrphanClaimRelease, OrphanClaimReleaseCompletion, OrphanClaimReleasePayload, Topic,
    WorkpieceId,
};
use aether_bloomery_github::{SourceError, short_hex};
use aether_data::wire::{Error as WireError, from_bytes, to_vec};
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::{ClaimReleaseReactorCapability, ClaimReleaseReactorSetup};

use crate::bloomery::SourceShell;
use crate::bloomery::outbox::TopicOutbox;
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::control::ControlCore;
use crate::store::{SqliteStore, StoreBackend};

/// The self-addressed wake the poll timer fires each interval; its handler drains
/// the release topic and runs each release. Zero-field — the timer carries only
/// the schedule.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.claim_release.claim_release_tick")]
pub struct ClaimReleaseTick {}

/// Runtime state for [`ClaimReleaseReactorCapability`]. The shell + store are
/// `Some` only when configured; a disabled reactor holds neither and spawns no
/// timer.
pub struct ClaimReleaseReactorState {
    source: Option<SourceShell>,
    store: Option<SqliteStore>,
    control_mailbox: MailboxId,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    // The poll timer sidecar; `None` when disabled. Held for its `Drop`, which
    // stops + joins the thread on teardown.
    _timer: Option<TimerHandle>,
}

impl ClaimReleaseReactorState {
    /// Build state over an explicit shell + store — the seam the runtime tests
    /// drive with a fake-GitHub-backed shell and an in-memory store, bypassing
    /// `init` (which needs config and a real connect). Spawns no timer; a test
    /// drives the loop by feeding a [`ClaimReleaseTick`] into the handler.
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

/// The idempotency key a release completion admits under — deterministic in the
/// request digest, so a re-drain (before the ack lands, or after a crash) reduces
/// to a duplicate rather than a second completion. An authorized release
/// completes exactly once.
fn completion_key(request: &Digest) -> IdempotencyKey {
    let mut key = String::with_capacity(45 + 64);
    key.push_str("aether.bloomery.orphan_claim_release_completed:");
    key.push_str(&request.to_hex());
    IdempotencyKey(key)
}

/// Build the completion admit for one finished release.
fn completion_admit(request: &Digest, completion: OrphanClaimReleaseCompletion) -> Result<Admit, WireError> {
    let event = Event {
        idempotency_key: completion_key(request),
        fact: Fact::CompleteOrphanClaimRelease { request: *request, completion },
    };
    to_vec(&event).map(|event| Admit { event })
}

/// Run one authorized release against the source and map its outcome onto the
/// journaled completion vocabulary.
fn release(source: &SourceShell, target: &OrphanClaimRelease) -> Result<OrphanClaimReleaseCompletion, SourceError> {
    Ok(match source.complete_release(Some(&target.expected_holder), &target.ref_kind)? {
        ClaimReleaseOutcome::Released => OrphanClaimReleaseCompletion::Released,
        ClaimReleaseOutcome::AlreadyAbsent => OrphanClaimReleaseCompletion::AlreadyAbsent,
        ClaimReleaseOutcome::Changed { observed_holder } => OrphanClaimReleaseCompletion::Changed { observed_holder },
    })
}

/// Drain the release topic and run each entry's compare-and-swap, returning the
/// [`Admit`]s to forward to the control core and the highest
/// contiguously-processed outbox sequence to ack (`None` when nothing
/// processed). A decode failure, an encode failure, or a source fault stops the
/// ack prefix at the last success so the failed entry re-drains. The factored-out
/// network side, unit-testable against a `SqliteStore` + a fake-GitHub-backed
/// shell without the mail harness.
pub(super) fn drain_and_release(
    store: &mut dyn StoreBackend,
    source: &SourceShell,
) -> rusqlite::Result<(Vec<Admit>, Option<u64>)> {
    let entries = store.drain_topic(Topic::OrphanClaimRelease)?;
    let mut admits = Vec::new();
    let mut ack_through = None;
    for entry in entries {
        let Ok(payload) = from_bytes::<OrphanClaimReleasePayload>(&entry.payload) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::claim_release",
                sequence = entry.sequence,
                "orphan claim release outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        let completion = match release(source, &payload.target) {
            Ok(completion) => completion,
            Err(error) => {
                // Operational, therefore retryable: leave the entry unacked so the
                // request stays pending rather than journaling a terminal result
                // the source never actually reached.
                tracing::warn!(
                    target: "aether_chassis_bloomery::claim_release",
                    sequence = entry.sequence,
                    %error,
                    "orphan claim release failed; stopping the ack prefix to re-drive",
                );
                break;
            }
        };
        match completion_admit(&payload.request, completion) {
            Ok(admit) => {
                tracing::info!(
                    target: "aether_chassis_bloomery::claim_release",
                    sequence = entry.sequence,
                    ?completion,
                    "authorized orphan claim release completed",
                );
                admits.push(admit);
                ack_through = Some(entry.sequence);
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::claim_release",
                    sequence = entry.sequence,
                    %error,
                    "orphan claim release completion did not encode; stopping the ack prefix to re-drive",
                );
                break;
            }
        }
    }
    Ok((admits, ack_through))
}

/// Drain the withdrawn-member release topic and free one claim ref per entry
/// (#5327), returning the highest contiguously-processed outbox sequence to
/// ack.
///
/// The single-ref door rather than the seal-wide one: `release_seal` folds
/// every member release into one mail and the host adds the *admission* ref to
/// it, which would free `refs/bloomery/admission/mainline` while the bloom is
/// still walking. This calls the same expected-holder compare-and-swap the
/// orphan door uses, naming one [`ClaimRefKind::Workpiece`].
///
/// No completion fact is journaled: unlike the orphan door, whose authority is
/// an operator request the journal has to close out, the authority here is the
/// journaled withdrawal itself. `AlreadyAbsent` is a clean success — that is
/// what a redrive after a crash between the ref delete and the ack sees — and
/// `Changed` is a warning rather than a delete, because the CAS read-guard has
/// already spared a ref some later bloom re-claimed.
pub(super) fn drain_and_release_members(
    store: &mut dyn StoreBackend,
    source: &SourceShell,
) -> rusqlite::Result<Option<u64>> {
    let entries = store.drain_topic(Topic::MemberClaimRelease)?;
    let mut ack_through = None;
    for entry in entries {
        let Ok(payload) = from_bytes::<MemberClaimReleasePayload>(&entry.payload) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::claim_release",
                sequence = entry.sequence,
                "member claim release outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        match release_member_ref(source, &payload) {
            Ok(outcome) => {
                tracing::info!(
                    target: "aether_chassis_bloomery::claim_release",
                    sequence = entry.sequence,
                    bloom = %short_hex(&payload.bloom),
                    workpiece = %payload.workpiece.0,
                    ?outcome,
                    "withdrawn member's claim ref released",
                );
                ack_through = Some(entry.sequence);
            }
            Err(error) => {
                // Operational, therefore retryable: leave the entry unacked so
                // the next tick re-drives rather than dropping a ref release
                // the withdrawal already journaled.
                tracing::warn!(
                    target: "aether_chassis_bloomery::claim_release",
                    sequence = entry.sequence,
                    %error,
                    "member claim release failed; stopping the ack prefix to re-drive",
                );
                break;
            }
        }
    }
    Ok(ack_through)
}

/// Run one withdrawn member's expected-holder compare-and-swap against the
/// source.
fn release_member_ref(
    source: &SourceShell,
    payload: &MemberClaimReleasePayload,
) -> Result<ClaimReleaseOutcome, SourceError> {
    let holder = BloomId(payload.bloom);
    source.complete_release(Some(&holder), &ClaimRefKind::Workpiece(WorkpieceId(payload.workpiece.0.clone())))
}

#[runtime]
impl NativeActor for ClaimReleaseReactorCapability {
    type State = ClaimReleaseReactorState;
    type Config = ();
    type Params = ClaimReleaseReactorSetup;

    const NAMESPACE: &'static str = "aether.bloomery.claim_release";

    fn init(
        (): (),
        config: ClaimReleaseReactorSetup,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<ClaimReleaseReactorState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let control_mailbox = <ControlCore as Addressable>::resolve(0, ());

        // Unconfigured → disabled: no shell, no store, no timer. There is no
        // shared ref namespace to release into, so the topic simply accumulates
        // until a token/owner/repo is supplied.
        let Some(source) = config.source else {
            tracing::info!(
                target: "aether_chassis_bloomery::claim_release",
                "claim-release reactor mounted disabled (unconfigured token/owner/repo)",
            );
            return Ok(ClaimReleaseReactorState {
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
            ClaimReleaseTick::ID,
            ClaimReleaseTick::default().encode_into_bytes(),
            "aether-bloomery-claim-release",
            interval,
        );
        tracing::info!(
            target: "aether_chassis_bloomery::claim_release",
            poll_interval_secs = config.poll_interval_secs,
            "claim-release reactor mounted; polling the store for authorized orphan releases",
        );
        Ok(ClaimReleaseReactorState {
            source: Some(source),
            store: Some(store),
            control_mailbox,
            mailer,
            self_mailbox,
            _timer: Some(timer),
        })
    }

    /// Fire an immediate boot tick so a release left undrained by a prior crash
    /// runs without waiting a full poll interval — which is also the redrive that
    /// closes the deleted-but-uncompleted window. Disabled reactors push nothing.
    fn wire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        if state.source.is_some() {
            state.mailer.push(Mail::new(
                state.self_mailbox,
                ClaimReleaseTick::ID,
                ClaimReleaseTick::default().encode_into_bytes(),
                1,
            ));
        }
    }

    /// Poll wake: drain + run the release topic, acking the processed prefix and
    /// forwarding each completion to the control core.
    #[handler::single]
    fn on_claim_release_tick(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: ClaimReleaseTick) {
        let Some(source) = state.source.clone() else {
            return;
        };
        let control_mailbox = state.control_mailbox;
        let Some(store) = state.store.as_mut() else {
            return;
        };

        match drain_and_release(store, &source) {
            Ok((admits, ack_through)) => {
                if let Some(sequence) = ack_through
                    && let Err(error) = store.ack_topic(Topic::OrphanClaimRelease, sequence)
                {
                    tracing::warn!(
                        target: "aether_chassis_bloomery::claim_release",
                        %error,
                        "orphan claim release ack failed; entries re-drive",
                    );
                }
                for admit in admits {
                    // Fire-and-forget: the control core's `on_admit` is reliable
                    // local mail and the completion key dedups a resend.
                    let _ = ctx.send_envelope_detached(control_mailbox, Admit::ID, &admit.encode_into_bytes());
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::claim_release",
                    %error,
                    "orphan claim release drain failed",
                );
            }
        }

        // Withdrawn members' single-ref releases ride the same tick and the
        // same ack-prefix discipline (#5327). Nothing is admitted back: the
        // journaled withdrawal is the authority, so there is no completion to
        // forward to the control core.
        match drain_and_release_members(store, &source) {
            Ok(Some(sequence)) => {
                if let Err(error) = store.ack_topic(Topic::MemberClaimRelease, sequence) {
                    tracing::warn!(
                        target: "aether_chassis_bloomery::claim_release",
                        %error,
                        "member claim release ack failed; entries re-drive",
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::claim_release",
                    %error,
                    "member claim release drain failed",
                );
            }
        }
    }

    /// Control's reply to a fire-and-forget admit. Ok is a no-op; Err is the
    /// refused-admission event that used to miss dispatch.
    #[handler::single]
    fn on_admit_result(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: AdmitResult) {
        if let AdmitResult::Err { error } = mail {
            tracing::error!(target: "aether_chassis_bloomery::claim_release", %error, "admit refused");
        }
    }
}
