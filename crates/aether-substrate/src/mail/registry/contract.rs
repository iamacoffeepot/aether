//! The contract a route publishes (ADR-0231 §4): the `(KindId,
//! ReplyContract)` rows its actor handles and whether it has a `#[fallback]`,
//! written onto the route record by the same registry apply that makes it
//! `Live` or stages it as an inline `Alias`, and the one rule (§5) a
//! republished contract must keep.

use std::fmt;
use std::sync::Arc;

use aether_data::ReplyContract;
use aether_kinds::ComponentCapabilities;

use crate::actor::native::{Dispatch, NativeActor};
use crate::mail::KindId;

/// The contract rows one route publishes, sorted by kind with one row per
/// kind, and whether its actor has a `#[fallback]`.
///
/// Both transports build it from the same receive surface: a native actor
/// from its `Dispatch::capabilities()`, a wasm guest from its module's
/// `ActorInputs`. A closure route (an inline handler, a relay inbox, a test
/// sink) publishes the empty contract, which proves nothing about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteContract {
    rows: Arc<[(KindId, ReplyContract)]>,
    fallback: bool,
}

/// The first way a successor contract fails to keep its predecessor's
/// (ADR-0231 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractBreak {
    /// The successor drops the predecessor's row for this kind, or changes
    /// its reply.
    Row(KindId),
    /// The predecessor has a `#[fallback]` and the successor does not.
    Fallback,
}

impl fmt::Display for ContractBreak {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Row(kind) => write!(formatter, "drops or changes its row for {kind}"),
            Self::Fallback => formatter.write_str("drops its fallback"),
        }
    }
}

impl RouteContract {
    /// The contract a receive surface declares: one row per handler, and the
    /// fallback flag from its fallback record. A kind listed twice keeps its
    /// first row.
    #[must_use]
    pub fn from_capabilities(capabilities: &ComponentCapabilities) -> Self {
        let mut rows: Vec<(KindId, ReplyContract)> =
            capabilities.handlers.iter().map(|handler| (handler.id, handler.reply)).collect();
        rows.sort_by_key(|(kind, _)| *kind);
        rows.dedup_by_key(|(kind, _)| *kind);
        Self { rows: rows.into(), fallback: capabilities.fallback.is_some() }
    }

    /// The first break `successor` makes in this contract, or `None` when it
    /// keeps every row and, if this contract has one, the fallback. Rows the
    /// successor adds, and a fallback it adds, are allowed.
    ///
    /// Consumers: the registry's republish guard, which keeps a published
    /// contract monotone, and the wasm trampoline's replace refusal.
    #[must_use]
    pub fn first_break(&self, successor: &Self) -> Option<ContractBreak> {
        aether_data::first_contract_break(self.rows.iter().copied(), successor.rows.iter().copied())
            .map(ContractBreak::Row)
            .or_else(|| (self.fallback && !successor.fallback).then_some(ContractBreak::Fallback))
    }

    /// The contract of a closure route, which publishes no rows.
    pub(crate) fn empty() -> Self {
        Self { rows: Arc::from([]), fallback: false }
    }

    /// The contract native actor `A` declares through its `#[actor]`
    /// dispatch table.
    pub(crate) fn of<A: NativeActor>() -> Self {
        Self::from_capabilities(&<A as Dispatch<A::State>>::capabilities())
    }

    /// The rows and the fallback flag, for the test door that reads a
    /// published contract.
    pub(crate) fn into_parts(self) -> (Vec<(KindId, ReplyContract)>, bool) {
        (self.rows.to_vec(), self.fallback)
    }

    /// The rows alone, shared rather than copied, for the registry's
    /// [`PublishedRoutes`](aether_data::wire::PublishedRoutes) answer, which
    /// a `ProtocolPath` decode checks coverage against.
    pub(crate) fn into_rows(self) -> Arc<[(KindId, ReplyContract)]> {
        self.rows
    }
}
