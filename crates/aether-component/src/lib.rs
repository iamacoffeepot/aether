// aether-component: guest-side SDK for WASM components that run on the
// substrate. See ADR-0012 for motivation — this crate wraps the raw
// `extern "C"` + `static mut u32` + sentinel pattern every component
// previously wrote by hand, and presents typed handles instead.
//
// Scope per ADR-0012: send-side ergonomics (`Sink<K>`, `send`) and
// init-side resolution (`resolve::<K>()`, `resolve_sink::<K>(name)`).
// The `Component` trait, `InitCtx`/`Ctx`, lifecycle hooks, and the
// `export!` macro that owns `#[no_mangle]` are deliberately deferred
// to ADR-0014 / ADR-0015 / ADR-0016. Until those land, a component
// still writes its own `#[unsafe(no_mangle)] extern "C"` exports; the
// SDK only removes the un-ergonomic bits inside those bodies.

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
}
