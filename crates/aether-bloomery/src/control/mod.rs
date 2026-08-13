//! The control loop — admitted events driven through the reducer with
//! boot-time journal replay (ADR-0149 §The control core, §Migration step 1).
//!
//! This module carries the control-plane mail vocabulary — the
//! `aether.bloomery.{admit,query}` ingress plus the store/source transact-mails
//! the control core drives — always compiled so every side shares one
//! definition. The control core that owns the live
//! [`Snapshot`](crate::reduce::Snapshot), drives [`reduce`](crate::reduce::reduce),
//! and commits through `aether.store` is a **native** capability in
//! `aether-chassis-bloomery` (ADR-0149 §The boundary, amended); this crate is the
//! pure leaf it and the GitHub adapter both depend on.
//!
//! # Why the store transact-mails live here
//!
//! [`Commit`] and the [`ReplayJournal`] family are the store's own
//! transact-mails (ADR-0149 §The boundary), defined here in `aether-bloomery`
//! rather than in `aether-chassis-bloomery` alongside the rest of the
//! `aether.store.*` family, so the value layer and the host's store + control
//! caps share one definition. The wire contract is the `#[kind(name = "…")]`
//! literal wherever the type is declared, and the host's `StoreCapability`
//! imports them inward exactly as it imports [`Event`](crate::reduce::Event).
//! The wire contract is identical wherever the type is declared: the
//! `#[kind(name = "…")]` literal is the identity.
//!
//! Like the rest of the `aether.store.*` family, these kinds carry the
//! bloom-protocol payloads as their canonical [`aether_data::wire`] bytes (the
//! journaled [`Event`](crate::reduce::Event), the outbox receipt payloads)
//! rather than typed fields — the store journals opaque bytes and the reducer
//! decodes them on replay. The membership mutations the store applies to its
//! `active_membership` table are the typed axes it keys on, mirroring
//! `ClaimSeal` / `Supersede`.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::ids::{StageId, WorkpieceId};
use crate::reduce::Decision;
use crate::values::{AgentProfile, ConfigRegistry, MemberCandidate, OrphanClaimRelease, Transformation};

/// One active-membership mutation the store applies inside the combined
/// [`Commit`] transaction: a workpiece claimed (or released) for a bloom. The
/// bloom is its [`BloomId`](crate::ids::BloomId) digest's raw bytes, matching
/// the opaque-bytes convention `ClaimSeal` / `Supersede` already use for the
/// `bloom` axis.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MembershipMutation {
    /// The workpiece whose active membership changes.
    pub workpiece: String,
    /// The bloom the claim attaches to (or releases from) — its digest bytes.
    #[serde(with = "aether_data::bytes")]
    pub bloom: Vec<u8>,
}

/// One outbox entry the combined [`Commit`] enqueues inside its transaction —
/// a caller-defined topic plus opaque payload bytes, carried inline so the
/// enqueue is atomic with the journal append.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OutboxPayload {
    /// A caller-defined topic naming what the payload is, so a republisher can
    /// route it.
    pub topic: String,
    /// The opaque payload bytes to republish.
    #[serde(with = "aether_data::bytes")]
    pub payload: Vec<u8>,
}

impl OutboxPayload {
    /// Build an outbox entry under a reducer [`Topic`] — the producer edge's
    /// one topic-to-string conversion. The raw fields stay public plain data
    /// (the wire decodes into them, and the string surface stays open for
    /// caller-defined topics); reducer-enqueued entries construct through here
    /// so a producer call site cannot spell an arbitrary topic string.
    #[must_use]
    pub fn new(topic: Topic, payload: Vec<u8>) -> Self {
        Self { topic: topic.as_str().to_owned(), payload }
    }
}

/// Declares the closed [`Topic`] vocabulary and its complete, duplicate-free
/// [`Topic::ALL`] enumeration from one variant list, so the set and its array
/// can never drift: a new topic extends both in lockstep, and the pairing
/// tripwire that maps over `ALL` picks it up automatically rather than relying
/// on a hand-maintained parallel array (the `stage_vocabulary!` idiom in
/// `crate::ids`). The per-variant minting-class docs ride through unchanged.
macro_rules! topic_vocabulary {
    ($($(#[$vmeta:meta])* $variant:ident),+ $(,)?) => {
        /// A bloomery outbox topic — the routing key naming what an outbox row
        /// carries, so exactly one host reactor drains it across the store
        /// boundary (ADR-0149 §The boundary). A topic is a producer/reactor
        /// contract: a payload is enqueued under it and exactly one host reactor
        /// drains it, so a drifted spelling would enqueue under a topic nobody
        /// drains, accumulating undelivered rows silently (#3668). An outbox
        /// topic is only useful if compiled code drains it — there is no runtime
        /// drainer registration — so the set of meaningful topics is closed by
        /// construction and this enumeration is total. Always compiled like the
        /// payload types, so the `default-features = false` host reactors reach
        /// it without the `runtime` actor.
        ///
        /// A fieldless enum, so anything that can be a topic is one and every
        /// match over topics is exhaustive. The variants fall in two minting
        /// classes:
        ///
        /// - **Reducer-minted** — the projection of an effectful [`Decision`]
        ///   variant. The exhaustive [`of_decision`](Self::of_decision) match
        ///   mints exactly one for each effectful decision, so a new effectful
        ///   `Decision` variant fails to compile until it names its topic (the
        ///   `StageCatalog::binding_of` idiom). `of_decision` returns only these
        ///   — it never returns a host-minted value.
        /// - **Host-minted** — a projection the host both produces and drains,
        ///   with no `Decision` behind it (e.g.
        ///   [`ViewDocument`](Self::ViewDocument)).
        ///
        /// [`ALL`](Self::ALL) enumerates the closed set — both classes — that the
        /// producer/reactor pairing tripwire walks against the host reactors.
        ///
        /// Each variant maps through [`as_str`](Self::as_str) to a
        /// `topic:`-prefixed display spelling — the string persisted to the
        /// outbox row and matched by the draining reactor, preserved exactly (a
        /// changed spelling would strand undelivered rows). A topic is a
        /// store-local routing key between a producer's payload and the reactor
        /// that drains it, never an actor address — no mail can be sent to it —
        /// and the sigil (impossible in a dot-separated aether name) makes that
        /// unmistakable.
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum Topic {
            $($(#[$vmeta])* $variant),+
        }

        impl Topic {
            /// Every bloomery outbox topic — both reducer-minted and host-minted
            /// — the closed enumeration the producer/reactor pairing tripwire
            /// walks against the host reactors. Generated with the enum from the
            /// same variant list, so it is complete and duplicate-free by
            /// construction: a new topic cannot be silently omitted.
            pub const ALL: &'static [Topic] = &[$(Topic::$variant),+];
        }
    };
}

topic_vocabulary! {
    /// A landing receipt (reducer-minted, from [`Decision::EmitReceipt`]),
    /// drained by the mirror reactor and routed to #3499's republisher.
    LandingReceipt,
    /// A stage re-dispatch (reducer-minted, from [`Decision::RedispatchStage`]),
    /// re-assembling the held attempt naming both question and answer digests
    /// (ADR-0151). Still awaiting a draining reactor (#3664).
    Redispatch,
    /// A per-member attempt dispatch (reducer-minted, from
    /// [`Decision::DispatchAttempt`]), drained by the executor reactor, wrapped in
    /// a work order, and submitted through the executor port (ADR-0149 §The
    /// line).
    Dispatch,
    /// A land dispatch (reducer-minted, from [`Decision::DispatchLand`]), drained
    /// by the land reactor, which issues the source-port compare-and-swap land
    /// (ADR-0149 migration step 3).
    Land,
    /// An integration dispatch (reducer-minted, from
    /// [`Decision::DispatchIntegration`]), drained by the integrate reactor, which
    /// folds the claimed candidates onto the bloom's integration branch
    /// (ADR-0152).
    Integrate,
    /// A whole-bloom aggregate-review dispatch (reducer-minted, from
    /// [`Decision::DispatchAggregateReview`]), drained by the executor reactor,
    /// which runs the `review.critic` lane against the integrated head under a
    /// bloom-level order record (ADR-0153).
    AggregateReview,
    /// A whole-bloom aggregate-verify dispatch (reducer-minted, from
    /// [`Decision::DispatchAggregateVerify`]), drained by the executor reactor,
    /// which runs the mechanical `verify.check` lane against the folded head
    /// under a bloom-level order record. Appended so the prior topics' display
    /// spellings and ordering are unchanged.
    AggregateVerify,
    /// A whole-document projection (host-minted): the view-document producer
    /// (#3497) enqueues [`ViewDocument`](crate::port::ViewDocument) payloads and
    /// the mirror reactor drains them onto the outward mirror. No [`Decision`]
    /// projects onto it — it is host-produced and host-drained, so
    /// [`of_decision`](Self::of_decision) never returns it — but it is a real
    /// outbox topic exactly one reactor drains, so it belongs to the closed set.
    /// Its payload type [`ViewDocument`](crate::port::ViewDocument) already lives
    /// in this crate.
    ViewDocument,
    /// An authorized orphan-claim release (reducer-minted, from
    /// [`Decision::DispatchOrphanClaimRelease`]), drained by the claim-release
    /// reactor, which runs the source port's expected-holder compare-and-swap and
    /// admits the terminal result (ADR-0179). Appended so the prior topics'
    /// display spellings and ordering are unchanged.
    OrphanClaimRelease,
}

impl Topic {
    /// The `topic:` display spelling — the exact string persisted to the outbox
    /// row and matched by the draining reactor. One exhaustive match over the
    /// closed enum, so a new variant fails to compile until it names its wire
    /// spelling, and the spellings appear only here (and the boundary
    /// constructors). The wire and `SQLite` surfaces carry this plain string; the
    /// [`Topic`] type is the closed producer / reactor edge over it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LandingReceipt => "topic:landing_receipt",
            Self::Redispatch => "topic:redispatch",
            Self::Dispatch => "topic:dispatch",
            Self::Land => "topic:land",
            Self::Integrate => "topic:integrate",
            Self::AggregateReview => "topic:aggregate_review",
            Self::AggregateVerify => "topic:aggregate_verify",
            Self::ViewDocument => "topic:view_document",
            Self::OrphanClaimRelease => "topic:orphan_claim_release",
        }
    }

    /// The outbox [`Topic`] an effectful [`Decision`]
    /// enqueues its payload under, or `None` for a snapshot-only or
    /// membership-only decision that carries no outbox row. An exhaustive match
    /// over the closed `Decision` enum — the compile-time guard that a new
    /// effectful variant names its topic before it compiles (the
    /// `StageCatalog::binding_of` idiom). This is the sole decision → topic
    /// classifier; the producer projection and the pairing tripwire both read
    /// the vocabulary it defines.
    #[must_use]
    pub fn of_decision(decision: &Decision) -> Option<Self> {
        match decision {
            Decision::EmitReceipt(_) => Some(Self::LandingReceipt),
            Decision::RedispatchStage { .. } => Some(Self::Redispatch),
            Decision::DispatchAttempt { .. } => Some(Self::Dispatch),
            Decision::DispatchLand { .. } => Some(Self::Land),
            Decision::DispatchIntegration { .. } => Some(Self::Integrate),
            Decision::DispatchAggregateReview { .. } => Some(Self::AggregateReview),
            Decision::DispatchAggregateVerify { .. } => Some(Self::AggregateVerify),
            Decision::DispatchOrphanClaimRelease { .. } => Some(Self::OrphanClaimRelease),
            Decision::ClaimMembership { .. }
            | Decision::ReleaseMembership { .. }
            | Decision::InheritClaim { .. }
            | Decision::RecordResolution { .. }
            | Decision::RecordEvidence { .. }
            | Decision::MarkSuperseded { .. }
            | Decision::SetResolved { .. }
            | Decision::AdvanceMainline { .. }
            | Decision::ReleaseHold { .. }
            | Decision::AdvanceStage { .. }
            | Decision::RecordIntegration { .. }
            | Decision::RecordAggregateRoll { .. }
            | Decision::RecordAggregateVerifyRoll { .. }
            | Decision::RecordLandingRoll { .. }
            | Decision::SetUnresolved { .. }
            | Decision::RevokeResolution { .. }
            | Decision::RecordReviewPark { .. }
            // Snapshot-only: the wedge reaches the outward mirror through the
            // member's `MemberView`, on the same reconcile the rest of the
            // member's state rides. A topic of its own would need a reactor to
            // drain it, and an undrained topic accumulates rows forever.
            | Decision::RecordWedge { .. }
            // Snapshot-only: recording the observed head moves no bloom and
            // opens no work. What acts on it is a later supersession, which
            // carries its own topics.
            | Decision::RecordObservation { .. }
            // Snapshot-only: the release record is the status route's read
            // surface. The effect that reaches the source is its sibling
            // `DispatchOrphanClaimRelease`, and giving the record its own topic
            // would enqueue a row nothing drains.
            | Decision::RecordOrphanClaimRelease { .. }
            // Snapshot-only: the verify memo and its reuse receipts are read
            // off the record, and a memo hit's whole point is that no lane is
            // dispatched — a topic here would enqueue rows for work nobody runs.
            | Decision::RecordVerifyProof { .. }
            | Decision::RecordVerifyReuse { .. } => None,
        }
    }
}

/// A drained outbox row's topic string compares directly against a [`Topic`]
/// (`entry.topic == Topic::Land`), so a reactor classifies entries through the
/// typed vocabulary without re-spelling the persisted string at the call site.
impl PartialEq<Topic> for str {
    fn eq(&self, other: &Topic) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<Topic> for &str {
    fn eq(&self, other: &Topic) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<Topic> for String {
    fn eq(&self, other: &Topic) -> bool {
        self == other.as_str()
    }
}

/// The control-core actor's namespace — the sole owner of the literal.
/// Defined here (always compiled) and forward-fed into the `runtime`-gated
/// actor's `NAMESPACE` (the `EMBEDDED_SCOPE` forward-feed precedent in
/// `aether-actor`), so the `default-features = false` host reactors resolve
/// the loaded component from the exact const the actor registers under
/// (#3668). The lineage math (`aether.component/aether.embedded:<this>`) is
/// the component host's, never re-spelled here: the host resolves the mailbox
/// through `aether_component::resolve_embedded(CONTROL_CORE_NAMESPACE)`.
pub const CONTROL_CORE_NAMESPACE: &str = "aether.bloomery.control";

/// The re-dispatch outbox payload (ADR-0151): the bloom, the released question,
/// and the adopting answer, each by digest. The wasm control actor enqueues it
/// under [`Topic::Redispatch`] from a
/// [`Decision::RedispatchStage`]; the
/// executor dispatch reactor (#3505) decodes it to re-assemble the held attempt
/// naming both digests. Defined here (always compiled) so the host reactor can
/// decode it inward, cycle-free — like [`OutboxPayload`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RedispatchPayload {
    /// The bloom whose held stage is re-dispatched.
    pub bloom: Digest,
    /// The released question's digest.
    pub question: Digest,
    /// The adopting answer's digest.
    pub answer: Digest,
    /// The answer statement's exact asserted bytes — the decision text the
    /// executor reactor overlays onto the re-dispatched lane's advisory
    /// channel (#3664). Carried through rather than resolved host-side: the
    /// host has no store of statement bodies, and the reducer held the
    /// statement when it decided.
    pub words: Vec<u8>,
}

/// The per-member attempt dispatch outbox payload (ADR-0149 §The line): the
/// bloom, the member, the stage, and the fully-built portable
/// [`Transformation`] the executor dispatch
/// reactor (#3505) wraps in a work order (adding an idempotency nonce) and
/// submits through the executor port. The wasm control actor enqueues it under
/// [`Topic::Dispatch`] from a
/// [`Decision::DispatchAttempt`].
/// Defined here (always compiled) so the host reactor can decode it inward,
/// cycle-free — like [`OutboxPayload`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DispatchPayload {
    /// The bloom the dispatched member belongs to.
    pub bloom: Digest,
    /// The member workpiece the attempt runs against.
    pub workpiece: WorkpieceId,
    /// The stage the attempt executes.
    pub stage: StageId,
    /// The portable transformation to submit.
    pub transformation: Transformation,
    /// The member's frozen scope-revision digest, explicit so the host reactor
    /// never infers it from `transformation.inputs` (ADR-0152).
    pub scope_revision: Digest,
    /// The candidate tree the attempt runs against, when the member has one
    /// (ADR-0152). The reactor displays it as the evidence-binding digest;
    /// `None` displays the scope revision.
    pub candidate: Option<Digest>,
    /// The [`AgentProfile`] the bloom's sealed stage catalog calibrates this stage
    /// at (ADR-0174) — carried because only the reducer holds the catalog, and the
    /// reactor must not fall back to the compiled line for a bloom that sealed one.
    pub profile: AgentProfile,
    /// The configuration the attempt runs under (ADR-0174) — the member's
    /// registry layered over the bloom's. The reactor resolves each address it
    /// needs against the store.
    pub configs: ConfigRegistry,
}

/// The integration dispatch outbox payload (ADR-0152 §Resolution drives
/// integration): the wasm control actor enqueues it under
/// [`Topic::Integrate`] from a
/// [`Decision::DispatchIntegration`];
/// the host integrate reactor drains it, folds each candidate tree onto the
/// bloom's integration branch in member order, and admits the resulting
/// [`Fact::Resolve`](crate::reduce::Fact::Resolve).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IntegratePayload {
    /// The bloom whose members all carry claims.
    pub bloom: Digest,
    /// The sealed base the integration branch bootstraps at.
    pub base: Digest,
    /// Every member's workpiece and claimed candidate tree, in member order.
    pub members: Vec<MemberCandidate>,
    /// The predecessor whose candidate refs this fold adopts first, when the
    /// bloom inherited its whole claim set from one.
    pub adopt_from: Option<Digest>,
}

/// The whole-bloom aggregate-review dispatch outbox payload (ADR-0153): the
/// reviewed bloom, the review-lane [`Transformation`] (its `inputs[0]` the
/// integrated tree the returned evidence binds, its `checkout` the landable
/// head the critic checks out), and which review pass this is. The wasm
/// control actor enqueues it under [`Topic::AggregateReview`] from a
/// [`Decision::DispatchAggregateReview`].
/// Defined here (always compiled) so the host reactor can decode it inward,
/// cycle-free — like [`OutboxPayload`] / [`DispatchPayload`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AggregateReviewPayload {
    /// The reviewed bloom.
    pub bloom: Digest,
    /// The review-lane transformation to submit.
    pub transformation: Transformation,
    /// Which pass of the two-pass review this dispatches (ADR-0153).
    pub pass: ReviewPass,
    /// The [`AgentProfile`] the bloom's sealed stage catalog calibrates
    /// `AggregateReview` at (ADR-0174).
    pub profile: AgentProfile,
}

/// The payload a [`Topic::AggregateVerify`] outbox row carries — the
/// whole-bloom mechanical gate the executor reactor runs over the folded head.
///
/// Carries no pass discriminator: the verify lane runs the same `verify.check`
/// fan-out every roll, with nothing analogous to the review's delta-confirm
/// narrowing, so the roll count lives on the record and never changes what is
/// dispatched.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AggregateVerifyPayload {
    /// The verified bloom.
    pub bloom: Digest,
    /// The verify-lane transformation to submit.
    pub transformation: Transformation,
    /// The [`AgentProfile`] the bloom's sealed stage catalog calibrates
    /// `AggregateVerify` at (ADR-0174).
    pub profile: AgentProfile,
}

/// Which pass of the two-pass whole-bloom aggregate review (ADR-0153) a dispatch
/// is. Replaces the former `roll: u32` (`1` the full review, `2` the
/// delta-confirm): the two passes are a closed set the reducer's ceiling caps at,
/// so the type names them rather than leaving a reader to decode a magic count.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewPass {
    /// The first review — the full critic pass over the whole integrated head.
    Full,
    /// The second review — the delta-confirm against the frozen finding set the
    /// first failing pass raised, judging only whether those findings are
    /// resolved rather than hunting fresh ones.
    DeltaConfirm,
}

impl ReviewPass {
    /// Map the reducer's 1-based aggregate-review roll counter onto the pass —
    /// the first roll is the [`Full`](Self::Full) review, every later roll the
    /// [`DeltaConfirm`](Self::DeltaConfirm). The reducer caps the counter at the
    /// two-pass ceiling, so only these two passes ever dispatch (ADR-0153). The
    /// reducer's [`Decision`] and [`Outcome`](crate::reduce::Outcome) keep the
    /// numeric roll (their own journaled/wire vocabulary); this is the edge that
    /// projects it onto the typed outbox payload.
    #[must_use]
    pub fn from_roll(roll: u32) -> Self {
        if roll > 1 {
            Self::DeltaConfirm
        } else {
            Self::Full
        }
    }
}

/// The land dispatch outbox payload (ADR-0149 §The boundary, migration step 3):
/// the resolved bloom plus the compare-and-swap arguments the host's land reactor
/// issues through the source port's `aether.source.land` op. The wasm control
/// actor enqueues it under [`Topic::Land`] from a
/// [`Decision::DispatchLand`] the moment a
/// bloom resolves. Defined here (always compiled) so the host reactor can decode
/// it inward, cycle-free — like [`OutboxPayload`] / [`DispatchPayload`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LandPayload {
    /// The resolved bloom to land.
    pub bloom: Digest,
    /// The sealed base the CAS lands on — a moved mainline is a clean base-moved
    /// refusal, not a land onto the new head.
    pub expected_base: Digest,
    /// The head mainline advances to on a successful land.
    pub new_head: Digest,
}

/// The authorized orphan-claim release outbox payload (ADR-0179): the request
/// digest the completion admits back under, plus the typed target the source
/// port's expected-holder compare-and-swap runs against. The control core
/// enqueues it under [`Topic::OrphanClaimRelease`] from a
/// [`Decision::DispatchOrphanClaimRelease`]; the claim-release reactor drains it.
/// Defined here (always compiled) so the host reactor can decode it inward,
/// cycle-free — like [`OutboxPayload`] / [`LandPayload`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OrphanClaimReleasePayload {
    /// The request digest — the idempotency anchor the completion admits under.
    pub request: Digest,
    /// The signed target: which typed ref, and the holder the CAS expects.
    pub target: OrphanClaimRelease,
}

/// The combined atomic store commit (ADR-0149 §The control core). One
/// transact-mail carrying the idempotency-keyed journal event plus the
/// reducer's membership mutations and outbox payloads, applied in a **single**
/// `SQLite` transaction. This is the primitive the wasm control actor drives
/// after reducing an event — "one store transaction" cannot be assembled from
/// the separate `append_event` / `claim_seal` / `enqueue_outbox` mails (three
/// transactions; a crash between them breaks atomicity), and a wasm actor
/// cannot hold a `SQLite` transaction open across host round-trips.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.commit")]
pub struct Commit {
    /// The event's idempotency key — the inbox dedup axis. A key already
    /// journaled makes the whole commit a [`CommitResult::Duplicate`] no-op.
    pub idempotency_key: String,
    /// The event's canonical `aether_data::wire` bytes (an encoded
    /// [`Event`](crate::reduce::Event)) — the durable replay source.
    #[serde(with = "aether_data::bytes")]
    pub event: Vec<u8>,
    /// The reducer's [`Decisions`](crate::reduce::Decisions) — outcome plus
    /// ordered effects — as canonical `aether_data::wire` bytes, journaled
    /// beside the event so replay folds the recorded decision instead of
    /// re-deciding under whatever reducer is current (ADR-0190).
    #[serde(with = "aether_data::bytes")]
    pub decisions: Vec<u8>,
    /// The identity of the build whose reducer decided this event — the
    /// ADR-0190 decider stamp, for offline divergence audits.
    pub decider: String,
    /// The workpieces this decision releases from their blooms. Applied before
    /// the claims, so a superseding successor can reclaim a workpiece its
    /// predecessor freed in the same transaction.
    pub releases: Vec<MembershipMutation>,
    /// The workpieces this decision claims for their blooms, under the
    /// at-most-one-active-bloom-per-workpiece uniqueness constraint. A conflict
    /// on any one aborts the whole commit.
    pub claims: Vec<MembershipMutation>,
    /// The outbox entries this decision enqueues (e.g. a landing receipt).
    pub outbox: Vec<OutboxPayload>,
}

/// Reply to [`Commit`]. Echoes the `idempotency_key` so the control actor can
/// correlate the reply to the admit it is still holding a reply handle for —
/// the store is addressed by runtime name (`send_to_named`), which carries no
/// typed reply context, so the key is the correlation axis.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.commit_result")]
pub enum CommitResult {
    /// The whole decision committed atomically at this journal sequence.
    Applied {
        /// The idempotency key of the committed event (correlation).
        idempotency_key: String,
        /// The journal sequence the event landed at.
        sequence: u64,
    },
    /// The idempotency key was already journaled — nothing was applied.
    Duplicate {
        /// The idempotency key that was already present (correlation).
        idempotency_key: String,
    },
    /// A claimed workpiece was already held by an active bloom; the whole
    /// commit rolled back and applied nothing (all-or-nothing admission).
    Conflict {
        /// The idempotency key of the refused event (correlation).
        idempotency_key: String,
        /// The first conflicting workpiece.
        workpiece: String,
    },
    /// The commit failed for a non-conflict reason.
    Err {
        /// The idempotency key of the failed event (correlation).
        idempotency_key: String,
        /// A human-readable failure reason.
        error: String,
    },
}

/// Read the whole journal, in sequence order — the recovery replay source
/// (ADR-0149 §Migration step 1). The control actor sends this from `wire` at
/// boot; its reply rebuilds the snapshot.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.replay_journal")]
pub struct ReplayJournal;

/// One journaled event, in the [`ReplayJournalResult`] stream.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    /// The event's journal sequence.
    pub sequence: u64,
    /// The event's idempotency key.
    pub idempotency_key: String,
    /// The event's canonical `aether_data::wire` bytes.
    #[serde(with = "aether_data::bytes")]
    pub event: Vec<u8>,
    /// The wire-encoded [`Decisions`](crate::reduce::Decisions) recorded when
    /// the event was admitted (ADR-0190) — what boot replay folds.
    #[serde(with = "aether_data::bytes")]
    pub decisions: Vec<u8>,
    /// The identity of the build whose reducer decided this event.
    pub decider: String,
}

/// Reply to [`ReplayJournal`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.replay_journal_result")]
pub enum ReplayJournalResult {
    /// The journal, in sequence order.
    Ok {
        /// Every journaled event, oldest first.
        records: Vec<JournalRecord>,
    },
    /// The replay failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Read every stored configuration — the content behind the addresses sealed
/// registries name (ADR-0174). The control actor sends this at boot before it
/// replays, and again when an admit names an address it does not hold.
///
/// Whole-table rather than a requested address list, for the same reason
/// [`ReplayJournal`] is: the set is small (one row per distinct authored value),
/// and a request carrying addresses would have to be assembled from the very
/// registries the caller is trying to resolve. A miss is driven by an operator
/// authoring a new configuration, so re-reading on one is bounded by how fast a
/// person can write them.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.load_configs")]
pub struct LoadConfigs;

/// One stored configuration, in the [`LoadConfigsResult`] stream.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ConfigRecord {
    /// The content address (the digest's raw bytes) a registry entry names it by.
    #[serde(with = "aether_data::bytes")]
    pub digest: Vec<u8>,
    /// The kind name the bytes decode as, which is also the registry key.
    pub kind: String,
    /// The configuration's canonical `aether_data::wire` bytes.
    #[serde(with = "aether_data::bytes")]
    pub bytes: Vec<u8>,
}

/// Reply to [`LoadConfigs`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.store.load_configs_result")]
pub enum LoadConfigsResult {
    /// Every stored configuration.
    Ok {
        /// The records, in address order.
        records: Vec<ConfigRecord>,
    },
    /// The read failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Admit one event to the control loop (ADR-0149 §The control core). The
/// `aether.bloomery.admit` ingress carries an [`Event`](crate::reduce::Event)
/// as canonical `aether_data::wire` bytes (the opaque-bytes convention the
/// store family uses), so an external RPC client or a peer capability admits a
/// fact without the control actor sharing its typed value vocabulary over the
/// wire.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.bloomery.admit")]
pub struct Admit {
    /// The event's canonical `aether_data::wire` bytes (an encoded
    /// [`Event`](crate::reduce::Event)).
    #[serde(with = "aether_data::bytes")]
    pub event: Vec<u8>,
}

/// Reply to [`Admit`]: the reducer [`Outcome`](crate::reduce::Outcome) the
/// event resolved to, as canonical `aether_data::wire` bytes. A caller decodes
/// it back into an `Outcome` to learn whether the fact sealed, integrated,
/// landed, or was refused (and why).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.bloomery.admit_result")]
pub enum AdmitResult {
    /// The event reduced (and durably committed) to an outcome; `outcome` is
    /// the wire-encoded [`Outcome`](crate::reduce::Outcome).
    Ok {
        /// The wire-encoded reducer outcome.
        #[serde(with = "aether_data::bytes")]
        outcome: Vec<u8>,
    },
    /// The admitted bytes did not decode into an [`Event`](crate::reduce::Event),
    /// or the durable commit failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Read the live projection (ADR-0149 §The boundary). The control actor is the
/// single owner of the live [`Snapshot`](crate::reduce::Snapshot), so reads
/// come from here rather than rebuilding a snapshot per request. With `bloom`
/// unset the reply carries the whole [`ViewDocument`](crate::port::ViewDocument);
/// with `bloom` set to a bloom-id's digest bytes it carries that one bloom's
/// [`BloomView`](crate::port::BloomView).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.bloomery.query")]
pub struct Query {
    /// The bloom to read, as its digest bytes; unset reads the whole document.
    pub bloom: Option<Vec<u8>>,
    /// An orphan-claim release request to read, as its digest bytes (ADR-0179).
    ///
    /// Its own field rather than a second meaning for `bloom` because the two
    /// name different things — a bloom id and a release request digest — and a
    /// shared field would make an unrecognised digest ambiguous between "no such
    /// bloom" and "no such release". Takes precedence when both are set; a
    /// request digest is the more specific read.
    #[serde(default)]
    pub release: Option<Vec<u8>>,
}

/// Reply to [`Query`]: the requested projection as canonical
/// `aether_data::wire` bytes — a [`ViewDocument`](crate::port::ViewDocument)
/// for a whole-document read, or a [`BloomView`](crate::port::BloomView) for a
/// single-bloom read.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.bloomery.query_result")]
pub enum QueryResult {
    /// The whole live projection, wire-encoded
    /// [`ViewDocument`](crate::port::ViewDocument).
    Document {
        /// The wire-encoded `ViewDocument`.
        #[serde(with = "aether_data::bytes")]
        document: Vec<u8>,
    },
    /// One bloom's view, wire-encoded [`BloomView`](crate::port::BloomView).
    Bloom {
        /// The wire-encoded `BloomView`.
        #[serde(with = "aether_data::bytes")]
        view: Vec<u8>,
    },
    /// No bloom with the requested id is known.
    NotFound,
    /// Encoding the requested projection into wire bytes failed — the read
    /// could not be served rather than being answered with an empty payload.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
    /// One orphan-claim release request's journal-derived state, wire-encoded
    /// [`OrphanClaimReleaseRecord`](crate::values::OrphanClaimReleaseRecord)
    /// (ADR-0179). Appended so the prior variants' wire discriminants are
    /// unchanged.
    Release {
        /// The wire-encoded `OrphanClaimReleaseRecord` — its `completion` is
        /// `None` while the release is still pending.
        #[serde(with = "aether_data::bytes")]
        record: Vec<u8>,
    },
    /// No orphan-claim release request with the requested digest is known
    /// (ADR-0179).
    ///
    /// Its own variant rather than a second meaning for [`NotFound`](Self::NotFound)
    /// for the same reason [`Query::release`] is its own field: the reply is what
    /// the reader renders, so collapsing the two makes an unrecognised digest
    /// name the wrong resource — a missing release would report "no bloom". The
    /// [`Query`] doc already refuses that ambiguity on the request side; this
    /// keeps the answer side honest too. Appended so the prior variants' wire
    /// discriminants are unchanged.
    ReleaseNotFound,
}

/// The `aether.source.*` claim transact-mail kinds the control core sends to
/// the native source-port capability. Always compiled (like the store's
/// [`Commit`] family) so the host can re-export them inward, cycle-free.
mod source_mail;
pub use source_mail::{
    ClaimResult, ClaimSeal, CompleteRelease, CompleteReleaseResult, CompleteTransfer, EnumerateClaims,
    EnumerateClaimsResult, ObserveMainline, ObserveMainlineResult, ReleaseSeal, TransferSeal,
};

mod claim_plan;
pub use claim_plan::{
    HealOp, ReconcileOp, held_to_seal_error, held_to_supersede_error, plan_heals, reconcile_op, release_reclaim_mail,
    release_seal_mail, seal_claim_mail, transfer_seal_mail,
};
