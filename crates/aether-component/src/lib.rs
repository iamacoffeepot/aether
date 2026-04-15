// aether-component: guest-side SDK for WASM components that run on the
// substrate. See ADR-0012 for motivation — this crate wraps the raw
// `extern "C"` + `static mut u32` + sentinel pattern every component
// previously wrote by hand, and presents typed handles instead.
//
// Shipped surfaces:
//   - ADR-0012: `Sink<K>`, `KindId<K>`, `resolve::<K>()`, `resolve_sink::<K>(name)`,
//     and the `raw` FFI module with host-target panicking stubs.
//   - ADR-0014: `Component` trait (`init` / `receive`), typed `InitCtx`
//     and `Ctx` with init-vs-receive capability fencing, `Mail<'_>` for
//     inbound with typed `decode` / `decode_slice`, and the `export!`
//     macro that owns the `#[no_mangle]` init/receive shims.
//
// Still deferred:
//   - Lifecycle hooks beyond init/receive (ADR-0015).
//   - Persistent state across hot reload (ADR-0016).
//   - Reply-to-sender (ADR-0013). `Mail::sender()` is present as
//     `Option<&Sender>` and always returns `None` today; once 0013
//     lands on the substrate side, the `export!` macro will flip to
//     the 4-param receive ABI and the field will start carrying a
//     handle. Component authors won't see the ABI change — it lands
//     inside the macro.

#![no_std]

use core::marker::PhantomData;

use aether_mail::Kind;

pub mod raw;

/// Sentinel returned by `raw::resolve_kind` when the substrate has not
/// registered the requested kind name. Mirrors the host constant.
pub const KIND_NOT_FOUND: u32 = u32::MAX;

/// Sentinel returned by `raw::resolve_mailbox` when the substrate has
/// not registered the requested mailbox name. Mirrors the host constant.
pub const MAILBOX_NOT_FOUND: u32 = u32::MAX;

/// Phantom-typed wrapper around a resolved kind id. A `KindId<Tick>`
/// cannot be passed where a `KindId<DrawTriangle>` is expected — the
/// mismatch is a compile error rather than a runtime bad-dispatch.
///
/// Constructed via `resolve::<K>()` during component init. The raw
/// id is retrievable via `.raw()` for comparison against incoming
/// `kind` parameters in a hand-rolled `receive` shim (ADR-0014's
/// `Mail::decode` will make the raw-int compare go away).
pub struct KindId<K: Kind> {
    raw: u32,
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
    /// The raw kind id the substrate assigned. Exposed for hand-rolled
    /// receive shims that `match` on the inbound `kind: u32` parameter.
    pub fn raw(self) -> u32 {
        self.raw
    }

    /// Returns `true` if `raw` is the id the substrate assigned to `K`.
    /// Convenience over `kind_id.raw() == raw`.
    pub fn matches(self, raw: u32) -> bool {
        self.raw == raw
    }
}

/// Phantom-typed send target. Wraps a mailbox id plus the kind id that
/// the sink accepts. `Sink<DrawTriangle>` can only `send` a
/// `&DrawTriangle` or `&[DrawTriangle]` — the kind is fixed at
/// resolution time.
///
/// Built via `resolve_sink::<K>(name)` during init.
pub struct Sink<K: Kind> {
    mailbox: u32,
    kind: u32,
    _k: PhantomData<fn() -> K>,
}

impl<K: Kind> Copy for Sink<K> {}
impl<K: Kind> Clone for Sink<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K: Kind> Sink<K> {
    /// Raw mailbox id. Exposed for components that need to pass the
    /// id to a host fn not yet wrapped by the SDK.
    pub fn mailbox(self) -> u32 {
        self.mailbox
    }

    /// Raw kind id. Exposed for the same reason as `mailbox`.
    pub fn kind(self) -> u32 {
        self.kind
    }
}

impl<K: Kind + bytemuck::NoUninit> Sink<K> {
    /// Send a single typed payload. The substrate's `count` field is 1.
    /// Bytemuck handles the `&K → &[u8]` cast.
    pub fn send(self, payload: &K) {
        let bytes = bytemuck::bytes_of(payload);
        unsafe {
            raw::send_mail(
                self.mailbox,
                self.kind,
                bytes.as_ptr().addr() as u32,
                bytes.len() as u32,
                1,
            );
        }
    }

    /// Send a slice of typed payloads as a contiguous buffer. The
    /// substrate's `count` field is `payloads.len()`.
    pub fn send_many(self, payloads: &[K]) {
        let bytes: &[u8] = bytemuck::cast_slice(payloads);
        unsafe {
            raw::send_mail(
                self.mailbox,
                self.kind,
                bytes.as_ptr().addr() as u32,
                bytes.len() as u32,
                payloads.len() as u32,
            );
        }
    }
}

/// Resolve a kind by `K::NAME` and return a typed id. Panics if the
/// substrate does not know the name — by ADR-0012's design, failed
/// resolution is a loud init-time failure rather than a silent
/// sentinel that shows up as mail to mailbox 0 at first send.
///
/// Must be called from within `init` (or a context where the substrate
/// has the registry available). Calling post-init is not defined.
pub fn resolve<K: Kind>() -> KindId<K> {
    let name = K::NAME;
    let id = unsafe { raw::resolve_kind(name.as_ptr().addr() as u32, name.len() as u32) };
    if id == KIND_NOT_FOUND {
        panic!("aether-component: resolve_kind failed");
    }
    KindId {
        raw: id,
        _k: PhantomData,
    }
}

/// Resolve a mailbox by its registered name and bind it to kind `K`,
/// producing a typed `Sink<K>`. Panics if either resolution fails —
/// same "loud at init" discipline as `resolve`.
pub fn resolve_sink<K: Kind>(mailbox_name: &str) -> Sink<K> {
    let mailbox = unsafe {
        raw::resolve_mailbox(
            mailbox_name.as_ptr().addr() as u32,
            mailbox_name.len() as u32,
        )
    };
    if mailbox == MAILBOX_NOT_FOUND {
        panic!("aether-component: resolve_mailbox failed");
    }
    let kind = resolve::<K>().raw();
    Sink {
        mailbox,
        kind,
        _k: PhantomData,
    }
}

/// User-implemented component. ADR-0014 commits to `Self`-is-state —
/// cached kind ids, cached sinks, and any domain fields live on the
/// implementor. `init` runs once before any `receive`; `receive` is
/// called with the stored `&mut self` on every inbound mail.
///
/// The `#[no_mangle]` `init` / `receive` exports that actually cross
/// the WASM FFI are generated by `export!(MyComponent)`; implementors
/// do not write `extern "C"` by hand.
pub trait Component: Sized + 'static {
    /// Runs once. Resolve kinds and sinks via `ctx` and return the
    /// initial component state. A failed `resolve` panics — see
    /// ADR-0012 §2 ("loud at init").
    fn init(ctx: &mut InitCtx<'_>) -> Self;

    /// Runs on every inbound mail. Component decides dispatch by
    /// matching `mail.kind()` against cached `KindId<K>` values, or by
    /// calling `mail.decode::<K>(kind_id)` directly — the `Option`
    /// return doubles as the match.
    fn receive(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>);
}

/// Init-only capability handle. The type split between `InitCtx` and
/// `Ctx` fences "when can I resolve?" (init only) and "when can I
/// send?" (receive only) at compile time — calling `resolve` from a
/// `&mut Ctx` is a type error, not a convention.
pub struct InitCtx<'a> {
    _borrow: PhantomData<&'a ()>,
}

impl InitCtx<'_> {
    /// Not part of the public API; called only by `export!`.
    #[doc(hidden)]
    pub fn __new() -> Self {
        InitCtx {
            _borrow: PhantomData,
        }
    }

    /// Resolve a kind by `K::NAME`. Panics on failure (ADR-0012 §2).
    pub fn resolve<K: Kind>(&self) -> KindId<K> {
        resolve::<K>()
    }

    /// Resolve a mailbox by name and bind it to kind `K`, producing a
    /// typed `Sink<K>`. Panics on failure.
    pub fn resolve_sink<K: Kind>(&self, name: &str) -> Sink<K> {
        resolve_sink::<K>(name)
    }
}

/// Per-receive capability handle. Exposes send primitives only.
/// Resolution is intentionally absent — runtime resolution after init
/// is not a supported shape.
pub struct Ctx<'a> {
    _borrow: PhantomData<&'a ()>,
}

impl Ctx<'_> {
    /// Not part of the public API; called only by `export!`.
    #[doc(hidden)]
    pub fn __new() -> Self {
        Ctx {
            _borrow: PhantomData,
        }
    }

    /// Send a single payload to `sink`. Typed wrapper around
    /// `Sink::send` — having the same entry point through both
    /// `Ctx` and `Sink` is deliberate: `Ctx` is the receive-time
    /// vocabulary, `Sink::send` is the universal one.
    pub fn send<K: Kind + bytemuck::NoUninit>(&self, sink: &Sink<K>, payload: &K) {
        sink.send(payload);
    }

    /// Send a slice of payloads as a contiguous batch.
    pub fn send_many<K: Kind + bytemuck::NoUninit>(&self, sink: &Sink<K>, payloads: &[K]) {
        sink.send_many(payloads);
    }
}

/// Reply-to-sender handle. Placeholder until ADR-0013 lands — today
/// nothing on the substrate side produces a non-`None` sender, so
/// `Mail::sender()` always returns `None`. Kept on the surface so
/// consumers structure their receive bodies around
/// `Option<&Sender>` from day one.
pub struct Sender {
    _priv: (),
}

/// Inbound mail, as received by `Component::receive`. Wraps the raw
/// `(kind, ptr, count)` FFI parameters with typed decode helpers.
///
/// The lifetime `'a` ties the returned references back to the receive
/// call; holding a decoded `&K` past the return of `receive` is a
/// compile error. The underlying bytes live in the component's own
/// linear memory (the substrate placed them there before the FFI
/// call), so zero-copy is possible when alignment permits.
pub struct Mail<'a> {
    kind: u32,
    // Stored as `usize` so `Mail::decode` can reconstruct a full host
    // pointer for tests, while the FFI path (`__from_raw`) widens the
    // incoming `u32` address. On wasm32 `usize == u32` so this is a
    // no-op; on 64-bit hosts it lets us unit-test with real pointers.
    ptr: usize,
    count: u32,
    _borrow: PhantomData<&'a [u8]>,
}

impl<'a> Mail<'a> {
    /// Not part of the public API; called only by `export!`. The FFI
    /// delivers `ptr` as a wasm32 offset (`u32`); this widens it.
    #[doc(hidden)]
    pub unsafe fn __from_raw(kind: u32, ptr: u32, count: u32) -> Self {
        Mail {
            kind,
            ptr: ptr as usize,
            count,
            _borrow: PhantomData,
        }
    }

    /// Not part of the public API; unit tests that fabricate `Mail`
    /// from a host pointer go through here so 64-bit addresses survive.
    #[doc(hidden)]
    #[cfg(test)]
    unsafe fn __from_ptr_test(kind: u32, ptr: usize, count: u32) -> Self {
        Mail {
            kind,
            ptr,
            count,
            _borrow: PhantomData,
        }
    }

    /// Raw kind id the substrate routed this mail under. Match against
    /// a cached `KindId<K>` via `kind_id.matches(mail.kind())`, or use
    /// `decode::<K>(kind_id)` and let it be the discriminator.
    pub fn kind(&self) -> u32 {
        self.kind
    }

    /// Number of items carried on the mail frame — 1 for a single
    /// payload send, N for a batch send of N elements.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Reply handle for the session that originated this mail.
    /// Always `None` today; ADR-0013 will start returning `Some`.
    pub fn sender(&self) -> Option<&Sender> {
        None
    }

    /// Decode as a single owned `K`. Returns `None` if the kind does
    /// not match or if `count` is not 1. Copies rather than borrows so
    /// alignment of the underlying bytes doesn't matter.
    pub fn decode<K: Kind + bytemuck::AnyBitPattern>(&self, kind_id: KindId<K>) -> Option<K> {
        if !kind_id.matches(self.kind) || self.count != 1 {
            return None;
        }
        let byte_len = core::mem::size_of::<K>();
        let bytes = unsafe { core::slice::from_raw_parts(self.ptr as *const u8, byte_len) };
        Some(bytemuck::pod_read_unaligned(bytes))
    }

    /// Decode as a zero-copy slice of `K`. Returns `None` if the kind
    /// does not match or the bytes are not aligned for `K`. The
    /// returned slice borrows from component linear memory for the
    /// lifetime of this `Mail`.
    pub fn decode_slice<K: Kind + bytemuck::AnyBitPattern>(
        &self,
        kind_id: KindId<K>,
    ) -> Option<&'a [K]> {
        if !kind_id.matches(self.kind) {
            return None;
        }
        let byte_len = core::mem::size_of::<K>() * self.count as usize;
        let bytes = unsafe { core::slice::from_raw_parts(self.ptr as *const u8, byte_len) };
        bytemuck::try_cast_slice(bytes).ok()
    }
}

/// Macro-use backing store for the one `Component` instance per
/// guest. WASM components are single-threaded per instance (ADR-0010
/// §5 — the substrate holds a read lock across `deliver`), so an
/// `UnsafeCell` with a blanket `Sync` impl is sound *provided the
/// macro is the only caller*. The `export!` macro orchestrates
/// `set` / `get_mut` from within `init` / `receive` shims that the
/// substrate serializes.
pub struct Slot<T> {
    inner: core::cell::UnsafeCell<Option<T>>,
}

impl<T> Slot<T> {
    /// Build an empty slot. `const` so it can live in a `static`.
    pub const fn new() -> Self {
        Slot {
            inner: core::cell::UnsafeCell::new(None),
        }
    }

    /// # Safety
    /// Caller must guarantee no aliasing access. Intended to be called
    /// exactly once, from within the `init` shim, before any other
    /// access.
    pub unsafe fn set(&self, value: T) {
        unsafe {
            *self.inner.get() = Some(value);
        }
    }

    /// # Safety
    /// Caller must guarantee no aliasing access. Intended to be called
    /// from within the `receive` shim, after `init` has completed.
    // Returning `&mut T` from `&self` is the load-bearing pattern
    // here — the `UnsafeCell` makes this sound under the substrate's
    // serialized-dispatch guarantee. Clippy's `mut_from_ref` lint
    // catches this as a footgun in general; we're the exception the
    // lint is designed around.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut(&self) -> Option<&mut T> {
        unsafe { (*self.inner.get()).as_mut() }
    }
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot::new()
    }
}

// Single-threaded WASM + serialized FFI entry points mean the
// `UnsafeCell` is only ever touched from one thread at a time. The
// `Sync` impl unlocks `static SLOT: Slot<MyComponent>` without
// needing `std::sync` types the `no_std` surface can't provide.
unsafe impl<T> Sync for Slot<T> {}

/// Bind a `Component` implementor to the guest's `#[no_mangle]`
/// `init` / `receive` exports. Expands to:
///
/// - A `static` `Slot<T>` that backs the component instance.
/// - `extern "C" fn init() -> u32` — builds an `InitCtx`, calls
///   `T::init`, stashes the result in the slot.
/// - `extern "C" fn receive(kind, ptr, count) -> u32` — builds
///   `Ctx` and `Mail`, calls `T::receive` on the stashed instance.
///
/// Only one component per guest crate. A second `export!` call in
/// the same crate is a duplicate-symbol compile error on the shared
/// `init` / `receive` names — ADR-0014 §4 parks multi-component
/// crates as out of scope.
///
/// ```ignore
/// pub struct Hello { /* fields */ }
/// impl aether_component::Component for Hello { /* init + receive */ }
/// aether_component::export!(Hello);
/// ```
#[macro_export]
macro_rules! export {
    ($component:ty) => {
        static __AETHER_COMPONENT: $crate::Slot<$component> = $crate::Slot::new();

        /// # Safety
        /// Called exactly once by the substrate before any `receive`.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn init() -> u32 {
            let mut ctx = $crate::InitCtx::__new();
            let instance = <$component as $crate::Component>::init(&mut ctx);
            unsafe {
                __AETHER_COMPONENT.set(instance);
            }
            0
        }

        /// # Safety
        /// Called by the substrate with `(kind, ptr, count)` matching
        /// the FFI contract in `aether-substrate/src/host_fns.rs`.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn receive(kind: u32, ptr: u32, count: u32) -> u32 {
            let Some(instance) = (unsafe { __AETHER_COMPONENT.get_mut() }) else {
                return 1;
            };
            let mut ctx = $crate::Ctx::__new();
            let mail = unsafe { $crate::Mail::__from_raw(kind, ptr, count) };
            <$component as $crate::Component>::receive(instance, &mut ctx, mail);
            0
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeKind;
    impl Kind for FakeKind {
        const NAME: &'static str = "test.fake";
    }

    #[test]
    fn kind_id_equality_and_matches() {
        let a: KindId<FakeKind> = KindId {
            raw: 7,
            _k: PhantomData,
        };
        let b: KindId<FakeKind> = KindId {
            raw: 7,
            _k: PhantomData,
        };
        let c: KindId<FakeKind> = KindId {
            raw: 8,
            _k: PhantomData,
        };
        assert!(a == b);
        assert!(a != c);
        assert!(a.matches(7));
        assert!(!a.matches(8));
        assert_eq!(a.raw(), 7);
    }

    #[test]
    fn sink_accessors() {
        let s: Sink<FakeKind> = Sink {
            mailbox: 3,
            kind: 11,
            _k: PhantomData,
        };
        assert_eq!(s.mailbox(), 3);
        assert_eq!(s.kind(), 11);
    }

    #[test]
    fn slot_set_then_get_mut_returns_value() {
        let slot: Slot<u32> = Slot::new();
        unsafe {
            slot.set(42);
        }
        let got = unsafe { slot.get_mut() };
        assert_eq!(got.copied(), Some(42));
    }

    #[test]
    fn slot_get_mut_before_set_is_none() {
        let slot: Slot<u32> = Slot::new();
        let got = unsafe { slot.get_mut() };
        assert!(got.is_none());
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
    struct FakePod {
        a: u32,
        b: u32,
    }
    impl Kind for FakePod {
        const NAME: &'static str = "test.fake_pod";
    }

    #[test]
    fn mail_decode_single_roundtrip() {
        // Build a `Mail` by hand that points at a local `FakePod`.
        // This is the only place in the crate that fabricates a Mail
        // outside the FFI path; the unsafe is load-bearing because
        // decode treats `ptr` as a guest-memory address and the test
        // has to arrange for that address to point at valid bytes.
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        let mail = unsafe { Mail::__from_ptr_test(7, ptr_raw, 1) };
        let kind: KindId<FakePod> = KindId {
            raw: 7,
            _k: PhantomData,
        };
        let out = mail.decode(kind).unwrap();
        assert_eq!(out, value);
    }

    #[test]
    fn mail_decode_wrong_kind_returns_none() {
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        let mail = unsafe { Mail::__from_ptr_test(7, ptr_raw, 1) };
        let wrong: KindId<FakePod> = KindId {
            raw: 8,
            _k: PhantomData,
        };
        assert!(mail.decode(wrong).is_none());
    }

    #[test]
    fn mail_decode_wrong_count_returns_none() {
        let values = [FakePod { a: 5, b: 9 }, FakePod { a: 1, b: 1 }];
        let ptr_raw = values.as_ptr().addr();
        let mail = unsafe { Mail::__from_ptr_test(7, ptr_raw, 2) };
        let kind: KindId<FakePod> = KindId {
            raw: 7,
            _k: PhantomData,
        };
        // `decode` requires count == 1; use `decode_slice` for batches.
        assert!(mail.decode(kind).is_none());
    }

    #[test]
    fn mail_decode_slice_roundtrip() {
        let values = [FakePod { a: 1, b: 2 }, FakePod { a: 3, b: 4 }];
        let ptr_raw = values.as_ptr().addr();
        let mail = unsafe { Mail::__from_ptr_test(7, ptr_raw, 2) };
        let kind: KindId<FakePod> = KindId {
            raw: 7,
            _k: PhantomData,
        };
        let out = mail.decode_slice(kind).unwrap();
        assert_eq!(out, &values);
    }

    #[test]
    fn mail_sender_is_always_none_today() {
        let mail = unsafe { Mail::__from_ptr_test(0, 0, 0) };
        assert!(mail.sender().is_none());
    }
}
