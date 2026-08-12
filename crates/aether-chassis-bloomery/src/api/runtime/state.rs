//! The router's state: the pre-seal shaping maps, the ceilings that bound
//! them, the in-flight reply-correlation tables, and the [`Routed`] disposition
//! every route helper returns for [`finish`] to settle against those tables.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use aether_actor::{HandlesKind, Manual};
use aether_bloomery::{Admit, BloomDraft, BloomId, Digest, Event, Statement, Workpiece};
use aether_data::wire::to_vec;
use aether_data::{Kind, MailId, MailboxId};
use aether_http as http;
use aether_http::HttpServerResponse;
use aether_kinds::trace::Settled;
use aether_substrate::actor::native::{NativeActorMailbox, NativeCtx};
use aether_substrate::{InboundMail, Mailer};

use super::configs::ConfigView;
use super::response::error_response;
use crate::bloomery::ApprovalPolicy;
// The control core is a native sibling cap since the wasm-boundary retirement
// (ADR-0149 §The boundary, amended), addressed as a typed peer
// (`ctx.actor::<ControlCore>()`) rather than a `resolve_embedded` component lineage.
use crate::control::ControlCore;

/// Per-process ceilings on the pre-seal shaping maps. Staged workpieces and
/// open drafts are pure in-memory shaping state with no durable owner to evict
/// them, so the router caps each map and rejects growth past the cap rather
/// than letting an operator (or a runaway client) grow it without bound
/// (CLAUDE.md §Runtime: error rather than grow unboundedly). Capacity frees
/// only when the shaping session restarts.
pub(super) const MAX_STAGED_WORKPIECES: usize = 1024;
pub(super) const MAX_OPEN_DRAFTS: usize = 1024;

/// Ceiling on the outstanding deferred-seal count — the `seals` map (and its
/// paired `seal_verifications`) grow one entry per above-auto seal in flight, so
/// cap the map the same way `staged` / `drafts` are capped and refuse a new
/// deferred seal at the ceiling before dispatching any verification (issue #3599).
pub(super) const MAX_OPEN_SEALS: usize = 1024;

/// Ceiling on one seal's membership count. Each above-auto member fans out one
/// `aether.signing` `Verify` dispatch and one held `SealVerify` correlation, so a
/// single seal request amplifies into N in-flight verifications plus a held
/// `PendingSeal`; cap the draft's member count and refuse a seal past it before
/// any dispatch, so one request cannot grow the in-flight `seals` /
/// `seal_verifications` maps without bound (issue #3599). A bloom's realistic
/// membership is a handful; the ceiling is generous headroom over that.
pub(super) const MAX_SEAL_MEMBERS: usize = 256;

/// The control-plane REST router state: the pre-seal shaping maps plus the
/// in-flight reply-correlation table.
pub struct ApiCapabilityState {
    /// This cap's own mailbox, the settlement-notice target.
    pub(super) self_mailbox: MailboxId,
    /// The parsed tier policy the pre-seal approve gate decides over (issue
    /// #3583). `None` when the policy file was unreadable or malformed at init —
    /// the gate then fails closed (no member resolves `auto`).
    pub(super) policy: Option<ApprovalPolicy>,
    /// Cached mailer for `send_envelope_detached` settlement subscriptions.
    pub(super) mailer: Arc<Mailer>,
    /// Staged workpieces, keyed by their workpiece id.
    pub(super) staged: BTreeMap<String, Workpiece>,
    /// Open drafts, keyed by a monotonic per-process handle.
    pub(super) drafts: BTreeMap<u64, BloomDraft>,
    /// The next draft handle to mint.
    pub(super) next_draft: u64,
    /// Deferred HTTP replies awaiting a downstream cap reply, keyed by the
    /// downstream dispatch's `MailId.correlation_id`.
    pub(super) pending: HashMap<u64, InboundMail>,
    /// Answer requests awaiting a signature verification from the
    /// `aether.signing` capability, keyed by the verify dispatch's
    /// `MailId.correlation_id`. On a verified reply the held request admits its
    /// stashed `Fact::AdoptAnswer` event (re-deferring into `pending` on the
    /// reducer reply); on a rejection it answers `400`.
    pub(super) verifying: HashMap<u64, VerifyPending>,
    /// Seals held across N above-auto member signature verifications, keyed by a
    /// minted `next_seal` handle. The held seal admits `Fact::Seal` only when
    /// every above-auto member's signature has verified (issue #3599); any
    /// rejection refuses the whole seal (`422`, fail closed) and tears down its
    /// sibling verify correlations.
    pub(super) seals: HashMap<u64, PendingSeal>,
    /// The next seal handle to mint.
    pub(super) next_seal: u64,
    /// Authored scope revisions held across their store write (#4588), keyed by
    /// the write dispatch's `MailId.correlation_id`. The reply carries only
    /// success or failure, so the view the caller gets back waits here rather
    /// than being rebuilt from it.
    pub(super) configs: HashMap<u64, ConfigView>,
    /// Each in-flight above-auto member verification, keyed by its `Verify`
    /// dispatch `MailId.correlation_id`, back-pointing at the held [`PendingSeal`]
    /// and the member it forms the approval for on a verified reply.
    pub(super) seal_verifications: HashMap<u64, SealVerify>,
    /// Orphan-claim release submissions awaiting their signature verification
    /// (ADR-0179), keyed by the verify dispatch's `MailId.correlation_id`. Held
    /// separately from `verifying` so the reply handler can tell a release verify
    /// from an answer verify by correlation alone, the same way `seal_verifications`
    /// separates the seal-member verifies.
    pub(super) releasing: HashMap<u64, ReleasePending>,
    /// The request digest each in-flight release admit will report, keyed by the
    /// admit dispatch's `MailId.correlation_id`. A release answers `202` with its
    /// digest rather than the bare reducer outcome every other write route
    /// returns, and the digest is not recoverable from the admit reply.
    pub(super) release_admits: HashMap<u64, Digest>,
}

/// A release submission held across the signature-verification round trip: the
/// reply obligation, the request digest the `202` carries, and the request event
/// to admit once (and only if) the signature verifies.
pub(super) struct ReleasePending {
    /// The held HTTP reply obligation.
    pub(super) inbound: InboundMail,
    /// The request digest — the handle the accepted reply hands back.
    pub(super) request: Digest,
    /// The `Fact::RequestOrphanClaimRelease` event to admit on a verified
    /// signature.
    pub(super) event: Event,
}

/// An answer request held across the signature-verification round trip: the
/// reply obligation and the adoption event to admit once (and only if) the
/// signature verifies.
pub(super) struct VerifyPending {
    /// The held HTTP reply obligation.
    pub(super) inbound: InboundMail,
    /// The `Fact::AdoptAnswer` event to admit on a verified signature.
    pub(super) event: Event,
}

/// A seal held across N above-auto member signature verifications (issue #3599).
/// The gate resolved every auto member synchronously (their approvals already
/// sit in `gated.proposals`); each above-auto member's approval is slotted in as
/// its signature verifies, and the last verification seals `gated` and admits
/// `Fact::Seal`.
pub(super) struct PendingSeal {
    /// The held HTTP reply obligation.
    pub(super) inbound: InboundMail,
    /// The predecessor this seal supersedes, or `None` for a first seal.
    pub(super) predecessor: Option<BloomId>,
    /// The gated draft: auto members carry their gate-formed approval; each
    /// above-auto member's approval is overwritten by
    /// [`verified_statement_approval`](crate::bloomery::verified_statement_approval) as its signature verifies.
    pub(super) gated: BloomDraft,
    /// The operator-supplied per-member work-order descriptions (#3595),
    /// persisted once the seal completes — the same fire-and-forget store write
    /// the synchronous path makes.
    pub(super) descriptions: BTreeMap<String, String>,
    /// The admit idempotency key override, defaulted to the sealed bloom id when
    /// the last verification seals the spec.
    pub(super) idempotency_key: Option<String>,
    /// Above-auto members whose signature has not yet verified; the verification
    /// that drops this to zero seals and admits.
    pub(super) remaining: usize,
}

/// One in-flight above-auto member verification: which held seal and member it
/// resolves, and the statement whose verified form becomes that member's
/// approval evidence.
pub(super) struct SealVerify {
    /// The held [`PendingSeal`] handle this verification belongs to.
    pub(super) seal: u64,
    /// The index into the seal's `gated.proposals` of the member it forms.
    pub(super) member_index: usize,
    /// The scope revision the formed approval binds.
    pub(super) scope_revision: Digest,
    /// The signed statement whose verified form (`verified_statement_approval`)
    /// becomes the member's approval.
    pub(super) statement: Statement,
}

/// The pre-wired parts of a deferred seal, handed from [`ApiCapabilityState::seal_draft`]
/// to [`finish`] so the handle mint and map inserts (which need
/// `&mut ApiCapabilityState`) happen there, mirroring how `DeferredVerify` defers
/// its map insert to the same adapter.
pub(super) struct PendingSealSetup {
    pub(super) gated: BloomDraft,
    /// The predecessor this seal supersedes, or `None` for a first seal — which
    /// door the completed seal admits through (#4638).
    pub(super) predecessor: Option<BloomId>,
    pub(super) descriptions: BTreeMap<String, String>,
    pub(super) idempotency_key: Option<String>,
    /// One entry per above-auto member — the dispatched `Verify` correlation and
    /// the member it forms.
    pub(super) verifications: Vec<PendingVerify>,
}

/// One above-auto member's dispatched verification, pre-`PendingSeal`-handle.
pub(super) struct PendingVerify {
    pub(super) correlation: u64,
    pub(super) member_index: usize,
    pub(super) scope_revision: Digest,
    pub(super) statement: Statement,
}

/// A route's disposition: an immediate response (the in-memory shaping routes),
/// a deferral keyed by the downstream dispatch correlation (the durable
/// read/write routes), or a signature-gated deferral (the answer route).
pub(super) enum Routed {
    /// Reply now with this response.
    Reply(HttpServerResponse),
    /// Await the downstream reply correlated by this id.
    Deferred(u64),
    /// Await the `aether.signing` verify reply for an orphan-claim release
    /// (ADR-0179), then admit `event` on a verified signature and answer `202`
    /// with `request`, or answer `400` on a rejection.
    DeferredRelease {
        /// The verify dispatch correlation the reply will echo.
        correlation: u64,
        /// The request digest the accepted reply reports.
        request: Digest,
        /// The release request event to admit once the signature verifies.
        event: Box<Event>,
    },
    /// Await the `aether.signing` verify reply correlated by this id, then admit
    /// `event` on a verified signature or answer `400` on a rejection.
    DeferredVerify {
        /// The verify dispatch correlation the reply will echo.
        correlation: u64,
        /// The adoption event to admit once the signature verifies.
        event: Box<Event>,
    },
    /// Await N `aether.signing` verify replies for an above-auto seal (issue
    /// #3599); the last verified reply seals and admits, any rejection refuses
    /// the whole seal (`422`, fail closed).
    DeferredSeal(Box<PendingSealSetup>),
}

impl ApiCapabilityState {
    /// Encode the event, wrap it in an [`Admit`], and dispatch it to the
    /// control core, deferring the HTTP reply on the admit reply.
    pub(super) fn admit(&self, ctx: &NativeCtx<'_, Manual>, event: &Event) -> Routed {
        let bytes = match to_vec(event) {
            Ok(bytes) => bytes,
            Err(error) => return Routed::Reply(error_response(500, &format!("event encode failed: {error}"))),
        };
        Routed::Deferred(self.send_tracked(ctx.actor::<ControlCore>(), &Admit { event: bytes }))
    }

    /// Dispatch a mail to a peer cap's typed handle as a fresh causal root,
    /// subscribe to its settlement (the no-reply safety net), and return the
    /// correlation the reply will echo. The `HandlesKind` gate makes a
    /// wrong-kind dispatch a compile error — the raw-envelope form this
    /// replaces had no such check.
    pub(super) fn send_tracked<R, K>(&self, target: NativeActorMailbox<'_, R>, payload: &K) -> u64
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        self.track(target.send_detached_tracked(payload))
    }

    /// Subscribe this cap to `mail_id`'s settlement and return the correlation
    /// id that keys the held HTTP reply guard.
    fn track(&self, mail_id: MailId) -> u64 {
        if let Some(registry) = self.mailer.settlement_registry() {
            registry.subscribe_settlement_mail(
                mail_id,
                self.self_mailbox,
                <Settled as Kind>::ID,
                Arc::clone(&self.mailer),
            );
        }
        mail_id.correlation_id
    }

    /// Answer a deferred request from a downstream reply: the reply's
    /// `sender.correlation_id` is auto-echoed (ADR-0042) to the dispatch that
    /// deferred it, so it recovers the held reply guard.
    pub(super) fn answer(&mut self, ctx: &NativeCtx<'_, Manual>, response: &HttpServerResponse) {
        let correlation = ctx.reply_target().correlation_id;
        if let Some(inbound) = self.pending.remove(&correlation) {
            inbound.reply(response);
        }
    }
}

/// Adapt a route helper's [`Routed`] disposition into the `http::Outcome` a
/// `#[http::route]` method returns, carrying the reply-obligation stash that the
/// single `on_request` ingress did before the router became per-route (#3672).
/// `Reply` answers inline — the macro glue replies through the still-held
/// inbound — while every deferred variant moves the request's inbound into its
/// correlation table and returns `Deferred` so the glue does not also answer;
/// the reply / settlement handlers recover it by correlation exactly as before.
pub(super) fn finish(
    state: &mut ApiCapabilityState,
    mut ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
    routed: Routed,
) -> http::Outcome {
    match routed {
        Routed::Reply(response) => http::Outcome::Reply(response),
        Routed::Deferred(correlation) => {
            state.pending.insert(correlation, ctx.take_inbound());
            http::Outcome::Deferred
        }
        Routed::DeferredVerify { correlation, event } => {
            state.verifying.insert(correlation, VerifyPending { inbound: ctx.take_inbound(), event: *event });
            http::Outcome::Deferred
        }
        Routed::DeferredRelease { correlation, request, event } => {
            state.releasing.insert(correlation, ReleasePending { inbound: ctx.take_inbound(), request, event: *event });
            http::Outcome::Deferred
        }
        Routed::DeferredSeal(setup) => {
            let PendingSealSetup { gated, predecessor, descriptions, idempotency_key, verifications } = *setup;
            let seal = state.next_seal;
            state.next_seal += 1;
            let remaining = verifications.len();
            for verify in verifications {
                let PendingVerify { correlation, member_index, scope_revision, statement } = verify;
                state
                    .seal_verifications
                    .insert(correlation, SealVerify { seal, member_index, scope_revision, statement });
            }
            let inbound = ctx.take_inbound();
            state
                .seals
                .insert(seal, PendingSeal { inbound, predecessor, gated, descriptions, idempotency_key, remaining });
            http::Outcome::Deferred
        }
    }
}
