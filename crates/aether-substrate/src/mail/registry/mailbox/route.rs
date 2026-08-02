//! The route record itself: what the registry stores under a `MailboxId`,
//! the lifecycle it is in, and the endpoint a live one dispatches to.
//!
//! Records only. Which lifecycle a route is in and when it may change is
//! decided by the writers in [`super::apply`], and the endpoint's two
//! conversions below are total maps between the same two shapes, so
//! there is no behaviour here to pin that its readers and writers do not
//! already own.

use std::sync::Arc;

use crate::mail::MailboxId;
use crate::mail::registry::effect::ActivationToken;
use crate::mail::registry::handlers::{InboxHandler, InlineHandler};

use super::{MailboxEntry, SeizeCell};

#[derive(Clone)]
pub(super) struct RouteRecord {
    pub(super) canonical_name: String,
    pub(super) lifecycle: RouteLifecycle,
}

#[derive(Clone)]
pub(super) enum RouteLifecycle {
    Starting {
        token: ActivationToken,
    },
    Live {
        endpoint: RouteEndpoint,
    },
    /// Logical Wasm inline-child route. Dispatch follows the target's
    /// current lifecycle and endpoint while preserving the alias as the
    /// routed recipient for guest membrane demux.
    Alias {
        target_parent: MailboxId,
    },
    Dropped,
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
