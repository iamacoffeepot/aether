//! Logical inline-child alias routes (ADR-0114 section 2) — first-class
//! addresses that carry no actor slot of their own and follow a parent.

use crate::mail::MailboxId;

use super::Registry;
use super::resolve::{ResolvedRoute, resolve_route};
use super::route::RouteLifecycle;

impl Registry {
    /// The parent `alias` routes to, or `None` when `alias` is not a logical
    /// inline-child route (ADR-0114). This reads route identity, not endpoint
    /// identity — a live alias to a `Starting` or `Dropped` parent still
    /// answers with that parent.
    pub(crate) fn alias_parent(&self, alias: MailboxId) -> Option<MailboxId> {
        match self.routes.load().entry_for(&alias).map(|route| &route.lifecycle) {
            Some(RouteLifecycle::Alias { target_parent }) => Some(*target_parent),
            _ => None,
        }
    }

    /// Test whether `alias` is the logical inline-child route owned by
    /// `target_parent`. This checks route identity, not endpoint identity.
    pub(crate) fn is_alias_to(&self, alias: MailboxId, target_parent: MailboxId) -> bool {
        self.alias_parent(alias) == Some(target_parent)
    }

    /// Whether `alias` is a logical inline-child route (ADR-0114 §2) that
    /// resolves to a live endpoint right now — a first-class address with no
    /// actor slot of its own, mailable this instant. The lifecycle surface
    /// (`monitor`) reads this where an ordinary actor's liveness comes from
    /// the [`ActorRegistry`](crate::ActorRegistry), which knows nothing of
    /// aliases.
    pub(crate) fn is_live_alias(&self, alias: MailboxId) -> bool {
        let routes = self.routes.load();
        matches!(routes.entry_for(&alias).map(|route| &route.lifecycle), Some(RouteLifecycle::Alias { .. }))
            && matches!(resolve_route(alias, |candidate| routes.entry_for(&candidate)), ResolvedRoute::Live { .. })
    }

    /// Every logical inline-child alias routing to `target_parent`
    /// (ADR-0114 §2) — the addresses whose occupant departs with
    /// `target_parent`'s, so an actor-lifecycle fan-out can name each of
    /// them. A scan of the published route view: it runs once per departure,
    /// and the alternative (a parent-keyed reverse index) would have to be
    /// maintained on every alias publication to serve a per-departure read.
    pub(crate) fn aliases_of(&self, target_parent: MailboxId) -> Vec<MailboxId> {
        self.routes
            .load()
            .entries()
            .filter(|(_, route)| {
                matches!(route.lifecycle, RouteLifecycle::Alias { target_parent: parent } if parent == target_parent)
            })
            .map(|(alias, _)| *alias)
            .collect()
    }
}
