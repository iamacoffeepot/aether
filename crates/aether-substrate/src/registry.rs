// Mailbox registry. Maps stable string names to `MailboxId`s and records
// whether each mailbox is a WASM component or a substrate-owned sink.
// Fixed at substrate boot for milestone 1; dynamic registration is a
// later-milestone concern (issue #18).

use std::collections::HashMap;
use std::sync::Arc;

use crate::mail::MailboxId;

/// Handler invoked when mail is delivered to a substrate-owned sink.
/// Called on a scheduler worker thread; must be `Send + Sync`. Argument
/// is the payload bytes; second argument is the kind-implied count.
pub type SinkHandler = Arc<dyn Fn(&[u8], u32) + Send + Sync + 'static>;

/// What a given mailbox actually is. The registry records this so the
/// scheduler can dispatch appropriately without a per-mail type check.
pub enum MailboxEntry {
    /// Mail goes to a WASM component's `receive` function on a worker.
    Component,
    /// Mail is handled inline by a substrate-native closure.
    Sink(SinkHandler),
}

pub struct Registry {
    by_name: HashMap<String, MailboxId>,
    entries: Vec<MailboxEntry>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            by_name: HashMap::new(),
            entries: Vec::new(),
        }
    }

    fn insert(&mut self, name: impl Into<String>, entry: MailboxEntry) -> MailboxId {
        let name = name.into();
        if self.by_name.contains_key(&name) {
            panic!("mailbox name already registered: {name}");
        }
        let id = MailboxId(self.entries.len() as u32);
        self.entries.push(entry);
        self.by_name.insert(name, id);
        id
    }

    /// Register a WASM component under `name`. The returned `MailboxId`
    /// is handed to the scheduler alongside the component's `Actor`.
    pub fn register_component(&mut self, name: impl Into<String>) -> MailboxId {
        self.insert(name, MailboxEntry::Component)
    }

    /// Register a substrate-owned sink. Mail to this mailbox is handled
    /// inline on the thread that delivered it (or on the host-function
    /// caller thread if a component sent it).
    pub fn register_sink(&mut self, name: impl Into<String>, handler: SinkHandler) -> MailboxId {
        self.insert(name, MailboxEntry::Sink(handler))
    }

    pub fn lookup(&self, name: &str) -> Option<MailboxId> {
        self.by_name.get(name).copied()
    }

    pub fn entry(&self, id: MailboxId) -> Option<&MailboxEntry> {
        self.entries.get(id.0 as usize)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[test]
    fn register_and_lookup_component() {
        let mut r = Registry::new();
        let id = r.register_component("physics");
        assert_eq!(id, MailboxId(0));
        assert_eq!(r.lookup("physics"), Some(id));
        assert!(matches!(r.entry(id), Some(MailboxEntry::Component)));
    }

    #[test]
    fn sink_handler_runs_on_call() {
        let mut r = Registry::new();
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = Arc::clone(&counter);
        let id = r.register_sink(
            "heartbeat",
            Arc::new(move |_bytes, count| {
                c2.fetch_add(count, Ordering::SeqCst);
            }),
        );
        let Some(MailboxEntry::Sink(h)) = r.entry(id) else {
            panic!("expected sink")
        };
        h(&[], 7);
        h(&[], 3);
        assert_eq!(counter.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn mailbox_ids_are_dense_and_sequential() {
        let mut r = Registry::new();
        let a = r.register_component("a");
        let b = r.register_sink("b", Arc::new(|_, _| {}));
        let c = r.register_component("c");
        assert_eq!(a, MailboxId(0));
        assert_eq!(b, MailboxId(1));
        assert_eq!(c, MailboxId(2));
        assert_eq!(r.len(), 3);
    }

    #[test]
    #[should_panic(expected = "mailbox name already registered")]
    fn duplicate_name_panics() {
        let mut r = Registry::new();
        r.register_component("x");
        r.register_component("x");
    }

    #[test]
    fn lookup_missing_returns_none() {
        let r = Registry::new();
        assert!(r.lookup("nope").is_none());
        assert!(r.entry(MailboxId(42)).is_none());
    }
}
