//! The `aether.artifacts.*` mail vocabulary (ADR-0149 §The boundary).
//!
//! The artifacts port is "digest-addressed bytes; canonical record, never
//! evicted." Two request kinds on the `"aether.artifacts"` mailbox — `put`
//! (digest-address bytes plus their derivation-DAG parents) and `get` (by
//! digest) — each paired 1:1 with an `Ok`/`Err` reply kind carrying a
//! structured [`ArtifactsError`] on failure. Modeled on the `aether.fs.*`
//! reply-enum template (`crates/aether-capabilities/src/fs/kinds.rs`),
//! including its `_result` reply-name suffix.
//!
//! Always-on (no `cfg` gate): a peer that addresses the cap via
//! `ctx.actor::<ArtifactsCapability>()` needs these types on the
//! target-agnostic build, so the whole family lives here rather than behind
//! the `runtime` feature.

use serde::{Deserialize, Serialize};

/// Structured failure reason for an artifacts request. `NotFound` is a `get`
/// of a digest the store does not hold; `AdapterError` preserves backend
/// detail (a disk read/write failure) as free-form text. Mirrors `FsError`'s
/// shape — an inner reply-payload enum, not a mailbox-addressable kind, so it
/// derives only `Schema` (plus serde), never `Kind`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ArtifactsError {
    NotFound,
    AdapterError(String),
}

/// `aether.artifacts.put` — sha256-address `bytes`, store them with their
/// declared derivation-DAG `parents` as sidecar metadata, and reply the
/// content digest. Idempotent: identical bytes address to the same digest and
/// dedup (the content store's existing behavior). The store records `parents`
/// as metadata and does not validate their existence — derivation-DAG
/// integrity is a reducer invariant over the journal (ADR-0149), not a
/// byte-store gate. Reply: [`PutResult`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.artifacts.put")]
pub struct Put {
    pub bytes: Vec<u8>,
    pub parents: Vec<String>,
}

/// Reply to [`Put`]. `Ok` carries the sha256 hex `digest` the bytes stored
/// under; `Err` carries an [`ArtifactsError`] — `AdapterError` when the bytes
/// could not be persisted.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.artifacts.put_result")]
pub enum PutResult {
    Ok { digest: String },
    Err { error: ArtifactsError },
}

/// `aether.artifacts.get` — look up an artifact by its content `digest` and
/// reply the bytes plus its recorded derivation-DAG parents. Reply:
/// [`GetResult`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.artifacts.get")]
pub struct Get {
    pub digest: String,
}

/// Reply to [`Get`]. Both arms echo the `digest` from the originating `Get` as
/// domain context. `Ok` carries the full bytes and the recorded `parents`;
/// `Err` carries an [`ArtifactsError`] — `NotFound` for an absent digest,
/// `AdapterError` for a disk read failure of an indexed entry.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.artifacts.get_result")]
pub enum GetResult {
    Ok { digest: String, bytes: Vec<u8>, parents: Vec<String> },
    Err { digest: String, error: ArtifactsError },
}
