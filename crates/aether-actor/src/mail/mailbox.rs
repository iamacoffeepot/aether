//! Typed addressing — [`KindId`], [`Mailbox`], and the const
//! resolvers.
//!
//! Issue 665 retired the parametric `Mailbox<K, T>` and
//! `ActorMailbox<'_, R, T>` shapes when the `MailTransport` trait that
//! T was bound by retired. [`Mailbox<K>`] is now a pure addressing
//! token: a (mailbox id, kind id) pair that callers thread around but
//! that doesn't carry its own dispatch path. Sends go through each
//! ctx's inherent / trait-provided `send` methods (FFI: bodies call
//! `crate::wasm::bridge::mail::send_mail`; native: bodies hit
//! `NativeBinding`'s inherent `send_mail`).
//!
//! Per-side actor-typed mailboxes ([`crate::wasm::WasmActorMailbox<R>`],
//! `aether_substrate::actor::native::NativeActorMailbox<'a, R>`)
//! replaced the parametric `ActorMailbox<'a, R, T>` for the
//! `ctx.actor::<R>().send(&payload)` chain — they're per-target so
//! each side can dispatch through its own primitive without a shared
//! trait.

use core::marker::PhantomData;

use aether_data::{Kind, mailbox_id_from_name};

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
        Self {
            raw,
            _k: PhantomData,
        }
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

/// Phantom-typed addressing token: a `(mailbox_id, kind_id)` pair
/// bound to send target kind `K` at compile time. Pure data — no
/// inherent send method, no transport coupling. Issue 665 stripped
/// the `T: MailTransport` parameter that previously gated dispatch;
/// sends route through the originating ctx's send methods now.
///
/// Built via `resolve_mailbox::<K>(name)` during init, or constructed
/// inline by hand-rolled callers that compute the mailbox id
/// themselves.
// The `mailbox` field name is intentional: this struct is a typed
// wrapper around the raw mailbox id, and `Mailbox::mailbox()` returns
// it — matching the existing kind/`kind()` pair on the same struct.
#[allow(clippy::struct_field_names)]
pub struct Mailbox<K: Kind> {
    mailbox: u64,
    kind: u64,
    _k: PhantomData<fn() -> K>,
}

impl<K: Kind> Copy for Mailbox<K> {}
impl<K: Kind> Clone for Mailbox<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K: Kind> Mailbox<K> {
    /// Not part of the public API; the const `resolve_mailbox::<K>`
    /// builder goes through here so the fields stay private to the SDK.
    #[doc(hidden)]
    #[must_use]
    pub const fn __new(mailbox: u64, kind: u64) -> Self {
        Self {
            mailbox,
            kind,
            _k: PhantomData,
        }
    }

    /// Raw mailbox id. Exposed for components that need to pass the
    /// id to a host fn not yet wrapped by the SDK.
    #[must_use]
    pub fn mailbox(self) -> u64 {
        self.mailbox
    }

    /// Raw kind id. Exposed for the same reason as `mailbox`.
    #[must_use]
    pub fn kind(self) -> u64 {
        self.kind
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

/// Bind a mailbox name to kind `K`, producing a typed `Mailbox<K>`.
/// The mailbox id is derived from the name client-side (ADR-0029
/// stable hash) and the kind id is `K::ID` (ADR-0030 Phase 2). No
/// host-fn round trip, no requirement that the target mailbox or
/// kind already exist on the substrate side at init time.
// SDK compile-time name→id primitive (the documented `Mailbox<K>` token
// path) — `mailbox_name` is a const, resolved before any runtime carry exists.
#[must_use]
#[allow(clippy::disallowed_methods)]
pub const fn resolve_mailbox<K: Kind>(mailbox_name: &str) -> Mailbox<K> {
    Mailbox::__new(mailbox_id_from_name(mailbox_name).0, K::ID.0)
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
