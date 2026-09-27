//! Append-only, single-writer log of typed events over one journal root, and
//! the `aether.bloomery.journal` actor that owns it (ADR-0220).
//!
//! The identity half of the ADR-0122 split is always on: the
//! [`JournalActor`] marker, the page and watcher limits callers observe
//! ([`MAX_READ_EVENTS`], [`MAX_HEAD_WATCHERS`]), and the artifact vocabulary
//! re-exported from `aether_bloomery_kinds`. The runtime half, behind the
//! `runtime` feature, is the event log in `journal.sqlite`, the
//! digest-named artifact files, the streaming artifact store, and the actor's
//! state; its module documentation describes the storage design.
//!
//! `no_std` without the `runtime` feature, so a wasm guest can name the
//! journal actor and send it kind-checked mail.

#![cfg_attr(not(feature = "runtime"), no_std)]

pub use aether_bloomery_kinds::{
    ArtifactHasher, DecodeError, Digest, Entry, OpaqueBytes, Ref, Seq, Utf8Text, artifact_blob, artifact_digest,
    artifact_prefix, hash_bytes,
};

#[cfg(feature = "runtime")]
pub use runtime::{
    AppendError, ArtifactBatch, ArtifactStore, Batch, BatchError, BlobFile, Clock, Closure, Draft, DraftError,
    GetError, Journal, JournalError, JournalIdentity, JournalReader, ReadCacheBudget, SystemClock, VerifiedBlob,
    split_artifact,
};

/// Maximum number of entries one read mail can return.
pub const MAX_READ_EVENTS: u32 = 128;

/// Maximum number of parked `WatchHead` replies. Journal writes and watches
/// are unauthenticated (ADR-0226 Consequences), and the one expected watcher
/// is the ADR-0226 driver, so this bounds an otherwise easy leak rather than
/// anticipating real concurrent demand.
pub const MAX_HEAD_WATCHERS: usize = 64;

/// `aether.bloomery.journal` actor **identity** (ADR-0122 split): one
/// independently named journal owner over its own journal root. A ZST
/// carrying only the addressing, the per-handler `HandlesKind` markers, the
/// contract rows, and the name-inventory row `#[actor]` emits always-on. It is
/// an instanced root, one per journal unit. The state-bearing runtime (the open
/// journal, its parked watchers, read queues, and read cache) lives behind
/// `feature = "runtime"`.
#[actor(instanced, root)]
pub struct JournalActor;

use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;
