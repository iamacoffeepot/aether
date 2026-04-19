// ADR-0027 typelist machinery. The user declares `type Kinds = (...)`
// on their `Component` impl; the SDK walks that list at init time,
// calls `resolve_kind` for each `K::NAME`, and stashes the resulting
// `(TypeId, raw_id)` pairs in a per-component `KindTable`. Receive-time
// helpers (`Mail::is::<K>()`, `Mail::decode_typed::<K>()`) consult the
// table by `TypeId::of::<K>()` — no `KindId<K>` field on `Self` needed.

use core::any::TypeId;
use core::cell::UnsafeCell;

use aether_mail::Kind;

use crate::{InitCtx, KindId};

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

/// Walks a typelist of `Kind`s and resolves each into a per-component
/// `KindTable`. Implemented for tuples 1..=32 and for `Nil` / `Cons`.
///
/// The trait is sealed via the `Kind: 'static` bound and the limited
/// implementor set; downstream crates do not need to write their own.
pub trait KindList {
    /// Walk the list, calling `resolve_kind` for each `K::NAME`, and
    /// insert `(TypeId::of::<K>(), raw_id)` into `table`. Called by
    /// the `export!` macro's init shim before user `init`.
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
        let id: KindId<H> = ctx.resolve::<H>();
        unsafe {
            table.insert(TypeId::of::<H>(), id.raw());
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
                    let id: KindId<$name> = ctx.resolve::<$name>();
                    unsafe { table.insert(TypeId::of::<$name>(), id.raw()); }
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

/// Per-component cache mapping `TypeId::of::<K>()` to the `raw` kind id
/// the substrate handed back at `resolve_kind(K::NAME)`. Sized at
/// `MAX_KINDS` and uses linear scan — N is small and bounded, so a hash
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
    raw: u32,
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
    pub unsafe fn insert(&self, type_id: TypeId, raw: u32) {
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
    pub fn lookup(&self, type_id: TypeId) -> Option<u32> {
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
    }
    struct B;
    impl Kind for B {
        const NAME: &'static str = "test.b";
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
