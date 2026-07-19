//! The runtime for the land-driver capability (ADR-0149 migration step 3 —
//! issue #3559).
//!
//! A poll-driven loop that turns the reducer's land decisions into the
//! source-port compare-and-swap that is now the landing of record:
//!
//! 1. **Drain.** Each tick drains the store's `aether.bloomery.land` outbox topic
//!    (its own connection, mirroring the executor driver's store ownership) and
//!    decodes each [`LandPayload`](aether_bloomery::LandPayload) — the resolving
//!    bloom, its sealed `expected_base`, and the `new_head` mainline advances to.
//! 2. **Land.** It issues the [`SourceShell::land`] compare-and-swap against
//!    `expected_base`. On [`LandOutcome::Landed`] it admits a [`Fact::Land`] back
//!    to the control core — where [`reduce_land`](aether_bloomery) advances the
//!    mainline and emits the [`LandingReceipt`](aether_bloomery::LandingReceipt)
//!    that the mirror driver projects outward. On [`LandOutcome::BaseMoved`] it
//!    declines to land: a moved mainline forces supersession, never a land onto
//!    the new head (ADR-0149 §The bloom), and V1 permits one unlanded bloom per
//!    mainline so this is the defensive case. The bloom stays `Resolved` and thus
//!    supersedable through the intent-native supersede path — a driver has no
//!    re-authored successor spec to fabricate, and the ADR's successor-seal is a
//!    caller act, not a driver one. The declined entry is acked (a definitive
//!    refusal, not a transient fault) so it does not re-drive forever; a transport
//!    fault stops the ack prefix so the entry re-drains next tick.
//!
//! Config-gated exactly like the mirror / executor drivers: unconfigured (empty
//! token/owner/repo) mounts disabled — no shell, no store, no timer — so a
//! zero-secret dev boot neither errors nor spins; the land outbox accumulates
//! until a token is supplied.

use std::sync::Arc;
use std::time::Duration;

use aether_actor::runtime;
use aether_bloomery::{Admit, BloomId, Digest, Event, Fact, IdempotencyKey, LandOutcome, LandPayload};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::LandDriverCapability;
use crate::bloomery::SourceShell;
use crate::bloomery::mirror::GithubMirrorConfig;
use crate::bloomery::outbox::TopicOutbox;
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::store::{SqliteStore, StoreBackend};

use aether_bloomery::{CONTROL_CORE_NAMESPACE, Topic};
use aether_capabilities::resolve_embedded;

/// The self-addressed wake the poll timer fires each interval; its handler drains
/// the land topic and issues each land. Zero-field — the timer carries only the
/// schedule.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.land.land_tick")]
pub struct LandTick {}

/// Runtime state for [`LandDriverCapability`]. The shell + store are `Some` only
/// when configured; a disabled driver holds neither and spawns no timer.
pub struct LandDriverState {
    source: Option<SourceShell>,
    store: Option<SqliteStore>,
    control_mailbox: MailboxId,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    // The poll timer sidecar; `None` when disabled. Held for its `Drop`, which
    // stops + joins the thread on teardown.
    _timer: Option<TimerHandle>,
}

impl LandDriverState {
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
            control_mailbox: resolve_embedded(CONTROL_CORE_NAMESPACE),
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

/// Drain the land topic and issue each entry's compare-and-swap, returning the
/// [`Admit`]s to forward to the control core (one per landed bloom) and the
/// highest contiguously-processed outbox sequence to ack (`None` when nothing
/// processed). A decode failure, an encode failure, or a transport fault stops
/// the ack prefix at the last success so the failed entry re-drains; a clean
/// base-moved refusal is a processed entry (acked, no admit). The factored-out
/// network side, unit-testable against a `SqliteStore` + a fake-GitHub-backed
/// shell without the mail harness.
fn drain_and_land(store: &mut dyn StoreBackend, source: &SourceShell) -> rusqlite::Result<(Vec<Admit>, Option<u64>)> {
    let entries = store.drain_topic(Topic::LAND)?;
    let mut admits = Vec::new();
    let mut ack_through = None;
    for entry in entries {
        let Ok(payload) = from_bytes::<LandPayload>(&entry.payload) else {
            tracing::warn!(
                target: "aether_bloomery_host::land",
                sequence = entry.sequence,
                "land outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        let bloom = BloomId(payload.bloom);
        match source.land(&bloom, &payload.expected_base, &payload.new_head) {
            Ok(LandOutcome::Landed(_receipt)) => {
                // The durable CAS advanced mainline — admit `Fact::Land` so the
                // reducer folds the advanced mainline + landing receipt. The
                // receipt from the source is the same one `reduce_land` recomputes
                // from `expected_base` + `new_head`, so we carry only the head.
                let event = Event {
                    idempotency_key: land_key(&payload.bloom),
                    fact: Fact::Land { bloom, new_head: payload.new_head },
                };
                let Ok(bytes) = to_vec(&event) else {
                    tracing::warn!(
                        target: "aether_bloomery_host::land",
                        sequence = entry.sequence,
                        "land event did not encode; stopping the ack prefix to re-drive",
                    );
                    break;
                };
                admits.push(Admit { event: bytes });
                ack_through = Some(entry.sequence);
            }
            Ok(LandOutcome::BaseMoved { .. }) => {
                // A moved mainline forces supersession, never a land onto the new
                // head (ADR-0149 §The bloom). The driver declines: the bloom stays
                // Resolved and supersedable through the intent-native path. Ack the
                // definitive refusal so it does not re-drive on every tick.
                tracing::warn!(
                    target: "aether_bloomery_host::land",
                    sequence = entry.sequence,
                    "land refused — mainline moved off the sealed base; the resolved bloom stays supersedable, declining to land",
                );
                ack_through = Some(entry.sequence);
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_bloomery_host::land",
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

#[runtime]
impl NativeActor for LandDriverCapability {
    type State = LandDriverState;
    type Config = GithubMirrorConfig;

    const NAMESPACE: &'static str = "aether.bloomery.land";

    fn init(config: GithubMirrorConfig, ctx: &mut NativeInitCtx<'_>) -> Result<LandDriverState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let control_mailbox = resolve_embedded(CONTROL_CORE_NAMESPACE);

        // Unconfigured → disabled: no shell, no store, no timer. The land outbox
        // accumulates and drains once a token/owner/repo is supplied.
        let configured = !(config.token.is_empty() || config.owner.is_empty() || config.repo.is_empty());
        if !configured {
            tracing::info!(
                target: "aether_bloomery_host::land",
                "land driver mounted disabled (unconfigured token/owner/repo); land outbox will accumulate",
            );
            return Ok(LandDriverState {
                source: None,
                store: None,
                control_mailbox,
                mailer,
                self_mailbox,
                _timer: None,
            });
        }

        let source = SourceShell::connect(&config).map_err(|e| BootError::Other(Box::new(e)))?;
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
            target: "aether_bloomery_host::land",
            owner = %config.owner,
            repo = %config.repo,
            poll_interval_secs = config.poll_interval_secs,
            cas_land_enabled = config.cas_land_enabled,
            "land driver mounted; polling the store for land decisions",
        );
        Ok(LandDriverState {
            source: Some(source),
            store: Some(store),
            control_mailbox,
            mailer,
            self_mailbox,
            _timer: Some(timer),
        })
    }

    /// Fire an immediate boot tick so a land left undrained by a prior crash
    /// issues without waiting a full poll interval. Disabled drivers push nothing.
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
                    && let Err(error) = store.ack_topic(Topic::LAND, sequence)
                {
                    tracing::warn!(target: "aether_bloomery_host::land", %error, "land ack failed; entries re-drive");
                }
                for admit in admits {
                    // Fire-and-forget: the control actor's on_admit is reliable
                    // local mail, and the reducer's idempotency key dedups a
                    // resend, so no settlement handle is needed here.
                    let _ = ctx.send_envelope_detached(control_mailbox, Admit::ID, &admit.encode_into_bytes());
                }
            }
            Err(error) => {
                tracing::warn!(target: "aether_bloomery_host::land", %error, "land drain failed");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
