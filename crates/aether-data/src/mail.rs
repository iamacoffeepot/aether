//! Mail-sender types shared by every mail-bearing surface (ADR-0008,
//! ADR-0037, ADR-0042, ADR-0083). Lives in `aether-data` so the
//! `Dispatch` trait in `aether-actor` (which references `Source` in its
//! signature) can name them without depending on `aether-actor`-internal
//! modules. The wasm-side `aether_actor::mail::ReplyHandle` is a `u32`
//! FFI handle distinct in shape from this substrate-side `Source` type;
//! ADR-0076 and ADR-0083 document the split.
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
/// `sender` is [`MailboxId::CHASSIS_MAILBOX_ID`] for chassis-originated
/// mail (the reserved name `"aether.chassis"`, issue iamacoffeepot/aether#725).
/// Per-actor mints use the owning actor's `MailboxId`. The
/// [`MailId::NONE`] sentinel below carries `MailboxId::NONE` instead —
/// "no inbound mail" is structurally distinct from "chassis as sender".
///
/// Serde-serializable so the ADR-0080 `TraceEvent` (and its
/// structured `TraceRingEntry`, the per-actor ring's wire element)
/// can carry `MailId` when a trace ring is queried over the wire. The
/// substrate's host-side `Envelope` and `Mail` types do not serialize,
/// so the field additions on those remain wire-free.
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
    pub const NONE: Self = Self {
        sender: MailboxId::NONE,
        correlation_id: 0,
    };

    /// Construct a `MailId` from a sender mailbox and correlation id.
    /// Producer paths (`NativeBinding::send_mail`, plus the future
    /// drainer and chassis-pushed sites) call this immediately after
    /// fetching the next correlation from the per-actor counter.
    #[must_use]
    pub const fn new(sender: MailboxId, correlation_id: u64) -> Self {
        Self {
            sender,
            correlation_id,
        }
    }
}

/// The addressable mailbox of a mail's *immediate* sender — where a
/// reply routes. Strictly an addressing hint: mail is pushed at a
/// recipient, and the substrate auto-stamps this to the sending
/// actor's own mailbox on every send, so it changes every hop
/// (ADR-0083). It is the immediate sender, not the chain origin; the
/// origin lives in the tracing layer (`root` / `parent_mail`,
/// ADR-0080), where it is observable but deliberately not addressable.
///
/// `None` is the default — broadcast / substrate-generated mail with
/// no identifiable sender. `Session` tags mail that arrived from a
/// Claude MCP session, so replies route back to that session
/// (ADR-0008). `EngineMailbox` tags mail bubbled up from a component
/// on another engine, so replies route to the originating engine's
/// mailbox (ADR-0037 Phase 2). `Component` tags mailbox-bound mail
/// that a local component pushed through `ComponentCtx::send`, so
/// reply paths (ADR-0041's io capability is the motivating case) can
/// route the `*Result` back to the component via the mailer rather
/// than the hub.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SourceAddr {
    None,
    Session(SessionToken),
    EngineMailbox {
        engine_id: EngineId,
        mailbox_id: MailboxId,
    },
    Component(MailboxId),
}

/// The immediate sender of a substrate-side `Mail` — the addressing
/// layer's "who sent this" (ADR-0083). `addr` is the sender's
/// addressable mailbox (where a reply goes); `correlation_id` is an
/// opaque `u64` the original sender attached so it can identify its
/// specific request among replies of the same kind. The mailer
/// auto-echoes `correlation_id` when constructing a reply via
/// `send_reply`, so reply-bearing capabilities don't need per-handler
/// echo code. `0` means "no correlation"; waits with
/// `expected_correlation == 0` match any correlation (backward-compat
/// and for non-correlating callers like broadcasts and input mail).
///
/// `Source` is the *immediate* sender, one hop, re-stamped to the
/// sending actor's own mailbox on every send — it is not the chain
/// origin. The thing a reader imagines "persists through the chain" is
/// the origin, and it does persist, in the tracing layer (`root` /
/// `parent_mail`, ADR-0080), not here. Addressing is deliberately
/// one-hop; the chain origin is observable, not addressable.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub addr: SourceAddr,
    pub correlation_id: u64,
}

impl Source {
    /// Sentinel for no correlation. Using the explicit constant makes
    /// call sites self-documenting; a plain `0` in code comments as
    /// "why zero?" whereas `NO_CORRELATION` makes the intent obvious.
    pub const NO_CORRELATION: u64 = 0;

    /// `Source` with no addr and no correlation.
    pub const NONE: Self = Self {
        addr: SourceAddr::None,
        correlation_id: Self::NO_CORRELATION,
    };

    /// Sender addr alone, no correlation. Short form for mail paths
    /// that want to address a reply but don't participate in the
    /// ADR-0042 correlation scheme (the hub's inbound session mail
    /// today — a future change could have sessions carry correlation
    /// when the MCP `send_mail` tool grows to expose it).
    #[must_use]
    pub fn to(addr: SourceAddr) -> Self {
        Self {
            addr,
            correlation_id: Self::NO_CORRELATION,
        }
    }

    /// Addr + correlation. The common sync-wrapper shape.
    #[must_use]
    pub fn with_correlation(addr: SourceAddr, correlation_id: u64) -> Self {
        Self {
            addr,
            correlation_id,
        }
    }

    /// Whether the sender addr is `None`. Callers that would otherwise
    /// pattern-match on the `SourceAddr::None` variant use this instead.
    #[must_use]
    pub fn is_none(&self) -> bool {
        matches!(self.addr, SourceAddr::None)
    }
}
