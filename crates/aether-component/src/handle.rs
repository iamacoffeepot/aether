//! ADR-0045 typed-handle SDK, guest side. The substrate's
//! `"aether.sink.handle"` sink (ADR-0058) owns a refcounted byte cache;
//! components publish values into
//! it (postcard-encoded) and receive a fresh ephemeral handle id back
//! that they can embed in mail as `Ref::Handle { id, kind_id }`. The
//! substrate's dispatch path resolves the handle to its `Ref::Inline`
//! form before delivery, so recipients see a normal inline value.
//!
//! Wire shape mirrors the io and net helpers (ADR-0041, ADR-0043):
//! components mail one of the four typed request kinds and either fire
//! and forget or block on the paired `*Result` reply.
//!
//! Quick tour:
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
//!         let Some(handle) = ctx.publish(&inner) else { return };
//!         let outer = MyParent {
//!             held: handle.as_ref(),
//!             ...
//!         };
//!         BROADCAST.send(&outer);
//!         // `handle` drops here → fire-and-forget HandleRelease,
//!         // refcount goes to zero, entry stays in the substrate's
//!         // store subject to LRU eviction. Pin if the cached bytes
//!         // need to outlive the local guard.
//!     }
//! }
//! ```

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::marker::PhantomData;

use aether_kinds::{
    HandleError, HandlePin, HandlePinResult, HandlePublish, HandlePublishResult, HandleRelease,
    HandleReleaseResult, HandleUnpin, HandleUnpinResult,
};
use aether_mail::{Kind, Ref};
use serde::Serialize;

use crate::{raw, resolve_sink};

/// Mailbox name the substrate registers its handle sink under
/// (ADR-0045, namespaced under `aether.sink.*` per ADR-0058). Exposed
/// for components that want to bypass the typed helpers and build a
/// `Sink<HandlePublish>` directly without duplicating the string
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
/// inc-ref would cause a double-release on drop; if a component
/// needs multiple references it pins the handle and reads the raw
/// id via `Handle::id`.
pub struct Handle<K> {
    id: u64,
    _k: PhantomData<fn() -> K>,
}

impl<K> Handle<K> {
    /// Raw handle id. Exposed for hand-rolled callers that need to
    /// pass the id through host fns the SDK doesn't yet wrap, or to
    /// detach the handle from its RAII guard via
    /// [`core::mem::forget`].
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Drop the publisher's reference. Sync — blocks the component
    /// thread until the substrate replies, with a 5s default
    /// timeout. Returns `Err(Handle(UnknownHandle))` if the entry
    /// has already been evicted; otherwise `Ok(())`. Use the
    /// implicit `Drop` if you don't care about errors during
    /// teardown.
    pub fn release(self) -> Result<(), SyncHandleError> {
        let id = self.id;
        // Suppress the Drop impl so we don't release twice.
        core::mem::forget(self);
        sync_release(id)
    }

    /// Pin against LRU eviction. Useful when the publisher wants to
    /// release its local guard (drop the `Handle`) but keep the
    /// cached bytes available — pin first, then drop.
    pub fn pin(&self) -> Result<(), SyncHandleError> {
        sync_pin(self.id)
    }

    /// Clear the pinned flag. Doesn't drop the entry; only makes
    /// it eligible for LRU eviction once `refcount == 0`.
    pub fn unpin(&self) -> Result<(), SyncHandleError> {
        sync_unpin(self.id)
    }
}

impl<K: Kind> Handle<K> {
    /// Wire-shaped reference to this handle. Embed in a `Ref<K>`
    /// field on an outgoing kind so the substrate's dispatch path
    /// resolves the inline bytes before delivery. `as_ref` is a
    /// borrow, not a transfer — the `Handle` keeps its refcount on
    /// the publisher side.
    pub fn as_ref(&self) -> Ref<K> {
        Ref::Handle {
            id: self.id,
            kind_id: K::ID,
        }
    }
}

impl<K> Drop for Handle<K> {
    fn drop(&mut self) {
        // Fire-and-forget: a panicking wait would poison teardown.
        // The substrate's release dispatch is idempotent — calling
        // it on an already-released id saturates harmlessly.
        let req = HandleRelease { id: self.id };
        resolve_sink::<HandleRelease>(HANDLE_SINK_NAME).send(&req);
    }
}

/// Postcard-encode `value` and round-trip a `HandlePublish` request
/// through the `"aether.sink.handle"` sink. Returns the typed `Handle<K>` on
/// success or a `SyncHandleError` describing the failure (substrate
/// timed out, eviction failed, kind id mismatch on a re-publish, …).
///
/// Shared by `InitCtx::publish` / `Ctx::publish` / `DropCtx::publish`
/// — keep the wire shape and timeouts in one place.
pub fn publish<K: Kind + Serialize>(value: &K) -> Result<Handle<K>, SyncHandleError> {
    let bytes = postcard::to_allocvec(value).expect("postcard encode to Vec is infallible");
    let req = HandlePublish {
        kind_id: K::ID,
        bytes,
    };
    resolve_sink::<HandlePublish>(HANDLE_SINK_NAME).send(&req);
    let correlation = unsafe { raw::prev_correlation() };
    let result: HandlePublishResult = wait(DEFAULT_TIMEOUT_MS, SMALL_REPLY_CAP, correlation)?;
    match result {
        HandlePublishResult::Ok { id, .. } => Ok(Handle {
            id,
            _k: PhantomData,
        }),
        HandlePublishResult::Err { error, .. } => Err(SyncHandleError::Handle(error)),
    }
}

fn sync_release(id: u64) -> Result<(), SyncHandleError> {
    let req = HandleRelease { id };
    resolve_sink::<HandleRelease>(HANDLE_SINK_NAME).send(&req);
    let correlation = unsafe { raw::prev_correlation() };
    let result: HandleReleaseResult = wait(DEFAULT_TIMEOUT_MS, SMALL_REPLY_CAP, correlation)?;
    match result {
        HandleReleaseResult::Ok { .. } => Ok(()),
        HandleReleaseResult::Err { error, .. } => Err(SyncHandleError::Handle(error)),
    }
}

fn sync_pin(id: u64) -> Result<(), SyncHandleError> {
    let req = HandlePin { id };
    resolve_sink::<HandlePin>(HANDLE_SINK_NAME).send(&req);
    let correlation = unsafe { raw::prev_correlation() };
    let result: HandlePinResult = wait(DEFAULT_TIMEOUT_MS, SMALL_REPLY_CAP, correlation)?;
    match result {
        HandlePinResult::Ok { .. } => Ok(()),
        HandlePinResult::Err { error, .. } => Err(SyncHandleError::Handle(error)),
    }
}

fn sync_unpin(id: u64) -> Result<(), SyncHandleError> {
    let req = HandleUnpin { id };
    resolve_sink::<HandleUnpin>(HANDLE_SINK_NAME).send(&req);
    let correlation = unsafe { raw::prev_correlation() };
    let result: HandleUnpinResult = wait(DEFAULT_TIMEOUT_MS, SMALL_REPLY_CAP, correlation)?;
    match result {
        HandleUnpinResult::Ok { .. } => Ok(()),
        HandleUnpinResult::Err { error, .. } => Err(SyncHandleError::Handle(error)),
    }
}

/// Allocate a `capacity`-sized scratch buffer in guest memory, park
/// on `raw::wait_reply` for a mail of kind `K` with the given
/// `expected_correlation`, and postcard-decode the written bytes.
/// Mirror of io.rs's `wait` — kept private to this module rather
/// than factored out to share, since the buffer cap and timeout
/// defaults are kind-shaped (small replies for handle ops vs the
/// MB-sized buffer io::read_sync allocates).
fn wait<K>(
    timeout_ms: u32,
    capacity: usize,
    expected_correlation: u64,
) -> Result<K, SyncHandleError>
where
    K: Kind + serde::de::DeserializeOwned,
{
    let mut buf: Vec<u8> = vec![0u8; capacity];
    let rc = unsafe {
        raw::wait_reply(
            K::ID,
            buf.as_mut_ptr().addr() as u32,
            buf.len() as u32,
            timeout_ms,
            expected_correlation,
        )
    };
    match rc {
        -1 => Err(SyncHandleError::Timeout),
        -2 => Err(SyncHandleError::BufferTooSmall),
        -3 => Err(SyncHandleError::Cancelled),
        n if n >= 0 => {
            let len = n as usize;
            postcard::from_bytes(&buf[..len]).map_err(|e| SyncHandleError::Decode(format!("{e}")))
        }
        _ => Err(SyncHandleError::Decode(format!(
            "unexpected wait_reply return: {rc}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    /// Off-wasm we can't exercise the FFI host calls (`raw::send_mail`
    /// and friends panic with the host-target stub). Pin the wire
    /// shape by encoding the request kinds and asserting the bytes
    /// round-trip into the same kinds.
    #[derive(Kind, Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[kind(name = "test.handle.payload")]
    #[allow(dead_code)]
    struct Payload {
        seq: u32,
    }

    use aether_mail::{Kind, Schema};

    #[test]
    fn publish_request_bytes_decode_to_handle_publish() {
        let req = HandlePublish {
            kind_id: 0xCAFE,
            bytes: vec![1, 2, 3, 4, 5],
        };
        let encoded = postcard::to_allocvec(&req).unwrap();
        let decoded: HandlePublish = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(decoded.kind_id, 0xCAFE);
        assert_eq!(decoded.bytes, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn release_request_bytes_decode_to_handle_release() {
        let req = HandleRelease { id: 0xDEAD };
        let encoded = postcard::to_allocvec(&req).unwrap();
        let decoded: HandleRelease = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(decoded.id, 0xDEAD);
    }

    #[test]
    fn handle_as_ref_carries_kind_id() {
        // Construct a handle bypassing publish (which needs the
        // FFI). Pin the contract: `as_ref` reads `K::ID` from the
        // type parameter, not from a field, so the kind id matches
        // the type's compile-time constant.
        let handle: Handle<Payload> = Handle {
            id: 42,
            _k: PhantomData,
        };
        match handle.as_ref() {
            Ref::Handle { id, kind_id } => {
                assert_eq!(id, 42);
                assert_eq!(kind_id, Payload::ID);
            }
            Ref::Inline(_) => panic!("as_ref should produce Handle, not Inline"),
        }
        // Suppress Drop's host-fn call.
        core::mem::forget(handle);
    }
}
