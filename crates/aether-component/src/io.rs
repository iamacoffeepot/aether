//! ADR-0041 substrate file I/O, guest side. Thin helpers that build
//! the typed request kinds (`Read` / `Write` / `Delete` / `List`),
//! postcard-encode them, and send to the substrate's `"io"` sink —
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

use aether_kinds::{
    Delete, DeleteResult, IoError, List, ListResult, Read, ReadResult, Write, WriteResult,
};
use aether_mail::{Kind, mailbox_id_from_name};
use serde::Serialize;

use crate::raw;

/// Short mailbox name the substrate registers its I/O sink under
/// (ADR-0041). Exposed so components that want to bypass the
/// typed helpers and build a `Sink<K>` directly can do so without
/// duplicating the string literal.
pub const IO_MAILBOX_NAME: &str = "io";

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

fn send<K: Kind + Serialize>(value: &K) {
    let bytes = encode_postcard(value);
    unsafe {
        raw::send_mail(
            mailbox_id_from_name(IO_MAILBOX_NAME),
            K::ID,
            bytes.as_ptr().addr() as u32,
            bytes.len() as u32,
            1,
        );
    }
}

fn encode_postcard<K: Serialize>(value: &K) -> Vec<u8> {
    // postcard encode to Vec is infallible for well-formed serde
    // impls — the kinds here all derive Serialize via
    // `#[derive(Serialize)]`, so the `expect` is a "this can't
    // fail" guard, not a recoverable branch.
    postcard::to_allocvec(value).expect("postcard encode to Vec is infallible")
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
/// Use for multi-step I/O where the state-machine cost of the
/// async [`read`] + `#[handler] fn on_read_result` shape outweighs
/// the single-tracked-component cost of a sync wait. ADR-0042.
pub fn read_sync(namespace: &str, path: &str, timeout_ms: u32) -> Result<Vec<u8>, SyncIoError> {
    send(&Read {
        namespace: namespace.to_string(),
        path: path.to_string(),
    });
    let reply: ReadResult = wait::<ReadResult>(timeout_ms, READ_REPLY_CAP)?;
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
    send(&Write {
        namespace: namespace.to_string(),
        path: path.to_string(),
        bytes: bytes.to_vec(),
    });
    match wait::<WriteResult>(timeout_ms, SMALL_REPLY_CAP)? {
        WriteResult::Ok { .. } => Ok(()),
        WriteResult::Err { error, .. } => Err(SyncIoError::Io(error)),
    }
}

/// Synchronous counterpart to [`delete`]. Missing files surface as
/// `Err(SyncIoError::Io(IoError::NotFound))` — callers that don't
/// care about the distinction can `.ok()` and discard.
pub fn delete_sync(namespace: &str, path: &str, timeout_ms: u32) -> Result<(), SyncIoError> {
    send(&Delete {
        namespace: namespace.to_string(),
        path: path.to_string(),
    });
    match wait::<DeleteResult>(timeout_ms, SMALL_REPLY_CAP)? {
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
    send(&List {
        namespace: namespace.to_string(),
        prefix: prefix.to_string(),
    });
    match wait::<ListResult>(timeout_ms, LIST_REPLY_CAP)? {
        ListResult::Ok { entries, .. } => Ok(entries),
        ListResult::Err { error, .. } => Err(SyncIoError::Io(error)),
    }
}

/// Allocate a `capacity`-sized scratch buffer in guest memory, park
/// on `raw::wait_reply` for a mail of kind `K`, and postcard-decode
/// the written bytes. Shared by every `*_sync` wrapper.
fn wait<K>(timeout_ms: u32, capacity: usize) -> Result<K, SyncIoError>
where
    K: Kind + serde::de::DeserializeOwned,
{
    let mut buf: Vec<u8> = alloc::vec![0u8; capacity];
    let rc = unsafe {
        raw::wait_reply(
            K::ID,
            buf.as_mut_ptr().addr() as u32,
            buf.len() as u32,
            timeout_ms,
        )
    };
    match rc {
        -1 => Err(SyncIoError::Timeout),
        -2 => Err(SyncIoError::BufferTooSmall),
        -3 => Err(SyncIoError::Cancelled),
        n if n >= 0 => {
            let len = n as usize;
            postcard::from_bytes(&buf[..len])
                .map_err(|e| SyncIoError::Decode(alloc::format!("{e}")))
        }
        _ => Err(SyncIoError::Decode(alloc::format!(
            "unexpected wait_reply return: {rc}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_kinds::{
        Delete as DeleteKind, List as ListKind, Read as ReadKind, Write as WriteKind,
    };

    // The helpers' host-fn send path panics off-wasm (raw::send_mail
    // has a host-target stub). What we *can* test on host is the
    // encode step — the bytes the helper would have pushed through
    // the FFI should postcard-roundtrip into the same request kind.
    // That proves the wire shape stays identical to what the ADR-0041
    // substrate dispatcher decodes.

    #[test]
    fn read_encodes_to_postcard_read() {
        let encoded = encode_postcard(&ReadKind {
            namespace: "save".to_string(),
            path: "slot.bin".to_string(),
        });
        let back: ReadKind = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(back.namespace, "save");
        assert_eq!(back.path, "slot.bin");
    }

    #[test]
    fn write_encodes_to_postcard_write() {
        let encoded = encode_postcard(&WriteKind {
            namespace: "save".to_string(),
            path: "state.bin".to_string(),
            bytes: alloc::vec![1, 2, 3, 4],
        });
        let back: WriteKind = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(back.bytes, alloc::vec![1, 2, 3, 4]);
    }

    #[test]
    fn delete_encodes_to_postcard_delete() {
        let encoded = encode_postcard(&DeleteKind {
            namespace: "save".to_string(),
            path: "ghost.bin".to_string(),
        });
        let back: DeleteKind = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(back.path, "ghost.bin");
    }

    #[test]
    fn list_encodes_to_postcard_list() {
        let encoded = encode_postcard(&ListKind {
            namespace: "save".to_string(),
            prefix: "".to_string(),
        });
        let back: ListKind = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(back.namespace, "save");
        assert_eq!(back.prefix, "");
    }

    #[test]
    fn io_mailbox_name_is_short() {
        // Regression guard for the sink-names-vs-kind-prefixes
        // footgun. The helper must address the sink by its short
        // name, never by the kind namespace prefix.
        assert_eq!(IO_MAILBOX_NAME, "io");
        assert_ne!(IO_MAILBOX_NAME, "aether.io");
    }
}
