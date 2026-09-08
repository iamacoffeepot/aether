//! The runtime for the propose reactor capability (ADR-0205).
//!
//! 1. **Drain.** Each tick drains `topic:proposal` and inspects that row's
//!    persisted result before decoding the [`ProposalPayload`].
//! 2. **Host work.** An unrecorded entry writes the proposal's bytes into the
//!    configuration store, builds the memberless spec (the proposal address is
//!    the bloom-wide identity), and pushes the candidate ref under that bloom
//!    id with the composition workpiece. The exact [`Fact::Seal`] Event is
//!    recorded on the outbox row before a detached [`Admit`] is returned.
//! 3. **Ack.** The prefix advances only after the journal holds the receipt
//!    key. A journaled receipt acks without re-publishing or re-reading
//!    correspondence; a pending receipt resends its exact Admit batch and
//!    holds later rows.
//!
//! External publication before receipt persistence is still a window; this is
//! not atomic exactly-once. A recorded receipt is never republished.

use std::slice::from_ref;
use std::sync::Arc;
use std::time::Duration;

use aether_actor::Addressable;
use aether_actor::runtime;
use aether_bloomery::{
    Admit, BloomDraft, BloomSpec, ConfigKind, ConfigRegistry, Correspondence, Digest, Event, Fact, Forecast,
    IdempotencyKey, OperatorProposal, ProposalPayload, SharedCorrespondence, Topic, WorkpieceId, digest_of, encode_hex,
};
use aether_data::MailboxId;
use aether_data::wire::{from_bytes, to_vec};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;

use super::{ProposeReactorCapability, ProposeReactorSetup};

use crate::bloomery::CandidatePush;
use crate::bloomery::outbox::{OutboxResultDelivery, TopicOutbox};
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::bloomery::push_candidate;
use crate::control::ControlCore;
use crate::store::{SqliteStore, StoreBackend};

/// The self-addressed wake the poll timer fires each interval.
#[aether_data::kind(name = "aether.bloomery.propose.propose_tick", default)]
pub struct ProposeTick {}

/// Runtime state for [`ProposeReactorCapability`].
pub struct ProposeReactorState {
    correspondence: Option<SharedCorrespondence>,
    pusher: Option<Arc<dyn CandidatePush>>,
    publish_candidate: bool,
    store: Option<SqliteStore>,
    control_mailbox: MailboxId,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    _timer: Option<TimerHandle>,
}

impl ProposeReactorState {
    /// Build state over explicit parts — the seam the runtime tests drive
    /// without `init`. Spawns no timer.
    #[must_use]
    pub fn with_parts(
        correspondence: Option<SharedCorrespondence>,
        pusher: Option<Arc<dyn CandidatePush>>,
        publish_candidate: bool,
        store: Option<SqliteStore>,
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
    ) -> Self {
        Self {
            correspondence,
            pusher,
            publish_candidate,
            store,
            control_mailbox: <ControlCore as Addressable>::resolve(0, ()),
            mailer,
            self_mailbox,
            _timer: None,
        }
    }
}

fn seal_key(proposal: &OperatorProposal) -> IdempotencyKey {
    IdempotencyKey(format!("aether.bloomery.proposal_seal:{}", digest_of(proposal).to_hex()))
}

fn proposal_spec(proposal: &OperatorProposal, base: Digest) -> BloomSpec {
    let mut configs = ConfigRegistry::default();
    configs.insert::<OperatorProposal>(proposal.address());
    BloomDraft { proposals: Vec::new(), base, configs, forecast: Forecast::default() }.seal()
}

/// Build the exact seal Event the sidecar persists and the Admit later carries.
fn seal_event(proposal: &OperatorProposal, base: Digest) -> Event {
    Event { idempotency_key: seal_key(proposal), fact: Fact::Seal(proposal_spec(proposal, base)) }
}

fn seal_unrecorded(
    store: &mut dyn StoreBackend,
    correspondence: &dyn Correspondence,
    pusher: &dyn CandidatePush,
    publish_candidate: bool,
    sequence: u64,
    payload: &[u8],
) -> Option<Admit> {
    let Ok(payload) = from_bytes::<ProposalPayload>(payload) else {
        tracing::warn!(
            target: "aether_chassis_bloomery::propose",
            sequence,
            "proposal outbox entry did not decode; stopping the ack prefix to re-drain",
        );
        return None;
    };
    let bytes = match to_vec(&payload.proposal) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::propose",
                sequence,
                %error,
                "proposal did not encode; stopping the ack prefix to re-drain",
            );
            return None;
        }
    };
    let address = payload.proposal.address();
    if let Err(error) = store.record_config(address.as_bytes(), OperatorProposal::NAME, &bytes) {
        tracing::warn!(
            target: "aether_chassis_bloomery::propose",
            sequence,
            %error,
            "proposal config write failed; stopping the ack prefix to re-drive",
        );
        return None;
    }
    let spec = proposal_spec(&payload.proposal, payload.base);
    let bloom = spec.id();
    let object = match correspondence.resolve_backend_object(&payload.proposal.candidate.checkout) {
        Ok(Some(object)) => object,
        Ok(None) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::propose",
                sequence,
                "proposal checkout has no correspondence; stopping the ack prefix to re-drive",
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::propose",
                sequence,
                %error,
                "proposal correspondence lookup failed; stopping the ack prefix to re-drive",
            );
            return None;
        }
    };
    if publish_candidate
        && let Err(error) = push_candidate(pusher, &bloom, WorkpieceId::COMPOSITION, &encode_hex(object.as_bytes()))
    {
        tracing::warn!(
            target: "aether_chassis_bloomery::propose",
            sequence,
            %error,
            "proposal candidate push failed; stopping the ack prefix to re-drive",
        );
        return None;
    }
    let event = seal_event(&payload.proposal, payload.base);
    if let Err(error) = store.record_topic_results(Topic::Proposal, sequence, from_ref(&event)) {
        tracing::warn!(
            target: "aether_chassis_bloomery::propose",
            sequence,
            %error,
            "proposal result did not persist; leaving the entry undelivered",
        );
        return None;
    }
    match to_vec(&event) {
        Ok(bytes) => Some(Admit { event: bytes }),
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::propose",
                sequence,
                %error,
                "proposal seal did not encode; leaving the entry undelivered",
            );
            None
        }
    }
}

/// Drain the proposal topic and admit each memberless seal, returning the
/// [`Admit`]s to forward and the highest journal-confirmed outbox sequence to
/// ack (`None` when nothing is journal-confirmed). Replay inspects a persisted
/// seal before any payload decode, config write, correspondence lookup, or
/// candidate publication: a journaled receipt acks and continues; a pending
/// receipt resends its exact Admit batch and stops the prefix; an unrecorded
/// entry keeps current config / spec / correspondence / publication behaviour,
/// persists the exact Seal Event, returns its Admit, and holds the prefix
/// without acking. A decode failure, a store fault, or a publication fault
/// stops the ack prefix at the last success so the failed entry re-drains.
/// The host effect is not atomic with the receipt row.
pub(super) fn drain_and_seal(
    store: &mut dyn StoreBackend,
    correspondence: &dyn Correspondence,
    pusher: &dyn CandidatePush,
    publish_candidate: bool,
) -> rusqlite::Result<(Vec<Admit>, Option<u64>)> {
    let entries = store.drain_topic(Topic::Proposal)?;
    let mut admits = Vec::new();
    let mut ack_through = None;
    for entry in entries {
        match store.replay_topic_results(Topic::Proposal, entry.sequence) {
            Ok(OutboxResultDelivery::Journaled) => {
                ack_through = Some(entry.sequence);
                continue;
            }
            Ok(OutboxResultDelivery::Pending(pending)) => {
                admits.extend(pending);
                break;
            }
            Ok(OutboxResultDelivery::Unrecorded) => {}
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::propose",
                    sequence = entry.sequence,
                    %error,
                    "proposal result replay failed; leaving the entry undelivered",
                );
                break;
            }
        }
        let Some(admit) =
            seal_unrecorded(store, correspondence, pusher, publish_candidate, entry.sequence, &entry.payload)
        else {
            break;
        };
        admits.push(admit);
        break;
    }
    Ok((admits, ack_through))
}

#[runtime]
impl NativeActor for ProposeReactorCapability {
    type State = ProposeReactorState;
    type Config = ();
    type Params = ProposeReactorSetup;

    const NAMESPACE: &'static str = "aether.bloomery.propose";

    fn init(
        (): (),
        config: ProposeReactorSetup,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<ProposeReactorState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let control_mailbox = <ControlCore as Addressable>::resolve(0, ());

        let (Some(correspondence), Some(pusher)) = (config.correspondence, config.pusher) else {
            tracing::info!(
                target: "aether_chassis_bloomery::propose",
                "propose reactor mounted disabled (no correspondence or pusher)",
            );
            return Ok(ProposeReactorState {
                correspondence: None,
                pusher: None,
                publish_candidate: false,
                store: None,
                control_mailbox,
                mailer,
                self_mailbox,
                _timer: None,
            });
        };

        let store = SqliteStore::open(&config.store_path).map_err(|error| BootError::Other(Box::new(error)))?;
        let interval = Duration::from_secs(config.poll_interval_secs.max(1));
        let timer = spawn_timer(
            Arc::clone(&mailer),
            self_mailbox,
            ProposeTick::ID,
            ProposeTick::default().encode_into_bytes(),
            "aether-bloomery-propose",
            interval,
        );
        tracing::info!(
            target: "aether_chassis_bloomery::propose",
            poll_interval_secs = config.poll_interval_secs,
            "propose reactor mounted; polling the store for operator proposals",
        );
        Ok(ProposeReactorState {
            correspondence: Some(correspondence),
            pusher: Some(pusher),
            publish_candidate: config.publish_candidate,
            store: Some(store),
            control_mailbox,
            mailer,
            self_mailbox,
            _timer: Some(timer),
        })
    }

    fn wire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        if state.store.is_some() {
            state.mailer.push(Mail::new(
                state.self_mailbox,
                ProposeTick::ID,
                ProposeTick::default().encode_into_bytes(),
                1,
            ));
        }
    }

    #[handler::single]
    fn on_propose_tick(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: ProposeTick) {
        let (Some(correspondence), Some(pusher), Some(store)) =
            (state.correspondence.clone(), state.pusher.clone(), state.store.as_mut())
        else {
            return;
        };
        let control_mailbox = state.control_mailbox;
        match drain_and_seal(store, correspondence.as_ref(), pusher.as_ref(), state.publish_candidate) {
            Ok((admits, ack_through)) => {
                if let Some(sequence) = ack_through
                    && let Err(error) = store.ack_topic(Topic::Proposal, sequence)
                {
                    tracing::warn!(
                        target: "aether_chassis_bloomery::propose",
                        %error,
                        "proposal ack failed; entries re-drive",
                    );
                }
                for admit in admits {
                    let _ = ctx.send_envelope_detached(control_mailbox, Admit::ID, &admit.encode_into_bytes());
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::propose",
                    %error,
                    "proposal drain failed",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
