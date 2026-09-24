//! The selector-aware subscription table every window manager keeps. The
//! subscription *mail surface* over it lives in the sibling `manager` module,
//! alongside the rest of the shared manager surface (ADR-0169).

use std::collections::{BTreeSet, HashMap, HashSet};

use aether_actor::{ErasedActorRef, ReplyMode};
use aether_data::KindId;
use aether_substrate::actor::monitor::MonitorHandle;
use aether_substrate::actor::native::NativeCtx;

use crate::{WindowId, WindowSelector};

/// Selector-aware subscriptions for events originating at windows.
///
/// `All` and `One(window)` are stored separately so an all-window
/// subscription naturally includes windows created later. Recipient lookup
/// unions into a `BTreeSet` of proven subscribers, which makes an actor
/// subscribed through both selectors receive one copy.
///
/// `holders` is the reverse index: each subscriber's monitor and the exact
/// rows it holds, so a departure removes precisely that subscriber's rows
/// without scanning anyone else's.
pub struct WindowSubscribers {
    all: HashMap<KindId, BTreeSet<ErasedActorRef>>,
    specific: HashMap<(WindowId, KindId), BTreeSet<ErasedActorRef>>,
    holders: HashMap<ErasedActorRef, Holder>,
}

/// One subscriber's monitor and the rows it holds in the two selector maps.
#[derive(Default)]
struct Holder {
    monitor: Option<MonitorHandle>,
    rows: HashSet<Row>,
}

/// One row of a subscriber's, named by the selector map key it sits under.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Row {
    All(KindId),
    One(WindowId, KindId),
}

impl Row {
    const fn new(selector: WindowSelector, kind: KindId) -> Self {
        match selector {
            WindowSelector::All => Self::All(kind),
            WindowSelector::One(window) => Self::One(window, kind),
        }
    }
}

impl WindowSubscribers {
    pub fn new() -> Self {
        Self { all: HashMap::new(), specific: HashMap::new(), holders: HashMap::new() }
    }

    pub fn subscribe<A, M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        selector: WindowSelector,
        kind: KindId,
        subscriber: ErasedActorRef,
    ) {
        self.insert(selector, kind, subscriber);
        self.watch(ctx, subscriber);
    }

    pub fn subscribe_self<A, M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        selector: WindowSelector,
        kind: KindId,
    ) -> Result<(), String> {
        let subscriber = ctx.sender().ok_or_else(|| {
            "aether.window.subscribe_self requires a local component sender; an external session or remote engine \
             must use aether.window.subscribe with an explicit mailbox"
                .to_owned()
        })?;
        self.insert(selector, kind, subscriber);
        self.watch(ctx, subscriber);
        Ok(())
    }

    pub fn unsubscribe(&mut self, selector: WindowSelector, kind: KindId, subscriber: ErasedActorRef) {
        self.remove(selector, kind, subscriber);
    }

    pub fn unsubscribe_self<A, M: ReplyMode>(
        &mut self,
        ctx: &NativeCtx<'_, A, M>,
        selector: WindowSelector,
        kind: KindId,
    ) -> Result<(), String> {
        let subscriber = ctx.sender().ok_or_else(|| {
            "aether.window.unsubscribe_self requires a local component sender; an external session or remote engine \
             must use aether.window.unsubscribe with an explicit mailbox"
                .to_owned()
        })?;
        self.remove(selector, kind, subscriber);
        Ok(())
    }

    /// Drop every subscription `subscriber` holds and release its monitor.
    ///
    /// Two callers: `aether.window.unsubscribe_all`, whose position the
    /// manager proves once at receipt, and the `MonitorNotice` handlers,
    /// whose host-stamped sender is the departed subscriber (ADR-0230). The
    /// holder index names exactly the rows `subscriber` holds, so this
    /// removes those and touches nothing else.
    pub fn unsubscribe_all(&mut self, subscriber: ErasedActorRef) {
        let Some(holder) = self.holders.remove(&subscriber) else {
            return;
        };
        for row in holder.rows {
            self.remove_row(row, subscriber);
        }
    }

    pub fn recipients(&self, window: WindowId, kind: KindId) -> BTreeSet<ErasedActorRef> {
        let mut recipients = self.all.get(&kind).cloned().unwrap_or_default();
        if let Some(specific) = self.specific.get(&(window, kind)) {
            recipients.extend(specific);
        }
        recipients
    }

    fn insert(&mut self, selector: WindowSelector, kind: KindId, subscriber: ErasedActorRef) {
        match selector {
            WindowSelector::All => {
                self.all.entry(kind).or_default().insert(subscriber);
            }
            WindowSelector::One(window) => {
                self.specific.entry((window, kind)).or_default().insert(subscriber);
            }
        }
        self.holders.entry(subscriber).or_default().rows.insert(Row::new(selector, kind));
    }

    fn remove(&mut self, selector: WindowSelector, kind: KindId, subscriber: ErasedActorRef) {
        let row = Row::new(selector, kind);
        if let Some(holder) = self.holders.get_mut(&subscriber) {
            holder.rows.remove(&row);
        }
        self.remove_row(row, subscriber);
    }

    /// Remove `subscriber` from the selector map entry `row` names, dropping
    /// the entry once it is empty.
    fn remove_row(&mut self, row: Row, subscriber: ErasedActorRef) {
        match row {
            Row::All(kind) => {
                if self.all.get_mut(&kind).is_some_and(|recipients| {
                    recipients.remove(&subscriber);
                    recipients.is_empty()
                }) {
                    self.all.remove(&kind);
                }
            }
            Row::One(window, kind) => {
                if self.specific.get_mut(&(window, kind)).is_some_and(|recipients| {
                    recipients.remove(&subscriber);
                    recipients.is_empty()
                }) {
                    self.specific.remove(&(window, kind));
                }
            }
        }
    }

    fn watch<A, M: ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, A, M>, subscriber: ErasedActorRef) {
        let holder = self.holders.entry(subscriber).or_default();
        if holder.monitor.is_none() {
            holder.monitor = ctx.monitor(subscriber).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aether_data::{Kind, SessionToken, Uuid};
    use aether_kinds::{Key, MouseMove};
    use aether_substrate::Registry;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::registry::noop_handler;
    use aether_substrate::mail::{Source, SourceAddr};
    use aether_substrate::testing::{registered_ref, unrouted_binding};

    use super::*;

    fn fixture() -> (WindowSubscribers, Arc<NativeBinding>, Arc<Registry>) {
        let registry = Arc::new(Registry::new());
        let binding = unrouted_binding(&Arc::new(Mailer::new(Arc::clone(&registry))));

        (WindowSubscribers::new(), binding, registry)
    }

    /// Register a named test-local mailbox and return its proven reference.
    /// `aether-window` cannot construct an `ErasedActorRef` at all, so this is
    /// the only way a row reaches the table — the gate working.
    fn proven(registry: &Registry, name: &str) -> ErasedActorRef {
        registered_ref(registry, name, noop_handler())
    }

    /// The reflexive forms read their subscriber off the host-stamped
    /// envelope through `ctx.sender()`, so they are only meaningful for an
    /// in-process actor. A `Session` source (an external MCP session or a
    /// remote engine) must use the explicit-mailbox `subscribe` /
    /// `unsubscribe` instead, and gets an `Err` plus an untouched route
    /// table rather than a silently mis-attributed subscription.
    #[test]
    fn reflexive_subscribe_rejects_a_non_component_source_without_touching_routes() {
        let mut subscribers = WindowSubscribers::new();
        let transport = unrouted_binding(&Arc::new(Mailer::new(Arc::new(Registry::new()))));
        let source = Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0xFEED))));
        let mut ctx = NativeCtx::new(&transport, source, None, None);

        assert!(subscribers.subscribe_self(&mut ctx, WindowSelector::All, Key::ID).is_err());
        assert!(subscribers.unsubscribe_self(&ctx, WindowSelector::All, Key::ID).is_err());

        assert!(subscribers.recipients(WindowId(1), Key::ID).is_empty(), "a rejected subscribe inserts no route");
    }

    #[test]
    fn one_selector_routes_only_the_selected_window() {
        let (mut subscribers, binding, registry) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let subscriber = proven(&registry, "test.subscribers.one");

        subscribers.subscribe(&mut ctx, WindowSelector::One(WindowId(1)), Key::ID, subscriber);

        assert_eq!(subscribers.recipients(WindowId(1), Key::ID), BTreeSet::from([subscriber]));
        assert!(subscribers.recipients(WindowId(2), Key::ID).is_empty());
    }

    #[test]
    fn all_selector_is_prospective() {
        let (mut subscribers, binding, registry) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let subscriber = proven(&registry, "test.subscribers.all");

        subscribers.subscribe(&mut ctx, WindowSelector::All, MouseMove::ID, subscriber);

        assert_eq!(subscribers.recipients(WindowId(1), MouseMove::ID), BTreeSet::from([subscriber]));
        assert_eq!(subscribers.recipients(WindowId(99), MouseMove::ID), BTreeSet::from([subscriber]));
    }

    #[test]
    fn all_and_one_union_deduplicates_the_same_mailbox() {
        let (mut subscribers, binding, registry) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let subscriber = proven(&registry, "test.subscribers.union");
        let other = proven(&registry, "test.subscribers.union.other");

        subscribers.subscribe(&mut ctx, WindowSelector::All, Key::ID, subscriber);
        subscribers.subscribe(&mut ctx, WindowSelector::One(WindowId(7)), Key::ID, subscriber);
        subscribers.subscribe(&mut ctx, WindowSelector::One(WindowId(7)), Key::ID, other);

        assert_eq!(subscribers.recipients(WindowId(7), Key::ID), BTreeSet::from([subscriber, other]));
    }

    #[test]
    fn unsubscribe_and_bulk_cleanup_preserve_other_routes() {
        let (mut subscribers, binding, registry) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let subscriber = proven(&registry, "test.subscribers.cleanup");
        let other = proven(&registry, "test.subscribers.cleanup.other");

        subscribers.subscribe(&mut ctx, WindowSelector::All, Key::ID, subscriber);
        subscribers.subscribe(&mut ctx, WindowSelector::One(WindowId(3)), Key::ID, subscriber);
        subscribers.subscribe(&mut ctx, WindowSelector::One(WindowId(3)), Key::ID, other);

        subscribers.unsubscribe(WindowSelector::All, Key::ID, subscriber);
        assert_eq!(subscribers.recipients(WindowId(3), Key::ID), BTreeSet::from([subscriber, other]));

        subscribers.unsubscribe_all(subscriber);
        assert_eq!(subscribers.recipients(WindowId(3), Key::ID), BTreeSet::from([other]));
    }

    #[test]
    fn monitor_cleanup_purges_only_the_departed_mailbox_from_every_route() {
        let (mut subscribers, binding, registry) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let departed = proven(&registry, "test.subscribers.departed");
        let survivor = proven(&registry, "test.subscribers.survivor");

        subscribers.subscribe(&mut ctx, WindowSelector::All, Key::ID, departed);
        subscribers.subscribe(&mut ctx, WindowSelector::All, Key::ID, survivor);
        subscribers.subscribe(&mut ctx, WindowSelector::One(WindowId(3)), MouseMove::ID, departed);

        subscribers.unsubscribe_all(departed);

        assert_eq!(subscribers.recipients(WindowId(3), Key::ID), BTreeSet::from([survivor]));
        assert!(subscribers.recipients(WindowId(3), MouseMove::ID).is_empty());
    }
}
