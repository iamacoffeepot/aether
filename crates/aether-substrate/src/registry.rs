// Name registries. Two tables: mailboxes (MailboxId → name + entry,
// ids derived from name via ADR-0029's stable hash) and kinds (u64
// kind id → name + descriptor, ids derived from (name, schema) via
// ADR-0030 Phase 2's `kind_id_from_parts`). Both id spaces are a pure
// function of declaration-time data — no sequential allocation, no
// registration order dependence. The registry uses interior mutability
// (`RwLock`) so mailboxes and kinds can be added at runtime —
// ADR-0010's runtime component loading mutates both tables after an
// `Arc<Registry>` has already been shared with the scheduler and hub
// client. Reads take a shared lock and are cheap; writes are rare
// (boot + load/replace/drop).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use crate::mail::{KindId, MailboxId, ReplyTo};
use aether_data::canonical::{canonical_kind_bytes, kind_id_from_parts};
use aether_data::{KindDescriptor, SchemaType};

/// Closure invoked when mail is delivered to a chassis-bound mailbox.
/// Called on the caller's thread (or the platform thread for input
/// fan-out); must be `Send + Sync`. Arguments: the kind's id
/// (`K::ID`, ADR-0030 schema hash), the kind's registered name
/// (resolved by the dispatcher for diagnostic logging — handlers
/// that only match on id can ignore it), the sending mailbox's
/// registered name if the mail came from a component (`None` for
/// substrate-core pushes with no sending mailbox, per ADR-0011),
/// the remote reply target of the mail per ADR-0008 / ADR-0037
/// (`Sender::Session` for hub-inbound, `ReplyTo::EngineMailbox` for
/// bubbled-up, `ReplyTo::NONE` for substrate-local), payload bytes,
/// and the kind-implied count.
pub type MailboxHandler =
    Arc<dyn Fn(KindId, &str, Option<&str>, ReplyTo, &[u8], u32) + Send + Sync + 'static>;

/// What a given mailbox actually is. The registry records this so the
/// scheduler can dispatch appropriately without a per-mail type check.
/// `Clone` so readers can pull the entry out from under the `RwLock`
/// guard without holding it for the duration of the handler call.
///
/// Issue 634 Phase 4 retired the dedicated `Component` variant —
/// every loaded wasm component is now a `WasmTrampoline` registered
/// here as a `Closure` like every other actor.
#[derive(Clone)]
pub enum MailboxEntry {
    /// Mail is handled inline by a substrate-native closure (the
    /// actor-as-mailbox wrap installed by `claim_mailbox` /
    /// `register_closure`, including the wasm trampoline's spawn-time
    /// registration).
    Closure(MailboxHandler),
    /// Mailbox has been explicitly dropped (ADR-0010). Mail addressed
    /// to a `Dropped` slot is discarded by the scheduler / ctx dispatch
    /// until the same name is re-registered, at which point the slot
    /// transitions back to `Closure` under the same id (ADR-0029 ids
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

/// One kind's bookkeeping, keyed in the registry on the hashed id.
struct KindSlot {
    name: String,
    descriptor: KindDescriptor,
}

#[derive(Default)]
struct Inner {
    /// Sparse, keyed on the deterministic `MailboxId` (ADR-0029).
    /// Registration inserts; `drop_mailbox` transitions the entry to
    /// `Dropped` so the id stays addressable until re-registered.
    mailboxes: HashMap<MailboxId, Mailbox>,
    /// Sparse, keyed on the `kind_id_from_parts(name, schema)` hash
    /// (ADR-0030 Phase 2). Every descriptor registered with a given
    /// (name, schema) maps to the same id everywhere it's ever
    /// computed — derive-emitted `K::ID`, hub re-derived from
    /// `KindDescriptor`, substrate boot from `descriptors::all()`.
    kinds: HashMap<KindId, KindSlot>,
    /// O(1) name → id reverse lookup. Kept as a parallel map rather
    /// than scanning `kinds` because the dispatch path (reply_mail kind
    /// validation, hub_client inbound-mail name→id) runs on every mail.
    /// Every insert into `kinds` mirrors into `name_index`; every slot
    /// has exactly one entry here.
    name_index: HashMap<String, KindId>,
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
/// from `try_register_closure` (ADR-0010) so a runtime caller can
/// reply with an error instead of panicking. The boot path that
/// registers hard-coded mailbox names still uses `register_closure`
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
    AlreadyDropped(MailboxId),
}

impl fmt::Display for DropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DropError::UnknownId(id) => write!(f, "unknown mailbox id {:?}", id),
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

    /// Invalidate a closure-bound mailbox (ADR-0010). Transitions the
    /// entry to `Dropped` so dispatch-path readers can distinguish an
    /// intentional drop from an unknown id; the id itself (a function
    /// of the name per ADR-0029) stays addressable and a subsequent
    /// `try_register_closure` with the same name reuses it. Returns
    /// the released name on success.
    ///
    /// Issue 634 Phase 4 retired the dedicated `Component` variant,
    /// so this now drops any `Closure`-bound mailbox. Production has
    /// exactly one caller — `WasmTrampoline`'s shutdown path
    /// transitioning its own slot — chassis-cap mailboxes never
    /// route here.
    pub fn drop_mailbox(&self, id: MailboxId) -> Result<String, DropError> {
        let mut inner = self.inner.write().unwrap();
        let Some(slot) = inner.mailboxes.get_mut(&id) else {
            return Err(DropError::UnknownId(id));
        };
        match slot.entry {
            MailboxEntry::Closure(_) => {}
            MailboxEntry::Dropped => return Err(DropError::AlreadyDropped(id)),
        }
        slot.entry = MailboxEntry::Dropped;
        Ok(slot.name.clone())
    }

    /// Register a chassis-bound mailbox handled by `handler`. Mail to
    /// this mailbox runs the closure inline on the delivering thread
    /// (or on the host-function caller thread if a component sent it).
    /// Panics on a name collision — these are substrate-internal
    /// names, collisions are bugs.
    pub fn register_closure(&self, name: impl Into<String>, handler: MailboxHandler) -> MailboxId {
        let name = name.into();
        match self.insert(name.clone(), MailboxEntry::Closure(handler)) {
            Ok(id) => id,
            Err(_) => panic!("mailbox name already registered: {name}"),
        }
    }

    /// Non-panicking variant of `register_closure`. Returns
    /// `NameConflict` on a collision so callers that legitimately
    /// race (ADR-0070 capability boots, where the side-by-side
    /// extraction period puts legacy `register_closure` and a new
    /// capability claim against the same mailbox during the
    /// transition diff) can surface the collision as a typed error
    /// rather than aborting the chassis.
    pub fn try_register_closure(
        &self,
        name: impl Into<String>,
        handler: MailboxHandler,
    ) -> Result<MailboxId, NameConflict> {
        self.insert(name.into(), MailboxEntry::Closure(handler))
    }

    /// Issue 607 Phase 7: fully remove a closure-bound mailbox. Used
    /// in the chassis-boot unwind path when a singleton's `init` fails
    /// after `try_register_closure` claimed the slot — the partial-boot
    /// state must not leak into a later cap's namespace lookup.
    /// Returns `true` if the entry existed and was a `Closure` variant
    /// (and was removed), `false` if the id is unknown or refers to a
    /// non-closure entry. Component entries go through
    /// [`Self::drop_mailbox`] (which transitions to `Dropped` rather
    /// than removing) — the lifecycle difference is intentional:
    /// components can re-register the same id after a drop, closure-
    /// bound mailboxes are torn down on cap teardown and the id can be
    /// freshly recreated.
    pub(crate) fn remove_closure(&self, id: MailboxId) -> bool {
        let mut inner = self.inner.write().unwrap();
        match inner.mailboxes.get(&id) {
            Some(slot) if matches!(slot.entry, MailboxEntry::Closure(_)) => {
                inner.mailboxes.remove(&id);
                true
            }
            _ => false,
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
    /// caller can drop the internal lock before invoking a `Closure`
    /// handler (avoids holding the registry lock across arbitrary user
    /// code).
    pub fn entry(&self, id: MailboxId) -> Option<MailboxEntry> {
        self.inner
            .read()
            .unwrap()
            .mailboxes
            .get(&id)
            .map(|m| m.entry.clone())
    }

    /// Reverse of `lookup`: name for a given mailbox id, or `None` if
    /// the id is unknown. Used by the closure dispatch path to stamp
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
    /// (raw byte payload, no agent-encodable structure). The id is
    /// derived from `(name, SchemaType::Bytes)` — so the name-only path
    /// only collides with a `register_kind_with_descriptor` call that
    /// also uses the `Bytes` schema. Mostly a convenience for tests and
    /// substrate-internal registrations that don't need the hub to
    /// encode params; production init should prefer
    /// `register_kind_with_descriptor` so the descriptor stored here
    /// matches the type definition and the derived id agrees with
    /// `<K as Kind>::ID` on the guest side.
    pub fn register_kind(&self, name: impl Into<String>) -> KindId {
        let name = name.into();
        let descriptor = KindDescriptor {
            name: name.clone(),
            schema: SchemaType::Bytes,
            is_stream: false,
        };
        // A fresh `Bytes` descriptor can only conflict with a prior
        // `Bytes` registration under the same name — in which case the
        // schemas match and the call is idempotent. Not reachable.
        self.register_kind_internal(descriptor, /*reject_conflict=*/ false)
            .expect("Bytes default cannot produce a conflict")
    }

    /// Register a mail kind along with the descriptor the hub will
    /// use to encode agent-supplied params (ADR-0007). Per ADR-0030
    /// Phase 2:
    ///
    /// - Fresh `(name, schema)` hash → insert, return the id.
    /// - Existing id with identical descriptor → return the id
    ///   (idempotent — same kind registered twice, e.g. boot + load).
    /// - Existing id with a different descriptor → `KindConflict`. At
    ///   64-bit hash width this is only reachable via a genuine hash
    ///   collision between two distinct kinds; loud failure rather
    ///   than silent data corruption.
    ///
    /// Used by substrate boot (`descriptors::all()`) and `load_component`.
    pub fn register_kind_with_descriptor(
        &self,
        descriptor: KindDescriptor,
    ) -> Result<KindId, KindConflict> {
        self.register_kind_internal(descriptor, /*reject_conflict=*/ true)
    }

    fn register_kind_internal(
        &self,
        descriptor: KindDescriptor,
        reject_conflict: bool,
    ) -> Result<KindId, KindConflict> {
        let id = KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema));
        let mut inner = self.inner.write().unwrap();
        if let Some(slot) = inner.kinds.get(&id) {
            if reject_conflict
                && canonical_kind_bytes(&slot.descriptor.name, &slot.descriptor.schema)
                    != canonical_kind_bytes(&descriptor.name, &descriptor.schema)
            {
                // Same 64-bit id but distinct canonical bytes — a real
                // hash collision, keep the loud failure. Comparing
                // canonical bytes (not `SchemaType` PartialEq) means
                // nominal-only differences — named fields vs stripped
                // names from a manifest round-trip — are treated as
                // identical, since the canonical form is exactly the
                // structure the id hashes over.
                return Err(KindConflict {
                    name: descriptor.name,
                    existing: slot.descriptor.schema.clone(),
                    requested: descriptor.schema,
                });
            }
            return Ok(id);
        }
        inner.name_index.insert(descriptor.name.clone(), id);
        inner.kinds.insert(
            id,
            KindSlot {
                name: descriptor.name.clone(),
                descriptor,
            },
        );
        Ok(id)
    }

    /// Look up a kind's id by its canonical name. Under hashed ids the
    /// id is a function of `(name, schema)` — so this only finds a
    /// match if `register_kind_with_descriptor` was called with the
    /// exact descriptor the caller is thinking of. Primarily used by
    /// the hub-inbound dispatch path, which needs to convert an
    /// incoming `kind_name` back to the registered id.
    pub fn kind_id(&self, name: &str) -> Option<KindId> {
        self.inner.read().unwrap().name_index.get(name).copied()
    }

    /// Reverse of `kind_id`: name for a given id, or `None` if the id
    /// isn't registered. Used by the dispatch path to hand mailbox
    /// closure handlers a kind name without them keeping their own
    /// map.
    pub fn kind_name(&self, kind: KindId) -> Option<String> {
        self.inner
            .read()
            .unwrap()
            .kinds
            .get(&kind)
            .map(|s| s.name.clone())
    }

    /// The descriptor stored for a given kind id, or `None` if the id
    /// isn't registered. Returned as an owned clone so callers don't
    /// hold the read lock while inspecting the encoding.
    pub fn kind_descriptor(&self, kind: KindId) -> Option<KindDescriptor> {
        self.inner
            .read()
            .unwrap()
            .kinds
            .get(&kind)
            .map(|s| s.descriptor.clone())
    }

    /// Snapshot of every kind descriptor currently registered. Sorted
    /// by name so the hub sees a deterministic ordering (ids are a
    /// hash of declaration-time data, so sorting on id would scramble
    /// unrelated kinds; name order preserves a human-readable grouping).
    /// Used by the control plane to ship an authoritative view to the
    /// hub after a runtime load or replace (ADR-0010 §4).
    pub fn list_kind_descriptors(&self) -> Vec<KindDescriptor> {
        let mut out: Vec<KindDescriptor> = self
            .inner
            .read()
            .unwrap()
            .kinds
            .values()
            .map(|s| s.descriptor.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
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
    fn register_and_lookup_closure_mailbox() {
        let r = Registry::new();
        let id = r.register_closure("physics", Arc::new(|_, _, _, _, _, _| {}));
        assert_eq!(id, MailboxId::from_name("physics"));
        assert_eq!(r.lookup("physics"), Some(id));
        assert!(matches!(r.entry(id), Some(MailboxEntry::Closure(_))));
    }

    #[test]
    fn closure_handler_runs_on_call() {
        let r = Registry::new();
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = Arc::clone(&counter);
        let id = r.register_closure(
            "heartbeat",
            Arc::new(move |_kind_id, _kind, _origin, _sender, _bytes, count| {
                c2.fetch_add(count, Ordering::SeqCst);
            }),
        );
        let Some(MailboxEntry::Closure(h)) = r.entry(id) else {
            panic!("expected closure entry")
        };
        // Test-side id is irrelevant — the handler ignores it.
        h(KindId(0), "aether.tick", None, ReplyTo::NONE, &[], 7);
        h(
            KindId(0),
            "aether.tick",
            Some("physics"),
            ReplyTo::NONE,
            &[],
            3,
        );
        assert_eq!(counter.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn mailbox_ids_are_name_derived() {
        let r = Registry::new();
        let a = r.register_closure("a", Arc::new(|_, _, _, _, _, _| {}));
        let b = r.register_closure("b", Arc::new(|_, _, _, _, _, _| {}));
        let c = r.register_closure("c", Arc::new(|_, _, _, _, _, _| {}));
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
        r.register_closure("x", Arc::new(|_, _, _, _, _, _| {}));
        r.register_closure("x", Arc::new(|_, _, _, _, _, _| {}));
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
        let a = r.register_closure("physics", Arc::new(|_, _, _, _, _, _| {}));
        let b = r.register_closure("hub.claude.broadcast", Arc::new(|_, _, _, _, _, _| {}));
        assert_eq!(r.mailbox_name(a).as_deref(), Some("physics"));
        assert_eq!(r.mailbox_name(b).as_deref(), Some("hub.claude.broadcast"));
        assert!(r.mailbox_name(MailboxId(999)).is_none());
    }

    #[test]
    fn kind_ids_are_derived_from_name_and_schema() {
        let r = Registry::new();
        let a = r.register_kind("aether.tick");
        let b = r.register_kind("aether.key");
        let c = r.register_kind("hello.npc_health");
        // Ids are the fnv1a hash of canonical (name, schema) bytes —
        // distinct names under the same default schema must produce
        // distinct ids, and matching the expected const derivation
        // pins the hash contract with the derive.
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        assert_eq!(
            a,
            KindId(kind_id_from_parts("aether.tick", &SchemaType::Bytes))
        );
    }

    #[test]
    fn kind_registration_is_idempotent() {
        let r = Registry::new();
        let first = r.register_kind("aether.tick");
        let second = r.register_kind("aether.tick");
        assert_eq!(first, second);
        // Different name produces a different id — the id is a pure
        // function of the input, not an allocation order.
        assert_ne!(r.register_kind("aether.key"), first);
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
        assert!(r.kind_name(KindId(999)).is_none());
    }

    fn unit_desc(name: &str) -> KindDescriptor {
        KindDescriptor {
            name: name.to_string(),
            schema: SchemaType::Unit,
            is_stream: false,
        }
    }

    fn cast_struct_desc(name: &str) -> KindDescriptor {
        use aether_data::{NamedField, Primitive};
        KindDescriptor {
            name: name.to_string(),
            schema: SchemaType::Struct {
                repr_c: true,
                fields: vec![NamedField {
                    name: "x".into(),
                    ty: SchemaType::Scalar(Primitive::U32),
                }]
                .into(),
            },
            is_stream: false,
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

    /// The first registration stores the schema with named fields
    /// (e.g. substrate boot via `aether_kinds::descriptors::all()`); a
    /// second registration of the same structural kind with stripped
    /// names (e.g. reconstructed from a component's `aether.kinds`
    /// canonical bytes) must be accepted as idempotent because both
    /// produce the same kind id. This is the path `#[actor]`
    /// consumer-crate retention relies on for cross-crate kinds that
    /// duplicate boot-registered ones.
    #[test]
    fn register_kind_with_descriptor_accepts_nominal_only_differences() {
        use aether_data::{NamedField, Primitive};
        let r = Registry::new();
        let named_id = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("first");

        let unnamed = KindDescriptor {
            name: "aether.foo".into(),
            schema: SchemaType::Struct {
                repr_c: true,
                fields: vec![NamedField {
                    name: "".into(),
                    ty: SchemaType::Scalar(Primitive::U32),
                }]
                .into(),
            },
            is_stream: false,
        };
        let unnamed_id = r
            .register_kind_with_descriptor(unnamed)
            .expect("same canonical bytes = same id = idempotent");
        assert_eq!(named_id, unnamed_id);

        // Named version stays in the stored slot — first writer wins.
        let stored = r.kind_descriptor(named_id).expect("still there");
        if let SchemaType::Struct { fields, .. } = &stored.schema {
            assert_eq!(fields[0].name, "x");
        } else {
            panic!("expected struct schema");
        }
    }

    #[test]
    fn register_kind_with_descriptor_distinct_schemas_take_distinct_ids() {
        // Pre-ADR-0030-Phase-2 behavior was: same name + different
        // schema = `KindConflict`. Under hashed ids the id IS the
        // `(name, schema)` pair, so two schemas under the same name
        // land in two separate slots — conflict is only reachable via
        // a genuine hash collision. Document the post-Phase-2 shape
        // and let the conflict path stay exercised via the
        // `_is_idempotent_on_match` test (same-id reentry).
        let r = Registry::new();
        let unit_id = r
            .register_kind_with_descriptor(unit_desc("aether.foo"))
            .expect("first");
        let struct_id = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("second — different schema, no conflict under hashed ids");
        assert_ne!(unit_id, struct_id);
        assert_eq!(r.kind_descriptor(unit_id).unwrap().schema, SchemaType::Unit);
        assert!(matches!(
            r.kind_descriptor(struct_id).unwrap().schema,
            SchemaType::Struct { .. }
        ));
    }

    #[test]
    fn register_kind_defaults_to_bytes() {
        let r = Registry::new();
        let id = r.register_kind("aether.bar");
        let stored = r.kind_descriptor(id).expect("descriptor present");
        assert_eq!(stored.schema, SchemaType::Bytes);
    }

    #[test]
    fn name_only_and_with_descriptor_resolve_to_distinct_ids() {
        // Under hashed ids the id is a function of (name, schema).
        // The same name registered with two different schemas —
        // `Bytes` (via `register_kind`) and a real struct (via
        // `register_kind_with_descriptor`) — produces two *different*
        // ids, each stored under its own slot. `kind_id(name)` returns
        // whichever id was written to `name_index` most recently; this
        // is a test-only hazard and production callers go through
        // `register_kind_with_descriptor` exclusively.
        let r = Registry::new();
        let real = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("real schema");
        let bytes = r.register_kind("aether.foo");
        assert_ne!(real, bytes);
        assert!(matches!(
            r.kind_descriptor(real).unwrap().schema,
            SchemaType::Struct { .. }
        ));
        assert!(matches!(
            r.kind_descriptor(bytes).unwrap().schema,
            SchemaType::Bytes,
        ));
    }

    #[test]
    fn try_register_closure_is_non_panicking_on_collision() {
        let r = Registry::new();
        let first = r
            .try_register_closure("loaded", Arc::new(|_, _, _, _, _, _| {}))
            .expect("fresh name");
        let err = r
            .try_register_closure("loaded", Arc::new(|_, _, _, _, _, _| {}))
            .expect_err("collision must not panic");
        assert_eq!(err.name, "loaded");
        assert_eq!(r.lookup("loaded"), Some(first));
        // Entries count unchanged after the failed second attempt.
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn drop_mailbox_frees_name_and_marks_entry_dropped() {
        let r = Registry::new();
        let id = r
            .try_register_closure("loaded", Arc::new(|_, _, _, _, _, _| {}))
            .unwrap();
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
        let reloaded = r
            .try_register_closure("loaded", Arc::new(|_, _, _, _, _, _| {}))
            .unwrap();
        assert_eq!(reloaded, id);
        assert_eq!(r.lookup("loaded"), Some(reloaded));
        assert!(matches!(r.entry(reloaded), Some(MailboxEntry::Closure(_))));
    }

    #[test]
    fn drop_mailbox_rejects_unknown_and_repeat() {
        let r = Registry::new();
        assert!(matches!(
            r.drop_mailbox(MailboxId(999)),
            Err(DropError::UnknownId(_))
        ));
        let c = r
            .try_register_closure("x", Arc::new(|_, _, _, _, _, _| {}))
            .unwrap();
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
        let id = r2.register_closure("late", Arc::new(|_, _, _, _, _, _| {}));
        assert_eq!(r.lookup("late"), Some(id));
        let kind_id = r.register_kind("aether.late");
        assert_eq!(
            r.kind_id("aether.late"),
            Some(kind_id),
            "shared Arc registrations are visible through the original handle"
        );
    }
}
