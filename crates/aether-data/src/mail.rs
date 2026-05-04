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

use crate::{EngineId, MailboxId, SessionToken};

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
/// a local component pushed through `SubstrateCtx::send`, so sink
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
