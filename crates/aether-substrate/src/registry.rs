// Name registries. Two tables: mailboxes (MailboxId → name + entry,
// ids derived from name via ADR-0029's stable hash) and kinds (name →
// u32 kind id, per ADR-0005 — still sequentially assigned). The
// registry uses interior mutability (`RwLock`) so mailboxes and kinds
// can be added at runtime — ADR-0010's runtime component loading
// mutates both tables after an `Arc<Registry>` has already been shared
// with the scheduler and hub client. Reads take a shared lock and are
// cheap; writes are rare (boot + load/replace/drop).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use aether_hub_protocol::{KindDescriptor, SchemaType, SessionToken};

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
    /// Mailbox has been explicitly dropped (ADR-0010). Mail addressed
    /// to a `Dropped` slot is discarded by the scheduler / ctx dispatch
    /// until the same name is re-registered, at which point the slot
    /// transitions back to `Component` under the same id (ADR-0029 ids
    /// are a function of name, so they're stable across drop/reload).
    Dropped,
}

pub struct Registry {
    inner: RwLock<Inner>,
}

/// One mailbox's bookkeeping. Grouped so a single lookup hits name,
/// entry, and any future per-mailbox fields together.
struct Mailbox {
    name: String,
    entry: MailboxEntry,
}

#[derive(Default)]
struct Inner {
    /// Sparse, keyed on the deterministic `MailboxId` (ADR-0029).
    /// Registration inserts; `drop_mailbox` transitions the entry to
    /// `Dropped` so the id stays addressable until re-registered.
    mailboxes: HashMap<MailboxId, Mailbox>,
    kind_by_name: HashMap<String, u32>,
    /// Parallel index: `kind_names[id]` is the canonical name the kind
    /// was first registered with. Kept in sync with `kind_by_name` so
    /// `kind_name(id)` is O(1); used by `SinkHandler` dispatch to hand
    /// sinks the name without forcing them to keep their own map.
    kind_names: Vec<String>,
    /// Parallel index: `kind_descriptors[id]` is the descriptor the
    /// kind was first registered with. `register_kind` (name-only)
    /// defaults to `Opaque`; ADR-0010's runtime loader supplies a real
    /// descriptor via `register_kind_with_descriptor` and rejects
    /// conflicts against this stored copy.
    kind_descriptors: Vec<KindDescriptor>,
}

/// Rejected-load error returned when a runtime kind registration
/// names an existing kind but supplies a different descriptor than the
/// one first seen. Per ADR-0010, the load fails rather than silently
/// reinterpreting; agents rename, evolve the existing descriptor, or
/// restart the substrate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindConflict {
    pub name: String,
    pub existing: SchemaType,
    pub requested: SchemaType,
}

impl fmt::Display for KindConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "kind {:?} already registered with a different encoding (existing={:?}, requested={:?})",
            self.name, self.existing, self.requested
        )
    }
}

impl std::error::Error for KindConflict {}

/// A runtime mailbox registration lost to name collision. Returned
/// from `try_register_component` (ADR-0010) so the load handler can
/// reply with an error instead of panicking. The init path that
/// registers hard-coded mailbox names still uses `register_component`
/// and panics — collisions there are bugs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameConflict {
    pub name: String,
}

impl fmt::Display for NameConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mailbox name {:?} already registered", self.name)
    }
}

impl std::error::Error for NameConflict {}

/// Reasons `Registry::drop_mailbox` can refuse. Distinct from the
/// post-drop dispatch log, which the scheduler handles independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropError {
    UnknownId(MailboxId),
    NotComponent { id: MailboxId, kind: &'static str },
    AlreadyDropped(MailboxId),
}

impl fmt::Display for DropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DropError::UnknownId(id) => write!(f, "unknown mailbox id {:?}", id),
            DropError::NotComponent { id, kind } => {
                write!(f, "mailbox {:?} is a {kind}, not a component", id)
            }
            DropError::AlreadyDropped(id) => write!(f, "mailbox {:?} already dropped", id),
        }
    }
}

impl std::error::Error for DropError {}

impl Registry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
        }
    }

    /// Insert a mailbox, allocating its id from the name hash (ADR-0029).
    /// On a `Dropped` entry at the same id (same name re-registered
    /// after a drop), the entry transitions back to live. Any other
    /// occupied entry is a collision.
    fn insert(&self, name: String, entry: MailboxEntry) -> Result<MailboxId, NameConflict> {
        let id = MailboxId::from_name(&name);
        if id == MailboxId::NONE {
            // Practically impossible at 64 bits, but the sentinel is
            // reserved and silently shadowing it would break
            // Option<MailboxId> semantics for the sender path.
            return Err(NameConflict { name });
        }
        let mut inner = self.inner.write().unwrap();
        match inner.mailboxes.get_mut(&id) {
            Some(slot) if matches!(slot.entry, MailboxEntry::Dropped) && slot.name == name => {
                slot.entry = entry;
                Ok(id)
            }
            Some(_) => Err(NameConflict { name }),
            None => {
                inner.mailboxes.insert(id, Mailbox { name, entry });
                Ok(id)
            }
        }
    }

    /// Register a WASM component under `name`. Panics on a name
    /// collision — callers that cannot assume unique names (e.g.
    /// ADR-0010's load handler, which accepts names from an agent)
    /// should use `try_register_component` instead.
    pub fn register_component(&self, name: impl Into<String>) -> MailboxId {
        let name = name.into();
        match self.insert(name.clone(), MailboxEntry::Component) {
            Ok(id) => id,
            Err(_) => panic!("mailbox name already registered: {name}"),
        }
    }

    /// Non-panicking variant of `register_component` for runtime
    /// registrations. Returns `NameConflict` if the name is already
    /// bound to a live mailbox (or collides with a different name at
    /// the same hash — astronomically unlikely); otherwise derives the
    /// id from the name and records the component entry.
    pub fn try_register_component(
        &self,
        name: impl Into<String>,
    ) -> Result<MailboxId, NameConflict> {
        self.insert(name.into(), MailboxEntry::Component)
    }

    /// Invalidate a component mailbox (ADR-0010). Transitions the entry
    /// to `Dropped` so dispatch-path readers can distinguish an
    /// intentional drop from an unknown id; the id itself (a function
    /// of the name per ADR-0029) stays addressable and a subsequent
    /// `try_register_component` with the same name reuses it. Returns
    /// the released name on success. Refuses to drop `Sink` entries —
    /// those are substrate-owned and outlive the control plane.
    pub fn drop_mailbox(&self, id: MailboxId) -> Result<String, DropError> {
        let mut inner = self.inner.write().unwrap();
        let Some(slot) = inner.mailboxes.get_mut(&id) else {
            return Err(DropError::UnknownId(id));
        };
        match slot.entry {
            MailboxEntry::Component => {}
            MailboxEntry::Sink(_) => {
                return Err(DropError::NotComponent { id, kind: "sink" });
            }
            MailboxEntry::Dropped => return Err(DropError::AlreadyDropped(id)),
        }
        slot.entry = MailboxEntry::Dropped;
        Ok(slot.name.clone())
    }

    /// Register a substrate-owned sink. Mail to this mailbox is handled
    /// inline on the thread that delivered it (or on the host-function
    /// caller thread if a component sent it). Panics on a name
    /// collision — sinks are substrate-internal names, collisions are
    /// bugs.
    pub fn register_sink(&self, name: impl Into<String>, handler: SinkHandler) -> MailboxId {
        let name = name.into();
        match self.insert(name.clone(), MailboxEntry::Sink(handler)) {
            Ok(id) => id,
            Err(_) => panic!("mailbox name already registered: {name}"),
        }
    }

    /// Does a live (non-`Dropped`) mailbox exist under `name`? Returns
    /// its id if so. The id itself is deterministic (ADR-0029) —
    /// callers that just want the id without a liveness check can use
    /// `MailboxId::from_name` directly.
    pub fn lookup(&self, name: &str) -> Option<MailboxId> {
        let id = MailboxId::from_name(name);
        let inner = self.inner.read().unwrap();
        match inner.mailboxes.get(&id) {
            Some(slot) if slot.name == name && !matches!(slot.entry, MailboxEntry::Dropped) => {
                Some(id)
            }
            _ => None,
        }
    }

    /// Fetch the entry for a mailbox id. Returns an owned clone so the
    /// caller can drop the internal lock before invoking a sink handler
    /// (avoids holding the registry lock across arbitrary user code).
    pub fn entry(&self, id: MailboxId) -> Option<MailboxEntry> {
        self.inner
            .read()
            .unwrap()
            .mailboxes
            .get(&id)
            .map(|m| m.entry.clone())
    }

    /// Reverse of `lookup`: name for a given mailbox id, or `None` if
    /// the id is unknown. Used by the sink dispatch path to stamp
    /// `origin` on observation mail (ADR-0011).
    pub fn mailbox_name(&self, id: MailboxId) -> Option<String> {
        self.inner
            .read()
            .unwrap()
            .mailboxes
            .get(&id)
            .map(|m| m.name.clone())
    }

    /// Register a mail kind by name, defaulting the schema to `Bytes`
    /// (raw byte payload, no agent-encodable structure). Idempotent —
    /// re-registering a name returns the id it was first assigned,
    /// regardless of whether the first call supplied a descriptor.
    /// Kept as a convenience for tests and substrate-internal
    /// registrations that don't need the hub to encode params;
    /// production init should prefer `register_kind_with_descriptor`
    /// so the descriptor stored here matches the type definition.
    pub fn register_kind(&self, name: impl Into<String>) -> u32 {
        let name = name.into();
        let descriptor = KindDescriptor {
            name: name.clone(),
            schema: SchemaType::Bytes,
        };
        // Name-only registration never conflicts: if the name is new
        // we store the default schema; if it exists we return the
        // existing id and leave the stored descriptor untouched.
        self.register_kind_internal(name, descriptor, /*reject_conflict=*/ false)
            .expect("Bytes default cannot produce a conflict")
    }

    /// Register a mail kind along with the descriptor the hub will
    /// use to encode agent-supplied params (ADR-0007). Per ADR-0010:
    ///
    /// - Fresh name → assign a new id, store the descriptor.
    /// - Existing name with identical descriptor → return the id.
    /// - Existing name with a different descriptor → `KindConflict`.
    ///
    /// Used by substrate boot (to agree with `descriptors::all()`) and
    /// by the future `load_component` handler when a runtime-loaded
    /// component brings its own kind vocabulary.
    pub fn register_kind_with_descriptor(
        &self,
        descriptor: KindDescriptor,
    ) -> Result<u32, KindConflict> {
        let name = descriptor.name.clone();
        self.register_kind_internal(name, descriptor, /*reject_conflict=*/ true)
    }

    fn register_kind_internal(
        &self,
        name: String,
        descriptor: KindDescriptor,
        reject_conflict: bool,
    ) -> Result<u32, KindConflict> {
        let mut inner = self.inner.write().unwrap();
        if let Some(&id) = inner.kind_by_name.get(&name) {
            let existing = &inner.kind_descriptors[id as usize];
            if reject_conflict && existing.schema != descriptor.schema {
                return Err(KindConflict {
                    name,
                    existing: existing.schema.clone(),
                    requested: descriptor.schema,
                });
            }
            return Ok(id);
        }
        let id = inner.kind_names.len() as u32;
        inner.kind_names.push(name.clone());
        inner.kind_descriptors.push(descriptor);
        inner.kind_by_name.insert(name, id);
        Ok(id)
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

    /// The descriptor stored for a given kind id, or `None` if the id
    /// is out of range. Returned as an owned clone so callers don't
    /// hold the read lock while inspecting the encoding.
    pub fn kind_descriptor(&self, id: u32) -> Option<KindDescriptor> {
        self.inner
            .read()
            .unwrap()
            .kind_descriptors
            .get(id as usize)
            .cloned()
    }

    /// Snapshot of every kind descriptor currently registered, in id
    /// order. Used by the control plane to ship an authoritative view
    /// to the hub after a runtime load or replace (ADR-0010 §4).
    pub fn list_kind_descriptors(&self) -> Vec<KindDescriptor> {
        self.inner.read().unwrap().kind_descriptors.clone()
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap().mailboxes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().mailboxes.is_empty()
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
        assert_eq!(id, MailboxId::from_name("physics"));
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
    fn mailbox_ids_are_name_derived() {
        let r = Registry::new();
        let a = r.register_component("a");
        let b = r.register_sink("b", Arc::new(|_, _, _, _, _| {}));
        let c = r.register_component("c");
        assert_eq!(a, MailboxId::from_name("a"));
        assert_eq!(b, MailboxId::from_name("b"));
        assert_eq!(c, MailboxId::from_name("c"));
        // All three distinct names produce distinct ids.
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
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

    fn unit_desc(name: &str) -> KindDescriptor {
        KindDescriptor {
            name: name.to_string(),
            schema: SchemaType::Unit,
        }
    }

    fn cast_struct_desc(name: &str) -> KindDescriptor {
        use aether_hub_protocol::{NamedField, Primitive};
        KindDescriptor {
            name: name.to_string(),
            schema: SchemaType::Struct {
                repr_c: true,
                fields: vec![NamedField {
                    name: "x".to_string(),
                    ty: SchemaType::Scalar(Primitive::U32),
                }],
            },
        }
    }

    #[test]
    fn register_kind_with_descriptor_stores_schema() {
        let r = Registry::new();
        let id = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("fresh name");
        let stored = r.kind_descriptor(id).expect("descriptor present");
        assert_eq!(stored.schema, cast_struct_desc("aether.foo").schema);
    }

    #[test]
    fn register_kind_with_descriptor_is_idempotent_on_match() {
        let r = Registry::new();
        let first = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("first");
        let second = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("same schema should succeed");
        assert_eq!(first, second);
    }

    #[test]
    fn register_kind_with_descriptor_rejects_conflict() {
        let r = Registry::new();
        r.register_kind_with_descriptor(unit_desc("aether.foo"))
            .expect("first");
        let err = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect_err("different schema should conflict");
        assert_eq!(err.name, "aether.foo");
        assert_eq!(err.existing, SchemaType::Unit);
        assert!(matches!(err.requested, SchemaType::Struct { .. }));
    }

    #[test]
    fn register_kind_defaults_to_bytes() {
        let r = Registry::new();
        let id = r.register_kind("aether.bar");
        let stored = r.kind_descriptor(id).expect("descriptor present");
        assert_eq!(stored.schema, SchemaType::Bytes);
    }

    #[test]
    fn name_only_register_after_with_descriptor_preserves_stored_schema() {
        // The name-only path must not clobber a real descriptor that
        // was recorded first — tests frequently call `register_kind`
        // after main.rs has already registered via
        // `register_kind_with_descriptor`.
        let r = Registry::new();
        r.register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("first");
        let _ = r.register_kind("aether.foo");
        let stored = r.kind_descriptor(0).expect("descriptor present");
        assert!(matches!(stored.schema, SchemaType::Struct { .. }));
    }

    #[test]
    fn try_register_component_is_non_panicking_on_collision() {
        let r = Registry::new();
        let first = r.try_register_component("loaded").expect("fresh name");
        let err = r
            .try_register_component("loaded")
            .expect_err("collision must not panic");
        assert_eq!(err.name, "loaded");
        assert_eq!(r.lookup("loaded"), Some(first));
        // Entries count unchanged after the failed second attempt.
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn drop_mailbox_frees_name_and_marks_entry_dropped() {
        let r = Registry::new();
        let id = r.try_register_component("loaded").unwrap();
        let name = r.drop_mailbox(id).expect("drop");
        assert_eq!(name, "loaded");
        assert!(r.lookup("loaded").is_none(), "name should be reusable");
        assert!(
            matches!(r.entry(id), Some(MailboxEntry::Dropped)),
            "entry must mark id as dropped"
        );
        // Under ADR-0029 the id is a function of the name, so a
        // re-register produces the *same* id and flips the entry back
        // to `Component`.
        let reloaded = r.try_register_component("loaded").unwrap();
        assert_eq!(reloaded, id);
        assert_eq!(r.lookup("loaded"), Some(reloaded));
        assert!(matches!(r.entry(reloaded), Some(MailboxEntry::Component)));
    }

    #[test]
    fn drop_mailbox_rejects_sink_and_unknown_and_repeat() {
        let r = Registry::new();
        let sink = r.register_sink("heartbeat", Arc::new(|_, _, _, _, _| {}));
        assert!(matches!(
            r.drop_mailbox(sink),
            Err(DropError::NotComponent { .. })
        ));
        assert!(matches!(
            r.drop_mailbox(MailboxId(999)),
            Err(DropError::UnknownId(_))
        ));
        let c = r.try_register_component("x").unwrap();
        r.drop_mailbox(c).unwrap();
        assert!(matches!(
            r.drop_mailbox(c),
            Err(DropError::AlreadyDropped(_))
        ));
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
