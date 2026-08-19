//! The router's state: the pre-seal shaping maps, the ceilings that bound them,
//! the multi-hop join tables, and the [`Routed`] disposition every route helper
//! returns for [`finish`] to dispatch.
//!
//! # Two kinds of deferral
//!
//! A route that forwards one request and answers its one reply carries no state
//! here at all: [`finish`] hands it to `ctx.defer(&request).to::<Peer>()`, the
//! ADR-0154 relay, which stashes the requester's reply target in the ADR-0139
//! request-context table and lets the paired `#[http::reply]` route answer it.
//! The send *inherits* the request's causal chain, so the request stays in
//! flight across the round-trip and the HTTP server's own `502` / timeout nets
//! bound a downstream that never answers — nothing is held here and nothing
//! needs reaping.
//!
//! What remains are the genuine **multi-hop** flows, which the 1:1 relay cannot
//! express: the answer and orphan-claim-release routes verify a signature
//! *before* they admit ([`VerifyPending`], shared by both), and a seal joins N
//! member verifications before admitting once ([`PendingSeal`]). Each holds the
//! request across a hop whose reply is not the answer, so each keeps an explicit
//! obligation — and because their final `Admit` is dispatched from a reply
//! handler rather than a route, it re-defers into
//! [`pending`](ApiCapabilityState::pending) and is answered by hand. Those tables
//! are domain join state, not correlation bookkeeping.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use aether_actor::{HandlesKind, Manual};
#[cfg(feature = "github")]
use aether_bloomery::EnumerateClaims;
use aether_bloomery::{
    Admit, ApprovalPolicy, BloomDraft, BloomId, Event, MemberDependency, MetricsQuery, Query, ResolvedConfigs,
    SpendQuery, Statement, Workpiece, WorkpieceId,
};
use aether_data::wire::to_vec;
use aether_data::{Kind, MailId, MailboxId};
use aether_http as http;
use aether_http::HttpServerResponse;
use aether_kinds::trace::Settled;
use aether_substrate::actor::native::{NativeActorMailbox, NativeCtx};
use aether_substrate::{InboundMail, Mailer};

use super::response::error_response;
use crate::artifacts::{ArtifactsCapability, GetRange};
#[cfg(feature = "github")]
use crate::bloomery::{CandidatePush, DoctorBoard};
// The control core is a native sibling cap since the wasm-boundary retirement
// (ADR-0149 §The boundary, amended), addressed as a typed peer
// (`ctx.defer(&request).to::<ControlCore>()`) rather than a `resolve_embedded`
// component lineage.
use crate::artifacts::ArtifactsCapabilityState;
use crate::control::ControlCore;
#[cfg(feature = "github")]
use crate::source::SourceCapability;
use crate::store::{
    CreateCommission, ListBloomDispatches, ListCommissions, LoadCommission, LoadCommissionResult, LookupDispatch,
    PageJournal, RecordConfig, StoreCapability, WriteScopeRevision,
};

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
/// multi-hop join tables. A direct one-request/one-reply route holds nothing
/// here — the ADR-0154 relay carries it.
pub struct ApiCapabilityState {
    /// This cap's own mailbox, the settlement-notice target.
    pub(super) self_mailbox: MailboxId,
    /// The host's file-loaded tier policy — the fallback a draft that seals no
    /// `aether.bloomery.approval_policy` entry is gated against (issue #3583,
    /// #4616). `None` when the policy file was unreadable or malformed at init;
    /// a draft that seals none then has nothing to decide over and its seal fails
    /// closed (no member resolves `auto`).
    pub(super) file_policy: Option<ApprovalPolicy>,
    /// Configuration content behind the addresses a draft's registry seals — the
    /// same window onto the store the control core keeps (ADR-0174), filled from
    /// the boot [`LoadConfigs`](aether_bloomery::LoadConfigs) read and from every
    /// successful `POST /configs` write. The pre-seal gate is synchronous over
    /// in-memory state, so a sealed policy has to be resolvable without a store
    /// round trip inside the admission path.
    pub(super) configs: ResolvedConfigs,
    /// Whether the boot configuration read has landed. Until it has, this cap
    /// cannot tell an unsealed policy from one whose content it merely has not
    /// fetched — the two decide opposite tiers — so a seal arriving first is
    /// refused rather than gated against the fallback.
    pub(super) configs_ready: bool,
    /// Cached mailer for the multi-hop flows' settlement subscriptions.
    pub(super) mailer: Arc<Mailer>,
    /// The correspondence a `from_commit` repair records against — the same
    /// handle the executor uses, so a derived digest resolves for Verify.
    #[cfg(feature = "github")]
    pub(super) correspondence: Option<aether_bloomery::SharedCorrespondence>,
    /// The candidate-ref pusher a `from_commit` repair uses after recording.
    #[cfg(feature = "github")]
    pub(super) pusher: Option<Arc<dyn CandidatePush>>,
    /// Scratch-worktree base the evidence directories live under.
    pub(super) worktree_base: PathBuf,
    /// Optional artifacts handle for resolving study cost on the dispatch list.
    pub(super) artifacts: Option<ArtifactsCapabilityState>,
    /// Staged workpieces, keyed by their workpiece id.
    pub(super) staged: BTreeMap<String, Workpiece>,
    /// Open drafts, keyed by a monotonic per-process handle.
    pub(super) drafts: BTreeMap<u64, BloomDraft>,
    /// The next draft handle to mint.
    pub(super) next_draft: u64,
    /// Requests awaiting the **terminal** `Admit` of a multi-hop flow, keyed by
    /// that admit dispatch's `MailId.correlation_id`. Only the answer and seal
    /// flows reach here: their admit is dispatched from a reply handler, which
    /// has no route obligation to defer, so it is held and answered by hand. A
    /// direct admit route never inserts — the relay carries it.
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
    /// Each in-flight above-auto member verification, keyed by its `Verify`
    /// dispatch `MailId.correlation_id`, back-pointing at the held [`PendingSeal`]
    /// and the member it forms the approval for on a verified reply.
    pub(super) seal_verifications: HashMap<u64, SealVerify>,
    /// Bearer token commission routes require. Empty refuses every commission
    /// request.
    pub(super) control_token: String,
    /// The doctor's latest report, overlaid on `GET /view`.
    #[cfg(feature = "github")]
    pub(super) doctor: Option<DoctorBoard>,
    /// Commission approval / cancel requests awaiting a signature verification.
    pub(super) commission_verifying: HashMap<u64, super::commissions::CommissionVerify>,
    /// Commission writes dispatched after a verified signature, awaiting store.
    pub(super) commission_writing: HashMap<u64, InboundMail>,
    /// Commission list/show/workpiece reads awaiting the store, keyed by the
    /// load or list dispatch correlation.
    pub(super) commission_http: HashMap<u64, CommissionHttp>,
    /// Seals held across N commission loads, keyed by a minted handle.
    pub(super) commission_seals: HashMap<u64, PendingCommissionSeal>,
    /// The next commission-seal handle to mint.
    pub(super) next_commission_seal: u64,
    /// Each in-flight seal-time `LoadCommission`, keyed by its dispatch
    /// correlation, back-pointing at the held [`PendingCommissionSeal`].
    pub(super) seal_commission_loads: HashMap<u64, SealCommissionLoad>,
}

/// A request held across a signature-verification round trip: the reply
/// obligation and the event to admit once (and only if) the signature verifies.
///
/// Two routes hold one: `POST /blooms/{id}/answer/{question}` (its event is
/// `Fact::AdoptAnswer`) and `POST /claims/releases` (its event is
/// `Fact::RequestOrphanClaimRelease`, ADR-0179). They are the same flow —
/// verify, then admit — so they share the table rather than each keeping a
/// parallel one; `subject` is the only thing that differs, and it differs only
/// in what a rejection says the operator got wrong.
pub(super) struct VerifyPending {
    /// The held HTTP reply obligation.
    pub(super) inbound: InboundMail,
    /// What the operator submitted, named for the rejection message: an
    /// `"answer statement"` or a `"release authorization"`.
    pub(super) subject: &'static str,
    /// The event to admit on a verified signature.
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
    /// The door-resolved member-dependency graph (ADR-0196), carried across
    /// the verify hop so the eventual admit journals the same edges the
    /// synchronous path would have.
    pub(super) edges: Vec<MemberDependency>,
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
    /// The signed statement whose verified form (`verified_statement_approval`)
    /// becomes the member's approval. The evidence subject is the gated
    /// proposal's `subject()` — the store statement is bound to the scope
    /// revision, not the member subject.
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
    /// The door-resolved member-dependency graph, admitted with the sealed spec.
    pub(super) edges: Vec<MemberDependency>,
    /// One entry per above-auto member — the dispatched `Verify` correlation and
    /// the member it forms.
    pub(super) verifications: Vec<PendingVerify>,
}

/// One above-auto member's dispatched verification, pre-`PendingSeal`-handle.
pub(super) struct PendingVerify {
    pub(super) correlation: u64,
    pub(super) member_index: usize,
    pub(super) statement: Statement,
}

/// How a held commission read should render once the store replies.
pub(super) enum CommissionHttpRender {
    /// `GET /commissions/{id}`.
    Show,
    /// `GET /commissions`.
    List,
    /// `GET /workpieces`.
    Workpieces,
}

/// A commission HTTP read waiting on the store.
pub(super) struct CommissionHttp {
    /// The held HTTP reply obligation.
    pub(super) inbound: InboundMail,
    /// Which renderer answers.
    pub(super) render: CommissionHttpRender,
}

/// A seal held across N commission loads (#5048). Each member's store row
/// lands here; the last load materializes projections and continues into
/// [`gate_and_admit`](ApiCapabilityState::gate_and_admit).
pub(super) struct PendingCommissionSeal {
    /// The held HTTP reply obligation.
    pub(super) inbound: InboundMail,
    /// The draft being sealed.
    pub(super) draft: BloomDraft,
    /// The predecessor this seal supersedes, or `None` for a first seal.
    pub(super) predecessor: Option<BloomId>,
    /// The admit idempotency key override.
    pub(super) idempotency_key: Option<String>,
    /// Declared edges from the request, unioned with store-frozen edges after
    /// every member loads.
    pub(super) edges: Vec<MemberDependency>,
    /// Loads still outstanding.
    pub(super) remaining: usize,
    /// Loaded rows, keyed by workpiece id.
    pub(super) loaded: BTreeMap<String, LoadCommissionResult>,
}

/// The pre-wired parts of a store-backed seal, handed to [`finish`] so the
/// handle mint and map inserts happen there.
pub(super) struct PendingCommissionSealSetup {
    pub(super) draft: BloomDraft,
    pub(super) predecessor: Option<BloomId>,
    pub(super) idempotency_key: Option<String>,
    pub(super) edges: Vec<MemberDependency>,
    /// One entry per draft member — the dispatched load correlation and the
    /// workpiece it fetches.
    pub(super) loads: Vec<(u64, WorkpieceId)>,
}

/// One in-flight seal-time commission load.
pub(super) struct SealCommissionLoad {
    /// The held [`PendingCommissionSeal`] handle.
    pub(super) seal: u64,
    /// The workpiece this load is for.
    pub(super) workpiece: WorkpieceId,
}

/// A route's disposition: an immediate response (the in-memory shaping routes),
/// a downstream request to relay (the durable read/write routes), or one of the
/// two multi-hop deferrals that hold their own join state.
///
/// The relay variants each carry the **request itself** rather than a dispatch
/// correlation: [`finish`] is what forwards them, so the route helper decides
/// *what* to ask and the adapter decides *whom* to ask. That is what lets the
/// send be an inherited `ctx.defer(…).to::<Peer>()` instead of a detached one —
/// a route helper holds no ctx it could defer through.
pub(super) enum Routed {
    /// Reply now with this response.
    Reply(HttpServerResponse),
    /// Relay to the control core; its `AdmitResult` answers.
    Admit(Admit),
    /// Relay to the control core; its `QueryResult` answers.
    Query(Query),
    /// Relay to the control core; its `MetricsQueryResult` answers.
    Metrics(MetricsQuery),
    /// Relay to the control core; its `SpendQueryResult` answers.
    Spend(SpendQuery),
    /// Relay to the store; its `PageJournalResult` answers.
    ReplayJournal(PageJournal),
    /// Relay to the store; its `ListBloomDispatchesResult` answers.
    ListBloomDispatches(ListBloomDispatches),
    /// Relay to the store; its `LookupDispatchResult` answers.
    LookupDispatch(LookupDispatch),
    /// Relay to the artifacts cap; its `GetRangeResult` answers.
    GetRange(GetRange),
    /// Relay to the store; its `RecordConfigResult` answers.
    RecordConfig(RecordConfig),
    /// Relay to the source cap; its `EnumerateClaimsResult` answers (ADR-0179).
    #[cfg(feature = "github")]
    EnumerateClaims(EnumerateClaims),
    /// Await the `aether.signing` verify reply correlated by this id, then admit
    /// `event` on a verified signature or answer `400` naming `subject` on a
    /// rejection.
    DeferredVerify {
        /// The verify dispatch correlation the reply will echo.
        correlation: u64,
        /// What the operator submitted, for the rejection message.
        subject: &'static str,
        /// The event to admit once the signature verifies.
        event: Box<Event>,
    },
    /// Await N `aether.signing` verify replies for an above-auto seal (issue
    /// #3599); the last verified reply seals and admits, any rejection refuses
    /// the whole seal (`422`, fail closed).
    DeferredSeal(Box<PendingSealSetup>),
    /// Relay a commission create to the store.
    CreateCommission(CreateCommission),
    /// Relay a scope-revision write to the store.
    WriteScopeRevision(WriteScopeRevision),
    /// Relay a commission load to the store.
    LoadCommission(LoadCommission),
    /// Relay a commission list to the store.
    ListCommissions(ListCommissions),
    /// Relay an open-commission list rendered as workpieces.
    ListOpenWorkpieces(ListCommissions),
    /// Await N commission loads, then gate and admit (#5048).
    DeferredCommissionSeal(Box<PendingCommissionSealSetup>),
    /// Await a signing-cap verify, then persist the commission write.
    DeferredCommissionVerify {
        /// The verify dispatch correlation the reply will echo.
        correlation: u64,
        /// The write to persist once the signature verifies.
        write: super::commissions::CommissionWrite,
    },
}

/// Encode `event` and wrap it in an [`Admit`] for the control core, or answer
/// `500` if it will not encode. Shared by every route that admits directly —
/// grant, and the all-auto seal / supersede fast path.
pub(super) fn admit(event: &Event) -> Routed {
    match to_vec(event) {
        Ok(bytes) => Routed::Admit(Admit { event: bytes }),
        Err(error) => Routed::Reply(error_response(500, &format!("event encode failed: {error}"))),
    }
}

impl ApiCapabilityState {
    /// Dispatch a mail to a peer cap's typed handle as a fresh causal root,
    /// subscribe to its settlement (the no-reply safety net), and return the
    /// correlation the reply will echo. The `HandlesKind` gate makes a
    /// wrong-kind dispatch a compile error.
    ///
    /// Only the multi-hop flows dispatch this way. A hop taken from a reply
    /// handler has no request chain to inherit and no route obligation to
    /// defer, so it starts a fresh root and pairs with an explicit settlement
    /// subscription; a direct route relays through
    /// [`Ctx::defer`](aether_http::Ctx::defer) instead and needs neither.
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

    /// Answer a multi-hop flow's held request from its terminal `Admit` reply:
    /// the reply's `sender.correlation_id` is auto-echoed (ADR-0042) to the
    /// dispatch that deferred it, so it recovers the held obligation. A miss is
    /// a no-op — a relay-backed admit was already answered through its deferred
    /// source, and the two stores are mutually exclusive.
    pub(super) fn answer(&mut self, ctx: &NativeCtx<'_, Manual>, response: &HttpServerResponse) {
        let correlation = ctx.reply_target().correlation_id;
        if let Some(inbound) = self.pending.remove(&correlation) {
            inbound.reply(response);
        }
    }
}

/// Adapt a route helper's [`Routed`] disposition into the `http::Outcome` a
/// `#[http::route]` method returns.
///
/// `Reply` answers inline — the macro glue replies through the still-held
/// inbound. Each relay variant forwards its request with
/// `ctx.defer(&request).to::<Peer>()`: the send inherits the request's causal
/// chain, so the request stays in flight and the requester's reply target rides
/// the ADR-0139 context table to the paired `#[http::reply]` route. Nothing is
/// taken from the inbound and nothing is recorded here — that is the whole point
/// of the migration (ADR-0154 §3).
///
/// The two multi-hop variants still move the request's inbound into their join
/// table and return `Deferred`, because their next hop is not the answer.
pub(super) fn finish(
    state: &mut ApiCapabilityState,
    mut ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
    routed: Routed,
) -> http::Outcome {
    match routed {
        Routed::Reply(response) => http::Outcome::Reply(response),
        Routed::Admit(request) => ctx.defer(&request).to::<ControlCore>(),
        Routed::Query(request) => ctx.defer(&request).to::<ControlCore>(),
        Routed::Metrics(request) => ctx.defer(&request).to::<ControlCore>(),
        Routed::Spend(request) => ctx.defer(&request).to::<ControlCore>(),
        Routed::ReplayJournal(request) => ctx.defer(&request).to::<StoreCapability>(),
        Routed::ListBloomDispatches(request) => ctx.defer(&request).to::<StoreCapability>(),
        Routed::LookupDispatch(request) => ctx.defer(&request).to::<StoreCapability>(),
        Routed::GetRange(request) => ctx.defer(&request).to::<ArtifactsCapability>(),
        Routed::RecordConfig(request) => ctx.defer(&request).to::<StoreCapability>(),
        #[cfg(feature = "github")]
        Routed::EnumerateClaims(request) => ctx.defer(&request).to::<SourceCapability>(),
        Routed::DeferredVerify { correlation, subject, event } => {
            state.verifying.insert(correlation, VerifyPending { inbound: ctx.take_inbound(), subject, event: *event });
            http::Outcome::Deferred
        }
        Routed::DeferredSeal(setup) => {
            state.begin_deferred_seal(ctx.take_inbound(), *setup);
            http::Outcome::Deferred
        }
        Routed::CreateCommission(request) => ctx.defer(&request).to::<StoreCapability>(),
        Routed::WriteScopeRevision(request) => ctx.defer(&request).to::<StoreCapability>(),
        Routed::LoadCommission(request) => hold_commission_http(state, &mut ctx, &request, CommissionHttpRender::Show),
        Routed::ListCommissions(request) => hold_commission_http(state, &mut ctx, &request, CommissionHttpRender::List),
        Routed::ListOpenWorkpieces(request) => {
            hold_commission_http(state, &mut ctx, &request, CommissionHttpRender::Workpieces)
        }
        Routed::DeferredCommissionSeal(setup) => {
            let PendingCommissionSealSetup { draft, predecessor, idempotency_key, edges, loads } = *setup;
            let seal = state.next_commission_seal;
            state.next_commission_seal += 1;
            let remaining = loads.len();
            for (correlation, workpiece) in loads {
                state.seal_commission_loads.insert(correlation, SealCommissionLoad { seal, workpiece });
            }
            let inbound = ctx.take_inbound();
            state.commission_seals.insert(
                seal,
                PendingCommissionSeal {
                    inbound,
                    draft,
                    predecessor,
                    idempotency_key,
                    edges,
                    remaining,
                    loaded: BTreeMap::new(),
                },
            );
            http::Outcome::Deferred
        }
        Routed::DeferredCommissionVerify { correlation, write } => {
            state
                .commission_verifying
                .insert(correlation, super::commissions::CommissionVerify { inbound: ctx.take_inbound(), write });
            http::Outcome::Deferred
        }
    }
}

/// Hold a commission HTTP read across a store hop and answer it from the
/// matching result handler. Seal loads share the `LoadCommission` kind, so
/// these reads cannot ride the ADR-0154 relay.
fn hold_commission_http<K>(
    state: &mut ApiCapabilityState,
    ctx: &mut http::Ctx<'_, NativeCtx<'_, Manual>>,
    request: &K,
    render: CommissionHttpRender,
) -> http::Outcome
where
    StoreCapability: HandlesKind<K>,
    K: Kind,
{
    let correlation = state.send_tracked(ctx.actor::<StoreCapability>(), request);
    state.commission_http.insert(correlation, CommissionHttp { inbound: ctx.take_inbound(), render });
    http::Outcome::Deferred
}
