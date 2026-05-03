//! ADR-0045 typed-handle SDK, actor side. The substrate's
//! `"aether.sink.handle"` sink (ADR-0058) owns a refcounted byte cache;
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
//! (`WasmTransport`) and the future native path (`NativeTransport`)
//! share one code body.
//!
//! Quick tour (wasm guest):
//!
//! ```ignore
//! use aether_component::{Component, Ctx, InitCtx};
//!
//! #[handlers]
//! impl Component for MyComp {
//!     fn init(_ctx: &mut InitCtx<'_>) -> Self { Self }
//!
//!     #[handler]
//!     fn on_tick(&mut self, ctx: &mut Ctx<'_>, _t: Tick) {
//!         let inner = MyValue { ... };
//!         let Ok(handle) = ctx.publish(&inner) else { return };
//!         let outer = MyParent {
//!             held: handle.as_ref(),
//!             ..
//!         };
//!         BROADCAST.send(&outer);
//!         // `handle` drops here → fire-and-forget HandleRelease,
//!         // refcount goes to zero, entry stays in the substrate's
//!         // store subject to LRU eviction. Pin if the cached bytes
//!         // need to outlive the local guard.
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

use crate::sink::resolve_sink;
use crate::sync::{WaitError, wait_reply};
use crate::transport::MailTransport;

/// Mailbox name the substrate registers its handle sink under
/// (ADR-0045, namespaced under `aether.sink.*` per ADR-0058). Exposed
/// for actors that want to bypass the typed helpers and build a
/// `Sink<HandlePublish, T>` directly without duplicating the string
/// literal.
pub const HANDLE_SINK_NAME: &str = "aether.sink.handle";

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

/// Typed wrapper around a substrate-side handle id. Carries an RAII
/// drop-release: when the value goes out of scope, a fire-and-forget
/// `HandleRelease` mail tells the substrate to drop one reference.
/// `K` is phantom — the underlying id is type-agnostic on the wire,
/// but `as_ref` pulls `K::ID` so the resulting `Ref::Handle` carries
/// the right kind id.
///
/// Not `Copy` / `Clone`. Cloning a refcounted handle without an
/// inc-ref would cause a double-release on drop; if an actor needs
/// multiple references it pins the handle and reads the raw id via
/// `Handle::id`.
///
/// `T: MailTransport` is on the type so `Drop` and the inherent
/// `release` / `pin` / `unpin` methods can dispatch through the
/// right transport without the caller threading it. `Handle<K, T>`
/// in `aether-component` is aliased to `Handle<K, WasmTransport>`,
/// so user code keeps writing `Handle<MyKind>`.
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
    /// pass the id through host fns the SDK doesn't yet wrap, or to
    /// detach the handle from its RAII guard via
    /// [`core::mem::forget`].
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Drop the publisher's reference. Sync — blocks the actor
    /// thread until the substrate replies, with a 5s default
    /// timeout. Returns `Err(Handle(UnknownHandle))` if the entry
    /// has already been evicted; otherwise `Ok(())`. Use the
    /// implicit `Drop` if you don't care about errors during
    /// teardown.
    pub fn release(self) -> Result<(), SyncHandleError> {
        let id = self.id;
        // Suppress the Drop impl so we don't release twice.
        core::mem::forget(self);
        sync_release::<T>(id)
    }

    /// Pin against LRU eviction. Useful when the publisher wants to
    /// release its local guard (drop the `Handle`) but keep the
    /// cached bytes available — pin first, then drop.
    pub fn pin(&self) -> Result<(), SyncHandleError> {
        sync_pin::<T>(self.id)
    }

    /// Clear the pinned flag. Doesn't drop the entry; only makes
    /// it eligible for LRU eviction once `refcount == 0`.
    pub fn unpin(&self) -> Result<(), SyncHandleError> {
        sync_unpin::<T>(self.id)
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

impl<K, T: MailTransport> Drop for Handle<K, T> {
    fn drop(&mut self) {
        // Fire-and-forget: a panicking wait would poison teardown.
        // The substrate's release dispatch is idempotent — calling
        // it on an already-released id saturates harmlessly.
        let req = HandleRelease {
            id: ::aether_data::HandleId(self.id),
        };
        resolve_sink::<HandleRelease, T>(HANDLE_SINK_NAME).send(&req);
    }
}

/// Postcard-encode `value` and round-trip a `HandlePublish` request
/// through the `"aether.sink.handle"` sink. Returns the typed
/// `Handle<K, T>` on success or a `SyncHandleError` describing the
/// failure (substrate timed out, eviction failed, kind id mismatch
/// on a re-publish, …).
///
/// Shared by `InitCtx::publish` / `Ctx::publish` / `DropCtx::publish`
/// — keep the wire shape and timeouts in one place.
pub fn publish<K: Kind + Serialize, T: MailTransport>(
    value: &K,
) -> Result<Handle<K, T>, SyncHandleError> {
    let bytes = postcard::to_allocvec(value).expect("postcard encode to Vec is infallible");
    let req = HandlePublish {
        kind_id: K::ID,
        bytes,
    };
    resolve_sink::<HandlePublish, T>(HANDLE_SINK_NAME).send(&req);
    let correlation = T::prev_correlation();
    let result: HandlePublishResult =
        wait_reply::<_, SyncHandleError, T>(DEFAULT_TIMEOUT_MS, SMALL_REPLY_CAP, correlation)?;
    match result {
        HandlePublishResult::Ok { id, .. } => Ok(Handle::__from_id(id.0)),
        HandlePublishResult::Err { error, .. } => Err(SyncHandleError::Handle(error)),
    }
}

fn sync_release<T: MailTransport>(id: u64) -> Result<(), SyncHandleError> {
    let req = HandleRelease {
        id: ::aether_data::HandleId(id),
    };
    resolve_sink::<HandleRelease, T>(HANDLE_SINK_NAME).send(&req);
    let correlation = T::prev_correlation();
    let result: HandleReleaseResult =
        wait_reply::<_, SyncHandleError, T>(DEFAULT_TIMEOUT_MS, SMALL_REPLY_CAP, correlation)?;
    match result {
        HandleReleaseResult::Ok { .. } => Ok(()),
        HandleReleaseResult::Err { error, .. } => Err(SyncHandleError::Handle(error)),
    }
}

fn sync_pin<T: MailTransport>(id: u64) -> Result<(), SyncHandleError> {
    let req = HandlePin {
        id: ::aether_data::HandleId(id),
    };
    resolve_sink::<HandlePin, T>(HANDLE_SINK_NAME).send(&req);
    let correlation = T::prev_correlation();
    let result: HandlePinResult =
        wait_reply::<_, SyncHandleError, T>(DEFAULT_TIMEOUT_MS, SMALL_REPLY_CAP, correlation)?;
    match result {
        HandlePinResult::Ok { .. } => Ok(()),
        HandlePinResult::Err { error, .. } => Err(SyncHandleError::Handle(error)),
    }
}

fn sync_unpin<T: MailTransport>(id: u64) -> Result<(), SyncHandleError> {
    let req = HandleUnpin {
        id: ::aether_data::HandleId(id),
    };
    resolve_sink::<HandleUnpin, T>(HANDLE_SINK_NAME).send(&req);
    let correlation = T::prev_correlation();
    let result: HandleUnpinResult =
        wait_reply::<_, SyncHandleError, T>(DEFAULT_TIMEOUT_MS, SMALL_REPLY_CAP, correlation)?;
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

    /// Off-target we can't exercise the FFI host calls (`raw::send_mail`
    /// and friends panic with the host-target stub). Pin the wire
    /// shape by encoding the request kinds and asserting the bytes
    /// round-trip into the same kinds.
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

    /// Stub transport for the host-side `as_ref` test — `Drop` would
    /// call `T::send_mail`, which we suppress via `mem::forget`.
    /// Lives next to the test that uses it; not a public surface.
    struct NoopTransport;
    impl MailTransport for NoopTransport {
        fn send_mail(_: u64, _: u64, _: &[u8], _: u32) -> u32 {
            0
        }
        fn reply_mail(_: u32, _: u64, _: &[u8], _: u32) -> u32 {
            0
        }
        fn save_state(_: u32, _: &[u8]) -> u32 {
            0
        }
        fn wait_reply(_: u64, _: &mut [u8], _: u32, _: u64) -> i32 {
            -1
        }
        fn prev_correlation() -> u64 {
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
        // Suppress Drop's host-fn call.
        core::mem::forget(handle);
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
