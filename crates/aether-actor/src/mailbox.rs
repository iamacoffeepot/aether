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
/// API: `ActorMailbox<'a, R, T>` addresses the mailbox of receiver
/// actor `R`, not a single kind. Carries a borrow of the sender's
/// transport so `send` / `send_many` are `&self`-receiver and don't
/// require threading a transport reference at every call site.
///
/// Multi-kind by construction: `send::<K>` is gated on `R: HandlesKind<K>`,
/// so the same `ActorMailbox<'_, RenderCapability, T>` accepts both
/// `&DrawTriangle` and `&Camera`. Wrong-kind sends are compile errors.
///
/// Constructed by [`crate::Ctx::actor`] (singleton shortcut) or
/// [`crate::Ctx::resolve_actor`] (multi-instance, by name) — there is
/// no public free-fn constructor because the lifetime ties the handle
/// to the borrowed transport, which only exists inside a ctx.
pub struct ActorMailbox<'a, R, T: MailTransport> {
    mailbox: u64,
    transport: &'a T,
    _r: PhantomData<fn() -> R>,
}

impl<'a, R, T: MailTransport> Copy for ActorMailbox<'a, R, T> {}
impl<'a, R, T: MailTransport> Clone for ActorMailbox<'a, R, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, R, T: MailTransport> ActorMailbox<'a, R, T> {
    /// Not part of the public API; the ctx-level constructors go
    /// through here so the field stays private.
    #[doc(hidden)]
    pub fn __new(mailbox: u64, transport: &'a T) -> Self {
        ActorMailbox {
            mailbox,
            transport,
            _r: PhantomData,
        }
    }

    /// The receiver's typed mailbox id. Exposed for callers that need
    /// it for diagnostics or a host fn the SDK doesn't yet wrap.
    pub fn mailbox_id(&self) -> ::aether_data::MailboxId {
        ::aether_data::MailboxId(self.mailbox)
    }
}

impl<'a, R: crate::Actor, T: MailTransport> ActorMailbox<'a, R, T> {
    /// Send a single payload of kind `K` to actor `R`. Compile-checked
    /// against `R: HandlesKind<K>` — wrong-kind sends are rejected at
    /// the call site.
    ///
    /// Wire shape (cast or postcard) follows `Kind::encode_into_bytes`
    /// — same single source of truth as the kind-typed `Mailbox::send`
    /// per issue #240.
    ///
    /// ```compile_fail
    /// use aether_actor::{Actor, HandlesKind, Singleton, MailTransport};
    /// use aether_actor::wasm::WASM_TRANSPORT;
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
    /// // Build a ctx, then take the mailbox via ctx.actor::<R>().
    /// let ctx = aether_actor::Ctx::__new(&WASM_TRANSPORT);
    /// ctx.actor::<R>().send(&KindWrong);   // ← compile error: R does not impl HandlesKind<KindWrong>
    /// ```
    pub fn send<K>(&self, payload: &K)
    where
        R: crate::HandlesKind<K>,
        K: Kind,
    {
        let bytes = payload.encode_into_bytes();
        self.transport.send_mail(self.mailbox, K::ID.0, &bytes, 1);
    }

    /// Send a slice of payloads as a contiguous batch. Cast-only —
    /// see [`Mailbox::send_many`] for the wire-shape rationale.
    pub fn send_many<K>(&self, payloads: &[K])
    where
        R: crate::HandlesKind<K>,
        K: Kind + bytemuck::NoUninit,
    {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        self.transport
            .send_mail(self.mailbox, K::ID.0, bytes, payloads.len() as u32);
    }
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

    /// ADR-0075 actor-typed sender API. `ActorMailbox<'a, R, T>` is keyed
    /// on the receiver actor `R` and carries a borrow of the sender's
    /// transport; `send::<K>` is gated on `R: HandlesKind<K>` so
    /// wrong-kind sends are rejected at the call site.
    mod actor_typed_send {
        use super::super::ActorMailbox;
        use crate::actor::{Actor, HandlesKind, Singleton};
        use crate::transport::MailTransport;
        use ::aether_data::{Kind, MailboxId, mailbox_id_from_name};
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
        fn singleton_mailbox_addresses_namespace() {
            let transport = RecordingTransport::new();
            let h: ActorMailbox<'_, PingActor, RecordingTransport> =
                ActorMailbox::__new(mailbox_id_from_name(PingActor::NAMESPACE).0, &transport);
            assert_eq!(
                h.mailbox_id(),
                MailboxId(mailbox_id_from_name(PingActor::NAMESPACE).0)
            );
        }

        #[test]
        fn named_mailbox_addresses_runtime_name() {
            let transport = RecordingTransport::new();
            let h: ActorMailbox<'_, PingActor, RecordingTransport> =
                ActorMailbox::__new(mailbox_id_from_name("instance_42").0, &transport);
            assert_eq!(
                h.mailbox_id(),
                MailboxId(mailbox_id_from_name("instance_42").0)
            );
        }

        #[test]
        fn actor_mailbox_send_records_recipient_and_kind() {
            let transport = RecordingTransport::new();
            let h: ActorMailbox<'_, PingActor, RecordingTransport> =
                ActorMailbox::__new(mailbox_id_from_name(PingActor::NAMESPACE).0, &transport);
            let payload = PingKind { tag: 0xCAFE_BABE };
            h.send(&payload);

            let snap = transport.snapshot();
            assert_eq!(snap.len(), 1);
            let entry = &snap[0];
            assert_eq!(
                entry.recipient,
                mailbox_id_from_name(PingActor::NAMESPACE).0
            );
            assert_eq!(entry.kind, PingKind::ID.0);
            assert_eq!(entry.bytes.len(), core::mem::size_of::<PingKind>());
            assert_eq!(entry.count, 1);
        }
    }
}
