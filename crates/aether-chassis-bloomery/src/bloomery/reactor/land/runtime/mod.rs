//! The runtime for the land-reactor capability (ADR-0149 migration step 3 —
//! issue #3559).
//!
//! A poll-driven loop that turns the reducer's land decisions into a landing
//! proposal on the source port, and carries that proposal to its terminal.
//!
//! Landing is a proposal and then an acceptance rather than one write, because
//! the pull request is where a landing becomes correspondence a person can read.
//! Both halves are the reactor's (issue #4953): it proposes the resolved head,
//! merges its own proposal once the gate is green, and admits the landing it
//! then observes.
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
//! 3. **Accept.** On [`LandOutcome::Proposed`] it polls the proposal in the same
//!    pass, and while that proposal is open it asks
//!    [`SourceShell::accept_land`] to merge it. That merge happens only on a
//!    green gate and only while the proposal still offers the exact head the
//!    bloom proved onto the exact base it sealed against; anything moved refuses
//!    and surfaces instead. A refusal that says mainline moved leaves the bloom
//!    where a moved base always leaves it — `Resolved` and supersedable; every
//!    other refusal admits a [`Fact::LandingRejected`], which is what renders as
//!    a blocked landing in the outward view rather than a bloom sitting quietly.
//! 4. **Observe.** Once the proposal reads merged it admits a [`Fact::Land`]
//!    back to the control core — where [`reduce_land`](aether_bloomery) advances
//!    the mainline and emits the
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
//! A merged proposal is not acked on observation. The journal is the
//! acknowledgement oracle (ADR-0149): the entry stays in the contiguous prefix
//! and its idempotent [`Admit`] is resent until [`StoreBackend::journal_holds_any`]
//! sees the deterministic land key. Only then is the prefix advanced, and
//! without re-merging or fabricating another receipt. A dispatch miss or restart
//! therefore cannot forget a landing GitHub has already merged.
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
use aether_bloomery_github::{LandAcceptance, LandingRefusal, SourceError, canonical_issue_number, short_hex};

use crate::bloomery::LandReactorSetup;
use crate::bloomery::SourceShell;
use crate::bloomery::outbox::TopicOutbox;
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::control::ControlCore;
use crate::store::{SqliteStore, StoreBackend};

use aether_bloomery::Topic;

mod proposal;

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

/// Whether the journal already records this bloom's deterministic land admit —
/// the acknowledgement oracle for a merged proposal.
fn journal_holds_land(store: &mut dyn StoreBackend, bloom: &Digest) -> rusqlite::Result<bool> {
    store.journal_holds_any(&[land_key(bloom).0])
}

/// The idempotency key a landing rejection admits under.
///
/// Keyed by the proposed head and the cause as well as the bloom. The same
/// refusal re-observed on the next tick (same head, same cause) reduces to a
/// duplicate, while a later landing of the same bloom — a new head after a
/// repair, even when CI names the same failing checks — admits a second,
/// distinct fact. Keyed by bloom and cause alone, that second refusal was
/// discarded as a replay and the bloom sat `Resolved` with its land entry
/// acked (#5106).
fn rejected_key(bloom: &Digest, head: &Digest, cause: &str) -> IdempotencyKey {
    use core::fmt::Write;
    let mut key = String::from("aether.bloomery.landing_rejected:");
    for byte in bloom.as_bytes() {
        let _ = write!(key, "{byte:02x}");
    }
    key.push(':');
    for byte in head.as_bytes() {
        let _ = write!(key, "{byte:02x}");
    }
    let _ = write!(key, ":{cause}");
    IdempotencyKey(key)
}

/// The [`Watched::Rejected`] a refused landing folds to: a `Fact::LandingRejected`
/// keyed by the proposed head and `cause`, carrying `findings` for the repair
/// the admit dispatches.
///
/// The rejection binds the head that was proposed — the reducer refuses one
/// naming any other head, so a rejection left over from a superseded landing
/// cannot re-open members under a newer one. The detail artifact is the same
/// head: the findings are persisted beside the bloom rather than
/// content-addressed here. The head also belongs in the idempotency key, or a
/// later landing of this bloom that fails the same way is discarded as a
/// replay of this one (#5106).
fn rejected(bloom: &BloomId, payload: &LandPayload, cause: String, findings: String) -> Result<Watched, SourceError> {
    let event = Event {
        idempotency_key: rejected_key(&payload.bloom, &payload.new_head, &cause),
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
        .map(|bytes| Watched::Rejected { admit: Admit { event: bytes }, cause, findings })
        .map_err(|error| SourceError::Malformed(format!("landing rejection did not encode: {error}")))
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

/// The findings text a refused *acceptance* leaves for the repair dispatch —
/// the same bloom-row channel a red gate writes, so one re-entry prompt reads
/// both.
///
/// The refusal states itself and the prompt does not restate it: each variant
/// names a different thing that stopped the merge, and spelling one of them out
/// here would put a wrong sentence over the other two.
fn refusal_findings(refusal: &LandingRefusal) -> String {
    format!(
        "The landing proposal carrying this bloom's integrated head could not be merged: {refusal}\n\nRepair this \
         bloom against current mainline, which has moved since the bloom sealed, and resolve it again."
    )
}

/// What watching an issued land proposal told the drain loop to do with its
/// outbox entry.
enum Watched {
    /// Still open — leave the entry unacked so it re-drains next tick.
    Open,
    /// Terminal with nothing to admit: the proposal was declined, or the merge
    /// refused because mainline moved off the sealed base. Both leave the bloom
    /// `Resolved` and supersedable. Carries the line the drain journals.
    Declined(String),
    /// Terminal — mainline moved; admit this and ack the entry.
    Landed(Admit),
    /// The landing was refused: its checks failed (#4689), or its proposal
    /// drifted off the head the bloom proved (#4953). Terminal *for this entry*
    /// — the admit routes the bloom back into repair or parks it — so the entry
    /// is acked rather than left polling a proposal nothing will accept.
    Rejected {
        /// The `Fact::LandingRejected` to forward.
        admit: Admit,
        /// A one-line reason, for the journal line the drain writes.
        cause: String,
        /// The findings the repair dispatch is directed by, which the caller
        /// persists beside the bloom.
        findings: String,
    },
}

/// Poll an issued land proposal, accept it when its gate has gone green, and
/// fold what happened into the drain loop's outcomes.
fn watch_proposal(
    source: &SourceShell,
    bloom: &BloomId,
    payload: &LandPayload,
    number: u64,
) -> Result<Watched, SourceError> {
    match source.poll_land(bloom, &payload.expected_base, number)? {
        LandProposal::Open => accept_proposal(source, bloom, payload, number),
        LandProposal::Declined => Ok(Watched::Declined("the landing proposal was declined".to_owned())),
        LandProposal::ChecksFailed { failing } => {
            rejected(bloom, payload, failing.join(","), landing_findings(&failing))
        }
        LandProposal::Landed(receipt) => landed(bloom, payload, receipt.new_head),
    }
}

/// Accept the still-open proposal: merge it, once its gate is green and it
/// still offers the head this bloom proved (issue #4953).
///
/// The merge used to be an operator's — the one human action left in an
/// otherwise unattended loop, invisible while it was owed. It is the same trust
/// decision the pipeline already made: ADR-0186 gives the daily ref no required
/// checks because bloomery's own gates prove each landing, so the proposal is
/// correspondence ceremony rather than a review surface.
fn accept_proposal(
    source: &SourceShell,
    bloom: &BloomId,
    payload: &LandPayload,
    number: u64,
) -> Result<Watched, SourceError> {
    match source.accept_land(bloom, &payload.expected_base, &payload.new_head, number)? {
        // Nothing green to press yet. The entry stays unacked and the poll
        // cadence — not this function — decides when to look again, which is
        // what keeps a waiting gate from becoming a spin.
        LandAcceptance::Pending => Ok(Watched::Open),
        LandAcceptance::Accepted => match source.poll_land(bloom, &payload.expected_base, number)? {
            LandProposal::Landed(receipt) => landed(bloom, payload, receipt.new_head),
            // Accepted, but the proposal does not read merged back yet. Leave
            // the entry unacked: the acceptance is idempotent, so the next pass
            // observes the landing rather than pressing anything twice.
            _ => Ok(Watched::Open),
        },
        // A moved mainline forces supersession, never a land onto the new head
        // (ADR-0149 §The bloom) — the same answer `LandOutcome::BaseMoved` gets
        // when the base moves before the proposal is opened rather than after.
        LandAcceptance::Refused(refusal @ LandingRefusal::BaseMoved { .. }) => {
            Ok(Watched::Declined(refusal.to_string()))
        }
        LandAcceptance::Refused(refusal) => rejected(bloom, payload, refusal.to_string(), refusal_findings(&refusal)),
    }
}

/// The [`Watched::Landed`] a merged proposal folds to.
///
/// `new_head` is the head the *receipt* attests — the commit mainline actually
/// became, which under a squash accept is not the head that was proposed.
/// Re-deriving it from the payload would record a mainline that exists nowhere
/// and seal the next bloom on it.
fn landed(bloom: &BloomId, payload: &LandPayload, new_head: Digest) -> Result<Watched, SourceError> {
    let event = Event { idempotency_key: land_key(&payload.bloom), fact: Fact::Land { bloom: *bloom, new_head } };
    to_vec(&event)
        .map(|bytes| Watched::Landed(Admit { event: bytes }))
        .map_err(|error| SourceError::Malformed(format!("land event did not encode: {error}")))
}

/// Drain the land topic and issue each entry's compare-and-swap, returning the
/// [`Admit`]s to forward to the control core (one per landed bloom whose journal
/// key is not yet present) and the highest contiguously-processed outbox sequence
/// to ack (`None` when nothing processed). A merged landing is acknowledged only
/// after the journal holds its deterministic land key — observation produces an
/// admit and holds the prefix; a later drain that sees the key acknowledges
/// without re-merging. A decode failure, an encode failure, a journal lookup
/// fault, or a transport fault stops the ack prefix at the last success so the
/// failed entry re-drains; a clean base-moved refusal is a processed entry
/// (acked, no admit). The factored-out network side, unit-testable against a
/// `SqliteStore` + a fake-GitHub-backed shell without the mail harness.
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
        // The journal is the acknowledgement oracle. A committed land key means
        // this row is done: ack it and do not propose, merge, or admit again.
        match journal_holds_land(store, &payload.bloom) {
            Ok(true) => {
                ack_through = Some(entry.sequence);
                continue;
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::land",
                    sequence = entry.sequence,
                    %error,
                    "land journal lookup failed; leaving the entry durable",
                );
                break;
            }
        }
        let bloom = BloomId(payload.bloom);
        // Assemble the proposal from what the bloom's own lanes wrote, before the
        // land: the title is the mainline commit's subject forever, so it is
        // authored here rather than defaulted in the adapter.
        let assembled = proposal::assemble(store, source, &bloom)?;
        match source.land(&bloom, &payload.expected_base, &payload.new_head, Some(&assembled)) {
            Ok(LandOutcome::Proposed { number }) => {
                match watch_proposal(source, &bloom, &payload, number) {
                    Ok(Watched::Landed(admit)) => {
                        close_member_issues(store, source, &bloom, number);
                        // External merge is observed; the receipt is not durable
                        // until the journal holds `land_key`. Hold the prefix and
                        // return the idempotent Admit so a miss or restart resends.
                        admits.push(admit);
                        break;
                    }
                    Ok(Watched::Rejected { admit, cause, findings }) => {
                        // Persist the findings on the bloom row before the ack,
                        // so the repair dispatch the admit triggers finds them
                        // already there. The empty workpiece key is the
                        // bloom-scope row a failing aggregate verdict uses, and
                        // every re-opened member reads it.
                        store.record_review_findings(payload.bloom.as_bytes(), "", &findings)?;
                        tracing::warn!(
                            target: "aether_chassis_bloomery::land",
                            sequence = entry.sequence,
                            number,
                            %cause,
                            "the landing was refused; routing the bloom back into the line",
                        );
                        admits.push(admit);
                        ack_through = Some(entry.sequence);
                    }
                    Ok(Watched::Declined(why)) => {
                        tracing::warn!(
                            target: "aether_chassis_bloomery::land",
                            sequence = entry.sequence,
                            number,
                            %why,
                            "the landing did not proceed; the resolved bloom stays supersedable",
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

/// Close each member source issue after a bloom lands. Best-effort: a GitHub
/// hiccup, a missing object, or a store read that cannot name the roster is
/// warned and dropped so the land itself still admits. Members whose workpiece
/// ids do not name an issue are skipped with no write.
fn close_member_issues(store: &mut dyn StoreBackend, source: &SourceShell, bloom: &BloomId, pull_request: u64) {
    let members = match store.list_dispatch_descriptions(bloom.0.as_bytes()) {
        Ok(members) => members,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::land",
                %error,
                "could not list members to close source issues after land; the landing itself stands",
            );
            return;
        }
    };
    let comment = format!("**Landed** — bloom `{}` landed via pull request #{pull_request}.", short_hex(&bloom.0));
    for (workpiece, _) in members {
        let Some(issue) = canonical_issue_number(&workpiece) else {
            continue;
        };
        if let Err(error) = source.close_issue(issue, &comment) {
            tracing::warn!(
                target: "aether_chassis_bloomery::land",
                workpiece = workpiece.as_str(),
                issue,
                pull_request,
                %error,
                "failed to close the member's source issue after land; the landing itself stands",
            );
        }
    }
}

#[runtime]
impl NativeActor for LandReactorCapability {
    type State = LandReactorState;
    type Config = ();
    type Params = LandReactorSetup;

    const NAMESPACE: &'static str = "aether.bloomery.land";

    fn init((): (), config: LandReactorSetup, ctx: &mut NativeInitCtx<'_>) -> Result<LandReactorState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let control_mailbox = <ControlCore as Addressable>::resolve(0, ());

        // Unconfigured → disabled: no shell, no store, no timer. The land outbox
        // accumulates and drains once a token/owner/repo is supplied, unless
        // the `fake` backend is selected (#4732).
        let Some(source) = config.source else {
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
        };

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
            repository = ?config.repository,
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

    /// Poll wake: drain + land the land topic, acking the journal-confirmed
    /// prefix and forwarding each still-uncommitted `Fact::Land` to the control
    /// core. The GitHub call runs inline on the dispatcher (the poll cadence
    /// spaces them).
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
                    // Fire-and-forget: the outbox stays unacked until the journal
                    // holds this admit's key, so a dispatch miss redrives, and the
                    // reducer's idempotency key dedups a resend.
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
