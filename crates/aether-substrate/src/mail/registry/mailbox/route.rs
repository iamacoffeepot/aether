//! The route record itself: what the registry stores under a `MailboxId`,
//! the lifecycle it is in, and the endpoint a live one dispatches to.
//!
//! Records only. Which lifecycle a route is in and when it may change is
//! decided by the writers in [`super::apply`], and the endpoint's two
//! conversions below are total maps between the same two shapes, so
//! there is no behaviour here to pin that its readers and writers do not
//! already own.

use std::process::abort;
use std::sync::Arc;

use aether_data::ErasedActorPath;

use crate::mail::MailboxId;
use crate::mail::registry::RouteContract;
use crate::mail::registry::effect::ActivationToken;
use crate::mail::registry::handlers::{InboxHandler, InlineHandler};

use super::{MailboxEntry, SeizeCell};

/// When a route record was first inserted, among every record this registry
/// has ever inserted (ADR-0248 §5). The registry draws one for each birth and
/// nothing else writes one, so two records compare in the order the registry
/// applied their births. Serials are registry-local: a caller may compare and
/// copy one but cannot make one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct BirthSerial(u64);

impl BirthSerial {
    /// Below every serial [`Self::next`] hands out, which start at one. It
    /// fills the slots of a `LineageOrder` past its depth, which nothing
    /// reads, and no record carries it.
    pub(super) const BEFORE_ANY: Self = Self(0);

    pub(super) fn next(counter: &mut u64) -> Self {
        *counter = counter.checked_add(1).unwrap_or_else(|| {
            tracing::error!("birth serial sequence exhausted; registry cannot keep lineage order");
            abort();
        });
        Self(*counter)
    }
}

/// A route's name is proven against the ADR-0166 grammar once, when a writer
/// in [`super::apply`] first publishes it, so reading one back for a
/// reference (`Mailer::actor_path`) has no failure to report.
///
/// `born` is stamped by the same writer, when the record is first inserted,
/// and every later write of the record carries it unchanged (ADR-0248 §5):
/// a route keeps its serial from `Starting` through `Live` to `Dropped`.
#[derive(Clone)]
pub(super) struct RouteRecord {
    pub(super) canonical_name: ErasedActorPath,
    pub(super) born: BirthSerial,
    pub(super) lifecycle: RouteLifecycle,
}

#[derive(Clone)]
pub(super) enum RouteLifecycle {
    Starting {
        token: ActivationToken,
    },
    /// A published actor route. `contract` is what its actor handles
    /// (ADR-0231 §4), written by the same apply that made it `Live`.
    Live {
        endpoint: RouteEndpoint,
        contract: RouteContract,
    },
    /// Logical Wasm inline-child route. Dispatch follows the target's
    /// current lifecycle and endpoint while preserving the alias as the
    /// routed recipient for guest membrane demux. `contract` is the child
    /// type's own, written when the alias is staged.
    Alias {
        target_parent: MailboxId,
        contract: RouteContract,
    },
    /// A retired route: its actor has closed, and the route stays as a
    /// tombstone under its proven name (ADR-0079 §7). `contract` is the one
    /// the route last published, moved across by the apply that retired it,
    /// so a typed path naming the closed actor still proves its type at
    /// decode (ADR-0231 §3) and the receiver's `resolve` answers "not live".
    Dropped {
        contract: RouteContract,
    },
}

#[derive(Clone)]
pub enum RouteEndpoint {
    Inbox { handler: Arc<dyn InboxHandler>, seize: SeizeCell },
    Inline(Arc<dyn InlineHandler>),
}

impl RouteEndpoint {
    pub(super) fn from_entry(entry: MailboxEntry) -> Self {
        match entry {
            MailboxEntry::Inbox { handler, seize } => Self::Inbox { handler, seize },
            MailboxEntry::Inline(handler) => Self::Inline(handler),
            MailboxEntry::Dropped => unreachable!("Dropped is a lifecycle, not a live route endpoint"),
        }
    }

    pub(super) fn as_entry(&self) -> MailboxEntry {
        match self {
            Self::Inbox { handler, seize } => {
                MailboxEntry::Inbox { handler: Arc::clone(handler), seize: Arc::clone(seize) }
            }
            Self::Inline(handler) => MailboxEntry::Inline(Arc::clone(handler)),
        }
    }
}
