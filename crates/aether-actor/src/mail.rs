//! Inbound mail and prior-state bundles. Pure decoders — no transport
//! coupling, so these types live as non-generic structs in the SDK.
//! The transport-coupled send / reply / save lives on `Ctx`,
//! `DropCtx`, and `Sink`.

use core::marker::PhantomData;

use aether_data::{Kind, Schema};

use crate::sink::KindId;

/// Sentinel the substrate passes as the reply-handle parameter on
/// the `receive` shim when there is no reply target — for
/// component-originated mail (no Claude session involved) and for
/// broadcast-origin mail. `Mail::reply_to()` returns `None` in this
/// case; `ReplyTo` is only constructable via the `Mail` accessor.
pub const NO_REPLY_HANDLE: u32 = u32::MAX;

/// Opaque per-instance handle identifying the reply destination for
/// an inbound mail. Pass it back to `Ctx::reply` to answer — the
/// substrate routes it to the right target (a Claude MCP session,
/// another local component, or a remote engine's mailbox) depending
/// on where the inbound came from. Mail is pushed at a recipient
/// and has no real "from" concept; this handle is purely a
/// reply-to address.
///
/// `Copy` because the handle is a `u32` underneath; cloning is free.
/// Cloning is also fine for stashing across receives — the substrate
/// guarantees the handle stays valid for the lifetime of the
/// receiving component instance.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReplyTo {
    pub(crate) raw: u32,
}

impl ReplyTo {
    /// Not part of the public API; the `Ctx` reply path round-trips
    /// the raw handle through here so siblings outside `mail.rs` can
    /// reconstruct a `ReplyTo` without touching the private field.
    /// Sentinel handling is the caller's responsibility — this
    /// constructor accepts any `u32`.
    #[doc(hidden)]
    pub fn __from_raw(raw: u32) -> Self {
        ReplyTo { raw }
    }

    /// Raw handle value. Exposed for components that need to call a
    /// host fn the SDK doesn't yet wrap.
    pub fn raw(self) -> u32 {
        self.raw
    }
}

/// Inbound mail, as received by `Component::receive`. Wraps the raw
/// `(kind, ptr, count, sender)` FFI parameters with typed decode helpers.
///
/// The lifetime `'a` ties the returned references back to the receive
/// call; holding a decoded `&K` past the return of `receive` is a
/// compile error. The underlying bytes live in the actor's own
/// linear memory (the substrate placed them there before the FFI
/// call), so zero-copy is possible when alignment permits.
pub struct Mail<'a> {
    kind: u64,
    // Stored as `usize` so `Mail::decode` can reconstruct a full host
    // pointer for tests, while the FFI path (`__from_raw`) widens the
    // incoming `u32` address. On wasm32 `usize == u32` so this is a
    // no-op; on 64-bit hosts it lets us unit-test with real pointers.
    ptr: usize,
    // Total payload bytes valid at `ptr` for this delivery. Substrate
    // sources from `mail.payload.len()` and threads through the
    // receive ABI as a frame parameter (sibling of `kind`/`count`/
    // `sender`). Cast decoders sanity-check against
    // `size_of::<K>() * count`; postcard decoders use it as the
    // exact slice length so the parser can't run past the substrate-
    // written bytes into adjacent linear memory.
    byte_len: u32,
    count: u32,
    sender: u32,
    _borrow: PhantomData<&'a [u8]>,
}

impl<'a> Mail<'a> {
    /// Not part of the public API; called only by `export!`. The FFI
    /// delivers `ptr` as a wasm32 offset (`u32`); this widens it.
    #[doc(hidden)]
    pub unsafe fn __from_raw(kind: u64, ptr: u32, byte_len: u32, count: u32, sender: u32) -> Self {
        Mail {
            kind,
            ptr: ptr as usize,
            byte_len,
            count,
            sender,
            _borrow: PhantomData,
        }
    }

    /// Not part of the public API; native callers (and the SDK's own
    /// host-side unit tests) build `Mail` from a real host pointer
    /// rather than a wasm32 offset, so they go through here to keep
    /// the wider address.
    #[doc(hidden)]
    pub unsafe fn __from_ptr(
        kind: u64,
        ptr: usize,
        byte_len: u32,
        count: u32,
        sender: u32,
    ) -> Self {
        Mail {
            kind,
            ptr,
            byte_len,
            count,
            sender,
            _borrow: PhantomData,
        }
    }

    /// Raw kind id the substrate routed this mail under. Match against
    /// a cached `KindId<K>` via `kind_id.matches(mail.kind())`, or use
    /// `decode::<K>(kind_id)` and let it be the discriminator.
    pub fn kind(&self) -> u64 {
        self.kind
    }

    /// Number of items carried on the mail frame — 1 for a single
    /// payload send, N for a batch send of N elements.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Total bytes the substrate placed at `ptr` for this delivery.
    /// Cast decoders treat this as a sanity check
    /// (`size_of::<K>() * count`); postcard decoders use it as the
    /// exact slice length so the parser is bounded by the substrate-
    /// written region rather than reading into adjacent memory.
    pub fn byte_len(&self) -> u32 {
        self.byte_len
    }

    /// Reply handle for the session that originated this mail. `None`
    /// for component-to-component mail and broadcast-origin mail;
    /// `Some(ReplyTo)` when the inbound came from a Claude session and
    /// can be answered via `Ctx::reply`.
    pub fn reply_to(&self) -> Option<ReplyTo> {
        if self.sender == NO_REPLY_HANDLE {
            None
        } else {
            Some(ReplyTo { raw: self.sender })
        }
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
    /// returned slice borrows from actor linear memory for the
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

    /// True if the inbound mail's kind id matches `<K as Kind>::ID`
    /// (ADR-0030 compile-time hash). Zero-cost — just a `u64` compare
    /// against a const. Useful as the discriminator before deciding
    /// how to handle a kind, or as a signal check when `K` is a
    /// zero-sized input marker like `Tick` / `MouseButton`.
    pub fn is<K: Kind>(&self) -> bool {
        self.kind == K::ID.0
    }

    /// Type-driven sibling of `decode`: takes `K` as a type parameter
    /// and uses `<K as Kind>::ID` directly (ADR-0030 compile-time hash),
    /// so no `KindId<K>` thread-through is needed. Returns `None` if
    /// the inbound kind doesn't match `K::ID`, if `count != 1`, or
    /// if `byte_len` doesn't equal `size_of::<K>()` (a sender/receiver
    /// schema-skew guard the substrate's frame metadata makes free).
    /// Copies rather than borrows so alignment of the underlying bytes
    /// doesn't matter — same semantics as `decode`.
    pub fn decode_typed<K: Kind + bytemuck::AnyBitPattern>(&self) -> Option<K> {
        if self.kind != K::ID.0 || self.count != 1 {
            return None;
        }
        let byte_len = core::mem::size_of::<K>();
        if self.byte_len as usize != byte_len {
            return None;
        }
        let bytes = unsafe { core::slice::from_raw_parts(self.ptr as *const u8, byte_len) };
        Some(bytemuck::pod_read_unaligned(bytes))
    }

    /// Type-driven sibling of `decode_slice`. Borrowed, alignment
    /// required (returns `None` if misaligned).
    pub fn decode_slice_typed<K: Kind + bytemuck::AnyBitPattern>(&self) -> Option<&'a [K]> {
        if self.kind != K::ID.0 {
            return None;
        }
        let byte_len = core::mem::size_of::<K>() * self.count as usize;
        if self.byte_len as usize != byte_len {
            return None;
        }
        let bytes = unsafe { core::slice::from_raw_parts(self.ptr as *const u8, byte_len) };
        bytemuck::try_cast_slice(bytes).ok()
    }

    /// Decode a single inbound `K` via the wire shape `K`'s `Kind`
    /// derive baked into `Kind::decode_from_bytes` — cast for
    /// `#[repr(C)]` + `Pod` types, postcard for schema-shaped types.
    /// This is the canonical receive-side decode and what the
    /// `#[handlers]` dispatcher calls on every typed handler;
    /// `decode` / `decode_typed` / `decode_slice` / `decode_slice_typed`
    /// remain as low-level escape hatches for fallback handlers that
    /// want explicit wire-shape control.
    ///
    /// Hands `K::decode_from_bytes` exactly `byte_len` bytes from
    /// `ptr` so the decoder is bounded by the substrate-written
    /// frame and can't read past it into adjacent linear memory.
    /// Returns `None` on kind mismatch, on `count != 1` (batch
    /// receives go through `decode_slice_typed`), or when
    /// `K::decode_from_bytes` itself returns `None` — which can be
    /// either the default body for hand-rolled `Kind` impls that
    /// didn't override, a cast-size mismatch, or a postcard decode
    /// error.
    pub fn decode_kind<K: Kind>(&self) -> Option<K> {
        if self.kind != K::ID.0 || self.count != 1 {
            return None;
        }
        let bytes =
            unsafe { core::slice::from_raw_parts(self.ptr as *const u8, self.byte_len as usize) };
        K::decode_from_bytes(bytes)
    }
}

/// Opaque view of a prior state bundle handed to `on_rehydrate` by
/// the substrate. Populated when the predecessor called
/// `DropCtx::save_state` during its own `on_replace`; empty otherwise
/// (and in that case `on_rehydrate` is not called at all — ADR-0016
/// §3).
///
/// The lifetime `'a` ties `bytes()` back to the call; holding a
/// reference past return is a compile error.
pub struct PriorState<'a> {
    version: u32,
    ptr: usize,
    len: usize,
    _borrow: PhantomData<&'a [u8]>,
}

impl<'a> PriorState<'a> {
    /// Not part of the public API; called only by `export!`. The FFI
    /// delivers the buffer as wasm32 `(u32, u32)`; this widens.
    #[doc(hidden)]
    pub unsafe fn __from_raw(version: u32, ptr: u32, len: u32) -> Self {
        PriorState {
            version,
            ptr: ptr as usize,
            len: len as usize,
            _borrow: PhantomData,
        }
    }

    /// Not part of the public API; mirrors `Mail::__from_ptr` for the
    /// host-pointer construction path (native callers, host-side unit
    /// tests).
    #[doc(hidden)]
    pub unsafe fn __from_ptr(version: u32, ptr: usize, len: usize) -> Self {
        PriorState {
            version,
            ptr,
            len,
            _borrow: PhantomData,
        }
    }

    /// Component-defined schema version. The substrate does not
    /// interpret it — see ADR-0016.
    pub fn schema_version(&self) -> u32 {
        self.version
    }

    /// Bytes the previous instance saved via `DropCtx::save_state`.
    pub fn bytes(&self) -> &'a [u8] {
        if self.len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.ptr as *const u8, self.len) }
        }
    }

    /// Decode the prior-state bundle as kind `K` (ADR-0040). Returns
    /// `Some(K)` when the leading 8 bytes match `K::ID` (little-
    /// endian) and the trailing bytes decode cleanly via postcard;
    /// `None` on id mismatch, short buffer (fewer than 8 bytes), or
    /// decode failure.
    ///
    /// Id mismatch is how schema evolution manifests: changing the
    /// shape of `K` changes `K::ID`, so a replacement instance
    /// compiled against the new schema sees `None` from the old
    /// instance's save and boots fresh. Components that want to
    /// migrate across a schema change can reach for `bytes()` +
    /// `schema_version()` directly, or try `as_kind::<OldShape>()`
    /// first and fall back if it returns `None`.
    pub fn as_kind<K>(&self) -> Option<K>
    where
        K: Kind + Schema + serde::de::DeserializeOwned,
    {
        let bytes = self.bytes();
        if bytes.len() < 8 {
            return None;
        }
        let (id_bytes, payload) = bytes.split_at(8);
        let mut id_arr = [0u8; 8];
        id_arr.copy_from_slice(id_bytes);
        let id = u64::from_le_bytes(id_arr);
        if id != K::ID.0 {
            return None;
        }
        postcard::from_bytes(payload).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::KindId;
    use alloc::vec::Vec;
    use serde::{Deserialize, Serialize};

    /// Hand-rolled `Kind` with a stable test sentinel id so the
    /// decode tests can fabricate matching `Mail` frames without
    /// depending on a real schema-hashed id.
    struct FakeKind;
    impl Kind for FakeKind {
        const NAME: &'static str = "test.fake";
        const ID: ::aether_data::KindId = ::aether_data::KindId(0xDEAD_BEEF_0001_0001);
    }

    /// Cast-shape Pod kind for the slice / single-decode happy paths.
    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
    struct FakePod {
        a: u32,
        b: u32,
    }
    impl Kind for FakePod {
        const NAME: &'static str = "test.fake_pod";
        const ID: ::aether_data::KindId = ::aether_data::KindId(0xDEAD_BEEF_0001_0002);
    }

    /// Postcard-shape kind for the schema-driven `decode_kind` path.
    #[derive(
        ::aether_data::Kind,
        ::aether_data::Schema,
        Serialize,
        Deserialize,
        Debug,
        Clone,
        PartialEq,
    )]
    #[kind(name = "test.fake_postcard")]
    struct FakePostcard {
        tag: alloc::string::String,
        ids: alloc::vec::Vec<u32>,
    }

    #[test]
    fn mail_decode_single_roundtrip() {
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        let byte_len = core::mem::size_of::<FakePod>() as u32;
        let mail = unsafe { Mail::__from_ptr(7, ptr_raw, byte_len, 1, NO_REPLY_HANDLE) };
        let kind: KindId<FakePod> = KindId::__new(7);
        let out = mail.decode(kind).unwrap();
        assert_eq!(out, value);
    }

    #[test]
    fn mail_decode_wrong_kind_returns_none() {
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        let byte_len = core::mem::size_of::<FakePod>() as u32;
        let mail = unsafe { Mail::__from_ptr(7, ptr_raw, byte_len, 1, NO_REPLY_HANDLE) };
        let wrong: KindId<FakePod> = KindId::__new(8);
        assert!(mail.decode(wrong).is_none());
    }

    #[test]
    fn mail_decode_wrong_count_returns_none() {
        let values = [FakePod { a: 5, b: 9 }, FakePod { a: 1, b: 1 }];
        let ptr_raw = values.as_ptr().addr();
        let byte_len = (core::mem::size_of::<FakePod>() * 2) as u32;
        let mail = unsafe { Mail::__from_ptr(7, ptr_raw, byte_len, 2, NO_REPLY_HANDLE) };
        let kind: KindId<FakePod> = KindId::__new(7);
        // `decode` requires count == 1; use `decode_slice` for batches.
        assert!(mail.decode(kind).is_none());
    }

    #[test]
    fn mail_decode_slice_roundtrip() {
        let values = [FakePod { a: 1, b: 2 }, FakePod { a: 3, b: 4 }];
        let ptr_raw = values.as_ptr().addr();
        let byte_len = (core::mem::size_of::<FakePod>() * 2) as u32;
        let mail = unsafe { Mail::__from_ptr(7, ptr_raw, byte_len, 2, NO_REPLY_HANDLE) };
        let kind: KindId<FakePod> = KindId::__new(7);
        let out = mail.decode_slice(kind).unwrap();
        assert_eq!(out, &values);
    }

    #[test]
    fn mail_sender_none_for_sentinel_handle() {
        let mail = unsafe { Mail::__from_ptr(0, 0, 0, 0, NO_REPLY_HANDLE) };
        assert!(mail.reply_to().is_none());
    }

    #[test]
    fn mail_sender_some_for_real_handle() {
        let mail = unsafe { Mail::__from_ptr(0, 0, 0, 0, 42) };
        let s = mail.reply_to().expect("non-sentinel handle yields Some");
        assert_eq!(s.raw(), 42);
    }

    #[test]
    fn prior_state_empty_bytes_does_not_deref() {
        // With len=0, `bytes()` must not materialise a pointer — a
        // raw 0 ptr with len>0 would be UB if anyone called
        // `from_raw_parts` on it. The `if self.len == 0` branch in
        // `bytes()` is what guarantees this.
        let prior = unsafe { PriorState::__from_raw(7, 0, 0) };
        assert_eq!(prior.schema_version(), 7);
        assert_eq!(prior.bytes(), &[] as &[u8]);
    }

    #[test]
    fn prior_state_nonempty_bytes_roundtrip() {
        let buf: [u8; 4] = [1, 2, 3, 4];
        let prior = unsafe { PriorState::__from_ptr(3, buf.as_ptr().addr(), buf.len()) };
        assert_eq!(prior.schema_version(), 3);
        assert_eq!(prior.bytes(), &buf);
    }

    #[test]
    fn mail_is_typed_matches_kind_id() {
        let mail = unsafe { Mail::__from_ptr(FakeKind::ID.0, 0, 0, 0, NO_REPLY_HANDLE) };
        assert!(mail.is::<FakeKind>());
        assert!(!mail.is::<FakePod>());
    }

    #[test]
    fn mail_decode_typed_roundtrip() {
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        let byte_len = core::mem::size_of::<FakePod>() as u32;
        let mail = unsafe { Mail::__from_ptr(FakePod::ID.0, ptr_raw, byte_len, 1, NO_REPLY_HANDLE) };
        let out = mail.decode_typed::<FakePod>().unwrap();
        assert_eq!(out, value);
    }

    #[test]
    fn mail_decode_typed_wrong_kind_returns_none() {
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        let byte_len = core::mem::size_of::<FakePod>() as u32;
        // Kind id deliberately mismatched (FakeKind instead of FakePod).
        let mail =
            unsafe { Mail::__from_ptr(FakeKind::ID.0, ptr_raw, byte_len, 1, NO_REPLY_HANDLE) };
        assert!(mail.decode_typed::<FakePod>().is_none());
    }

    #[test]
    fn mail_decode_typed_wrong_count_returns_none() {
        let values = [FakePod { a: 5, b: 9 }, FakePod { a: 1, b: 1 }];
        let ptr_raw = values.as_ptr().addr();
        let byte_len = (core::mem::size_of::<FakePod>() * 2) as u32;
        let mail =
            unsafe { Mail::__from_ptr(FakePod::ID.0, ptr_raw, byte_len, 2, NO_REPLY_HANDLE) };
        assert!(mail.decode_typed::<FakePod>().is_none());
    }

    #[test]
    fn mail_decode_slice_typed_roundtrip() {
        let values = [FakePod { a: 1, b: 2 }, FakePod { a: 3, b: 4 }];
        let ptr_raw = values.as_ptr().addr();
        let byte_len = (core::mem::size_of::<FakePod>() * 2) as u32;
        let mail = unsafe { Mail::__from_ptr(FakePod::ID.0, ptr_raw, byte_len, 2, NO_REPLY_HANDLE) };
        let out = mail.decode_slice_typed::<FakePod>().unwrap();
        assert_eq!(out, &values);
    }

    #[test]
    fn mail_decode_kind_postcard_roundtrip() {
        let value = FakePostcard {
            tag: alloc::string::String::from("greet"),
            ids: alloc::vec![1, 2, 3, 4],
        };
        let bytes = postcard::to_allocvec(&value).unwrap();
        let mail = unsafe {
            Mail::__from_ptr(
                FakePostcard::ID.0,
                bytes.as_ptr().addr(),
                bytes.len() as u32,
                1,
                NO_REPLY_HANDLE,
            )
        };
        let out = mail.decode_kind::<FakePostcard>().expect("decode");
        assert_eq!(out, value);
    }

    #[test]
    fn mail_decode_kind_cast_roundtrip() {
        // Cast arm — Kind derive on a `#[repr(C)] + Pod` type emits
        // `decode_cast` as the body, so `decode_kind` lands on the
        // bytemuck reader without any per-handler annotation.
        #[repr(C)]
        #[derive(
            Copy,
            Clone,
            Debug,
            PartialEq,
            bytemuck::Pod,
            bytemuck::Zeroable,
            ::aether_data::Kind,
            ::aether_data::Schema,
        )]
        #[kind(name = "test.fake_cast_kind")]
        struct FakeCastKind {
            a: u32,
            b: u32,
        }

        let value = FakeCastKind { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakeCastKind).addr();
        let byte_len = core::mem::size_of::<FakeCastKind>() as u32;
        let mail = unsafe {
            Mail::__from_ptr(FakeCastKind::ID.0, ptr_raw, byte_len, 1, NO_REPLY_HANDLE)
        };
        let out = mail.decode_kind::<FakeCastKind>().expect("decode");
        assert_eq!(out, value);
    }

    #[test]
    fn mail_decode_kind_wrong_kind_returns_none() {
        let value = FakePostcard {
            tag: alloc::string::String::from("x"),
            ids: alloc::vec![],
        };
        let bytes = postcard::to_allocvec(&value).unwrap();
        let mail = unsafe {
            Mail::__from_ptr(
                FakeKind::ID.0,
                bytes.as_ptr().addr(),
                bytes.len() as u32,
                1,
                NO_REPLY_HANDLE,
            )
        };
        assert!(mail.decode_kind::<FakePostcard>().is_none());
    }

    #[test]
    fn mail_decode_kind_wrong_count_returns_none() {
        let value = FakePostcard {
            tag: alloc::string::String::from("x"),
            ids: alloc::vec![],
        };
        let bytes = postcard::to_allocvec(&value).unwrap();
        let mail = unsafe {
            Mail::__from_ptr(
                FakePostcard::ID.0,
                bytes.as_ptr().addr(),
                bytes.len() as u32,
                2,
                NO_REPLY_HANDLE,
            )
        };
        assert!(mail.decode_kind::<FakePostcard>().is_none());
    }

    #[test]
    fn mail_decode_kind_truncated_bytes_returns_none() {
        let value = FakePostcard {
            tag: alloc::string::String::from("longer"),
            ids: alloc::vec![1, 2, 3],
        };
        let bytes = postcard::to_allocvec(&value).unwrap();
        // Pretend the substrate only wrote the first 2 bytes —
        // `decode_from_bytes` (postcard arm) gets the truncated slice
        // and surfaces the parse error as `None`.
        let mail = unsafe {
            Mail::__from_ptr(FakePostcard::ID.0, bytes.as_ptr().addr(), 2, 1, NO_REPLY_HANDLE)
        };
        assert!(mail.decode_kind::<FakePostcard>().is_none());
    }

    #[test]
    fn mail_decode_kind_default_body_returns_none_for_handrolled_kind() {
        // FakeKind is a hand-rolled Kind with no `decode_from_bytes`
        // override, so the default trait body returns None — dispatch
        // surfaces this as DISPATCH_UNKNOWN_KIND in real components.
        // Use a real (empty) buffer — `slice::from_raw_parts(NULL, 0)`
        // is UB even when the length is zero.
        let buf: [u8; 1] = [0];
        let mail =
            unsafe { Mail::__from_ptr(FakeKind::ID.0, buf.as_ptr().addr(), 0, 1, NO_REPLY_HANDLE) };
        assert!(mail.decode_kind::<FakeKind>().is_none());
    }

    #[test]
    fn mail_decode_typed_byte_len_mismatch_returns_none() {
        // Cast decode now sanity-checks byte_len against
        // `size_of::<K>() * count`. If the substrate ever delivers a
        // mail whose declared byte_len doesn't match the kind's size,
        // decode bails rather than reading the wrong window.
        let value = FakePod { a: 5, b: 9 };
        let ptr_raw = (&value as *const FakePod).addr();
        let bogus_byte_len = (core::mem::size_of::<FakePod>() + 4) as u32;
        let mail =
            unsafe { Mail::__from_ptr(FakePod::ID.0, ptr_raw, bogus_byte_len, 1, NO_REPLY_HANDLE) };
        assert!(mail.decode_typed::<FakePod>().is_none());
    }

    // ADR-0040 typed-state framing. `DropCtx::save_state_kind` can't be
    // exercised end-to-end on host (the underlying `T::save_state`
    // panics on the wasm transport's host stub), so these tests pair
    // a hand-built bundle matching the documented framing
    // (`[0..8) = K::ID LE`, `[8..) = postcard(value)`) against
    // `PriorState::as_kind` — the one we *can* unit-test on host. A
    // mismatch between framing and decode surfaces here before either
    // diverges from the ADR's wire shape.

    #[derive(
        ::aether_data::Kind,
        ::aether_data::Schema,
        Serialize,
        Deserialize,
        Debug,
        Clone,
        PartialEq,
    )]
    #[kind(name = "test.state.struct")]
    struct StateStruct {
        tag: u32,
        label: alloc::string::String,
        items: Vec<u32>,
    }

    #[derive(
        ::aether_data::Kind,
        ::aether_data::Schema,
        Serialize,
        Deserialize,
        Debug,
        Clone,
        PartialEq,
    )]
    #[kind(name = "test.state.other")]
    struct OtherState {
        flag: bool,
    }

    fn frame_bundle<K: Kind + Schema + Serialize>(value: &K) -> Vec<u8> {
        let mut out = Vec::from(K::ID.0.to_le_bytes());
        let payload = postcard::to_allocvec(value).unwrap();
        out.extend_from_slice(&payload);
        out
    }

    fn prior_from(buf: &[u8], version: u32) -> PriorState<'_> {
        unsafe { PriorState::__from_ptr(version, buf.as_ptr().addr(), buf.len()) }
    }

    #[test]
    fn as_kind_roundtrip() {
        let value = StateStruct {
            tag: 11,
            label: alloc::string::String::from("phase-2"),
            items: alloc::vec![1, 2, 3],
        };
        let buf = frame_bundle(&value);
        let prior = prior_from(&buf, 0);
        let decoded = prior.as_kind::<StateStruct>().unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn as_kind_id_mismatch_returns_none() {
        // Frame under one kind, decode as a different one — the
        // leading `K::ID` compare rejects before postcard runs.
        let value = OtherState { flag: true };
        let buf = frame_bundle(&value);
        let prior = prior_from(&buf, 0);
        assert!(prior.as_kind::<StateStruct>().is_none());
    }

    #[test]
    fn as_kind_short_buffer_returns_none() {
        // Buffer shorter than the 8-byte leading id — not a kind-
        // typed save (or corrupt). Must not panic.
        let buf: [u8; 3] = [1, 2, 3];
        let prior = prior_from(&buf, 0);
        assert!(prior.as_kind::<StateStruct>().is_none());
    }

    #[test]
    fn as_kind_empty_buffer_returns_none() {
        // `on_rehydrate` only fires when the predecessor saved
        // something, but a hypothetical zero-length buffer must
        // still fall through cleanly.
        let prior = unsafe { PriorState::__from_raw(0, 0, 0) };
        assert!(prior.as_kind::<StateStruct>().is_none());
    }

    #[test]
    fn as_kind_correct_id_garbage_payload_returns_none() {
        // Leading id matches but the postcard tail is truncated.
        // Decode error must surface as None, not a panic.
        let mut buf = Vec::from(StateStruct::ID.0.to_le_bytes());
        buf.push(0xff);
        let prior = prior_from(&buf, 0);
        assert!(prior.as_kind::<StateStruct>().is_none());
    }

    #[test]
    fn as_kind_preserves_raw_access_for_migration() {
        // ADR-0040 keeps the raw bytes + version reachable so a
        // component that sees `as_kind::<New>() = None` can pivot to
        // an explicit migration path.
        let value = OtherState { flag: false };
        let buf = frame_bundle(&value);
        let prior = prior_from(&buf, 7);
        assert!(prior.as_kind::<StateStruct>().is_none());
        assert_eq!(prior.schema_version(), 7);
        assert_eq!(prior.bytes(), buf.as_slice());
    }
}
