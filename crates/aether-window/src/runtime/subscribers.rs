//! The selector-aware subscription table every window manager keeps. The
//! subscription *mail surface* over it lives in the sibling `manager` module,
//! alongside the rest of the shared manager surface (ADR-0169).

use std::collections::{BTreeSet, HashMap};

use aether_actor::{AnyActorRef, ReplyMode};
use aether_data::{KindId, MailboxId};
use aether_kinds::MonitorNotice;
use aether_substrate::actor::monitor::MonitorHandle;
use aether_substrate::actor::native::{Erased, NativeCtx};

use crate::{WindowId, WindowSelector};

/// Selector-aware subscriptions for events originating at windows.
///
/// `All` and `One(window)` are stored separately so an all-window
/// subscription naturally includes windows created later. Recipient lookup
/// unions into a `BTreeSet` of proven subscribers, which makes an actor
/// subscribed through both selectors receive one copy.
pub struct WindowSubscribers {
    all: HashMap<KindId, BTreeSet<AnyActorRef>>,
    specific: HashMap<(WindowId, KindId), BTreeSet<AnyActorRef>>,
    monitors: HashMap<AnyActorRef, MonitorHandle>,
}

impl WindowSubscribers {
    pub fn new() -> Self {
        Self { all: HashMap::new(), specific: HashMap::new(), monitors: HashMap::new() }
    }

    pub fn subscribe<M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, Erased, M>,
        selector: WindowSelector,
        kind: KindId,
        subscriber: AnyActorRef,
    ) {
        self.insert(selector, kind, subscriber);
        self.watch(ctx, subscriber);
    }

    pub fn subscribe_self<M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, Erased, M>,
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

    pub fn unsubscribe(&mut self, selector: WindowSelector, kind: KindId, subscriber: AnyActorRef) {
        self.remove(selector, kind, subscriber);
    }

    pub fn unsubscribe_self<M: ReplyMode>(
        &mut self,
        ctx: &NativeCtx<'_, Erased, M>,
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

    /// Drop every subscription held at `mailbox`, addressed by position
    /// rather than by proof.
    ///
    /// `aether.window.unsubscribe_all` exists to reclaim a mailbox that is
    /// usually already gone — a component that unloaded, an actor that
    /// retired — so demanding a proof would refuse exactly the request the
    /// kind is for. Nothing here sends: the removal is a key comparison over
    /// rows this table already owns.
    pub fn unsubscribe_all(&mut self, mailbox: MailboxId) {
        self.forget(mailbox);
    }

    /// Drop the departed actor's rows on its `MonitorNotice`.
    ///
    /// The notice names a position, and the table's rows are proofs, so this
    /// compares rather than exchanges. `ActorRef::entomb` trades a reference
    /// for a `Tombstone<R>`, but there is no erased tombstone for an
    /// `AnyActorRef` — and the window wants the rows *gone*, not replaced by
    /// a tombstone it would then have to skip on every fan-out. ADR-0230's
    /// named-consumer rule makes the comparison the right call here, not a
    /// shortcut around a missing type.
    pub fn purge_departed(&mut self, notice: MonitorNotice) {
        self.monitors.retain(|reference, _| reference.id() != notice.target);
        self.forget(notice.target);
    }

    pub fn recipients(&self, window: WindowId, kind: KindId) -> BTreeSet<AnyActorRef> {
        let mut recipients = self.all.get(&kind).cloned().unwrap_or_default();
        if let Some(specific) = self.specific.get(&(window, kind)) {
            recipients.extend(specific);
        }
        recipients
    }

    fn insert(&mut self, selector: WindowSelector, kind: KindId, subscriber: AnyActorRef) {
        match selector {
            WindowSelector::All => {
                self.all.entry(kind).or_default().insert(subscriber);
            }
            WindowSelector::One(window) => {
                self.specific.entry((window, kind)).or_default().insert(subscriber);
            }
        }
    }

    fn remove(&mut self, selector: WindowSelector, kind: KindId, subscriber: AnyActorRef) {
        let empty = match selector {
            WindowSelector::All => self.all.get_mut(&kind).is_some_and(|recipients| {
                recipients.remove(&subscriber);
                recipients.is_empty()
            }),
            WindowSelector::One(window) => self.specific.get_mut(&(window, kind)).is_some_and(|recipients| {
                recipients.remove(&subscriber);
                recipients.is_empty()
            }),
        };
        if empty {
            match selector {
                WindowSelector::All => {
                    self.all.remove(&kind);
                }
                WindowSelector::One(window) => {
                    self.specific.remove(&(window, kind));
                }
            }
        }
    }

    /// Drop every row whose subscriber sits at `position`, from both maps.
    fn forget(&mut self, position: MailboxId) {
        self.all.retain(|_, recipients| {
            recipients.retain(|reference| reference.id() != position);
            !recipients.is_empty()
        });
        self.specific.retain(|_, recipients| {
            recipients.retain(|reference| reference.id() != position);
            !recipients.is_empty()
        });
    }

    fn watch<M: ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, Erased, M>, subscriber: AnyActorRef) {
        if !self.monitors.contains_key(&subscriber)
            && let Ok(handle) = ctx.monitor(subscriber)
        {
            self.monitors.insert(subscriber, handle);
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
    use aether_substrate::mail::registry::MailDispatch;
    use aether_substrate::mail::{MailId, Source, SourceAddr};
    use aether_substrate::testing::boot_authority;

    use super::*;

    fn fixture() -> (WindowSubscribers, Arc<NativeBinding>, Arc<Mailer>) {
        let mailer = Arc::new(Mailer::new(Arc::new(Registry::new())));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));

        (WindowSubscribers::new(), binding, mailer)
    }

    /// Register a named inline mailbox and prove it through the ctx verb.
    /// `aether-window` cannot construct an `AnyActorRef` at all, so this is
    /// the only way a row reaches the table — the gate working.
    fn proven(mailer: &Mailer, ctx: &NativeCtx<'_>, name: &str) -> AnyActorRef {
        let position = mailer.registry().register_inline(&boot_authority(), name, Arc::new(|_: MailDispatch<'_>| {}));

        ctx.resolve_live(position).expect("a freshly registered inline mailbox proves")
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
        let transport =
            Arc::new(NativeBinding::new_for_test(Arc::new(Mailer::new(Arc::new(Registry::new()))), MailboxId(0)));
        let source = Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0xFEED))));
        let mut ctx = NativeCtx::new(&transport, source, MailId::NONE, MailId::NONE);

        assert!(subscribers.subscribe_self(&mut ctx, WindowSelector::All, Key::ID).is_err());
        assert!(subscribers.unsubscribe_self(&ctx, WindowSelector::All, Key::ID).is_err());

        assert!(subscribers.recipients(WindowId(1), Key::ID).is_empty(), "a rejected subscribe inserts no route");
    }

    #[test]
    fn one_selector_routes_only_the_selected_window() {
        let (mut subscribers, binding, mailer) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let subscriber = proven(&mailer, &ctx, "test.subscribers.one");

        subscribers.subscribe(&mut ctx, WindowSelector::One(WindowId(1)), Key::ID, subscriber);

        assert_eq!(subscribers.recipients(WindowId(1), Key::ID), BTreeSet::from([subscriber]));
        assert!(subscribers.recipients(WindowId(2), Key::ID).is_empty());
    }

    #[test]
    fn all_selector_is_prospective() {
        let (mut subscribers, binding, mailer) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let subscriber = proven(&mailer, &ctx, "test.subscribers.all");

        subscribers.subscribe(&mut ctx, WindowSelector::All, MouseMove::ID, subscriber);

        assert_eq!(subscribers.recipients(WindowId(1), MouseMove::ID), BTreeSet::from([subscriber]));
        assert_eq!(subscribers.recipients(WindowId(99), MouseMove::ID), BTreeSet::from([subscriber]));
    }

    #[test]
    fn all_and_one_union_deduplicates_the_same_mailbox() {
        let (mut subscribers, binding, mailer) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let subscriber = proven(&mailer, &ctx, "test.subscribers.union");
        let other = proven(&mailer, &ctx, "test.subscribers.union.other");

        subscribers.subscribe(&mut ctx, WindowSelector::All, Key::ID, subscriber);
        subscribers.subscribe(&mut ctx, WindowSelector::One(WindowId(7)), Key::ID, subscriber);
        subscribers.subscribe(&mut ctx, WindowSelector::One(WindowId(7)), Key::ID, other);

        assert_eq!(subscribers.recipients(WindowId(7), Key::ID), BTreeSet::from([subscriber, other]));
    }

    #[test]
    fn unsubscribe_and_bulk_cleanup_preserve_other_routes() {
        let (mut subscribers, binding, mailer) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let subscriber = proven(&mailer, &ctx, "test.subscribers.cleanup");
        let other = proven(&mailer, &ctx, "test.subscribers.cleanup.other");

        subscribers.subscribe(&mut ctx, WindowSelector::All, Key::ID, subscriber);
        subscribers.subscribe(&mut ctx, WindowSelector::One(WindowId(3)), Key::ID, subscriber);
        subscribers.subscribe(&mut ctx, WindowSelector::One(WindowId(3)), Key::ID, other);

        subscribers.unsubscribe(WindowSelector::All, Key::ID, subscriber);
        assert_eq!(subscribers.recipients(WindowId(3), Key::ID), BTreeSet::from([subscriber, other]));

        subscribers.unsubscribe_all(subscriber.id());
        assert_eq!(subscribers.recipients(WindowId(3), Key::ID), BTreeSet::from([other]));
    }

    #[test]
    fn monitor_cleanup_purges_only_the_departed_mailbox_from_every_route() {
        let (mut subscribers, binding, mailer) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let departed = proven(&mailer, &ctx, "test.subscribers.departed");
        let survivor = proven(&mailer, &ctx, "test.subscribers.survivor");

        subscribers.subscribe(&mut ctx, WindowSelector::All, Key::ID, departed);
        subscribers.subscribe(&mut ctx, WindowSelector::All, Key::ID, survivor);
        subscribers.subscribe(&mut ctx, WindowSelector::One(WindowId(3)), MouseMove::ID, departed);

        subscribers.purge_departed(MonitorNotice { target: departed.id() });

        assert_eq!(subscribers.recipients(WindowId(3), Key::ID), BTreeSet::from([survivor]));
        assert!(subscribers.recipients(WindowId(3), MouseMove::ID).is_empty());
    }
}
