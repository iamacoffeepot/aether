//! Typed kind ids — [`KindId<K>`] and the const [`resolve`] builder
//! over the `const ID` the `Kind` derive emits. The id is a pure
//! function of the kind's name and schema, so no mailbox or host-fn
//! round trip is involved.

use core::marker::PhantomData;

use aether_data::Kind;

/// Phantom-typed wrapper around a resolved kind id. A `KindId<Tick>`
/// cannot be passed where a `KindId<DrawTriangle>` is expected — the
/// mismatch is a compile error rather than a runtime bad-dispatch.
///
/// Constructed via `resolve::<K>()` during component init. The raw
/// id is retrievable via `.raw()` for comparison against incoming
/// `kind` parameters in a hand-rolled `receive` shim (`Mail::decode`
/// makes the raw-int compare go away for typed handlers).
pub struct KindId<K: Kind> {
    raw: u64,
    _k: PhantomData<fn() -> K>,
}

impl<K: Kind> Copy for KindId<K> {}
impl<K: Kind> Clone for KindId<K> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K: Kind> PartialEq for KindId<K> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl<K: Kind> Eq for KindId<K> {}

impl<K: Kind> KindId<K> {
    /// Not part of the public API; the const `resolve::<K>()` builder
    /// goes through here so the field stays private to the SDK.
    #[doc(hidden)]
    #[must_use]
    pub const fn __new(raw: u64) -> Self {
        Self { raw, _k: PhantomData }
    }

    /// The raw kind id the substrate assigned. Exposed for hand-rolled
    /// receive shims that `match` on the inbound `kind: u64` parameter.
    #[must_use]
    pub fn raw(self) -> u64 {
        self.raw
    }

    /// Returns `true` if `raw` is the id the substrate assigned to `K`.
    /// Convenience over `kind_id.raw() == raw`.
    #[must_use]
    pub fn matches(self, raw: u64) -> bool {
        self.raw == raw
    }
}

/// Resolve a kind, producing a typed id from the `const ID` the derive
/// emits on the `Kind` impl. ADR-0030 Phase 2 made kind ids a pure
/// function of `(name, schema)` at compile time — no host-fn round
/// trip, no "kind not registered" failure mode at the guest boundary.
/// The substrate and guest compute the same id independently; a
/// mismatch means one side was compiled against a different schema
/// revision, and that surfaces as "kind not found" on the first mail.
#[must_use]
pub const fn resolve<K: Kind>() -> KindId<K> {
    KindId::__new(K::ID.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::KindId as DataKindId;

    // `super::*` brings the local generic `KindId<K>` into scope;
    // tests need the raw `aether_data::KindId` newtype for the
    // const-init sentinel so we alias it.

    /// Hand-rolled Kind with a stable test sentinel id — distinct
    /// from the schema-hashed ids real types get from the derive.
    struct FakeKind;
    impl Kind for FakeKind {
        const NAME: &'static str = "test.fake";
        const ID: DataKindId = DataKindId(0xDEAD_BEEF_0001_0001);
    }

    #[test]
    fn kind_id_equality_and_matches() {
        let a: KindId<FakeKind> = KindId::__new(7);
        let b: KindId<FakeKind> = KindId::__new(7);
        let c: KindId<FakeKind> = KindId::__new(8);
        assert!(a == b);
        assert!(a != c);
        assert!(a.matches(7));
        assert!(!a.matches(8));
        assert_eq!(a.raw(), 7);
    }
}
