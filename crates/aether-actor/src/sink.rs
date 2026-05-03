//! Typed addressing — `KindId<K>`, `Sink<K, T>`, and the const
//! resolvers. `Sink` is generic over the transport so `send` /
//! `send_many` dispatch through the consumer crate's `MailTransport`
//! impl. The sink itself stores no transport state; the trait is
//! purely associated functions.

use core::marker::PhantomData;

use aether_data::{Kind, mailbox_id_from_name};

use crate::transport::MailTransport;

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
    pub const fn __new(raw: u64) -> Self {
        KindId {
            raw,
            _k: PhantomData,
        }
    }

    /// The raw kind id the substrate assigned. Exposed for hand-rolled
    /// receive shims that `match` on the inbound `kind: u64` parameter.
    pub fn raw(self) -> u64 {
        self.raw
    }

    /// Returns `true` if `raw` is the id the substrate assigned to `K`.
    /// Convenience over `kind_id.raw() == raw`.
    pub fn matches(self, raw: u64) -> bool {
        self.raw == raw
    }
}

/// Phantom-typed send target. Wraps a mailbox id plus the kind id that
/// the sink accepts. `Sink<DrawTriangle, T>` can only `send` a
/// `&DrawTriangle` or `&[DrawTriangle]` — the kind is fixed at
/// resolution time.
///
/// Built via `resolve_sink::<K, T>(name)` during init. The `T`
/// parameter selects the transport — it's `WasmTransport` inside a
/// guest cdylib (via the `aether-component::Sink<K>` 1-arg alias) and
/// will be `NativeTransport` inside a native capability when ADR-0074
/// Phase 2 lands.
pub struct Sink<K: Kind, T: MailTransport> {
    mailbox: u64,
    kind: u64,
    _k: PhantomData<fn() -> K>,
    _t: PhantomData<fn() -> T>,
}

impl<K: Kind, T: MailTransport> Copy for Sink<K, T> {}
impl<K: Kind, T: MailTransport> Clone for Sink<K, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K: Kind, T: MailTransport> Sink<K, T> {
    /// Not part of the public API; the const `resolve_sink::<K, T>`
    /// builder goes through here so the fields stay private to the SDK.
    #[doc(hidden)]
    pub const fn __new(mailbox: u64, kind: u64) -> Self {
        Sink {
            mailbox,
            kind,
            _k: PhantomData,
            _t: PhantomData,
        }
    }

    /// Raw mailbox id. Exposed for components that need to pass the
    /// id to a host fn not yet wrapped by the SDK.
    pub fn mailbox(self) -> u64 {
        self.mailbox
    }

    /// Raw kind id. Exposed for the same reason as `mailbox`.
    pub fn kind(self) -> u64 {
        self.kind
    }
}

impl<K: Kind, T: MailTransport> Sink<K, T> {
    /// Send a single typed payload. The substrate's `count` field is 1.
    ///
    /// `transport` is the actor-bound `MailTransport` instance — the
    /// `&self`-receiver design means each send call carries an
    /// explicit transport reference, so the actor's identity (and per-
    /// actor state like the correlation counter on `NativeTransport`)
    /// is type-system-tracked rather than hidden in a thread-local.
    /// `WasmTransport` is a ZST, so passing `&WASM_TRANSPORT` from
    /// `aether-component` is free; `NativeTransport` rides on the
    /// capability that owns it.
    ///
    /// Issue #240: routes through `Kind::encode_into_bytes`, which the
    /// derive specializes to either a bytemuck cast or a postcard
    /// encode based on whether the type carries `#[repr(C)]`. One call
    /// site for both wire shapes — the wire choice is the kind's, not
    /// the call's.
    pub fn send(self, transport: &T, payload: &K) {
        let bytes = payload.encode_into_bytes();
        transport.send_mail(self.mailbox, self.kind, &bytes, 1);
    }
}

impl<K: Kind + bytemuck::NoUninit, T: MailTransport> Sink<K, T> {
    /// Send a slice of typed payloads as a contiguous buffer. The
    /// substrate's `count` field is `payloads.len()`.
    ///
    /// Cast-only — postcard has no efficient contiguous-batch wire
    /// shape (ADR-0019 §6 fixes the batch wire as raw bytes). A
    /// component that wants to fan out N postcard payloads calls
    /// `send` in a loop.
    pub fn send_many(self, transport: &T, payloads: &[K]) {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        transport.send_mail(self.mailbox, self.kind, bytes, payloads.len() as u32);
    }
}

/// Resolve a kind, producing a typed id from the `const ID` the derive
/// emits on the `Kind` impl. ADR-0030 Phase 2 made kind ids a pure
/// function of `(name, schema)` at compile time — no host-fn round
/// trip, no "kind not registered" failure mode at the guest boundary.
/// The substrate and guest compute the same id independently; a
/// mismatch means one side was compiled against a different schema
/// revision, and that surfaces as "kind not found" on the first mail.
pub const fn resolve<K: Kind>() -> KindId<K> {
    KindId::__new(K::ID.0)
}

/// Bind a mailbox name to kind `K`, producing a typed `Sink<K, T>`. The
/// mailbox id is derived from the name client-side (ADR-0029 stable
/// hash) and the kind id is `K::ID` (ADR-0030 Phase 2). No host-fn
/// round trip, no requirement that the target mailbox or kind already
/// exist on the substrate side at init time.
pub const fn resolve_sink<K: Kind, T: MailTransport>(mailbox_name: &str) -> Sink<K, T> {
    Sink::__new(mailbox_id_from_name(mailbox_name).0, K::ID.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-rolled Kind with a stable test sentinel id — distinct
    /// from the schema-hashed ids real types get from the derive.
    struct FakeKind;
    impl Kind for FakeKind {
        const NAME: &'static str = "test.fake";
        const ID: ::aether_data::KindId = ::aether_data::KindId(0xDEAD_BEEF_0001_0001);
    }

    /// Stub transport for the sink-accessor tests — `send` would
    /// invoke `T::send_mail` which we don't exercise here. Lives next
    /// to the tests that need it; not a public surface.
    struct NoopTransport;
    impl MailTransport for NoopTransport {
        fn send_mail(&self, _: u64, _: u64, _: &[u8], _: u32) -> u32 {
            0
        }
        fn reply_mail(&self, _: u32, _: u64, _: &[u8], _: u32) -> u32 {
            0
        }
        fn save_state(&self, _: u32, _: &[u8]) -> u32 {
            0
        }
        fn wait_reply(&self, _: u64, _: &mut [u8], _: u32, _: u64) -> i32 {
            -1
        }
        fn prev_correlation(&self) -> u64 {
            0
        }
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

    #[test]
    fn sink_accessors() {
        let s: Sink<FakeKind, NoopTransport> = Sink::__new(3u64, 11);
        assert_eq!(s.mailbox(), 3u64);
        assert_eq!(s.kind(), 11);
    }
}
