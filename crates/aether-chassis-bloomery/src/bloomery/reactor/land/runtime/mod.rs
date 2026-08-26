//! The runtime for the land-reactor capability (ADR-0149 migration step 3 —
//! issue #3559).
//!
//! A poll-driven loop that turns the reducer's land decisions into a landing
//! proposal on the source port, and carries that proposal to its terminal.
//!
//! Landing is a proposal and then an acceptance rather than one write, because
//! the pull request is where a landing becomes correspondence a person can read.
//! Both halves are the reactor's (issue #4953): it proposes the resolved head,
//! merges its own proposal once the structural gates hold, and admits the
//! landing it then observes.
//!
//! 1. **Drain.** Each tick drains the store's `aether.bloomery.land` outbox topic
//!    (its own connection, mirroring the executor reactor's store ownership) and
//!    decodes each [`LandPayload`] — the resolving
//!    bloom, its sealed `expected_base`, and the `new_head` being proposed.
//! 2. **Propose.** It issues [`LandingSource::land_proposal`] against
//!    `expected_base`. On [`ProposalOutcome::BaseMoved`] it declines: a moved
//!    mainline forces supersession, never a land onto the new head (ADR-0149
//!    §The bloom), and V1 permits one unlanded bloom per mainline so this is the
//!    defensive case. The bloom stays `Resolved` and thus supersedable through
//!    the intent-native supersede path — a reactor has no re-authored successor
//!    spec to fabricate, and the ADR's successor-seal is a caller act, not a
//!    reactor one.
//! 3. **Accept.** On [`ProposalOutcome::Proposed`] it polls the proposal in the same
//!    pass, and while that proposal is open it asks
//!    [`LandingSource::accept_land`] to merge it. That merge happens once the
//!    structural gates hold — the proposal is this bloom's landing branch
//!    aimed at mainline, still offering the exact head the bloom proved onto
//!    the exact base it sealed against — without consulting check state
//!    (ADR-0186, #5110). Anything moved refuses and surfaces instead. A
//!    refusal that says mainline moved leaves the bloom where a moved base
//!    always leaves it — `Resolved` and supersedable; every other refusal
//!    admits a [`Fact::LandingRejected`], which is what renders as a blocked
//!    landing in the outward view rather than a bloom sitting quietly.
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

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use aether_actor::Addressable;
use aether_actor::runtime;
use aether_bloomery::{
    Admit, AdmitResult, BloomId, CommissionStatus, Digest, Event, Evidence, EvidenceKind, Fact, IdempotencyKey,
    LandPayload, SourceReplicaPayload, WorkpieceId,
};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId};
use aether_substrate::Mail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use serde::{Deserialize, Serialize};

use super::LandReactorCapability;
use aether_bloomery_github::{
    LandAcceptance, LandProposal, LandingRefusal, LandingSource, ProposalOutcome, SourceError, canonical_issue_number,
    short_hex,
};

use crate::bloomery::LandReactorSetup;

use crate::bloomery::outbox::TopicOutbox;
use crate::bloomery::poll_timer::{TimerHandle, spawn_timer};
use crate::control::ControlCore;
use crate::store::membership;
use crate::store::{CANDIDATE_HASH_OCCASION_LAND, CommissionBackend, SqliteStore, StoreBackend};

use aether_bloomery::Topic;

mod proposal;
mod receipt;

/// The self-addressed wake the poll timer fires each interval; its handler drains
/// the land topic and issues each land. Zero-field — the timer carries only the
/// schedule.
#[derive(Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.bloomery.land.land_tick")]
pub struct LandTick {}

/// Catch-up cadence for closing GitHub issues named by terminal commissions.
///
/// At the 5-second default poll interval (`bloomery/config.rs`), 60 ticks is one
/// pass every five minutes. The per-pass cap exists because this handler makes
/// its GitHub calls inline on the dispatcher: an unbounded sweep over a cold
/// board would stall it for the length of every round trip.
const RECONCILE_EVERY_TICKS: u32 = 60;
const RECONCILE_CLOSES_PER_PASS: usize = 20;

/// Runtime state for [`LandReactorCapability`]. The shell + store are `Some` only
/// when configured; a disabled reactor holds neither and spawns no timer.
pub struct LandReactorState {
    source: Option<Arc<dyn LandingSource>>,
    store: Option<SqliteStore>,
    control_mailbox: MailboxId,
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    // The poll timer sidecar; `None` when disabled. Held for its `Drop`, which
    // stops + joins the thread on teardown.
    _timer: Option<TimerHandle>,
    emit_source_replica: bool,
    reconcile_countdown: u32,
    reconciled: BTreeSet<u64>,
}

impl LandReactorState {
    /// Build state over an explicit shell + store — the seam the runtime tests
    /// drive with a fake-GitHub-backed shell and an in-memory store, bypassing
    /// `init` (which needs config and a real connect). Spawns no timer; a test
    /// drives the loop by feeding a [`LandTick`] into the handler directly.
    #[must_use]
    pub fn with_parts(
        source: Option<Arc<dyn LandingSource>>,
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
            emit_source_replica: false,
            reconcile_countdown: RECONCILE_EVERY_TICKS,
            reconciled: BTreeSet::new(),
        }
    }
}

/// The idempotency key a bloom's land admits under — deterministic in the bloom,
/// so a re-drain (before the ack lands, or after a crash-and-replay) reduces to a
/// duplicate rather than a second land. A bloom lands exactly once.
fn land_key(bloom: &Digest) -> IdempotencyKey {
    let mut key = String::with_capacity(21 + 64);
    key.push_str("aether.bloomery.land:");
    key.push_str(&bloom.to_hex());
    IdempotencyKey(key)
}

/// Whether the journal already records this bloom's deterministic land admit —
/// the acknowledgement oracle for a merged proposal.
fn journal_holds_land(store: &mut dyn StoreBackend, bloom: &Digest) -> rusqlite::Result<bool> {
    store.journal_holds_any(&[land_key(bloom).0])
}

/// Host-mint the source-replica row after the land key is in the journal.
/// `false` leaves the land entry unacked so a encode/store fault redrives.
fn enqueue_source_replica(store: &mut dyn StoreBackend, new_head: &Digest) -> bool {
    match to_vec(&SourceReplicaPayload { new_head: *new_head }) {
        Ok(bytes) => match store.enqueue_outbox(Topic::SourceReplica.as_str(), &bytes, None) {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::land",
                    %error,
                    "source replica enqueue failed; leaving the land entry durable",
                );
                false
            }
        },
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::land",
                %error,
                "source replica payload did not encode; leaving the land entry durable",
            );
            false
        }
    }
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
    let mut key = String::from("aether.bloomery.landing_rejected:");
    key.push_str(&bloom.to_hex());
    key.push(':');
    key.push_str(&head.to_hex());
    key.push(':');
    key.push_str(cause);
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

/// The findings text a refused *acceptance* leaves for the repair dispatch —
/// the same bloom-row channel a failing aggregate review writes, so one
/// re-entry prompt reads both.
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
    ///
    /// `new_head` is the commit mainline actually became. Under a squash
    /// accept that is not the proposed head, so it rides here rather than
    /// being re-derived from [`LandPayload`] — that would name a commit
    /// that exists nowhere.
    Landed {
        /// The `Fact::Land` to forward.
        admit: Admit,
        /// The squash (or fast-forward) commit the receipt attests.
        new_head: Digest,
    },
    /// The landing was refused: its proposal drifted off the head the bloom
    /// proved (#4953), or the source refused the merge. Terminal *for this
    /// entry* — the admit routes the bloom back into repair or parks it — so
    /// the entry is acked rather than left polling a proposal nothing will
    /// accept.
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

/// Poll an issued land proposal, accept it when the structural gates hold, and
/// fold what happened into the drain loop's outcomes.
fn watch_proposal(
    source: &dyn LandingSource,
    bloom: &BloomId,
    payload: &LandPayload,
    number: u64,
) -> Result<Watched, SourceError> {
    match source.poll_land(bloom, &payload.expected_base, number)? {
        // Production no longer emits `ChecksFailed` (#5110). Treat a residual
        // encoding the same as `Open` rather than routing it to
        // [`Watched::Rejected`]: a check conclusion is not a landing verdict.
        LandProposal::Open | LandProposal::ChecksFailed { .. } => accept_proposal(source, bloom, payload, number),
        LandProposal::Declined => Ok(Watched::Declined("the landing proposal was declined".to_owned())),
        LandProposal::Landed(receipt) => landed(bloom, payload, receipt.new_head),
    }
}

/// Accept the still-open proposal: merge it once the structural gates hold and
/// it still offers the head this bloom proved (issue #4953).
///
/// The merge used to be an operator's — the one human action left in an
/// otherwise unattended loop, invisible while it was owed. It is the same trust
/// decision the pipeline already made: ADR-0186 gives the daily ref no required
/// checks because bloomery's own gates prove each landing, so the proposal is
/// correspondence ceremony rather than a review surface.
fn accept_proposal(
    source: &dyn LandingSource,
    bloom: &BloomId,
    payload: &LandPayload,
    number: u64,
) -> Result<Watched, SourceError> {
    match source.accept_land(bloom, &payload.expected_base, &payload.new_head, number)? {
        // Nothing to press yet. The entry stays unacked and the poll cadence
        // — not this function — decides when to look again.
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
        .map(|bytes| Watched::Landed { admit: Admit { event: bytes }, new_head })
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
#[cfg(test)]
fn drain_and_land(store: &mut SqliteStore, source: &dyn LandingSource) -> rusqlite::Result<(Vec<Admit>, Option<u64>)> {
    drain_and_land_emitting(store, source, false)
}

fn drain_and_land_emitting(
    store: &mut SqliteStore,
    source: &dyn LandingSource,
    emit_source_replica: bool,
) -> rusqlite::Result<(Vec<Admit>, Option<u64>)> {
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
                if emit_source_replica && !enqueue_source_replica(store, &payload.new_head) {
                    break;
                }
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
        let assembled = proposal::assemble(store, &bloom)?;
        match source.land_proposal(&bloom, &payload.expected_base, &payload.new_head, Some(&assembled)) {
            Ok(ProposalOutcome::Proposed { number }) => {
                match watch_proposal(source, &bloom, &payload, number) {
                    Ok(Watched::Landed { admit, new_head }) => {
                        mark_member_commissions_landed(store, &bloom);
                        record_landed_candidate_hashes(store, &bloom);
                        close_member_source_issues(store, source, &bloom, &payload.expected_base, &new_head);
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
            Ok(ProposalOutcome::BaseMoved { .. }) => {
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

/// Close each member's canonical source issue after the land is observed.
///
/// A day-branch merge does not fire GitHub's `Closes #N` keywords, so this
/// is the close. A workpiece that names no object is skipped; a per-issue
/// refusal is warned and dropped so one unreachable issue cannot cost the
/// others their close, or the land its admit.
fn close_member_source_issues(
    store: &mut SqliteStore,
    source: &dyn LandingSource,
    bloom: &BloomId,
    previous_base: &Digest,
    new_head: &Digest,
) {
    let members = match store.list_dispatch_descriptions(bloom.0.as_bytes()) {
        Ok(members) => members,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::land",
                %error,
                "could not list members to close source issues; the landing itself stands",
            );
            return;
        }
    };
    let key = format!("receipt:bloom:{}", short_hex(&bloom.0));
    let comment = receipt::landed_comment(store, bloom, previous_base, new_head).unwrap_or_else(|error| {
        tracing::warn!(
            target: "aether_chassis_bloomery::land",
            %error,
            "could not assemble the landing comment; writing the lead sentence so the close still proceeds",
        );
        format!(
            "**Landed** — bloom `{}` landed; mainline moved `{}` → `{}`.",
            short_hex(&bloom.0),
            short_hex(previous_base),
            short_hex(new_head)
        )
    });
    for (workpiece, _) in members {
        let Some(number) = canonical_issue_number(&workpiece) else {
            continue;
        };
        if let Err(error) = source.close_issue(number, &key, &comment) {
            tracing::warn!(
                target: "aether_chassis_bloomery::land",
                workpiece = workpiece.as_str(),
                number,
                %error,
                "failed to close the member source issue; the landing itself stands",
            );
        }
    }
}

/// Close GitHub issues named by terminal commissions that the live land path
/// never saw: a land that happened while the mirror was mounted disabled, or
/// an ack that already passed so the outbox cannot redrive the close.
///
/// Lists landed then cancelled commissions, maps each id through
/// [`canonical_issue_number`], skips numbers already in `reconciled`, and
/// closes at most [`RECONCILE_CLOSES_PER_PASS`] of the remainder. A per-issue
/// error is warned and the number is left out of the set so the next pass
/// retries it. A list error is warned and the pass returns 0 — a store fault
/// must not cost the drain its tick.
fn reconcile_terminal_commissions(
    store: &mut SqliteStore,
    source: &dyn LandingSource,
    reconciled: &mut BTreeSet<u64>,
) -> usize {
    let landed = match store.list(Some(CommissionStatus::Landed)) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::land",
                %error,
                "could not list landed commissions for issue reconcile; skipping this pass",
            );
            return 0;
        }
    };
    let cancelled = match store.list(Some(CommissionStatus::Cancelled)) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::land",
                %error,
                "could not list cancelled commissions for issue reconcile; skipping this pass",
            );
            return 0;
        }
    };

    let candidates: Vec<_> = landed
        .into_iter()
        .chain(cancelled)
        .filter_map(|head| {
            let number = canonical_issue_number(&head.id.0)?;
            (!reconciled.contains(&number)).then_some((head, number))
        })
        .take(RECONCILE_CLOSES_PER_PASS)
        .collect();

    let mut closed = 0;
    for (head, number) in candidates {
        let key = format!("commission:{}:reconciled", head.id.0);
        let comment = reconciled_comment(&head.id.0, head.status.as_str());
        match source.close_issue(number, &key, &comment) {
            Ok(()) => {
                reconciled.insert(number);
                closed += 1;
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::land",
                    workpiece = head.id.0.as_str(),
                    number,
                    %error,
                    "failed to close a terminal commission's source issue; will retry next pass",
                );
            }
        }
    }
    closed
}

/// The one-line catch-up close: this issue's commission is terminal on the
/// board, so the issue is being closed to match.
fn reconciled_comment(workpiece: &str, status: &str) -> String {
    format!("This issue's commission (`{workpiece}`) is {status} on the board, so the issue is being closed to match.")
}

/// Mark each *resolved* member's commission landed before the replica is
/// projected. Local status is the authority; a missing commission or a store
/// fault is warned and dropped so the land itself still admits. The mirror then
/// projects the landed replica and closes it best-effort (ADR-0199).
///
/// The sealed membership is not the landed set. A member an operator withdrew
/// while the bloom walked (#5327) produced no resolution claim and contributed
/// no candidate to the fold, so nothing of it is in the head being landed —
/// while it is still named in the spec and still has a `dispatch_description`
/// row. Stamping it landed strands the workpiece: every door that could
/// re-author or re-seal it requires `open`, so the id, its intent, and its
/// approvals all leave the line with a bloom that never ran them.
///
/// So the resolution — not the membership — decides, and an unreadable answer
/// marks nothing. A commission left open when it should be landed is one
/// `reopen`'s inverse away and is caught by the reconcile pass; a commission
/// wrongly stamped landed is the stranding this exists to stop.
fn mark_member_commissions_landed(store: &mut SqliteStore, bloom: &BloomId) {
    let members = match store.list_dispatch_descriptions(bloom.0.as_bytes()) {
        Ok(members) => members,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::land",
                %error,
                "could not list members to mark commissions landed; the landing itself stands",
            );
            return;
        }
    };
    let resolved = match membership::resolved_members(store, bloom) {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::land",
                %error,
                "could not read which members resolved; marking none landed, the landing itself stands",
            );
            return;
        }
    };
    for (workpiece, _) in members {
        let workpiece = WorkpieceId(workpiece);
        if !resolved.contains(&workpiece) {
            tracing::info!(
                target: "aether_chassis_bloomery::land",
                workpiece = workpiece.0.as_str(),
                "member did not resolve into the landed head; its commission stays open",
            );
            continue;
        }
        if let Err(error) = store.mark_landed(&workpiece) {
            tracing::warn!(
                target: "aether_chassis_bloomery::land",
                workpiece = workpiece.0.as_str(),
                %error,
                "failed to mark the member commission landed; the landing itself stands",
            );
        }
    }
}

/// Close the bloom's candidate-hash inventory at land (ADR-0211). One row per
/// resolved member restamps the newest recorded hash for that workpiece against
/// the landed bloom — matching on workpiece rather than bloom so an inherited
/// member, whose ref reached this namespace through fold adoption and never
/// pushed here, still names its commit. A member the record holds no hash for
/// is written as an unpublished empty hex rather than dropped. Best-effort: a
/// store fault is warned and never holds the land.
fn record_landed_candidate_hashes(store: &mut SqliteStore, bloom: &BloomId) {
    let resolved = match membership::resolved_members(store, bloom) {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::land",
                %error,
                "could not read which members resolved; leaving the candidate-hash inventory unclosed, the landing itself stands",
            );
            return;
        }
    };
    for workpiece in resolved {
        let latest = match store.latest_candidate_hash(&workpiece.0) {
            Ok(latest) => latest,
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::land",
                    workpiece = workpiece.0.as_str(),
                    %error,
                    "could not look up the member's candidate hash; the landing itself stands",
                );
                continue;
            }
        };
        let (ref_name, commit_hex, published) = match latest {
            Some(row) => (row.ref_name, row.commit_hex, row.published),
            None => (String::new(), String::new(), false),
        };
        if let Err(error) = store.record_candidate_hash(
            bloom.0.as_bytes(),
            &workpiece.0,
            &ref_name,
            &commit_hex,
            CANDIDATE_HASH_OCCASION_LAND,
            published,
        ) {
            tracing::warn!(
                target: "aether_chassis_bloomery::land",
                workpiece = workpiece.0.as_str(),
                %error,
                "could not journal the landed candidate hash; the landing itself stands",
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
                emit_source_replica: false,
                reconcile_countdown: RECONCILE_EVERY_TICKS,
                reconciled: BTreeSet::new(),
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
            emit_source_replica: config.emit_source_replica,
            reconcile_countdown: RECONCILE_EVERY_TICKS,
            reconciled: BTreeSet::new(),
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
    /// core, then (on cadence) reconcile terminal commissions onto closed source
    /// issues. The GitHub call runs inline on the dispatcher (the poll cadence
    /// spaces them). Reconcile follows the drain: the drain is the live path and
    /// must not wait behind a catch-up sweep.
    #[handler::single]
    fn on_land_tick(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: LandTick) {
        let Some(source) = state.source.clone() else {
            return;
        };
        let control_mailbox = state.control_mailbox;
        let Some(store) = state.store.as_mut() else {
            return;
        };

        match drain_and_land_emitting(store, source.as_ref(), state.emit_source_replica) {
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

        state.reconcile_countdown = state.reconcile_countdown.saturating_sub(1);
        if state.reconcile_countdown == 0 {
            state.reconcile_countdown = RECONCILE_EVERY_TICKS;
            let closed = reconcile_terminal_commissions(store, source.as_ref(), &mut state.reconciled);
            if closed > 0 {
                tracing::info!(
                    target: "aether_chassis_bloomery::land",
                    closed,
                    "reconciled terminal commissions onto closed source issues",
                );
            }
        }
    }

    /// Control's reply to a fire-and-forget admit. Ok is a no-op; Err is the
    /// refused-admission event that used to miss dispatch.
    #[handler::single]
    fn on_admit_result(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: AdmitResult) {
        if let AdmitResult::Err { error } = mail {
            tracing::error!(target: "aether_chassis_bloomery::land", %error, "admit refused");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
