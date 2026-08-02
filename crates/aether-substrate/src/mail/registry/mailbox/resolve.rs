//! Route resolution: the alias-following walk every dispatch and name
//! lookup shares, and the point-in-time answers it hands back.

use aether_data::{ScopePathError, mailbox_id_from_path, validate_scope_path};

use crate::mail::registry::{AddressResolutionError, ResolvedAddress};
use crate::mail::{KindId, MailboxId};
use crate::scheduler::SeizeHandle;

use super::route::{RouteEndpoint, RouteLifecycle, RouteRecord};
use super::{CapturedDisposition, MailboxEntry, Registry};

pub(super) enum ResolvedRoute<'a> {
    Starting { target: MailboxId },
    Live { endpoint: &'a RouteEndpoint },
    Dropped,
    Unknown,
}

pub(super) fn resolve_route<'a, F>(recipient: MailboxId, route_for: F) -> ResolvedRoute<'a>
where
    F: Fn(MailboxId) -> Option<&'a RouteRecord>,
{
    let Some(route) = route_for(recipient) else {
        return ResolvedRoute::Unknown;
    };
    match &route.lifecycle {
        RouteLifecycle::Starting { .. } => ResolvedRoute::Starting { target: recipient },
        RouteLifecycle::Live { endpoint } => ResolvedRoute::Live { endpoint },
        RouteLifecycle::Alias { target_parent } => match route_for(*target_parent).map(|route| &route.lifecycle) {
            Some(RouteLifecycle::Starting { .. }) => ResolvedRoute::Starting { target: *target_parent },
            Some(RouteLifecycle::Live { endpoint }) => ResolvedRoute::Live { endpoint },
            Some(RouteLifecycle::Dropped) => ResolvedRoute::Dropped,
            Some(RouteLifecycle::Alias { .. }) | None => ResolvedRoute::Unknown,
        },
        RouteLifecycle::Dropped => ResolvedRoute::Dropped,
    }
}

/// What the published route view resolves a `(kind, recipient)` pair to
/// (ADR-0165).
///
/// The summary form of a `RouteLookup`, carrying the disposition without the
/// endpoint or its seize handle — those are dispatch machinery and stay
/// internal. Returned by
/// [`Registry::resolve_route_state`](Registry::resolve_route_state).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteResolution {
    /// A live endpoint is published for this recipient.
    Live,
    /// The recipient is reserved but not yet activated, so mail addressed here
    /// parks rather than dropping.
    Starting,
    /// The recipient was registered and has since been dropped.
    Dropped,
    /// No route was ever published under this id.
    Unknown,
}

/// One point-in-time route lookup.
///
/// Carries no kind name. The name is derived data — every caller already holds
/// the `KindId` it would be resolved from — and attaching it here meant every
/// lookup took an increment and a decrement on **one** `Arc` strong count
/// shared by every reader, which is half of why registry reads did not scale
/// (iamacoffeepot/aether#4276, #4278). Render and wire sites resolve it through
/// [`Registry::kind_name`] when they actually need the string.
pub struct RouteLookup {
    endpoint: Option<RouteEndpoint>,
    starting: bool,
    dropped: bool,
    generation: u64,
}

impl RouteLookup {
    pub(crate) fn is_starting(&self) -> bool {
        self.starting
    }

    pub(crate) fn is_unknown(&self) -> bool {
        self.endpoint.is_none() && !self.starting && !self.dropped
    }

    pub(crate) fn seize_handle(&self) -> Option<&SeizeHandle> {
        match &self.endpoint {
            Some(RouteEndpoint::Inbox { seize, .. }) => seize.get(),
            Some(RouteEndpoint::Inline(_)) | None => None,
        }
    }

    pub(crate) fn into_captured(self) -> CapturedDisposition {
        match self.endpoint {
            Some(endpoint) => CapturedDisposition::Live { endpoint },
            None if self.dropped => CapturedDisposition::Dropped,
            None => CapturedDisposition::Unknown,
        }
    }

    /// Returns the route publication generation used for this lookup.
    #[must_use]
    #[allow(dead_code, reason = "carried now so later route coordinates do not change the lookup contract")]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl Registry {
    /// Does a live (non-`Dropped`) mailbox exist under `name`? Returns
    /// its id if so. The id itself is deterministic (ADR-0029) —
    /// callers that just want the id without a liveness check can use
    /// `MailboxId::from_name` directly.
    ///
    /// # Panics
    /// Panics if the inner routing lock is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn lookup(&self, name: &str) -> Option<MailboxId> {
        match self.resolve_address(name) {
            Ok(resolved) => Some(resolved.mailbox_id),
            Err(error @ (AddressResolutionError::PathTooDeep { .. } | AddressResolutionError::PathTooLong { .. })) => {
                tracing::warn!(name, ?error, "scope path over cap; resolution miss");
                None
            }
            Err(_) => None,
        }
    }

    /// Resolve a canonical or ADR-0166 abbreviated actor address to one
    /// live mailbox. Canonical inputs preserve the existing
    /// validate/fold/exact-name lookup. An abbreviated input expands
    /// through the generated root/child inventory before that canonical
    /// lookup, so aliases are never hashed, stored, or reverse-reported.
    pub fn resolve_address(&self, address: &str) -> Result<ResolvedAddress, AddressResolutionError> {
        let canonical_path = match address.split_once("://") {
            None => address.to_owned(),
            Some((root, relative)) => self.addresses.as_ref().map_err(Clone::clone)?.expand(root, relative)?,
        };
        let mailbox_id = self
            .lookup_canonical(&canonical_path)?
            .ok_or_else(|| AddressResolutionError::NoLiveMailbox { canonical_path: canonical_path.clone() })?;
        Ok(ResolvedAddress { mailbox_id, canonical_path })
    }

    fn lookup_canonical(&self, name: &str) -> Result<Option<MailboxId>, ScopePathError> {
        // ADR-0098 wire boundary: `name` is user-controlled (the MCP
        // `recipient_name` surface resolves here), so cap its scope depth
        // / byte size before it folds to a registry key. An over-cap name
        // is a resolution miss, not a key-space bloat.
        let segments: Vec<&str> = name.split('/').collect();
        validate_scope_path(&segments)?;
        // ADR-0099 §4: resolve a written name by the parse → fold (the
        // inverse of the `/`-render), not `hash(name)` — a hosted /
        // nested actor's id is the lineage fold, so the whole-string hash
        // would miss it. The depth-1 case (every root cap) folds to the
        // same id `hash(name)` gives.
        #[allow(clippy::disallowed_methods)]
        // the runtime-name resolution path itself — the registry is the one owner of the parse → fold
        let id = mailbox_id_from_path(name);
        let routes = self.routes.load();
        Ok(match routes.entry_for(&id) {
            Some(route)
                if route.canonical_name == name
                    && matches!(
                        resolve_route(id, |candidate| routes.entry_for(&candidate)),
                        ResolvedRoute::Starting { .. } | ResolvedRoute::Live { .. }
                    ) =>
            {
                Some(id)
            }
            _ => None,
        })
    }

    /// Fetch the entry for a mailbox id from a point-in-time view.
    /// Returns an owned compatibility projection of the private route.
    pub fn entry(&self, id: MailboxId) -> Option<MailboxEntry> {
        let routes = self.routes.load();
        match resolve_route(id, |candidate| routes.entry_for(&candidate)) {
            ResolvedRoute::Live { endpoint } => Some(endpoint.as_entry()),
            ResolvedRoute::Dropped => Some(MailboxEntry::Dropped),
            ResolvedRoute::Starting { .. } | ResolvedRoute::Unknown => None,
        }
    }

    /// Hot-path route lookup for the mailer's route step.
    ///
    /// Reads only the route view. It used to load the kind view as well and
    /// clone the kind's name out of it, which put an `Arc` strong count shared
    /// by every reader on the hot path for data the caller could already
    /// derive from the `kind` it passed in (iamacoffeepot/aether#4278).
    pub(crate) fn route_lookup(&self, _kind: KindId, recipient: MailboxId) -> RouteLookup {
        let (endpoint, starting, dropped, generation) = {
            let routes = self.routes.load();
            let (endpoint, starting, dropped) = match resolve_route(recipient, |id| routes.entry_for(&id)) {
                ResolvedRoute::Starting { .. } => (None, true, false),
                ResolvedRoute::Live { endpoint } => (Some(endpoint.clone()), false, false),
                ResolvedRoute::Dropped => (None, false, true),
                ResolvedRoute::Unknown => (None, false, false),
            };
            (endpoint, starting, dropped, routes.generation())
        };
        RouteLookup { endpoint, starting, dropped, generation }
    }

    /// What the published route view resolves `(kind, recipient)` to.
    ///
    /// Delegates to the hot-path `route_lookup`, so this reads exactly
    /// the lock-free published snapshot the mailer's route step reads — it is
    /// the same code, not a second walk that could drift from it. What it adds
    /// is a summary that crosses the crate boundary: `RouteLookup` carries a
    /// `RouteEndpoint` and its `SeizeHandle`, which are dispatch machinery no
    /// caller outside the substrate should hold.
    ///
    /// Reads never touch `Registry::inner`, so this contends with nothing —
    /// concurrent callers scale on the published view alone.
    #[must_use]
    pub fn resolve_route_state(&self, kind: KindId, recipient: MailboxId) -> RouteResolution {
        let lookup = self.route_lookup(kind, recipient);
        // Ordered as `route_lookup` builds them: a reservation wins over an
        // absent endpoint, and `dropped` is what distinguishes a retired route
        // from one that never existed.
        if lookup.is_starting() {
            RouteResolution::Starting
        } else if lookup.endpoint.is_some() {
            RouteResolution::Live
        } else if lookup.dropped {
            RouteResolution::Dropped
        } else {
            RouteResolution::Unknown
        }
    }

    /// Reverse of `lookup`: name for a given mailbox id, or `None` if
    /// the id is unknown. Used by the closure dispatch path to stamp
    /// `origin` on observation mail (ADR-0011).
    pub fn mailbox_name(&self, id: MailboxId) -> Option<String> {
        self.routes.load().entry_for(&id).map(|route| route.canonical_name.clone())
    }
}
