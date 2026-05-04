//! Typed addressing — `KindId<K>`, `Mailbox<K, T>`, and the const
//! resolvers. `Mailbox` is generic over the transport so `send` /
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
/// the sink accepts. `Mailbox<DrawTriangle, T>` can only `send` a
/// `&DrawTriangle` or `&[DrawTriangle]` — the kind is fixed at
/// resolution time.
///
/// Built via `resolve_mailbox::<K, T>(name)` during init. The `T`
/// parameter selects the transport — it's `WasmTransport` inside a
/// guest cdylib (via the `aether-component::Mailbox<K>` 1-arg alias) and
/// will be `NativeTransport` inside a native capability when ADR-0074
/// Phase 2 lands.
pub struct Mailbox<K: Kind, T: MailTransport> {
    mailbox: u64,
    kind: u64,
    _k: PhantomData<fn() -> K>,
    _t: PhantomData<fn() -> T>,
}

impl<K: Kind, T: MailTransport> Copy for Mailbox<K, T> {}
impl<K: Kind, T: MailTransport> Clone for Mailbox<K, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K: Kind, T: MailTransport> Mailbox<K, T> {
    /// Not part of the public API; the const `resolve_mailbox::<K, T>`
    /// builder goes through here so the fields stay private to the SDK.
    #[doc(hidden)]
    pub const fn __new(mailbox: u64, kind: u64) -> Self {
        Mailbox {
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

impl<K: Kind, T: MailTransport> Mailbox<K, T> {
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

impl<K: Kind + bytemuck::NoUninit, T: MailTransport> Mailbox<K, T> {
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

/// Bind a mailbox name to kind `K`, producing a typed `Mailbox<K, T>`. The
/// mailbox id is derived from the name client-side (ADR-0029 stable
/// hash) and the kind id is `K::ID` (ADR-0030 Phase 2). No host-fn
/// round trip, no requirement that the target mailbox or kind already
/// exist on the substrate side at init time.
pub const fn resolve_mailbox<K: Kind, T: MailTransport>(mailbox_name: &str) -> Mailbox<K, T> {
    Mailbox::__new(mailbox_id_from_name(mailbox_name).0, K::ID.0)
}

/// Phantom-typed receiver-actor handle. ADR-0075's actor-typed sender
/// API: `ActorMailbox<R, T>` addresses the mailbox of receiver actor `R`,
/// not a single kind. Lives alongside [`Mailbox<K, T>`] during the
/// migration — Phase 4 retires the kind-typed form and renames this
/// to `Mailbox`.
///
/// Multi-kind by construction: `send::<K>` is gated on `R: HandlesKind<K>`,
/// so the same `ActorMailbox<RenderCapability, T>` accepts both
/// `&DrawTriangle` and `&Camera` (instead of needing two `Mailbox<K>`
/// declarations as today). Wrong-kind sends are compile errors.
///
/// Built two ways:
///
/// - Singleton path: senders never construct one explicitly. Inside
///   `Ctx::send_to::<R>(&kind)` the SDK resolves the singleton mailbox
///   from `R::NAMESPACE` and dispatches in one call.
/// - Multi-instance path: `Ctx::resolve_actor::<R>(name)` returns an
///   `ActorMailbox<R, T>` value that the caller stores; subsequent sends
///   go through `actor_mailbox.send(transport, &kind)` and are
///   string-free.
pub struct ActorMailbox<R, T: MailTransport> {
    mailbox: u64,
    _r: PhantomData<fn() -> R>,
    _t: PhantomData<fn() -> T>,
}

impl<R, T: MailTransport> Copy for ActorMailbox<R, T> {}
impl<R, T: MailTransport> Clone for ActorMailbox<R, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R, T: MailTransport> ActorMailbox<R, T> {
    /// Not part of the public API; the const builders go through here
    /// so the field stays private.
    #[doc(hidden)]
    pub const fn __new(mailbox: u64) -> Self {
        ActorMailbox {
            mailbox,
            _r: PhantomData,
            _t: PhantomData,
        }
    }

    /// Raw mailbox id. Exposed for callers that need it for a
    /// host fn the SDK doesn't yet wrap.
    pub fn mailbox(self) -> u64 {
        self.mailbox
    }
}

impl<R: crate::Actor, T: MailTransport> ActorMailbox<R, T> {
    /// Send a single payload of kind `K` to actor `R`. Compile-checked
    /// against `R: HandlesKind<K>` — wrong-kind sends are rejected at
    /// the call site.
    ///
    /// Wire shape (cast or postcard) follows `Kind::encode_into_bytes`
    /// — same single source of truth as the kind-typed `Mailbox::send`
    /// per issue #240.
    ///
    /// ```compile_fail
    /// use aether_actor::{Actor, ActorMailbox, HandlesKind, Singleton, MailTransport};
    /// use aether_data::Kind;
    ///
    /// // Two kinds; the receiver only handles the first.
    /// struct KindOk;
    /// impl Kind for KindOk {
    ///     const NAME: &'static str = "doctest.ok";
    ///     const ID: aether_data::KindId = aether_data::KindId(1);
    /// }
    /// struct KindWrong;
    /// impl Kind for KindWrong {
    ///     const NAME: &'static str = "doctest.wrong";
    ///     const ID: aether_data::KindId = aether_data::KindId(2);
    /// }
    /// struct R;
    /// impl Actor for R { const NAMESPACE: &'static str = "doctest"; }
    /// impl Singleton for R {}
    /// impl HandlesKind<KindOk> for R {}     // R handles KindOk only
    ///
    /// struct T;
    /// impl MailTransport for T {
    ///     fn send_mail(&self, _: u64, _: u64, _: &[u8], _: u32) -> u32 { 0 }
    ///     fn reply_mail(&self, _: u32, _: u64, _: &[u8], _: u32) -> u32 { 0 }
    ///     fn save_state(&self, _: u32, _: &[u8]) -> u32 { 0 }
    ///     fn wait_reply(&self, _: u64, _: &mut [u8], _: u32, _: u64) -> i32 { -1 }
    ///     fn prev_correlation(&self) -> u64 { 0 }
    /// }
    ///
    /// let h: ActorMailbox<R, T> = aether_actor::resolve_actor::<R, T>();
    /// let t = T;
    /// h.send(&t, &KindWrong);   // ← compile error: R does not impl HandlesKind<KindWrong>
    /// ```
    pub fn send<K>(self, transport: &T, payload: &K)
    where
        R: crate::HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        transport.send_mail(self.mailbox, K::ID.0, &bytes, 1);
    }

    /// Send a slice of payloads as a contiguous batch. Cast-only —
    /// see [`Mailbox::send_many`] for the wire-shape rationale.
    pub fn send_many<K>(self, transport: &T, payloads: &[K])
    where
        R: crate::HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        transport.send_mail(self.mailbox, K::ID.0, bytes, payloads.len() as u32);
    }
}

/// Resolve an actor by its `Actor::NAMESPACE`, producing a typed
/// `ActorMailbox<R, T>` for the singleton instance. Pure compile-time
/// construction — the mailbox id is `mailbox_id_from_name(R::NAMESPACE)`.
///
/// For multi-instance actors loaded under a non-default name, use
/// [`resolve_actor_named`] (or `Ctx::resolve_actor`) instead.
pub const fn resolve_actor<R: crate::Actor, T: MailTransport>() -> ActorMailbox<R, T> {
    ActorMailbox::__new(mailbox_id_from_name(R::NAMESPACE).0)
}

/// Resolve a multi-instance actor by runtime name, producing a typed
/// `ActorMailbox<R, T>`. The string surfaces ONCE per handle; subsequent
/// sends through the returned handle are string-free and compile-checked
/// against `R: HandlesKind<K>`.
pub fn resolve_actor_named<R: crate::Actor, T: MailTransport>(name: &str) -> ActorMailbox<R, T> {
    ActorMailbox::__new(mailbox_id_from_name(name).0)
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
        let s: Mailbox<FakeKind, NoopTransport> = Mailbox::__new(3u64, 11);
        assert_eq!(s.mailbox(), 3u64);
        assert_eq!(s.kind(), 11);
    }

    /// ADR-0075 actor-typed sender API. `ActorMailbox<R, T>` is keyed on
    /// the receiver actor `R`; `send::<K>` is gated on `R: HandlesKind<K>`
    /// so wrong-kind sends are rejected at the call site.
    mod actor_typed_send {
        use super::super::{ActorMailbox, resolve_actor, resolve_actor_named};
        use crate::actor::{Actor, HandlesKind, Singleton};
        use crate::transport::MailTransport;
        use ::aether_data::{Kind, mailbox_id_from_name};
        use alloc::vec::Vec;
        use core::cell::RefCell;

        /// Cast-shaped kind — overrides `encode_into_bytes` so the
        /// default-panic doesn't trip.
        #[repr(C)]
        #[derive(Copy, Clone)]
        struct PingKind {
            tag: u32,
        }
        unsafe impl bytemuck::Zeroable for PingKind {}
        unsafe impl bytemuck::Pod for PingKind {}
        impl Kind for PingKind {
            const NAME: &'static str = "test.actor_typed.ping";
            const ID: ::aether_data::KindId = ::aether_data::KindId(0x1111_2222_3333_4444);
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
        }

        /// Singleton receiver. `HandlesKind<PingKind>` opens the gate.
        struct PingActor;
        impl Actor for PingActor {
            const NAMESPACE: &'static str = "test.ping_actor";
        }
        impl Singleton for PingActor {}
        impl HandlesKind<PingKind> for PingActor {}

        #[derive(Clone)]
        struct RecordedSend {
            recipient: u64,
            kind: u64,
            bytes: Vec<u8>,
            count: u32,
        }

        /// Recording transport so we can inspect what `send::<K>`
        /// actually plumbed through. `RefCell` is fine — the SDK is
        /// single-threaded per actor, and these tests run on one thread.
        struct RecordingTransport {
            sends: RefCell<Vec<RecordedSend>>,
        }
        impl RecordingTransport {
            fn new() -> Self {
                RecordingTransport {
                    sends: RefCell::new(Vec::new()),
                }
            }
            fn snapshot(&self) -> Vec<RecordedSend> {
                self.sends.borrow().clone()
            }
        }
        impl MailTransport for RecordingTransport {
            fn send_mail(&self, recipient: u64, kind: u64, bytes: &[u8], count: u32) -> u32 {
                self.sends.borrow_mut().push(RecordedSend {
                    recipient,
                    kind,
                    bytes: bytes.to_vec(),
                    count,
                });
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
        // Single-threaded test stub — Send + Sync needed because the
        // SDK trait requires them. Real transports (WasmTransport ZST,
        // NativeTransport) carry these properly.
        unsafe impl Send for RecordingTransport {}
        unsafe impl Sync for RecordingTransport {}

        #[test]
        fn resolve_actor_addresses_namespace() {
            let h: ActorMailbox<PingActor, RecordingTransport> = resolve_actor::<PingActor, _>();
            assert_eq!(h.mailbox(), mailbox_id_from_name(PingActor::NAMESPACE).0);
        }

        #[test]
        fn resolve_actor_named_addresses_runtime_name() {
            let h: ActorMailbox<PingActor, RecordingTransport> =
                resolve_actor_named::<PingActor, _>("instance_42");
            assert_eq!(h.mailbox(), mailbox_id_from_name("instance_42").0);
        }

        #[test]
        fn actor_mailbox_send_records_recipient_and_kind() {
            let transport = RecordingTransport::new();
            let h: ActorMailbox<PingActor, RecordingTransport> = resolve_actor::<PingActor, _>();
            let payload = PingKind { tag: 0xCAFE_BABE };
            h.send(&transport, &payload);

            let snap = transport.snapshot();
            assert_eq!(snap.len(), 1);
            let entry = &snap[0];
            assert_eq!(entry.recipient, mailbox_id_from_name(PingActor::NAMESPACE).0);
            assert_eq!(entry.kind, PingKind::ID.0);
            assert_eq!(entry.bytes.len(), core::mem::size_of::<PingKind>());
            assert_eq!(entry.count, 1);
        }
    }
}
