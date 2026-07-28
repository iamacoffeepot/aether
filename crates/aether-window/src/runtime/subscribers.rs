use std::collections::{BTreeSet, HashMap};

use aether_actor::ReplyMode;
use aether_data::{KindId, MailboxId};
use aether_substrate::actor::monitor::MonitorHandle;
use aether_substrate::actor::native::NativeCtx;
use aether_substrate::mail::MailboxEntry;

use crate::{WindowId, WindowSelector};

/// Selector-aware subscriptions for events originating at windows.
///
/// `All` and `One(window)` are stored separately so an all-window
/// subscription naturally includes windows created later. Recipient lookup
/// unions into a `BTreeSet`, which makes a mailbox subscribed through both
/// selectors receive one copy.
pub struct WindowSubscribers {
    all: HashMap<KindId, BTreeSet<MailboxId>>,
    specific: HashMap<(WindowId, KindId), BTreeSet<MailboxId>>,
    monitors: HashMap<MailboxId, MonitorHandle>,
}

impl WindowSubscribers {
    pub fn new() -> Self {
        Self { all: HashMap::new(), specific: HashMap::new(), monitors: HashMap::new() }
    }

    pub fn subscribe<M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, M>,
        selector: WindowSelector,
        kind: KindId,
        mailbox: MailboxId,
    ) {
        self.insert(selector, kind, mailbox);
        self.watch(ctx, mailbox);
    }

    pub fn subscribe_self<M: ReplyMode>(
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

    pub fn unsubscribe(&mut self, selector: WindowSelector, kind: KindId, mailbox: MailboxId) {
        self.remove(selector, kind, mailbox);
    }

    pub fn unsubscribe_self<M: ReplyMode>(
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

    pub fn unsubscribe_all(&mut self, mailbox: MailboxId) {
        self.all.retain(|_, recipients| {
            recipients.remove(&mailbox);
            !recipients.is_empty()
        });
        self.specific.retain(|_, recipients| {
            recipients.remove(&mailbox);
            !recipients.is_empty()
        });
    }

    pub fn purge_departed(&mut self, mailbox: MailboxId) {
        self.monitors.remove(&mailbox);
        self.unsubscribe_all(mailbox);
    }

    pub fn recipients(&self, window: WindowId, kind: KindId) -> BTreeSet<MailboxId> {
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

pub fn validate_subscriber_mailbox(ctx: &NativeCtx<'_>, mailbox: MailboxId) -> Result<(), String> {
    match ctx.mailer().registry().entry(mailbox) {
        Some(MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_)) => Ok(()),
        Some(MailboxEntry::Dropped) => Err(format!("mailbox {mailbox:?} already dropped")),
        None => Err(format!("unknown mailbox id {mailbox:?}")),
    }
}

#[cfg(test)]
mod tests {
    use aether_data::Kind;
    use aether_kinds::{Key, MouseMove};

    use super::*;

    fn fixture() -> (WindowSubscribers, MailboxId, MailboxId) {
        (WindowSubscribers::new(), MailboxId(1), MailboxId(2))
    }

    #[test]
    fn one_selector_routes_only_the_selected_window() {
        let (mut subscribers, mailbox, _) = fixture();
        subscribers.insert(WindowSelector::One(WindowId(1)), Key::ID, mailbox);

        assert_eq!(subscribers.recipients(WindowId(1), Key::ID), BTreeSet::from([mailbox]));
        assert!(subscribers.recipients(WindowId(2), Key::ID).is_empty());
    }

    #[test]
    fn all_selector_is_prospective() {
        let (mut subscribers, mailbox, _) = fixture();
        subscribers.insert(WindowSelector::All, MouseMove::ID, mailbox);

        assert_eq!(subscribers.recipients(WindowId(1), MouseMove::ID), BTreeSet::from([mailbox]));
        assert_eq!(subscribers.recipients(WindowId(99), MouseMove::ID), BTreeSet::from([mailbox]));
    }

    #[test]
    fn all_and_one_union_deduplicates_the_same_mailbox() {
        let (mut subscribers, mailbox, other) = fixture();
        subscribers.insert(WindowSelector::All, Key::ID, mailbox);
        subscribers.insert(WindowSelector::One(WindowId(7)), Key::ID, mailbox);
        subscribers.insert(WindowSelector::One(WindowId(7)), Key::ID, other);

        assert_eq!(subscribers.recipients(WindowId(7), Key::ID), BTreeSet::from([mailbox, other]));
    }

    #[test]
    fn unsubscribe_and_bulk_cleanup_preserve_other_routes() {
        let (mut subscribers, mailbox, other) = fixture();
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
        let (mut subscribers, departed, survivor) = fixture();
        subscribers.insert(WindowSelector::All, Key::ID, departed);
        subscribers.insert(WindowSelector::All, Key::ID, survivor);
        subscribers.insert(WindowSelector::One(WindowId(3)), MouseMove::ID, departed);

        subscribers.purge_departed(departed);

        assert_eq!(subscribers.recipients(WindowId(3), Key::ID), BTreeSet::from([survivor]));
        assert!(subscribers.recipients(WindowId(3), MouseMove::ID).is_empty());
    }
}
