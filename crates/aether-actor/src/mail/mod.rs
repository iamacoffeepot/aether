// Wire-encode: `usize → u32` narrowings forward sizes to the wasm32
// host-fn ABI (`_p32` convention, ADR-0024) and stage test payloads
// in the in-process decode-roundtrip fixtures.
#![allow(clippy::cast_possible_truncation)]

//! Mail layer of the actor SDK: the inbound `Mail` envelope,
//! `PriorState` bundle, and `ReplyHandle` opaque handle live here in
//! `mod.rs` (pure decoders, no transport coupling). The
//! [`Mailbox<K>`](mailbox) addressing token lives in
//! the [`mailbox`] submodule.
//!
//! Issue 665 retired the `MailTransport` trait that previously sat at
//! `transport.rs` here. Per-stage capability traits in
//! [`crate::model::ctx`] are the only cross-target trait surface;
//! per-target dispatch goes through [`crate::wasm::bridge`] (wasm) and
//! the inherent methods on `aether_substrate::actor::native::binding::NativeBinding`
//! (native).

use core::slice;
use serde::de::DeserializeOwned;
pub mod mailbox;

use core::marker::PhantomData;

use aether_data::{Kind, MailboxId, Schema, wire};

/// Sentinel the substrate passes as the reply-handle parameter on
/// the `receive` shim when there is no reply target — for
/// component-originated mail (no Claude session involved) and for
/// broadcast-origin mail. `Mail::reply_handle()` returns `None` in this
/// case; `ReplyHandle` is only constructable via the `Mail` accessor.
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
pub struct ReplyHandle {
    pub(crate) raw: u32,
}

impl ReplyHandle {
    /// Not part of the public API; the `Ctx` reply path round-trips
    /// the raw handle through here so siblings outside `mail.rs` can
    /// reconstruct a `ReplyHandle` without touching the private field.
    /// Sentinel handling is the caller's responsibility — this
    /// constructor accepts any `u32`.
    #[doc(hidden)]
    #[must_use]
    pub(crate) fn __from_raw(raw: u32) -> Self {
        Self { raw }
    }

    /// Raw handle value. Exposed for components that need to call a
    /// host fn the SDK doesn't yet wrap.
    #[must_use]
    pub fn raw(self) -> u32 {
        self.raw
    }
}

/// Inbound mail, as received by the `#[actor]`-synthesized
/// `__aether_dispatch` (driven by the guest's `receive` FFI export).
/// Wraps the raw
/// `(kind, ptr, count, sender, recipient)` FFI parameters with typed
/// decode helpers.
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
    // `size_of::<K>() * count`; structured decoders use it as the
    // exact slice length so the parser can't run past the substrate-
    // written bytes into adjacent linear memory.
    byte_len: u32,
    count: u32,
    sender: u32,
    // ADR-0114 decision #1: the mailbox the substrate routed this mail
    // to, carried as the raw `u64` the substrate stamped on its
    // `OwnedDispatch.recipient` and widened by the receive ABI. For a
    // normally-addressed actor this equals the actor's own mailbox id;
    // an inline-child membrane (ADR-0114) reads it to demux mail to the
    // co-located child the producer addressed. Surfaced via
    // [`Mail::recipient`].
    recipient: u64,
    _borrow: PhantomData<&'a [u8]>,
}

impl Mail<'_> {
    /// Not part of the public API; called only by `export!`. The FFI
    /// delivers `ptr` as a wasm32 offset (`u32`); this widens it.
    #[doc(hidden)]
    #[must_use]
    pub unsafe fn __from_raw(
        kind: u64,
        ptr: u32,
        byte_len: u32,
        count: u32,
        sender: u32,
        recipient: u64,
    ) -> Self {
        Mail {
            kind,
            ptr: ptr as usize,
            byte_len,
            count,
            sender,
            recipient,
            _borrow: PhantomData,
        }
    }

    /// Not part of the public API; native callers (and the SDK's own
    /// host-side unit tests) build `Mail` from a real host pointer
    /// rather than a wasm32 offset, so they go through here to keep
    /// the wider address.
    #[doc(hidden)]
    #[must_use]
    pub unsafe fn __from_ptr(
        kind: u64,
        ptr: usize,
        byte_len: u32,
        count: u32,
        sender: u32,
        recipient: u64,
    ) -> Self {
        Mail {
            kind,
            ptr,
            byte_len,
            count,
            sender,
            recipient,
            _borrow: PhantomData,
        }
    }

    /// Raw kind id the substrate routed this mail under. The canonical
    /// way to consume it is [`Self::decode_kind::<K>()`], which matches
    /// the kind id and decodes in one call.
    #[must_use]
    pub fn kind(&self) -> u64 {
        self.kind
    }

    /// Number of items carried on the mail frame — 1 for a single
    /// payload send, N for a batch send of N elements.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Total bytes the substrate placed at `ptr` for this delivery.
    /// Cast decoders treat this as a sanity check
    /// (`size_of::<K>() * count`); structured decoders use it as the
    /// exact slice length so the parser is bounded by the substrate-
    /// written region rather than reading into adjacent memory.
    #[must_use]
    pub fn byte_len(&self) -> u32 {
        self.byte_len
    }

    /// Reply handle for the inbound mail. `None` for broadcast-origin
    /// mail (and any sender the substrate stamped `SourceAddr::None`);
    /// `Some(ReplyHandle)` when the inbound carries an answerable
    /// source — a Claude session, a remote engine's mailbox, or a
    /// local component (the substrate's `deliver()` allocates a handle
    /// for `Component`-origin mail too, so component-to-component mail
    /// is answerable via `Ctx::reply`).
    #[must_use]
    pub fn reply_handle(&self) -> Option<ReplyHandle> {
        if self.sender == NO_REPLY_HANDLE {
            None
        } else {
            Some(ReplyHandle { raw: self.sender })
        }
    }

    /// Mailbox this mail was routed to (ADR-0114 decision #1). The
    /// substrate stamps it on the dispatch envelope and threads it
    /// through the receive ABI, so a guest can tell apart mail addressed
    /// to different lineage addresses that land in the same instance.
    /// For a normally-addressed actor this equals the actor's own
    /// mailbox id; an inline-child membrane reads it to demux inbound
    /// mail to the co-located child the producer addressed.
    #[must_use]
    pub fn recipient(&self) -> MailboxId {
        MailboxId(self.recipient)
    }

    /// Decode a single inbound `K` via the wire shape `K`'s `Kind`
    /// derive baked into `Kind::decode_from_bytes` — cast for
    /// `#[repr(C)]` + `Pod` types, structured for schema-shaped types.
    /// This is the canonical receive-side decode and what the
    /// `#[actor]` dispatcher calls on every typed handler.
    ///
    /// Hands `K::decode_from_bytes` exactly `byte_len` bytes from
    /// `ptr` so the decoder is bounded by the substrate-written
    /// frame and can't read past it into adjacent linear memory.
    /// Returns `None` on kind mismatch, on `count != 1`, or when
    /// `K::decode_from_bytes` itself returns `None` — which can be
    /// either the default body for hand-rolled `Kind` impls that
    /// didn't override, a cast-size mismatch, or a structured decode
    /// error.
    #[must_use]
    pub fn decode_kind<K: Kind>(&self) -> Option<K> {
        if self.kind != K::ID.0 || self.count != 1 {
            return None;
        }
        // SAFETY: `self.ptr` / `self.byte_len` originate from the
        // substrate's receive ABI; the substrate guarantees
        // `self.byte_len` bytes valid at `self.ptr` for this `Mail`'s
        // lifetime. Bounding the slice by `byte_len` keeps
        // `K::decode_from_bytes` (cast or structured) from running past
        // the substrate-written region into adjacent linear memory.
        let bytes = unsafe { slice::from_raw_parts(self.ptr as *const u8, self.byte_len as usize) };
        K::decode_from_bytes(bytes)
    }
}

/// Opaque view of a prior state bundle handed to `on_rehydrate` by
/// the substrate. Populated when the predecessor called
/// `WasmDropCtx::save_state` during its own `on_dehydrate`; empty
/// otherwise (and in that case `on_rehydrate` is not called at all —
/// ADR-0016 §3).
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
    #[must_use]
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
    #[must_use]
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
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.version
    }

    /// Bytes the previous instance saved via `DropCtx::save_state`.
    #[must_use]
    pub fn bytes(&self) -> &'a [u8] {
        if self.len == 0 {
            &[]
        } else {
            // SAFETY: `self.ptr` / `self.len` originate from the
            // substrate's `on_rehydrate` ABI (`PriorState::__from_raw`
            // / `__from_ptr`); the substrate guarantees `self.len`
            // bytes valid at `self.ptr` for this `PriorState`'s
            // lifetime. The `len == 0` branch above avoids forming a
            // slice over a possibly-null pointer.
            unsafe { slice::from_raw_parts(self.ptr as *const u8, self.len) }
        }
    }

    /// Decode the prior-state bundle as kind `K` (ADR-0040). Returns
    /// `Some(K)` when the leading 8 bytes match `K::ID` (little-
    /// endian) and the trailing bytes decode cleanly via wire;
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
    #[must_use]
    pub fn as_kind<K>(&self) -> Option<K>
    where
        K: Kind + Schema + DeserializeOwned,
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
        wire::from_bytes(payload).ok()
    }
}

#[cfg(test)]
// Mail-decode tests hold per-test guards / borrows across the assert
// block; the snapshot is the test's atomic read.
#[allow(clippy::significant_drop_tightening)]
mod tests {
    use super::*;
    use aether_data::KindId as DataKindId;
    use alloc::string::String;
    use alloc::vec::Vec;
    use serde::{Deserialize, Serialize};

    /// Hand-rolled `Kind` with a stable test sentinel id so the
    /// decode tests can fabricate mismatched `Mail` frames without
    /// depending on a real schema-hashed id.
    struct FakeKind;
    impl Kind for FakeKind {
        const NAME: &'static str = "test.fake";
        const ID: DataKindId = DataKindId(0xDEAD_BEEF_0001_0001);
    }

    /// Structured-shape kind for the schema-driven `decode_kind` path.
    #[derive(
        ::aether_data::Kind, ::aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq,
    )]
    #[kind(name = "test.fake_structured")]
    struct FakeStructured {
        tag: String,
        ids: Vec<u32>,
    }

    // SAFETY (test fixtures): each `Mail::__from_ptr` / `__from_raw` /
    // `PriorState::__from_ptr` / `__from_raw` below substitutes for the
    // substrate's receive ABI. The host pointer + length pair we pass
    // is derived from a local stack value or buffer whose lifetime
    // straddles the resulting `Mail`/`PriorState`, satisfying the
    // ABI's `(ptr, byte_len)` validity contract for the duration of
    // the test body. The `count` / `sender` / `recipient` / `version`
    // scalars are plain values with no aliasing implications. The `0,0,0` no-deref
    // variants rely on the `bytes()` / `decode*` accessors honouring
    // the `len == 0` early-return rather than forming a slice over the
    // null pointer.

    #[test]
    fn mail_sender_none_for_sentinel_handle() {
        // SAFETY: no pointer is dereferenced (`bytes()` and friends
        // are not called); we only inspect the sentinel `sender`.
        let mail = unsafe { Mail::__from_ptr(0, 0, 0, 0, NO_REPLY_HANDLE, 0) };
        assert!(mail.reply_handle().is_none());
    }

    #[test]
    fn mail_sender_some_for_real_handle() {
        // SAFETY: no pointer is dereferenced; we only inspect `sender`.
        let mail = unsafe { Mail::__from_ptr(0, 0, 0, 0, 42, 0) };
        let s = mail
            .reply_handle()
            .expect("non-sentinel handle yields Some");
        assert_eq!(s.raw(), 42);
    }

    #[test]
    fn prior_state_empty_bytes_does_not_deref() {
        // With len=0, `bytes()` must not materialise a pointer — a
        // raw 0 ptr with len>0 would be UB if anyone called
        // `from_raw_parts` on it. The `if self.len == 0` branch in
        // `bytes()` is what guarantees this.
        // SAFETY: `bytes()` returns `&[]` for `len == 0` without
        // forming a slice over the null pointer.
        let prior = unsafe { PriorState::__from_raw(7, 0, 0) };
        assert_eq!(prior.schema_version(), 7);
        assert_eq!(prior.bytes(), &[] as &[u8]);
    }

    #[test]
    fn prior_state_nonempty_bytes_roundtrip() {
        let buf: [u8; 4] = [1, 2, 3, 4];
        // SAFETY: `buf` outlives `prior`; the `(addr, len)` pair is
        // valid for `buf.len()` bytes for the rest of the test body.
        let prior = unsafe { PriorState::__from_ptr(3, buf.as_ptr().addr(), buf.len()) };
        assert_eq!(prior.schema_version(), 3);
        assert_eq!(prior.bytes(), &buf);
    }

    #[test]
    fn mail_decode_kind_structured_roundtrip() {
        let value = FakeStructured {
            tag: String::from("greet"),
            ids: alloc::vec![1, 2, 3, 4],
        };
        let bytes = value.encode_into_bytes();
        // SAFETY: `bytes` (a `Vec<u8>` from the kind encoder) outlives
        // `mail`; its `(addr, len)` pair is valid for the rest of the body.
        let mail = unsafe {
            Mail::__from_ptr(
                FakeStructured::ID.0,
                bytes.as_ptr().addr(),
                bytes.len() as u32,
                1,
                NO_REPLY_HANDLE,
                0,
            )
        };
        let out = mail.decode_kind::<FakeStructured>().expect("decode");
        assert_eq!(out, value);
    }

    #[test]
    fn mail_decode_kind_wrong_kind_returns_none() {
        let value = FakeStructured {
            tag: String::from("x"),
            ids: alloc::vec![],
        };
        let bytes = value.encode_into_bytes();
        // SAFETY: `bytes` outlives `mail`; the `(addr, len)` pair is
        // valid for `bytes.len()` bytes for the rest of the body.
        let mail = unsafe {
            Mail::__from_ptr(
                FakeKind::ID.0,
                bytes.as_ptr().addr(),
                bytes.len() as u32,
                1,
                NO_REPLY_HANDLE,
                0,
            )
        };
        assert!(mail.decode_kind::<FakeStructured>().is_none());
    }

    #[test]
    fn mail_decode_kind_wrong_count_returns_none() {
        let value = FakeStructured {
            tag: String::from("x"),
            ids: alloc::vec![],
        };
        let bytes = value.encode_into_bytes();
        // SAFETY: `bytes` outlives `mail`; the `(addr, len)` pair is
        // valid for `bytes.len()` bytes for the rest of the body.
        let mail = unsafe {
            Mail::__from_ptr(
                FakeStructured::ID.0,
                bytes.as_ptr().addr(),
                bytes.len() as u32,
                2,
                NO_REPLY_HANDLE,
                0,
            )
        };
        assert!(mail.decode_kind::<FakeStructured>().is_none());
    }

    #[test]
    fn mail_decode_kind_truncated_bytes_returns_none() {
        let value = FakeStructured {
            tag: String::from("longer"),
            ids: alloc::vec![1, 2, 3],
        };
        let bytes = value.encode_into_bytes();
        // Pretend the substrate only wrote the first 2 bytes —
        // `decode_from_bytes` gets the truncated slice and surfaces the
        // parse error as `None`.
        // SAFETY: `bytes` outlives `mail`; the declared `byte_len=2`
        // is within the actual allocation so the bounded read is
        // valid even though it's deliberately a truncation.
        let mail = unsafe {
            Mail::__from_ptr(
                FakeStructured::ID.0,
                bytes.as_ptr().addr(),
                2,
                1,
                NO_REPLY_HANDLE,
                0,
            )
        };
        assert!(mail.decode_kind::<FakeStructured>().is_none());
    }

    #[test]
    fn mail_decode_kind_default_body_returns_none_for_handrolled_kind() {
        // FakeKind is a hand-rolled Kind with no `decode_from_bytes`
        // override, so the default trait body returns None — dispatch
        // surfaces this as DISPATCH_UNKNOWN_KIND in real components.
        // Use a real (empty) buffer — `slice::from_raw_parts(NULL, 0)`
        // is UB even when the length is zero.
        let buf: [u8; 1] = [0];
        // SAFETY: `buf` outlives `mail`; the `(addr, 0)` pair points
        // into the live `buf` allocation, satisfying the validity
        // contract trivially for the zero-byte read.
        let mail = unsafe {
            Mail::__from_ptr(
                FakeKind::ID.0,
                buf.as_ptr().addr(),
                0,
                1,
                NO_REPLY_HANDLE,
                0,
            )
        };
        assert!(mail.decode_kind::<FakeKind>().is_none());
    }

    // ADR-0040 typed-state framing. `DropCtx::save_state_kind` can't be
    // exercised end-to-end on host (the underlying `T::save_state`
    // panics on the wasm transport's host stub), so these tests pair
    // a hand-built bundle matching the documented framing
    // (`[0..8) = K::ID LE`, `[8..) = wire(value)`) against
    // `PriorState::as_kind` — the one we *can* unit-test on host. A
    // mismatch between framing and decode surfaces here before either
    // diverges from the ADR's wire shape.

    #[derive(
        ::aether_data::Kind, ::aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq,
    )]
    #[kind(name = "test.state.struct")]
    struct StateStruct {
        tag: u32,
        label: String,
        items: Vec<u32>,
    }

    #[derive(
        ::aether_data::Kind, ::aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq,
    )]
    #[kind(name = "test.state.other")]
    struct OtherState {
        flag: bool,
    }

    fn frame_bundle<K: Kind + Schema + Serialize>(value: &K) -> Vec<u8> {
        let mut out = Vec::from(K::ID.0.to_le_bytes());
        let payload = wire::to_vec(value).expect("test setup: wire encodes test value");
        out.extend_from_slice(&payload);
        out
    }

    fn prior_from(buf: &[u8], version: u32) -> PriorState<'_> {
        // SAFETY: the returned `PriorState<'_>` borrows `buf` (via the
        // explicit lifetime); the `(addr, len)` pair derives from a
        // live slice the caller supplies, so validity holds for the
        // borrow's lifetime.
        unsafe { PriorState::__from_ptr(version, buf.as_ptr().addr(), buf.len()) }
    }

    #[test]
    fn as_kind_roundtrip() {
        let value = StateStruct {
            tag: 11,
            label: String::from("phase-2"),
            items: alloc::vec![1, 2, 3],
        };
        let buf = frame_bundle(&value);
        let prior = prior_from(&buf, 0);
        let decoded = prior
            .as_kind::<StateStruct>()
            .expect("test setup: round-trip frame decodes back to StateStruct");
        assert_eq!(decoded, value);
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
        // SAFETY: `bytes()` returns `&[]` for `len == 0` without
        // forming a slice over the null pointer; the decode reads
        // through that empty slice and short-circuits.
        let prior = unsafe { PriorState::__from_raw(0, 0, 0) };
        assert!(prior.as_kind::<StateStruct>().is_none());
    }

    #[test]
    fn as_kind_correct_id_garbage_payload_returns_none() {
        // Leading id matches but the wire tail is truncated.
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
