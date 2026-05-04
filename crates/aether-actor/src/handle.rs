//! ADR-0045 typed-handle SDK, actor side. The substrate's
//! `"aether.handle"` sink (ADR-0058) owns a refcounted byte cache;
//! actors publish values into it (postcard-encoded) and receive a
//! fresh ephemeral handle id back that they can embed in mail as
//! `Ref::Handle { id, kind_id }`. The substrate's dispatch path
//! resolves the handle to its `Ref::Inline` form before delivery, so
//! recipients see a normal inline value.
//!
//! Wire shape mirrors the io and net helpers (ADR-0041, ADR-0043):
//! actors mail one of the four typed request kinds and either fire
//! and forget or block on the paired `*Result` reply. Helpers here
//! are generic over `T: MailTransport` so the wasm guest path
//! (`WasmTransport`) and the native path (`NativeTransport`) share
//! one code body.
//!
//! Each helper takes a `&T` transport reference explicitly. ADR-0074
//! §Decision: the trait takes `&self`, the actor binding is
//! type-system-tracked through the reference, no thread-locals or
//! globals. From inside a `#[actor]` method the natural call shape
//! is `ctx.publish(...)` — the `Ctx` already holds the transport
//! borrow, so the user doesn't have to thread it through.
//!
//! Quick tour (wasm guest):
//!
//! ```ignore
//! use aether_component::{Component, Ctx, InitCtx};
//!
//! #[actor]
//! impl Component for MyComp {
//!     fn init(_ctx: &mut InitCtx<'_>) -> Result<Self, BootError> { Ok(Self) }
//!
//!     #[handler]
//!     fn on_tick(&mut self, ctx: &mut Ctx<'_>, _t: Tick) {
//!         let inner = MyValue { ... };
//!         let Ok(handle) = ctx.publish(&inner) else { return };
//!         let outer = MyParent {
//!             held: handle.as_ref(),
//!             ..
//!         };
//!         BROADCAST.send(ctx.transport(), &outer);
//!         // No auto-release on drop — the substrate's LRU evicts
//!         // forgotten handles. Call `handle.release(ctx.transport())`
//!         // for prompt cleanup.
//!     }
//! }
//! ```

use alloc::string::String;
use core::marker::PhantomData;

use aether_data::{Kind, Ref};
use aether_kinds::{
    HandleError, HandlePin, HandlePinResult, HandlePublish, HandlePublishResult, HandleRelease,
    HandleReleaseResult, HandleUnpin, HandleUnpinResult,
};
use serde::Serialize;

use crate::sink::resolve_mailbox;
use crate::sync::{WaitError, wait_reply};
use crate::transport::MailTransport;

/// Mailbox name the substrate registers its handle store under
/// (ADR-0045). ADR-0074 Phase 5 retired the `aether.sink.*` namespace
/// — chassis-owned mailboxes now address as `aether.<name>`. Exposed
/// for actors that want to bypass the typed helpers and build a
/// `Mailbox<HandlePublish, T>` directly without duplicating the
/// string literal.
pub const HANDLE_MAILBOX_NAME: &str = "aether.handle";

/// Wait-buffer capacity for the four reply kinds. Their `Ok` /
/// `Err` payloads are at most a couple of u64s plus an `HandleError`
/// — tiny. 4 KiB is generous and matches the small-reply cap io.rs
/// uses for the same kind of reply shape.
const SMALL_REPLY_CAP: usize = 4 * 1024;

/// Default timeout for the synchronous helpers. Generous because
/// the substrate-side dispatch is sub-millisecond — anything bigger
/// than a few seconds means the substrate is wedged.
pub const DEFAULT_TIMEOUT_MS: u32 = 5_000;

/// Errors surfaced by the synchronous wrappers (`Handle::release`,
/// `Handle::pin`, `Handle::unpin`, and the underlying `publish`
/// helper). Three sentinel-mapped variants from the host fn, plus
/// the substrate's structured `HandleError` and a postcard-decode
/// fallback for substrate/guest schema drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncHandleError {
    Timeout,
    BufferTooSmall,
    Cancelled,
    Handle(HandleError),
    Decode(String),
}

/// Typed wrapper around a substrate-side handle id. `K` is phantom —
/// the underlying id is type-agnostic on the wire, but `as_ref` pulls
/// `K::ID` so the resulting `Ref::Handle` carries the right kind id.
/// `T` is also phantom — held to enforce that the sync helper methods
/// receive a transport of the same flavor the handle was minted on.
///
/// Cloning a refcounted handle without an inc-ref would cause double
/// release issues with the prior auto-Drop design — that auto-release
/// is gone now (the substrate's LRU eviction handles forgotten
/// handles), so we could in principle make `Handle` `Copy`. Left
/// non-Copy for now to keep callsites explicit; a future PR can lift
/// the restriction if tests show real-world friction.
pub struct Handle<K, T: MailTransport> {
    id: u64,
    _k: PhantomData<fn() -> K>,
    _t: PhantomData<fn() -> T>,
}

impl<K, T: MailTransport> Handle<K, T> {
    /// Not part of the public API; the `publish` helper builds
    /// handles through here so the field stays private to the SDK.
    #[doc(hidden)]
    pub fn __from_id(id: u64) -> Self {
        Handle {
            id,
            _k: PhantomData,
            _t: PhantomData,
        }
    }

    /// Raw handle id. Exposed for hand-rolled callers that need to
    /// pass the id through host fns the SDK doesn't yet wrap.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Drop the publisher's reference. Sync — blocks the actor
    /// thread until the substrate replies, with a 5s default
    /// timeout. Returns `Err(Handle(UnknownHandle))` if the entry
    /// has already been evicted; otherwise `Ok(())`.
    ///
    /// Unlike the prior design, `Handle::Drop` is now a no-op —
    /// the substrate's LRU eviction handles forgotten handles. Call
    /// this method explicitly when you want prompt cleanup.
    pub fn release(self, transport: &T) -> Result<(), SyncHandleError> {
        sync_release::<T>(transport, self.id)
    }

    /// Pin against LRU eviction. Useful when the publisher wants to
    /// release its local guard (drop the `Handle`) but keep the
    /// cached bytes available — pin first, then drop.
    pub fn pin(&self, transport: &T) -> Result<(), SyncHandleError> {
        sync_pin::<T>(transport, self.id)
    }

    /// Clear the pinned flag. Doesn't drop the entry; only makes
    /// it eligible for LRU eviction once `refcount == 0`.
    pub fn unpin(&self, transport: &T) -> Result<(), SyncHandleError> {
        sync_unpin::<T>(transport, self.id)
    }
}

impl<K: Kind, T: MailTransport> Handle<K, T> {
    /// Wire-shaped reference to this handle. Embed in a `Ref<K>`
    /// field on an outgoing kind so the substrate's dispatch path
    /// resolves the inline bytes before delivery. `as_ref` is a
    /// borrow, not a transfer — the `Handle` keeps its refcount on
    /// the publisher side.
    pub fn as_ref(&self) -> Ref<K> {
        Ref::Handle {
            id: self.id,
            // `Ref::Handle.kind_id` is wire-format `u64`; `Kind::ID`
            // is typed `KindId` post-issue 466, so drop into `.0`.
            kind_id: K::ID.0,
        }
    }
}

// ADR-0074 §Decision: no auto-release on Drop. The prior design's
// `impl Drop for Handle` fired a fire-and-forget `HandleRelease` mail
// through a static `Sink::send` — that pattern doesn't survive the
// `&self` trait refactor (Drop has no transport ref to send through).
// The substrate's LRU eviction is the safety net; explicit
// `handle.release(transport)` is the prompt-cleanup path.

/// Postcard-encode `value` and round-trip a `HandlePublish` request
/// through the `"aether.handle"` sink. Returns the typed
/// `Handle<K, T>` on success or a `SyncHandleError` describing the
/// failure (substrate timed out, eviction failed, kind id mismatch
/// on a re-publish, …).
///
/// Shared by `InitCtx::publish` / `Ctx::publish` / `DropCtx::publish`
/// — keep the wire shape and timeouts in one place.
pub fn publish<K: Kind + Serialize, T: MailTransport>(
    transport: &T,
    value: &K,
) -> Result<Handle<K, T>, SyncHandleError> {
    let bytes = postcard::to_allocvec(value).expect("postcard encode to Vec is infallible");
    let req = HandlePublish {
        kind_id: K::ID,
        bytes,
    };
    resolve_mailbox::<HandlePublish, T>(HANDLE_MAILBOX_NAME).send(transport, &req);
    let correlation = transport.prev_correlation();
    let result: HandlePublishResult = wait_reply::<_, SyncHandleError, T>(
        transport,
        DEFAULT_TIMEOUT_MS,
        SMALL_REPLY_CAP,
        correlation,
    )?;
    match result {
        HandlePublishResult::Ok { id, .. } => Ok(Handle::__from_id(id.0)),
        HandlePublishResult::Err { error, .. } => Err(SyncHandleError::Handle(error)),
    }
}

fn sync_release<T: MailTransport>(transport: &T, id: u64) -> Result<(), SyncHandleError> {
    let req = HandleRelease {
        id: ::aether_data::HandleId(id),
    };
    resolve_mailbox::<HandleRelease, T>(HANDLE_MAILBOX_NAME).send(transport, &req);
    let correlation = transport.prev_correlation();
    let result: HandleReleaseResult = wait_reply::<_, SyncHandleError, T>(
        transport,
        DEFAULT_TIMEOUT_MS,
        SMALL_REPLY_CAP,
        correlation,
    )?;
    match result {
        HandleReleaseResult::Ok { .. } => Ok(()),
        HandleReleaseResult::Err { error, .. } => Err(SyncHandleError::Handle(error)),
    }
}

fn sync_pin<T: MailTransport>(transport: &T, id: u64) -> Result<(), SyncHandleError> {
    let req = HandlePin {
        id: ::aether_data::HandleId(id),
    };
    resolve_mailbox::<HandlePin, T>(HANDLE_MAILBOX_NAME).send(transport, &req);
    let correlation = transport.prev_correlation();
    let result: HandlePinResult = wait_reply::<_, SyncHandleError, T>(
        transport,
        DEFAULT_TIMEOUT_MS,
        SMALL_REPLY_CAP,
        correlation,
    )?;
    match result {
        HandlePinResult::Ok { .. } => Ok(()),
        HandlePinResult::Err { error, .. } => Err(SyncHandleError::Handle(error)),
    }
}

fn sync_unpin<T: MailTransport>(transport: &T, id: u64) -> Result<(), SyncHandleError> {
    let req = HandleUnpin {
        id: ::aether_data::HandleId(id),
    };
    resolve_mailbox::<HandleUnpin, T>(HANDLE_MAILBOX_NAME).send(transport, &req);
    let correlation = transport.prev_correlation();
    let result: HandleUnpinResult = wait_reply::<_, SyncHandleError, T>(
        transport,
        DEFAULT_TIMEOUT_MS,
        SMALL_REPLY_CAP,
        correlation,
    )?;
    match result {
        HandleUnpinResult::Ok { .. } => Ok(()),
        HandleUnpinResult::Err { error, .. } => Err(SyncHandleError::Handle(error)),
    }
}

impl WaitError for SyncHandleError {
    fn timeout() -> Self {
        Self::Timeout
    }
    fn buffer_too_small() -> Self {
        Self::BufferTooSmall
    }
    fn cancelled() -> Self {
        Self::Cancelled
    }
    fn decode(message: String) -> Self {
        Self::Decode(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;
    use serde::Deserialize;

    /// Off-target we can't exercise the full FFI round-trip (`raw::*`
    /// host stubs panic). Pin the wire shape by encoding the request
    /// kinds and asserting the bytes round-trip into the same kinds.
    #[derive(Kind, Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[kind(name = "test.handle.payload")]
    #[allow(dead_code)]
    struct Payload {
        seq: u32,
    }

    use aether_data::{Kind, Schema};
    #[test]
    fn publish_request_bytes_decode_to_handle_publish() {
        let req = HandlePublish {
            kind_id: ::aether_data::KindId(0xCAFE),
            bytes: vec![1, 2, 3, 4, 5],
        };
        let encoded = postcard::to_allocvec(&req).unwrap();
        let decoded: HandlePublish = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(decoded.kind_id, ::aether_data::KindId(0xCAFE));
        assert_eq!(decoded.bytes, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn release_request_bytes_decode_to_handle_release() {
        let req = HandleRelease {
            id: ::aether_data::HandleId(0xDEAD),
        };
        let encoded = postcard::to_allocvec(&req).unwrap();
        let decoded: HandleRelease = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(decoded.id, ::aether_data::HandleId(0xDEAD));
    }

    /// Stub transport for the host-side `as_ref` test. Lives next to
    /// the test that uses it; not a public surface.
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
    fn handle_as_ref_carries_kind_id() {
        // Construct a handle bypassing publish (which needs the
        // FFI). Pin the contract: `as_ref` reads `K::ID` from the
        // type parameter, not from a field, so the kind id matches
        // the type's compile-time constant.
        let handle: Handle<Payload, NoopTransport> = Handle::__from_id(42);
        match handle.as_ref() {
            Ref::Handle { id, kind_id } => {
                assert_eq!(id, 42);
                assert_eq!(kind_id, Payload::ID.0);
            }
            Ref::Inline(_) => panic!("as_ref should produce Handle, not Inline"),
        }
    }

    /// `SyncHandleError` is the [`crate::sync::WaitError`] impl the
    /// handle `sync_*` wrappers use, so the four trait constructors
    /// must land on the matching enum variants.
    #[test]
    fn wait_error_mapping_for_sync_handle_error() {
        use crate::sync::WaitError;
        assert_eq!(
            <SyncHandleError as WaitError>::timeout(),
            SyncHandleError::Timeout
        );
        assert_eq!(
            <SyncHandleError as WaitError>::buffer_too_small(),
            SyncHandleError::BufferTooSmall
        );
        assert_eq!(
            <SyncHandleError as WaitError>::cancelled(),
            SyncHandleError::Cancelled
        );
        assert_eq!(
            <SyncHandleError as WaitError>::decode("schema drift".to_string()),
            SyncHandleError::Decode("schema drift".to_string())
        );
    }
}
