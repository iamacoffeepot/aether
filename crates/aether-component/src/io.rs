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

use alloc::string::ToString;
use alloc::vec::Vec;

use aether_kinds::{Delete, List, Read, Write};
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
