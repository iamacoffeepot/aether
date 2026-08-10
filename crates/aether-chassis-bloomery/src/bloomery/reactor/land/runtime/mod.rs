//! The runtime for the land-reactor capability (ADR-0149 migration step 3 —
//! issue #3559).
//!
//! A poll-driven loop that turns the reducer's land decisions into a landing
//! proposal on the source port, and watches that proposal to its terminal.
//!
//! Mainline is protected, so landing is two steps rather than one write: the
//! port proposes the resolved head, a person accepts it, and the reactor admits
//! the landing it observes.
//!
//! 1. **Drain.** Each tick drains the store's `aether.bloomery.land` outbox topic
//!    (its own connection, mirroring the executor reactor's store ownership) and
//!    decodes each [`LandPayload`] — the resolving
//!    bloom, its sealed `expected_base`, and the `new_head` being proposed.
//! 2. **Propose.** It issues [`SourceShell::land`] against `expected_base`. On
//!    [`LandOutcome::BaseMoved`] it declines: a moved mainline forces
//!    supersession, never a land onto the new head (ADR-0149 §The bloom), and V1
//!    permits one unlanded bloom per mainline so this is the defensive case. The
//!    bloom stays `Resolved` and thus supersedable through the intent-native
//!    supersede path — a reactor has no re-authored successor spec to fabricate,
//!    and the ADR's successor-seal is a caller act, not a reactor one.
//! 3. **Watch.** On [`LandOutcome::Proposed`] it polls the proposal in the same
//!    pass. Accepted, it admits a [`Fact::Land`] back to the control core —
//!    where [`reduce_land`](aether_bloomery) advances the mainline and emits the
//!    [`LandingReceipt`](aether_bloomery::LandingReceipt) the mirror reactor
//!    projects outward — carrying the head the *receipt* attests, since a squash
//!    accept makes mainline a commit that is not the one proposed. Declined, the
//!    bloom lands in the same place a moved base leaves it.
//!
//! **The outbox is the watch.** A still-open proposal simply leaves its entry
//! unacked, so it re-drains next tick — no second table to keep in step, durable
//! and crash-replayed for free, and safe to redrive because issuing a land is
//! idempotent (a redrive adopts the proposal it already opened). A declined or
//! base-moved entry is acked (a definitive refusal, not a transient fault) so it
//! does not re-drive forever; a transport fault stops the ack prefix so the entry
//! re-drains next tick.
//!
//! Config-gated exactly like the mirror / executor reactors: unconfigured (empty
//! token/owner/repo) mounts disabled — no shell, no store, no timer — so a
//! zero-secret dev boot neither errors nor spins; the land outbox accumulates
//! until a token is supplied.

use std::sync::Arc;
use std::time::Duration;

use aether_actor::Addressable;
use aether_actor::runtime;
use aether_bloomery::{
    Admit, BloomId, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, LandOutcome, LandPayload, LandProposal,
};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::LandReactorCapability;
use aether_bloomery_github::SourceError;

use crate::bloomery::SourceShell;
use crate::bloomery::mirror::GithubMirrorConfig;
use crate::bloomery::outbox::TopicOutbox;
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::control::ControlCore;
use crate::store::{SqliteStore, StoreBackend};

use aether_bloomery::Topic;

/// The self-addressed wake the poll timer fires each interval; its handler drains
/// the land topic and issues each land. Zero-field — the timer carries only the
/// schedule.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.land.land_tick")]
pub struct LandTick {}

/// Runtime state for [`LandReactorCapability`]. The shell + store are `Some` only
/// when configured; a disabled reactor holds neither and spawns no timer.
pub struct LandReactorState {
    source: Option<SourceShell>,
    store: Option<SqliteStore>,
    control_mailbox: MailboxId,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    // The poll timer sidecar; `None` when disabled. Held for its `Drop`, which
    // stops + joins the thread on teardown.
    _timer: Option<TimerHandle>,
}

impl LandReactorState {
    /// Build state over an explicit shell + store — the seam the runtime tests
    /// drive with a fake-GitHub-backed shell and an in-memory store, bypassing
    /// `init` (which needs config and a real connect). Spawns no timer; a test
    /// drives the loop by feeding a [`LandTick`] into the handler directly.
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

/// The idempotency key a bloom's land admits under — deterministic in the bloom,
/// so a re-drain (before the ack lands, or after a crash-and-replay) reduces to a
/// duplicate rather than a second land. A bloom lands exactly once.
fn land_key(bloom: &Digest) -> IdempotencyKey {
    use core::fmt::Write;
    let mut key = String::with_capacity(21 + 64);
    key.push_str("aether.bloomery.land:");
    for byte in bloom.as_bytes() {
        let _ = write!(key, "{byte:02x}");
    }
    IdempotencyKey(key)
}

/// The idempotency key a landing rejection admits under.
///
/// Keyed by the failing set as well as the bloom, so the *same* red gate
/// re-observed on the next tick reduces to a duplicate, while a repaired bloom
/// whose landing fails a different way still admits. Without the failing set a
/// second, genuinely new rejection would be swallowed as a replay.
fn rejected_key(bloom: &Digest, failing: &[String]) -> IdempotencyKey {
    use core::fmt::Write;
    let mut key = String::from("aether.bloomery.landing_rejected:");
    for byte in bloom.as_bytes() {
        let _ = write!(key, "{byte:02x}");
    }
    let _ = write!(key, ":{}", failing.join(","));
    IdempotencyKey(key)
}

/// The findings text a landing rejection leaves for the repair dispatch — the
/// same bloom-row channel a failing aggregate review writes, so the Refine
/// re-entry prompt picks it up through the path that already exists rather than
/// a second one.
fn landing_findings(failing: &[String]) -> String {
    use core::fmt::Write;
    let mut findings = String::from(
        "The landing branch's CI refused this bloom's integrated head. These checks did not pass; \
         repair them against current mainline, which has moved since the bloom sealed.\n",
    );
    for check in failing {
        let _ = write!(findings, "\n- {check}");
    }
    findings
}

/// What watching an issued land proposal told the drain loop to do with its
/// outbox entry.
enum Watched {
    /// Still open — leave the entry unacked so it re-drains next tick.
    Open,
    /// Terminal with nothing to admit: the proposal was declined, the same place
    /// a moved base leaves the bloom.
    Declined,
    /// Terminal — mainline moved; admit this and ack the entry.
    Landed(Admit),
    /// The proposal's checks failed (#4689). Terminal *for this entry* — the
    /// admit routes the bloom back into repair or parks it — so the entry is
    /// acked rather than left polling a proposal nothing will accept. Carries
    /// the failing check names, which the caller persists as the findings the
    /// repair dispatch is directed by.
    Rejected(Admit, Vec<String>),
}

/// Poll an issued land proposal and fold its state into the drain loop's three
/// outcomes.
fn watch_proposal(
    source: &SourceShell,
    bloom: &BloomId,
    payload: &LandPayload,
    number: u64,
) -> Result<Watched, SourceError> {
    match source.poll_land(bloom, &payload.expected_base, number)? {
        LandProposal::Open => Ok(Watched::Open),
        LandProposal::Declined => Ok(Watched::Declined),
        LandProposal::ChecksFailed { failing } => {
            // The rejection binds the head that was proposed — the reducer
            // refuses one naming any other head, so a rejection left over from
            // a superseded landing cannot re-open members under a newer one.
            // The detail artifact is the same head: the failing check names are
            // the findings, and they are persisted beside the bloom rather than
            // content-addressed here.
            let event = Event {
                idempotency_key: rejected_key(&payload.bloom, &failing),
                fact: Fact::LandingRejected {
                    bloom: *bloom,
                    evidence: Evidence {
                        subject: payload.new_head,
                        kind: EvidenceKind::VerificationResult,
                        detail: payload.new_head,
                    },
                },
            };
            to_vec(&event)
                .map(|bytes| Watched::Rejected(Admit { event: bytes }, failing))
                .map_err(|error| SourceError::Malformed(format!("landing rejection did not encode: {error}")))
        }
        LandProposal::Landed(receipt) => {
            // Mainline actually moved. Admit `Fact::Land` carrying the head the
            // receipt attests — the commit mainline *became*, which under a
            // squash accept is not the head that was proposed. Re-deriving it
            // from the payload would record a mainline that exists nowhere and
            // seal the next bloom on it.
            let event = Event {
                idempotency_key: land_key(&payload.bloom),
                fact: Fact::Land { bloom: *bloom, new_head: receipt.new_head },
            };
            to_vec(&event)
                .map(|bytes| Watched::Landed(Admit { event: bytes }))
                .map_err(|error| SourceError::Malformed(format!("land event did not encode: {error}")))
        }
    }
}

/// Drain the land topic and issue each entry's compare-and-swap, returning the
/// [`Admit`]s to forward to the control core (one per landed bloom) and the
/// highest contiguously-processed outbox sequence to ack (`None` when nothing
/// processed). A decode failure, an encode failure, or a transport fault stops
/// the ack prefix at the last success so the failed entry re-drains; a clean
/// base-moved refusal is a processed entry (acked, no admit). The factored-out
/// network side, unit-testable against a `SqliteStore` + a fake-GitHub-backed
/// shell without the mail harness.
fn drain_and_land(store: &mut dyn StoreBackend, source: &SourceShell) -> rusqlite::Result<(Vec<Admit>, Option<u64>)> {
    let entries = store.drain_topic(Topic::Land)?;
    let mut admits = Vec::new();
    let mut ack_through = None;
    for entry in entries {
        let Ok(payload) = from_bytes::<LandPayload>(&entry.payload) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::land",
                sequence = entry.sequence,
                "land outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        let bloom = BloomId(payload.bloom);
        match source.land(&bloom, &payload.expected_base, &payload.new_head) {
            Ok(LandOutcome::Proposed { number }) => {
                match watch_proposal(source, &bloom, &payload, number) {
                    Ok(Watched::Landed(admit)) => {
                        admits.push(admit);
                        ack_through = Some(entry.sequence);
                    }
                    Ok(Watched::Rejected(admit, failing)) => {
                        // Persist the failing set on the bloom row before the
                        // ack, so the repair dispatch the admit triggers finds
                        // its findings already there. The empty workpiece key
                        // is the bloom-scope row a failing aggregate verdict
                        // uses, and every re-opened member reads it.
                        store.record_review_findings(payload.bloom.as_bytes(), "", &landing_findings(&failing))?;
                        tracing::warn!(
                            target: "aether_chassis_bloomery::land",
                            sequence = entry.sequence,
                            number,
                            failing = %failing.join(", "),
                            "landing checks failed; routing the bloom back into the line",
                        );
                        admits.push(admit);
                        ack_through = Some(entry.sequence);
                    }
                    Ok(Watched::Declined) => {
                        tracing::warn!(
                            target: "aether_chassis_bloomery::land",
                            sequence = entry.sequence,
                            number,
                            "landing proposal was declined; the resolved bloom stays supersedable",
                        );
                        ack_through = Some(entry.sequence);
                    }
                    // Still open: leave the entry unacked so it re-drains next
                    // tick. The outbox *is* the watch — durable already, and
                    // replayed after a crash — and issuing a land is idempotent,
                    // so the redrive adopts the same proposal rather than
                    // opening another. No second table to keep in step.
                    //
                    // An open proposal holds the ack prefix, which parks any
                    // later land entry behind it. That matches the invariant
                    // rather than fighting it: V1 permits one sealed, unlanded
                    // bloom per mainline, so there is at most one to park.
                    Ok(Watched::Open) => break,
                    Err(error) => {
                        tracing::warn!(
                            target: "aether_chassis_bloomery::land",
                            sequence = entry.sequence,
                            number,
                            %error,
                            "land watch failed; stopping the ack prefix to re-drive",
                        );
                        break;
                    }
                }
            }
            Ok(LandOutcome::BaseMoved { .. }) => {
                // A moved mainline forces supersession, never a land onto the new
                // head (ADR-0149 §The bloom). The reactor declines: the bloom stays
                // Resolved and supersedable through the intent-native path. Ack the
                // definitive refusal so it does not re-drive on every tick.
                tracing::warn!(
                    target: "aether_chassis_bloomery::land",
                    sequence = entry.sequence,
                    "land refused — mainline moved off the sealed base; the resolved bloom stays supersedable, declining to land",
                );
                ack_through = Some(entry.sequence);
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::land",
                    sequence = entry.sequence,
                    %error,
                    "land transport failed; stopping the ack prefix to re-drive",
                );
                break;
            }
        }
    }
    Ok((admits, ack_through))
}

fn connect_land_source(config: &GithubMirrorConfig) -> Result<SourceShell, BootError> {
    #[cfg(any(test, feature = "testing"))]
    if config.uses_fixture() {
        return Ok(config.fixture_source());
    }
    SourceShell::connect(config).map_err(|e| BootError::Other(Box::new(e)))
}

#[runtime]
impl NativeActor for LandReactorCapability {
    type State = LandReactorState;
    type Config = GithubMirrorConfig;

    const NAMESPACE: &'static str = "aether.bloomery.land";

    fn init(config: GithubMirrorConfig, ctx: &mut NativeInitCtx<'_>) -> Result<LandReactorState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let control_mailbox = <ControlCore as Addressable>::resolve(0, ());

        // Unconfigured → disabled: no shell, no store, no timer. The land outbox
        // accumulates and drains once a token/owner/repo is supplied, unless
        // the `fake` backend is selected (#4732).
        let configured =
            config.uses_fixture() || !(config.token.is_empty() || config.owner.is_empty() || config.repo.is_empty());
        if !configured {
            tracing::info!(
                target: "aether_chassis_bloomery::land",
                "land reactor mounted disabled (unconfigured token/owner/repo); land outbox will accumulate",
            );
            return Ok(LandReactorState {
                source: None,
                store: None,
                control_mailbox,
                mailer,
                self_mailbox,
                _timer: None,
            });
        }

        let source = connect_land_source(&config)?;
        let store = SqliteStore::open(&config.store_path).map_err(|e| BootError::Other(Box::new(e)))?;
        let interval = Duration::from_secs(config.poll_interval_secs.max(1));
        let timer = spawn_timer(
            Arc::clone(&mailer),
            self_mailbox,
            LandTick::ID,
            LandTick::default().encode_into_bytes(),
            "aether-bloomery-land",
            interval,
        );
        tracing::info!(
            target: "aether_chassis_bloomery::land",
            owner = %config.owner,
            repo = %config.repo,
            poll_interval_secs = config.poll_interval_secs,
            cas_land_enabled = config.cas_land_enabled,
            "land reactor mounted; polling the store for land decisions",
        );
        Ok(LandReactorState {
            source: Some(source),
            store: Some(store),
            control_mailbox,
            mailer,
            self_mailbox,
            _timer: Some(timer),
        })
    }

    /// Fire an immediate boot tick so a land left undrained by a prior crash
    /// issues without waiting a full poll interval. Disabled reactors push nothing.
    fn wire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        if state.source.is_some() {
            state.mailer.push(Mail::new(state.self_mailbox, LandTick::ID, LandTick::default().encode_into_bytes(), 1));
        }
    }

    /// Poll wake: drain + land the land topic, acking the processed prefix and
    /// forwarding each landed bloom's `Fact::Land` to the control core. The GitHub
    /// call runs inline on the dispatcher (the poll cadence spaces them).
    #[handler::single]
    fn on_land_tick(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: LandTick) {
        let Some(source) = state.source.clone() else {
            return;
        };
        let control_mailbox = state.control_mailbox;
        let Some(store) = state.store.as_mut() else {
            return;
        };

        match drain_and_land(store, &source) {
            Ok((admits, ack_through)) => {
                if let Some(sequence) = ack_through
                    && let Err(error) = store.ack_topic(Topic::Land, sequence)
                {
                    tracing::warn!(target: "aether_chassis_bloomery::land", %error, "land ack failed; entries re-drive");
                }
                for admit in admits {
                    // Fire-and-forget: the control actor's on_admit is reliable
                    // local mail, and the reducer's idempotency key dedups a
                    // resend, so no settlement handle is needed here.
                    let _ = ctx.send_envelope_detached(control_mailbox, Admit::ID, &admit.encode_into_bytes());
                }
            }
            Err(error) => {
                tracing::warn!(target: "aether_chassis_bloomery::land", %error, "land drain failed");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
