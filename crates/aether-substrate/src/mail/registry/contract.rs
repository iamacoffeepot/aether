//! The contract a route publishes (ADR-0231 §4): the `(KindId,
//! ReplyContract)` rows its actor handles and whether it has a `#[fallback]`,
//! written onto the route record by the same registry apply that makes it
//! `Live` or stages it as an inline `Alias`, the one rule (§5) a
//! republished contract must keep, and whether the rows cover a protocol
//! (§3), the answer a protocol path's receipt reads.

use std::any::TypeId;
use std::fmt;
use std::sync::{Arc, Mutex};

use aether_actor::{Protocol, RowSet};
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
///
/// It also keeps the protocols its rows were found to cover (ADR-0231 §3),
/// so a protocol path's receipt compares rows once per route and protocol.
/// Clones of one published contract share that cache across route-table
/// snapshots, and a republish installs a new contract with an empty one. A
/// kept answer never goes stale, because published rows only grow (§5). The
/// cache is not part of the contract: equality compares the rows and the
/// fallback flag alone.
#[derive(Clone, Debug)]
pub struct RouteContract {
    rows: Arc<[(KindId, ReplyContract)]>,
    fallback: bool,
    covered: Arc<Mutex<Vec<TypeId>>>,
}

impl PartialEq for RouteContract {
    fn eq(&self, other: &Self) -> bool {
        self.rows == other.rows && self.fallback == other.fallback
    }
}

impl Eq for RouteContract {}

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
        Self { rows: rows.into(), fallback: capabilities.fallback.is_some(), covered: Arc::default() }
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

    /// The first of `P`'s kinds these rows do not cover, or `None` when they
    /// cover every row of `P` (ADR-0231 §3).
    ///
    /// §5's comparison with `P`'s rows as the predecessor: each row needs a
    /// published row for its kind with the same reply. A protocol row is
    /// never `Manual`, so a published `Manual` row covers nothing, and the
    /// rows `P` does not list are free. A covered answer is kept, so a later
    /// call for the same `P` compares nothing.
    ///
    /// Consumer: `Registry::resolve_protocol`, the receipt of a protocol path.
    ///
    /// # Panics
    /// Panics if the cache lock is poisoned — fail-fast per ADR-0063.
    pub(crate) fn first_uncovered<P: Protocol + 'static>(&self) -> Option<KindId> {
        let protocol = TypeId::of::<P>();
        let mut covered = self.covered.lock().expect("coverage cache lock poisoned");
        if covered.contains(&protocol) {
            return None;
        }

        let uncovered = aether_data::first_contract_break(
            <P::Rows as RowSet>::CONTRACTS.iter().copied(),
            self.rows.iter().copied(),
        );
        if uncovered.is_none() {
            covered.push(protocol);
        }
        uncovered
    }

    /// The contract of a closure route, which publishes no rows.
    pub(crate) fn empty() -> Self {
        Self { rows: Arc::from([]), fallback: false, covered: Arc::default() }
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
}
