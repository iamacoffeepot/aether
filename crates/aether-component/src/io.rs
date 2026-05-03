//! ADR-0041 substrate file I/O, guest side. Thin helpers that build
//! the typed request kinds (`Read` / `Write` / `Delete` / `List`),
//! postcard-encode them, and send to the substrate's
//! `"aether.sink.io"` sink (ADR-0058) —
//! so component authors don't have to know the mailbox-name
//! convention, import the kinds manually, or hand-roll the encode.
//!
//! Fire-and-forget from the guest's perspective. Replies arrive
//! asynchronously as `ReadResult` / `WriteResult` / `DeleteResult` /
//! `ListResult` mail addressed at the calling component's mailbox
//! — declare a `#[handler]` for each reply kind the component
//! cares about and match on the `Ok` / `Err` variant. Example:
//!
//! ```ignore
//! use aether_component::io;
//! use aether_kinds::{ReadResult, IoError};
//!
//! #[handlers]
//! impl Component for MySaveLoader {
//!     fn init(ctx: &mut InitCtx<'_>) -> Self {
//!         io::read("save", "slot1.bin");
//!         Self { /* ... */ }
//!     }
//!
//!     #[handler]
//!     fn on_read_result(&mut self, _ctx: &mut Ctx<'_>, r: ReadResult) {
//!         match r {
//!             ReadResult::Ok { bytes } => self.load_from(&bytes),
//!             ReadResult::Err { error: IoError::NotFound } => self.boot_fresh(),
//!             ReadResult::Err { error } => tracing::error!(?error),
//!         }
//!     }
//! }
//! ```
//!
//! Components that make multiple in-flight requests correlate
//! replies by context (current phase, request queue, etc.) — the
//! reply kinds don't carry a request id today. If that becomes
//! painful, ADR-0041's parked sync-mail primitive removes the
//! state-machine bookkeeping; not v1.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use aether_actor::{MailTransport, WaitError, wait_reply};
use aether_data::Kind;
use aether_kinds::{
    Delete, DeleteResult, IoError, List, ListResult, Read, ReadResult, Write, WriteResult,
};

use crate::{WasmTransport, resolve_sink};

/// Mailbox name the substrate registers its I/O sink under (ADR-0041,
/// namespaced under `aether.sink.*` per ADR-0058). Exposed so
/// components that want to bypass the typed helpers and build a
/// `Sink<K>` directly can do so without duplicating the string literal.
pub const IO_MAILBOX_NAME: &str = "aether.sink.io";

/// Send an `aether.io.read` request to the substrate. The reply
/// arrives as a `ReadResult` on the calling component's mailbox —
/// wire a `#[handler] fn on_read_result(...)` to consume it.
///
/// `namespace` is the short prefix without the `://` (e.g. `"save"`,
/// `"assets"`, `"config"`). `path` is relative to the namespace root;
/// `..` and absolute prefixes are rejected at the adapter with
/// `IoError::Forbidden`.
pub fn read(namespace: &str, path: &str) {
    send(&Read {
        namespace: namespace.to_string(),
        path: path.to_string(),
    });
}

/// Send an `aether.io.write` request. Reply arrives as `WriteResult`.
/// The adapter stages writes via tmp+rename for atomicity; on success
/// the file contains `bytes`, on failure the old contents (if any)
/// are preserved intact.
pub fn write(namespace: &str, path: &str, bytes: &[u8]) {
    send(&Write {
        namespace: namespace.to_string(),
        path: path.to_string(),
        bytes: bytes.to_vec(),
    });
}

/// Send an `aether.io.delete` request. Reply arrives as `DeleteResult`.
/// Missing files surface as `IoError::NotFound` rather than silent
/// success, so callers that care about the distinction can tell;
/// callers that don't ignore the error.
pub fn delete(namespace: &str, path: &str) {
    send(&Delete {
        namespace: namespace.to_string(),
        path: path.to_string(),
    });
}

/// Send an `aether.io.list` request. Reply arrives as `ListResult`
/// carrying bare entry names (not fully-qualified paths) under
/// `prefix` in `namespace`. Empty `prefix` lists the namespace root.
/// Shallow (no recursion) — for a tree walk, paginate list calls
/// against deeper prefixes.
pub fn list(namespace: &str, prefix: &str) {
    send(&List {
        namespace: namespace.to_string(),
        prefix: prefix.to_string(),
    });
}

fn send<K: Kind>(value: &K) -> u64 {
    resolve_sink::<K>(IO_MAILBOX_NAME).send(value);
    // ADR-0042: capture the correlation the substrate just minted so
    // the sync wrappers can filter on it. For the async helpers
    // (`read` / `write` / `delete` / `list`), this is harmless noise —
    // they don't wait.
    WasmTransport::prev_correlation()
}

/// ADR-0042: errors surfaced by the `*_sync` wrappers. The first three
/// map 1:1 onto the host fn's return sentinels (`-1` / `-2` / `-3`);
/// `Io` carries an I/O-layer failure (ADR-0041's `IoError` taxonomy);
/// `Decode` covers the unlikely case where the reply bytes don't
/// postcard-decode — a substrate/guest schema divergence rather than
/// a usage error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncIoError {
    Timeout,
    BufferTooSmall,
    Cancelled,
    Io(IoError),
    Decode(String),
}

/// Default reply-buffer capacity for `read_sync`. Sized for save
/// and config files; streaming-asset workloads should not ride this
/// path (ADR-0041 flags a zero-copy host fn as the future answer).
/// Oversized replies return `SyncIoError::BufferTooSmall` and stay
/// parked on overflow so a caller can retry with a bigger buffer
/// via the raw host fn.
const READ_REPLY_CAP: usize = 8 * 1024 * 1024;
/// Reply capacity for `write_sync` / `delete_sync`. The reply is
/// `{Write,Delete}Result::Ok{namespace,path}` or `Err{error}` — a
/// few hundred bytes at most even with long paths.
const SMALL_REPLY_CAP: usize = 4 * 1024;
/// Reply capacity for `list_sync`. Entry lists can grow; default
/// is generous without being wasteful.
const LIST_REPLY_CAP: usize = 256 * 1024;

/// Synchronous counterpart to [`read`]. Sends the request, parks the
/// component thread until the reply arrives or `timeout_ms` elapses,
/// decodes `ReadResult`, and returns the bytes. Blocks only the
/// calling component; other components on the same substrate are
/// unaffected.
///
/// Uses ADR-0042 correlation to filter out stale replies: the
/// substrate mints a fresh id for each `send_mail`, auto-echoes it
/// on the reply, and the wrapper waits for *its* correlation
/// specifically — not the first `ReadResult` to arrive. This fixes
/// a footgun where a sync call could consume a prior async
/// `io::read` reply that happened to be queued.
pub fn read_sync(namespace: &str, path: &str, timeout_ms: u32) -> Result<Vec<u8>, SyncIoError> {
    let correlation = send(&Read {
        namespace: namespace.to_string(),
        path: path.to_string(),
    });
    let reply: ReadResult = wait_reply::<ReadResult, SyncIoError, WasmTransport>(
        timeout_ms,
        READ_REPLY_CAP,
        correlation,
    )?;
    match reply {
        ReadResult::Ok { bytes, .. } => Ok(bytes),
        ReadResult::Err { error, .. } => Err(SyncIoError::Io(error)),
    }
}

/// Synchronous counterpart to [`write`]. Returns `Ok(())` when the
/// adapter persisted the bytes (tmp+rename atomically under the
/// local-file adapter); `Err` carries the reason.
pub fn write_sync(
    namespace: &str,
    path: &str,
    bytes: &[u8],
    timeout_ms: u32,
) -> Result<(), SyncIoError> {
    let correlation = send(&Write {
        namespace: namespace.to_string(),
        path: path.to_string(),
        bytes: bytes.to_vec(),
    });
    match wait_reply::<WriteResult, SyncIoError, WasmTransport>(
        timeout_ms,
        SMALL_REPLY_CAP,
        correlation,
    )? {
        WriteResult::Ok { .. } => Ok(()),
        WriteResult::Err { error, .. } => Err(SyncIoError::Io(error)),
    }
}

/// Synchronous counterpart to [`delete`]. Missing files surface as
/// `Err(SyncIoError::Io(IoError::NotFound))` — callers that don't
/// care about the distinction can `.ok()` and discard.
pub fn delete_sync(namespace: &str, path: &str, timeout_ms: u32) -> Result<(), SyncIoError> {
    let correlation = send(&Delete {
        namespace: namespace.to_string(),
        path: path.to_string(),
    });
    match wait_reply::<DeleteResult, SyncIoError, WasmTransport>(
        timeout_ms,
        SMALL_REPLY_CAP,
        correlation,
    )? {
        DeleteResult::Ok { .. } => Ok(()),
        DeleteResult::Err { error, .. } => Err(SyncIoError::Io(error)),
    }
}

/// Synchronous counterpart to [`list`]. Returns the bare entry
/// names under `prefix` in `namespace` — compose
/// `{prefix}{entry}` to turn one back into a readable path.
pub fn list_sync(
    namespace: &str,
    prefix: &str,
    timeout_ms: u32,
) -> Result<Vec<String>, SyncIoError> {
    let correlation = send(&List {
        namespace: namespace.to_string(),
        prefix: prefix.to_string(),
    });
    match wait_reply::<ListResult, SyncIoError, WasmTransport>(
        timeout_ms,
        LIST_REPLY_CAP,
        correlation,
    )? {
        ListResult::Ok { entries, .. } => Ok(entries),
        ListResult::Err { error, .. } => Err(SyncIoError::Io(error)),
    }
}

impl WaitError for SyncIoError {
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
    use aether_kinds::{
        Delete as DeleteKind, List as ListKind, Read as ReadKind, Write as WriteKind,
    };
    use serde::Serialize;

    // The helpers' host-fn send path panics off-wasm (raw::send_mail
    // has a host-target stub). What we *can* test on host is the
    // encode step — the bytes the helper would have pushed through
    // the FFI should postcard-roundtrip into the same request kind.
    // That proves the wire shape stays identical to what the ADR-0041
    // substrate dispatcher decodes.

    // The request kinds derive `Serialize`/`Deserialize`, so postcard
    // roundtrip is what the substrate dispatcher observes on the wire.
    // Off-wasm we can't exercise the host fn, but we can prove the
    // encode shape survives a decode — a canary against an accidental
    // schema drift between what the guest builds and what the adapter
    // parses in `SinkHandler::handle`.

    fn postcard_bytes<T: Serialize>(value: &T) -> Vec<u8> {
        postcard::to_allocvec(value).unwrap()
    }

    #[test]
    fn read_encodes_to_postcard_read() {
        let encoded = postcard_bytes(&ReadKind {
            namespace: "save".to_string(),
            path: "slot.bin".to_string(),
        });
        let back: ReadKind = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(back.namespace, "save");
        assert_eq!(back.path, "slot.bin");
    }

    #[test]
    fn write_encodes_to_postcard_write() {
        let encoded = postcard_bytes(&WriteKind {
            namespace: "save".to_string(),
            path: "state.bin".to_string(),
            bytes: alloc::vec![1, 2, 3, 4],
        });
        let back: WriteKind = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(back.bytes, alloc::vec![1, 2, 3, 4]);
    }

    #[test]
    fn delete_encodes_to_postcard_delete() {
        let encoded = postcard_bytes(&DeleteKind {
            namespace: "save".to_string(),
            path: "ghost.bin".to_string(),
        });
        let back: DeleteKind = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(back.path, "ghost.bin");
    }

    #[test]
    fn list_encodes_to_postcard_list() {
        let encoded = postcard_bytes(&ListKind {
            namespace: "save".to_string(),
            prefix: "".to_string(),
        });
        let back: ListKind = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(back.namespace, "save");
        assert_eq!(back.prefix, "");
    }

    #[test]
    fn io_mailbox_name_is_namespaced() {
        // ADR-0058: chassis sinks live under `aether.sink.*`. Regression
        // guard so a future "simplification" that drops the prefix
        // collides with the user-space `"io"` namespace.
        assert_eq!(IO_MAILBOX_NAME, "aether.sink.io");
        assert_ne!(IO_MAILBOX_NAME, "io");
        assert_ne!(IO_MAILBOX_NAME, "aether.io");
    }

    /// `SyncIoError` is the [`WaitError`] impl the IO
    /// `*_sync` wrappers use, so the four trait constructors must
    /// land on the matching enum variants. A future enum reorder
    /// would otherwise silently re-route a sentinel rc to the wrong
    /// failure mode.
    #[test]
    fn wait_error_mapping_for_sync_io_error() {
        use WaitError;
        assert_eq!(<SyncIoError as WaitError>::timeout(), SyncIoError::Timeout);
        assert_eq!(
            <SyncIoError as WaitError>::buffer_too_small(),
            SyncIoError::BufferTooSmall
        );
        assert_eq!(
            <SyncIoError as WaitError>::cancelled(),
            SyncIoError::Cancelled
        );
        assert_eq!(
            <SyncIoError as WaitError>::decode("schema drift".to_string()),
            SyncIoError::Decode("schema drift".to_string())
        );
    }
}
