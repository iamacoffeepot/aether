// Name registries. Two parallel tables: mailboxes (name → MailboxId,
// tagged component-vs-sink) and kinds (name → u32 kind id, per
// ADR-0005). Both are populated at substrate boot and frozen when the
// registry is wrapped in Arc — readers see a stable snapshot and
// contend on nothing. Post-boot dynamic registration is deferred.

use std::collections::HashMap;
use std::sync::Arc;

use aether_hub_protocol::SessionToken;

use crate::mail::MailboxId;

/// Handler invoked when mail is delivered to a substrate-owned sink.
/// Called on a scheduler worker thread; must be `Send + Sync`.
/// Arguments: kind name (resolved by the dispatcher so sinks don't
/// need a reverse lookup), the originating Claude session token for
/// hub-inbound mail (or `SessionToken::NIL` for substrate-local mail)
/// per ADR-0008's reply-to-sender primitive, payload bytes, and the
/// kind-implied count.
pub type SinkHandler = Arc<dyn Fn(&str, SessionToken, &[u8], u32) + Send + Sync + 'static>;

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
    kind_by_name: HashMap<String, u32>,
    /// Parallel index: `kind_names[id]` is the canonical name the kind
    /// was first registered with. Kept in sync with `kind_by_name` so
    /// `kind_name(id)` is O(1); used by `SinkHandler` dispatch to hand
    /// sinks the name without forcing them to keep their own map.
    kind_names: Vec<String>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            by_name: HashMap::new(),
            entries: Vec::new(),
            kind_by_name: HashMap::new(),
            kind_names: Vec::new(),
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

    /// Register a mail kind by name. Idempotent — re-registering a name
    /// returns the id it was first assigned. Ids are dense and assigned
    /// in insertion order, per ADR-0005's kind-name registry.
    pub fn register_kind(&mut self, name: impl Into<String>) -> u32 {
        let name = name.into();
        if let Some(&id) = self.kind_by_name.get(&name) {
            return id;
        }
        let id = self.kind_names.len() as u32;
        self.kind_names.push(name.clone());
        self.kind_by_name.insert(name, id);
        id
    }

    pub fn kind_id(&self, name: &str) -> Option<u32> {
        self.kind_by_name.get(name).copied()
    }

    /// Reverse of `kind_id`: name for a given id, or `None` if the id
    /// is out of range. Used by the scheduler to hand sink handlers a
    /// kind name without them keeping their own map.
    pub fn kind_name(&self, id: u32) -> Option<&str> {
        self.kind_names.get(id as usize).map(|s| s.as_str())
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
            Arc::new(move |_kind, _sender, _bytes, count| {
                c2.fetch_add(count, Ordering::SeqCst);
            }),
        );
        let Some(MailboxEntry::Sink(h)) = r.entry(id) else {
            panic!("expected sink")
        };
        h("aether.tick", SessionToken::NIL, &[], 7);
        h("aether.tick", SessionToken::NIL, &[], 3);
        assert_eq!(counter.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn mailbox_ids_are_dense_and_sequential() {
        let mut r = Registry::new();
        let a = r.register_component("a");
        let b = r.register_sink("b", Arc::new(|_, _, _, _| {}));
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

    #[test]
    fn kind_ids_are_dense_and_sequential() {
        let mut r = Registry::new();
        let a = r.register_kind("aether.tick");
        let b = r.register_kind("aether.key");
        let c = r.register_kind("hello.npc_health");
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(c, 2);
    }

    #[test]
    fn kind_registration_is_idempotent() {
        let mut r = Registry::new();
        let first = r.register_kind("aether.tick");
        let second = r.register_kind("aether.tick");
        assert_eq!(first, second);
        assert_eq!(r.register_kind("aether.key"), 1);
    }

    #[test]
    fn kind_id_lookup() {
        let mut r = Registry::new();
        let id = r.register_kind("aether.tick");
        assert_eq!(r.kind_id("aether.tick"), Some(id));
        assert!(r.kind_id("absent").is_none());
    }

    #[test]
    fn kind_name_reverse_lookup() {
        let mut r = Registry::new();
        let a = r.register_kind("aether.tick");
        let b = r.register_kind("aether.key");
        assert_eq!(r.kind_name(a), Some("aether.tick"));
        assert_eq!(r.kind_name(b), Some("aether.key"));
        assert!(r.kind_name(999).is_none());
    }
}
