//! Reply-routing types shared by every mail-bearing surface (ADR-0008,
//! ADR-0037, ADR-0042). Lives in `aether-data` so the `Dispatch` trait
//! in `aether-actor` (which references `ReplyTo` in its signature) can
//! name them without depending on `aether-actor`-internal modules,
//! AND to avoid a name clash with `aether-actor`'s wasm-side `ReplyTo`
//! — that one is a `u32` FFI handle distinct in shape from this
//! substrate-side dispatch type. ADR-0076 documents the split.
//!
//! `aether-substrate` re-exports these from `aether_substrate::mail`
//! so existing call sites compile unchanged.

use alloc::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::schema::{LabelNode, NamedField, Primitive, SchemaType};
use crate::{EngineId, MailboxId, Schema, SessionToken};

/// ADR-0080 §1: the unique identity of a mail. A 128-bit composite of
/// the producer's mailbox id and the producer's per-actor monotonic
/// `correlation_id`. Exact-by-construction — no central minter to
/// contend on, no hash to collide.
///
/// `sender` is `MailboxId::NONE` for chassis-originated mail (the
/// reserved sentinel for "no actor mailbox"). Per-actor mints use the
/// owning actor's `MailboxId`.
///
/// Serde-serializable so PR 2's `TraceEvent` (and its postcard-shaped
/// `BatchedTraceEvents` envelope) can carry `MailId` over the in-
/// process trace pipeline. The substrate's host-side `Envelope` and
/// `Mail` types do not serialize, so the field additions on those
/// remain wire-free.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct MailId {
    pub sender: MailboxId,
    pub correlation_id: u64,
}

/// ADR-0080 §1: hand-written `Schema` impl. Cannot use the derive
/// because it lives in `aether-actor-derive` and emits
/// `aether_data::...` paths that don't resolve from inside `aether-
/// data` itself. The shape mirrors what the derive would produce for
/// a two-field non-`#[repr(C)]` struct.
impl Schema for MailId {
    const SCHEMA: SchemaType = SchemaType::Struct {
        fields: Cow::Borrowed(&[
            NamedField {
                name: Cow::Borrowed("sender"),
                ty: SchemaType::TypeId(MailboxId::TYPE_ID),
            },
            NamedField {
                name: Cow::Borrowed("correlation_id"),
                ty: SchemaType::Scalar(Primitive::U64),
            },
        ]),
        repr_c: false,
    };
    const LABEL: Option<&'static str> = Some("aether.mail_id");
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

impl MailId {
    /// Sentinel for "not yet stamped" / "chassis root". Equivalent to
    /// `MailId::default()`. The PR 2 dispatch path treats this value
    /// as the chassis-as-originator marker.
    pub const NONE: MailId = MailId {
        sender: MailboxId::NONE,
        correlation_id: 0,
    };

    /// Construct a `MailId` from a sender mailbox and correlation id.
    /// Producer paths (`NativeBinding::send_mail`, plus the future
    /// drainer and chassis-pushed sites) call this immediately after
    /// fetching the next correlation from the per-actor counter.
    pub const fn new(sender: MailboxId, correlation_id: u64) -> Self {
        Self {
            sender,
            correlation_id,
        }
    }
}

/// Where a reply-bearing mail should route when the recipient
/// answers. Strictly a routing hint: mail is pushed at a recipient,
/// not sent from a mailbox.
///
/// `None` is the default — broadcast / substrate-generated mail with
/// no meaningful reply target. `Session` tags mail that arrived from
/// a Claude MCP session, so replies route back to that session
/// (ADR-0008). `EngineMailbox` tags mail bubbled up from a component
/// on another engine, so replies route to the originating engine's
/// mailbox (ADR-0037 Phase 2). `Component` tags sink-bound mail that
/// a local component pushed through `ComponentCtx::send`, so sink
/// reply paths (ADR-0041's io sink is the motivating case) can route
/// the `*Result` back to the component via the mailer rather than
/// the hub.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReplyTarget {
    None,
    Session(SessionToken),
    EngineMailbox {
        engine_id: EngineId,
        mailbox_id: MailboxId,
    },
    Component(MailboxId),
}

/// Reply-routing info for a substrate-side `Mail`. `target` describes
/// where a reply goes; `correlation_id` is an opaque `u64` the
/// original sender attached so it can identify its specific request
/// among replies of the same kind. The mailer auto-echoes
/// `correlation_id` when constructing a reply via `send_reply`, so
/// reply-bearing sinks don't need per-sink echo code. `0` means "no
/// correlation"; waits with `expected_correlation == 0` match any
/// correlation (backward-compat and for non-correlating callers like
/// broadcasts and input mail).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReplyTo {
    pub target: ReplyTarget,
    pub correlation_id: u64,
}

impl ReplyTo {
    /// Sentinel for no correlation. Using the explicit constant makes
    /// call sites self-documenting; a plain `0` in code comments as
    /// "why zero?" whereas `NO_CORRELATION` makes the intent obvious.
    pub const NO_CORRELATION: u64 = 0;

    /// `ReplyTo` with no target and no correlation.
    pub const NONE: ReplyTo = ReplyTo {
        target: ReplyTarget::None,
        correlation_id: Self::NO_CORRELATION,
    };

    /// Reply target alone, no correlation. Short form for mail paths
    /// that want to address a reply but don't participate in the
    /// ADR-0042 correlation scheme (the hub's inbound session mail
    /// today — a future change could have sessions carry correlation
    /// when the MCP send_mail tool grows to expose it).
    pub fn to(target: ReplyTarget) -> Self {
        Self {
            target,
            correlation_id: Self::NO_CORRELATION,
        }
    }

    /// Target + correlation. The common sync-wrapper shape.
    pub fn with_correlation(target: ReplyTarget, correlation_id: u64) -> Self {
        Self {
            target,
            correlation_id,
        }
    }

    /// Whether the reply target is `None`. Existing callers that were
    /// pattern-matching on the pre-refactor `ReplyTo::None` variant
    /// use this instead.
    pub fn is_none(&self) -> bool {
        matches!(self.target, ReplyTarget::None)
    }
}
