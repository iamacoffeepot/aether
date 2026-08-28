//! The runtime for the propose reactor capability (ADR-0205).
//!
//! 1. **Drain.** Each tick drains `topic:proposal` and decodes each
//!    [`ProposalPayload`] — the queue head and the base it should seal against.
//! 2. **Host work.** It writes the proposal's bytes into the configuration
//!    store, builds the memberless spec (the proposal address is the bloom-wide
//!    identity), pushes the candidate ref under that bloom id with the
//!    composition workpiece, and admits [`Fact::Seal`].
//! 3. **Ack.** A successful admit (or a known-bloom redrive) advances the
//!    prefix. An in-flight bloom (`ActiveBloomExists`) leaves the entry
//!    unacked so the next land's offer re-drives it.

use std::sync::Arc;
use std::time::Duration;

use aether_actor::Addressable;
use aether_actor::runtime;
use aether_bloomery::{
    Admit, BloomDraft, BloomSpec, ConfigKind, ConfigRegistry, Correspondence, Digest, Event, Fact, Forecast,
    IdempotencyKey, OperatorProposal, ProposalPayload, SharedCorrespondence, Topic, WorkpieceId, digest_of, encode_hex,
};
use aether_data::wire::{Error as WireError, from_bytes, to_vec};
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::{ProposeReactorCapability, ProposeReactorSetup};

use crate::bloomery::CandidatePush;
use crate::bloomery::outbox::TopicOutbox;
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::bloomery::push_candidate;
use crate::control::ControlCore;
use crate::store::{SqliteStore, StoreBackend};

/// The self-addressed wake the poll timer fires each interval.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.propose.propose_tick")]
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

fn seal_admit(proposal: &OperatorProposal, base: Digest) -> Result<Admit, WireError> {
    let event = Event { idempotency_key: seal_key(proposal), fact: Fact::Seal(proposal_spec(proposal, base)) };
    to_vec(&event).map(|event| Admit { event })
}

/// Drain the proposal topic and admit each memberless seal, returning the
/// admits to forward and the highest contiguously-processed sequence to ack.
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
        let Ok(payload) = from_bytes::<ProposalPayload>(&entry.payload) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::propose",
                sequence = entry.sequence,
                "proposal outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        let bytes = match to_vec(&payload.proposal) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::propose",
                    sequence = entry.sequence,
                    %error,
                    "proposal did not encode; stopping the ack prefix to re-drain",
                );
                break;
            }
        };
        let address = payload.proposal.address();
        if let Err(error) = store.record_config(address.as_bytes(), OperatorProposal::NAME, &bytes) {
            tracing::warn!(
                target: "aether_chassis_bloomery::propose",
                sequence = entry.sequence,
                %error,
                "proposal config write failed; stopping the ack prefix to re-drive",
            );
            break;
        }
        let spec = proposal_spec(&payload.proposal, payload.base);
        let bloom = spec.id();
        let object = match correspondence.resolve_backend_object(&payload.proposal.candidate.checkout) {
            Ok(Some(object)) => object,
            Ok(None) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::propose",
                    sequence = entry.sequence,
                    "proposal checkout has no correspondence; stopping the ack prefix to re-drive",
                );
                break;
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::propose",
                    sequence = entry.sequence,
                    %error,
                    "proposal correspondence lookup failed; stopping the ack prefix to re-drive",
                );
                break;
            }
        };
        if publish_candidate
            && let Err(error) = push_candidate(pusher, &bloom, WorkpieceId::COMPOSITION, &encode_hex(object.as_bytes()))
        {
            tracing::warn!(
                target: "aether_chassis_bloomery::propose",
                sequence = entry.sequence,
                %error,
                "proposal candidate push failed; stopping the ack prefix to re-drive",
            );
            break;
        }
        match seal_admit(&payload.proposal, payload.base) {
            Ok(admit) => {
                admits.push(admit);
                ack_through = Some(entry.sequence);
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::propose",
                    sequence = entry.sequence,
                    %error,
                    "proposal seal did not encode; stopping the ack prefix to re-drive",
                );
                break;
            }
        }
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
