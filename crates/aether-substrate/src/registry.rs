// Name registries. Two parallel tables: mailboxes (name → MailboxId,
// tagged component-vs-sink) and kinds (name → u32 kind id, per
// ADR-0005). The registry uses interior mutability (`RwLock`) so
// mailboxes and kinds can be added at runtime — ADR-0010's runtime
// component loading mutates both tables after an `Arc<Registry>` has
// already been shared with the scheduler and hub client. Reads take
// a shared lock and are cheap; writes are rare (boot + load/replace
// /drop).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use aether_hub_protocol::SessionToken;

use crate::mail::MailboxId;

/// Handler invoked when mail is delivered to a substrate-owned sink.
/// Called on a scheduler worker thread; must be `Send + Sync`.
/// Arguments: kind name (resolved by the dispatcher so sinks don't
/// need a reverse lookup), the sending mailbox's registered name if
/// the mail came from a component (`None` for substrate-core pushes
/// with no sending mailbox, per ADR-0011), the originating Claude
/// session token for hub-inbound mail (or `SessionToken::NIL` for
/// substrate-local mail) per ADR-0008's reply-to-sender primitive,
/// payload bytes, and the kind-implied count.
pub type SinkHandler =
    Arc<dyn Fn(&str, Option<&str>, SessionToken, &[u8], u32) + Send + Sync + 'static>;

/// What a given mailbox actually is. The registry records this so the
/// scheduler can dispatch appropriately without a per-mail type check.
/// `Clone` so readers can pull the entry out from under the `RwLock`
/// guard without holding it for the duration of the handler call.
#[derive(Clone)]
pub enum MailboxEntry {
    /// Mail goes to a WASM component's `receive` function on a worker.
    Component,
    /// Mail is handled inline by a substrate-native closure.
    Sink(SinkHandler),
}

pub struct Registry {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    by_name: HashMap<String, MailboxId>,
    entries: Vec<MailboxEntry>,
    /// Parallel index: `mailbox_names[id]` is the name the mailbox was
    /// registered with. Enables the `MailboxId` → name reverse lookup
    /// used to stamp `origin` on observation mail (ADR-0011).
    mailbox_names: Vec<String>,
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
            inner: RwLock::new(Inner::default()),
        }
    }

    fn insert(&self, name: impl Into<String>, entry: MailboxEntry) -> MailboxId {
        let name = name.into();
        let mut inner = self.inner.write().unwrap();
        if inner.by_name.contains_key(&name) {
            panic!("mailbox name already registered: {name}");
        }
        let id = MailboxId(inner.entries.len() as u32);
        inner.entries.push(entry);
        inner.mailbox_names.push(name.clone());
        inner.by_name.insert(name, id);
        id
    }

    /// Register a WASM component under `name`. The returned `MailboxId`
    /// is handed to the scheduler alongside the component's `Actor`.
    pub fn register_component(&self, name: impl Into<String>) -> MailboxId {
        self.insert(name, MailboxEntry::Component)
    }

    /// Register a substrate-owned sink. Mail to this mailbox is handled
    /// inline on the thread that delivered it (or on the host-function
    /// caller thread if a component sent it).
    pub fn register_sink(&self, name: impl Into<String>, handler: SinkHandler) -> MailboxId {
        self.insert(name, MailboxEntry::Sink(handler))
    }

    pub fn lookup(&self, name: &str) -> Option<MailboxId> {
        self.inner.read().unwrap().by_name.get(name).copied()
    }

    /// Fetch the entry for a mailbox id. Returns an owned clone so the
    /// caller can drop the internal lock before invoking a sink handler
    /// (avoids holding the registry lock across arbitrary user code).
    pub fn entry(&self, id: MailboxId) -> Option<MailboxEntry> {
        self.inner
            .read()
            .unwrap()
            .entries
            .get(id.0 as usize)
            .cloned()
    }

    /// Reverse of `lookup`: name for a given mailbox id, or `None` if
    /// the id is out of range. Used by the sink dispatch path to stamp
    /// `origin` on observation mail (ADR-0011).
    pub fn mailbox_name(&self, id: MailboxId) -> Option<String> {
        self.inner
            .read()
            .unwrap()
            .mailbox_names
            .get(id.0 as usize)
            .cloned()
    }

    /// Register a mail kind by name. Idempotent — re-registering a name
    /// returns the id it was first assigned. Ids are dense and assigned
    /// in insertion order, per ADR-0005's kind-name registry.
    pub fn register_kind(&self, name: impl Into<String>) -> u32 {
        let name = name.into();
        let mut inner = self.inner.write().unwrap();
        if let Some(&id) = inner.kind_by_name.get(&name) {
            return id;
        }
        let id = inner.kind_names.len() as u32;
        inner.kind_names.push(name.clone());
        inner.kind_by_name.insert(name, id);
        id
    }

    pub fn kind_id(&self, name: &str) -> Option<u32> {
        self.inner.read().unwrap().kind_by_name.get(name).copied()
    }

    /// Reverse of `kind_id`: name for a given id, or `None` if the id
    /// is out of range. Used by the scheduler to hand sink handlers a
    /// kind name without them keeping their own map.
    pub fn kind_name(&self, id: u32) -> Option<String> {
        self.inner
            .read()
            .unwrap()
            .kind_names
            .get(id as usize)
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().entries.is_empty()
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
        let r = Registry::new();
        let id = r.register_component("physics");
        assert_eq!(id, MailboxId(0));
        assert_eq!(r.lookup("physics"), Some(id));
        assert!(matches!(r.entry(id), Some(MailboxEntry::Component)));
    }

    #[test]
    fn sink_handler_runs_on_call() {
        let r = Registry::new();
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = Arc::clone(&counter);
        let id = r.register_sink(
            "heartbeat",
            Arc::new(move |_kind, _origin, _sender, _bytes, count| {
                c2.fetch_add(count, Ordering::SeqCst);
            }),
        );
        let Some(MailboxEntry::Sink(h)) = r.entry(id) else {
            panic!("expected sink")
        };
        h("aether.tick", None, SessionToken::NIL, &[], 7);
        h("aether.tick", Some("physics"), SessionToken::NIL, &[], 3);
        assert_eq!(counter.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn mailbox_ids_are_dense_and_sequential() {
        let r = Registry::new();
        let a = r.register_component("a");
        let b = r.register_sink("b", Arc::new(|_, _, _, _, _| {}));
        let c = r.register_component("c");
        assert_eq!(a, MailboxId(0));
        assert_eq!(b, MailboxId(1));
        assert_eq!(c, MailboxId(2));
        assert_eq!(r.len(), 3);
    }

    #[test]
    #[should_panic(expected = "mailbox name already registered")]
    fn duplicate_name_panics() {
        let r = Registry::new();
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
    fn mailbox_name_reverse_lookup() {
        let r = Registry::new();
        let a = r.register_component("physics");
        let b = r.register_sink("hub.claude.broadcast", Arc::new(|_, _, _, _, _| {}));
        assert_eq!(r.mailbox_name(a).as_deref(), Some("physics"));
        assert_eq!(r.mailbox_name(b).as_deref(), Some("hub.claude.broadcast"));
        assert!(r.mailbox_name(MailboxId(999)).is_none());
    }

    #[test]
    fn kind_ids_are_dense_and_sequential() {
        let r = Registry::new();
        let a = r.register_kind("aether.tick");
        let b = r.register_kind("aether.key");
        let c = r.register_kind("hello.npc_health");
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(c, 2);
    }

    #[test]
    fn kind_registration_is_idempotent() {
        let r = Registry::new();
        let first = r.register_kind("aether.tick");
        let second = r.register_kind("aether.tick");
        assert_eq!(first, second);
        assert_eq!(r.register_kind("aether.key"), 1);
    }

    #[test]
    fn kind_id_lookup() {
        let r = Registry::new();
        let id = r.register_kind("aether.tick");
        assert_eq!(r.kind_id("aether.tick"), Some(id));
        assert!(r.kind_id("absent").is_none());
    }

    #[test]
    fn kind_name_reverse_lookup() {
        let r = Registry::new();
        let a = r.register_kind("aether.tick");
        let b = r.register_kind("aether.key");
        assert_eq!(r.kind_name(a).as_deref(), Some("aether.tick"));
        assert_eq!(r.kind_name(b).as_deref(), Some("aether.key"));
        assert!(r.kind_name(999).is_none());
    }

    #[test]
    fn registration_through_shared_arc() {
        // Interior mutability means Arc<Registry> can register after
        // it's already been shared — the dispatch path today never
        // exercises this, but PR 2+ will when `load_component` adds
        // mailboxes and kinds from a handler that holds an Arc.
        let r = Arc::new(Registry::new());
        let r2 = Arc::clone(&r);
        let id = r2.register_component("late");
        assert_eq!(r.lookup("late"), Some(id));
        assert_eq!(r.register_kind("aether.late"), 0);
    }
}
