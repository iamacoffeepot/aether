use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use aether_actor::ReplyMode;
use aether_data::{KindId, MailboxId};
use aether_substrate::actor::monitor::MonitorHandle;
use aether_substrate::actor::native::NativeCtx;
use aether_substrate::mail::registry::{MailboxEntry, Registry};

use crate::{WindowId, WindowSelector};

/// Selector-aware subscriptions for events originating at desktop windows.
///
/// `All` and `One(window)` are stored separately so an all-window
/// subscription naturally includes windows created later. Recipient lookup
/// unions into a `BTreeSet`, which makes a mailbox subscribed through both
/// selectors receive one copy.
pub(super) struct WindowSubscribers {
    registry: Arc<Registry>,
    all: HashMap<KindId, BTreeSet<MailboxId>>,
    specific: HashMap<(WindowId, KindId), BTreeSet<MailboxId>>,
    monitors: HashMap<MailboxId, MonitorHandle>,
}

impl WindowSubscribers {
    pub(super) fn new(registry: Arc<Registry>) -> Self {
        Self { registry, all: HashMap::new(), specific: HashMap::new(), monitors: HashMap::new() }
    }

    pub(super) fn subscribe<M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, M>,
        selector: WindowSelector,
        kind: KindId,
        mailbox: MailboxId,
    ) -> Result<(), String> {
        validate_subscriber_mailbox(&self.registry, mailbox)?;
        self.insert(selector, kind, mailbox);
        self.watch(ctx, mailbox);
        Ok(())
    }

    pub(super) fn subscribe_self<M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, M>,
        selector: WindowSelector,
        kind: KindId,
    ) -> Result<(), String> {
        let mailbox = ctx.source_mailbox().ok_or_else(|| {
            "aether.window.subscribe_self requires a local component sender; an external session or remote engine \
             must use aether.window.subscribe with an explicit mailbox"
                .to_owned()
        })?;
        self.insert(selector, kind, mailbox);
        self.watch(ctx, mailbox);
        Ok(())
    }

    pub(super) fn unsubscribe(
        &mut self,
        selector: WindowSelector,
        kind: KindId,
        mailbox: MailboxId,
    ) -> Result<(), String> {
        validate_subscriber_mailbox(&self.registry, mailbox)?;
        self.remove(selector, kind, mailbox);
        Ok(())
    }

    pub(super) fn unsubscribe_self<M: ReplyMode>(
        &mut self,
        ctx: &NativeCtx<'_, M>,
        selector: WindowSelector,
        kind: KindId,
    ) -> Result<(), String> {
        let mailbox = ctx.source_mailbox().ok_or_else(|| {
            "aether.window.unsubscribe_self requires a local component sender; an external session or remote engine \
             must use aether.window.unsubscribe with an explicit mailbox"
                .to_owned()
        })?;
        self.remove(selector, kind, mailbox);
        Ok(())
    }

    pub(super) fn unsubscribe_all(&mut self, mailbox: MailboxId) {
        self.all.retain(|_, recipients| {
            recipients.remove(&mailbox);
            !recipients.is_empty()
        });
        self.specific.retain(|_, recipients| {
            recipients.remove(&mailbox);
            !recipients.is_empty()
        });
    }

    pub(super) fn purge_departed(&mut self, mailbox: MailboxId) {
        self.monitors.remove(&mailbox);
        self.unsubscribe_all(mailbox);
    }

    pub(super) fn recipients(&self, window: WindowId, kind: KindId) -> BTreeSet<MailboxId> {
        let mut recipients = self.all.get(&kind).cloned().unwrap_or_default();
        if let Some(specific) = self.specific.get(&(window, kind)) {
            recipients.extend(specific);
        }
        recipients
    }

    fn insert(&mut self, selector: WindowSelector, kind: KindId, mailbox: MailboxId) {
        match selector {
            WindowSelector::All => {
                self.all.entry(kind).or_default().insert(mailbox);
            }
            WindowSelector::One(window) => {
                self.specific.entry((window, kind)).or_default().insert(mailbox);
            }
        }
    }

    fn remove(&mut self, selector: WindowSelector, kind: KindId, mailbox: MailboxId) {
        let empty = match selector {
            WindowSelector::All => self.all.get_mut(&kind).is_some_and(|recipients| {
                recipients.remove(&mailbox);
                recipients.is_empty()
            }),
            WindowSelector::One(window) => self.specific.get_mut(&(window, kind)).is_some_and(|recipients| {
                recipients.remove(&mailbox);
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

    fn watch<M: ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, M>, mailbox: MailboxId) {
        if !self.monitors.contains_key(&mailbox)
            && let Ok(handle) = ctx.monitor(mailbox)
        {
            self.monitors.insert(mailbox, handle);
        }
    }
}

fn validate_subscriber_mailbox(registry: &Registry, mailbox: MailboxId) -> Result<(), String> {
    match registry.entry(mailbox) {
        Some(MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_)) => Ok(()),
        Some(MailboxEntry::Dropped) => Err(format!("mailbox {mailbox:?} already dropped")),
        None => Err(format!("unknown mailbox id {mailbox:?}")),
    }
}

#[cfg(test)]
mod tests {
    use aether_data::Kind;
    use aether_kinds::{Key, MouseMove};
    use aether_substrate::mail::registry::MailDispatch;

    use super::*;

    fn fixture() -> (Arc<Registry>, WindowSubscribers, MailboxId, MailboxId) {
        let registry = Arc::new(Registry::new());
        let first = registry.register_inline("test.window.first", Arc::new(|_dispatch: MailDispatch<'_>| {}));
        let second = registry.register_inline("test.window.second", Arc::new(|_dispatch: MailDispatch<'_>| {}));
        let subscribers = WindowSubscribers::new(Arc::clone(&registry));
        (registry, subscribers, first, second)
    }

    #[test]
    fn one_selector_routes_only_the_selected_window() {
        let (_, mut subscribers, mailbox, _) = fixture();
        subscribers.insert(WindowSelector::One(WindowId(1)), Key::ID, mailbox);

        assert_eq!(subscribers.recipients(WindowId(1), Key::ID), BTreeSet::from([mailbox]));
        assert!(subscribers.recipients(WindowId(2), Key::ID).is_empty());
    }

    #[test]
    fn all_selector_is_prospective() {
        let (_, mut subscribers, mailbox, _) = fixture();
        subscribers.insert(WindowSelector::All, MouseMove::ID, mailbox);

        assert_eq!(subscribers.recipients(WindowId(1), MouseMove::ID), BTreeSet::from([mailbox]));
        assert_eq!(subscribers.recipients(WindowId(99), MouseMove::ID), BTreeSet::from([mailbox]));
    }

    #[test]
    fn all_and_one_union_deduplicates_the_same_mailbox() {
        let (_, mut subscribers, mailbox, other) = fixture();
        subscribers.insert(WindowSelector::All, Key::ID, mailbox);
        subscribers.insert(WindowSelector::One(WindowId(7)), Key::ID, mailbox);
        subscribers.insert(WindowSelector::One(WindowId(7)), Key::ID, other);

        assert_eq!(subscribers.recipients(WindowId(7), Key::ID), BTreeSet::from([mailbox, other]));
    }

    #[test]
    fn explicit_validation_rejects_unknown_and_dropped_mailboxes() {
        let (registry, _, mailbox, _) = fixture();
        assert_eq!(
            validate_subscriber_mailbox(&registry, MailboxId(0xBAD)),
            Err("unknown mailbox id 0x0000000000000bad".to_owned()),
        );

        registry.drop_mailbox(mailbox).expect("drop test mailbox");
        assert_eq!(
            validate_subscriber_mailbox(&registry, mailbox),
            Err(format!("mailbox {mailbox:?} already dropped")),
        );
    }

    #[test]
    fn unsubscribe_and_bulk_cleanup_preserve_other_routes() {
        let (_, mut subscribers, mailbox, other) = fixture();
        subscribers.insert(WindowSelector::All, Key::ID, mailbox);
        subscribers.insert(WindowSelector::One(WindowId(3)), Key::ID, mailbox);
        subscribers.insert(WindowSelector::One(WindowId(3)), Key::ID, other);

        subscribers.remove(WindowSelector::All, Key::ID, mailbox);
        assert_eq!(subscribers.recipients(WindowId(3), Key::ID), BTreeSet::from([mailbox, other]));

        subscribers.unsubscribe_all(mailbox);
        assert_eq!(subscribers.recipients(WindowId(3), Key::ID), BTreeSet::from([other]));
    }

    #[test]
    fn monitor_cleanup_purges_only_the_departed_mailbox_from_every_route() {
        let (_, mut subscribers, departed, survivor) = fixture();
        subscribers.insert(WindowSelector::All, Key::ID, departed);
        subscribers.insert(WindowSelector::All, Key::ID, survivor);
        subscribers.insert(WindowSelector::One(WindowId(3)), MouseMove::ID, departed);

        subscribers.purge_departed(departed);

        assert_eq!(subscribers.recipients(WindowId(3), Key::ID), BTreeSet::from([survivor]));
        assert!(subscribers.recipients(WindowId(3), MouseMove::ID).is_empty());
    }
}
