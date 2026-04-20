// ADR-0027 typelist machinery. The user declares `type Kinds = (...)`
// on their `Component` impl; the SDK walks that list at init time,
// reads each `K::ID` (the const schema-hashed id per ADR-0030 Phase 2),
// and stashes the resulting `(TypeId, K::ID)` pairs in a per-component
// `KindTable`. Receive-time helpers (`Mail::is::<K>()`,
// `Mail::decode_typed::<K>()`) consult the table by
// `TypeId::of::<K>()` — no `KindId<K>` field on `Self` needed.
//
// The walker also emits an `aether.control.subscribe_input` mail for
// each `K::IS_INPUT` kind, replacing the substrate-side auto-subscribe
// side effect that pre-Phase-2 rode on the `resolve_kind` host fn.

use core::any::TypeId;
use core::cell::UnsafeCell;

use aether_mail::Kind;

use crate::InitCtx;

/// Maximum number of kinds a single component may declare. Sized to
/// match the tuple impl ceiling (`KindList` for tuples is generated
/// 1..=32). Components that exceed it via the `Cons` / `Nil` cons-list
/// path panic at init — see `KindTable::insert`.
pub const MAX_KINDS: usize = 32;

/// SDK-internal: the cons-list head terminator.
///
/// Used in `Cons<H, T>` chains as the final tail when a component's
/// kind dependencies exceed the tuple impl ceiling. Most components
/// should reach for the tuple form (`type Kinds = (Tick, Key, ...)`)
/// instead — `Nil`/`Cons` exists as the unbounded-arity escape hatch.
pub struct Nil;

/// SDK-internal: the cons-list cell.
///
/// `Cons<Tick, Cons<Key, Nil>>` is the cons-list spelling of
/// `(Tick, Key)`. Both spellings implement `KindList` and feed the
/// same `KindTable`.
pub struct Cons<H, T>(core::marker::PhantomData<(H, T)>);

/// Walks a typelist of `Kind`s and populates a per-component
/// `KindTable`. Implemented for tuples 1..=32 and for `Nil` / `Cons`.
///
/// The trait is sealed via the `Kind: 'static` bound and the limited
/// implementor set; downstream crates do not need to write their own.
pub trait KindList {
    /// Insert `(TypeId::of::<K>(), K::ID)` into `table` for every `K`
    /// in the list. ADR-0030 Phase 2: `K::ID` is a compile-time
    /// function of `(name, schema)` — no host-fn call. Called by the
    /// `export!` macro's init shim before user `init`.
    ///
    /// For each `K` where `K::IS_INPUT`, the walker additionally mails
    /// `aether.control.subscribe_input` to add the component's own
    /// mailbox to the stream's subscriber set. This replaces the
    /// substrate-side side effect that pre-Phase-2 fired inside
    /// `resolve_kind_p32` (ADR-0021).
    ///
    /// # Safety
    /// Must be called exactly once per `KindTable`, before any reads
    /// of that table — same single-write/many-read invariant as
    /// `Slot<T>`.
    unsafe fn resolve_all(ctx: &mut InitCtx<'_>, table: &KindTable);
}

impl KindList for Nil {
    unsafe fn resolve_all(_ctx: &mut InitCtx<'_>, _table: &KindTable) {}
}

impl<H, T> KindList for Cons<H, T>
where
    H: Kind + 'static,
    T: KindList,
{
    unsafe fn resolve_all(ctx: &mut InitCtx<'_>, table: &KindTable) {
        unsafe {
            table.insert(TypeId::of::<H>(), H::ID);
            if H::IS_INPUT {
                ctx.subscribe_input::<H>();
            }
            <T as KindList>::resolve_all(ctx, table);
        }
    }
}

// Tuple impls 0..=32. The 0-tuple (`()`) is the default for components
// that declare no kind dependencies — they can still receive mail and
// dispatch via `mail.kind()` directly, just without the type-driven
// helpers.
impl KindList for () {
    unsafe fn resolve_all(_ctx: &mut InitCtx<'_>, _table: &KindTable) {}
}

macro_rules! impl_kindlist_tuple {
    ($($name:ident),+) => {
        impl<$($name),+> KindList for ($($name,)+)
        where
            $($name: Kind + 'static,)+
        {
            unsafe fn resolve_all(ctx: &mut InitCtx<'_>, table: &KindTable) {
                $(
                    unsafe { table.insert(TypeId::of::<$name>(), $name::ID); }
                    if $name::IS_INPUT {
                        ctx.subscribe_input::<$name>();
                    }
                )+
            }
        }
    };
}

impl_kindlist_tuple!(K1);
impl_kindlist_tuple!(K1, K2);
impl_kindlist_tuple!(K1, K2, K3);
impl_kindlist_tuple!(K1, K2, K3, K4);
impl_kindlist_tuple!(K1, K2, K3, K4, K5);
impl_kindlist_tuple!(K1, K2, K3, K4, K5, K6);
impl_kindlist_tuple!(K1, K2, K3, K4, K5, K6, K7);
impl_kindlist_tuple!(K1, K2, K3, K4, K5, K6, K7, K8);
impl_kindlist_tuple!(K1, K2, K3, K4, K5, K6, K7, K8, K9);
impl_kindlist_tuple!(K1, K2, K3, K4, K5, K6, K7, K8, K9, K10);
impl_kindlist_tuple!(K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11);
impl_kindlist_tuple!(K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12);
impl_kindlist_tuple!(K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13);
impl_kindlist_tuple!(K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19, K20
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19, K20, K21
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19, K20, K21,
    K22
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19, K20, K21,
    K22, K23
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19, K20, K21,
    K22, K23, K24
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19, K20, K21,
    K22, K23, K24, K25
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19, K20, K21,
    K22, K23, K24, K25, K26
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19, K20, K21,
    K22, K23, K24, K25, K26, K27
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19, K20, K21,
    K22, K23, K24, K25, K26, K27, K28
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19, K20, K21,
    K22, K23, K24, K25, K26, K27, K28, K29
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19, K20, K21,
    K22, K23, K24, K25, K26, K27, K28, K29, K30
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19, K20, K21,
    K22, K23, K24, K25, K26, K27, K28, K29, K30, K31
);
impl_kindlist_tuple!(
    K1, K2, K3, K4, K5, K6, K7, K8, K9, K10, K11, K12, K13, K14, K15, K16, K17, K18, K19, K20, K21,
    K22, K23, K24, K25, K26, K27, K28, K29, K30, K31, K32
);

/// Per-component cache mapping `TypeId::of::<K>()` to the raw `K::ID`
/// the derive emits on each `Kind` impl. Sized at `MAX_KINDS` and uses
/// linear scan — N is small and bounded, so a hash
/// map would cost more than it saves.
///
/// Single-threaded write-once-read-many: the `export!` macro's init
/// shim calls `KindList::resolve_all` (which calls `insert`) before
/// user `init` runs, and `Mail::is`/`Mail::decode_typed` only ever
/// `lookup`. Same `UnsafeCell` + blanket `Sync` pattern as `Slot<T>`
/// in `lib.rs`; sound under the substrate's serialized-dispatch
/// guarantee (ADR-0010 §5).
pub struct KindTable {
    inner: UnsafeCell<KindTableInner>,
}

struct KindTableInner {
    entries: [Entry; MAX_KINDS],
    len: usize,
}

#[derive(Copy, Clone)]
struct Entry {
    type_id: Option<TypeId>,
    raw: u64,
}

impl KindTable {
    /// Build an empty table. `const` so it can live in a `static`.
    pub const fn new() -> Self {
        KindTable {
            inner: UnsafeCell::new(KindTableInner {
                entries: [Entry {
                    type_id: None,
                    raw: 0,
                }; MAX_KINDS],
                len: 0,
            }),
        }
    }

    /// Insert a `(TypeId, raw)` pair. Panics if the table is full —
    /// MAX_KINDS is high enough that hitting the cap is a sign the
    /// component is too coarse and should be split.
    ///
    /// # Safety
    /// Caller must guarantee no concurrent access. Intended to be
    /// called only from `KindList::resolve_all`, which the `export!`
    /// macro's init shim invokes before any reads.
    pub unsafe fn insert(&self, type_id: TypeId, raw: u64) {
        let inner = unsafe { &mut *self.inner.get() };
        if inner.len >= MAX_KINDS {
            panic!("aether-component: KindTable overflow (>{MAX_KINDS} kinds)");
        }
        inner.entries[inner.len] = Entry {
            type_id: Some(type_id),
            raw,
        };
        inner.len += 1;
    }

    /// Linear-scan lookup. Returns `Some(raw)` if `type_id` was
    /// inserted, `None` otherwise.
    pub fn lookup(&self, type_id: TypeId) -> Option<u64> {
        let inner = unsafe { &*self.inner.get() };
        for i in 0..inner.len {
            let entry = inner.entries[i];
            if entry.type_id == Some(type_id) {
                return Some(entry.raw);
            }
        }
        None
    }

    /// Number of `(TypeId, raw)` pairs currently in the table.
    /// Primarily for tests.
    #[doc(hidden)]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        unsafe { (*self.inner.get()).len }
    }
}

impl Default for KindTable {
    fn default() -> Self {
        KindTable::new()
    }
}

// Same justification as `Slot<T>`: single-threaded WASM guest plus
// serialized FFI entry points means the `UnsafeCell` is touched from
// one thread at a time. The `Sync` impl unlocks `static __AETHER_KINDS:
// KindTable`.
unsafe impl Sync for KindTable {}

#[cfg(test)]
mod tests {
    use super::*;

    struct A;
    impl Kind for A {
        const NAME: &'static str = "test.a";
        const ID: u64 = aether_mail::mailbox_id_from_name(Self::NAME);
    }
    struct B;
    impl Kind for B {
        const NAME: &'static str = "test.b";
        const ID: u64 = aether_mail::mailbox_id_from_name(Self::NAME);
    }

    #[test]
    fn empty_table_lookups_return_none() {
        let table = KindTable::new();
        assert_eq!(table.lookup(TypeId::of::<A>()), None);
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn insert_then_lookup_roundtrip() {
        let table = KindTable::new();
        unsafe {
            table.insert(TypeId::of::<A>(), 7);
            table.insert(TypeId::of::<B>(), 11);
        }
        assert_eq!(table.lookup(TypeId::of::<A>()), Some(7));
        assert_eq!(table.lookup(TypeId::of::<B>()), Some(11));
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn lookup_unknown_type_returns_none() {
        let table = KindTable::new();
        unsafe {
            table.insert(TypeId::of::<A>(), 7);
        }
        struct Other;
        assert_eq!(table.lookup(TypeId::of::<Other>()), None);
    }
}
