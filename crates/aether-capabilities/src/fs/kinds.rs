//! The `aether.fs.*` mail vocabulary (ADR-0041 + issue 2132). The
//! `aether.fs` cap owns its kinds (ADR-0121): the substrate core never
//! dispatches an `aether.fs.*` kind, so the whole family lives here in
//! the capability crate rather than in `aether-kinds`.
//!
//! Always-on (no `cfg` gate): a wasm component that addresses the fs
//! cap via `ctx.actor::<FsCapability>()` and a render-only chassis
//! both need these types, so they ride the target-agnostic build.
//!
//! ADR-0041 substrate file I/O. Request kinds on the `"aether.fs"`
//! mailbox (read / write / copy / delete / list), paired 1:1 with
//! reply kinds that carry a structured `FsError` on failure. All
//! structured because every request carries `String` namespace/path
//! fields and writes carry `Vec<u8>` bytes.
//!
//! `namespace` is the logical prefix without the `://`: mail carries
//! `"save"`, not `"save://"`. Paths are relative to the namespace
//! root; `..` and absolute prefixes are rejected at the adapter
//! boundary as `FsError::Forbidden`.

use aether_data::{KindId, TransformId};
use serde::{Deserialize, Serialize};

/// Structured failure reason for an I/O request (ADR-0041 §1).
/// Components can pattern-match on the variant to decide whether
/// to retry (`AdapterError`), prompt the user (`NotFound`), or
/// surface a bug (`Forbidden` / `UnknownNamespace`). `AdapterError`
/// preserves backend-specific detail as free-form text — e.g.
/// permission-denied text from the OS, an HTTP status from a
/// future cloud adapter — without locking the enum shape to any
/// one backend.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    Forbidden,
    UnknownNamespace,
    AdapterError(String),
}

/// `aether.fs.read` — request the substrate read a file and reply
/// with its bytes. Mailed to the `"aether.fs"` mailbox; reply
/// lands via `reply_mail` as `ReadResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fs.read")]
pub struct Read {
    pub namespace: String,
    pub path: String,
}

/// Reply to `Read`. Both arms echo the `namespace` + `path` from
/// the originating `Read` so the caller can correlate the reply
/// to its source request without threading a pending-op queue or
/// allocating correlation ids — operation identity comes from the
/// reply kind itself (`aether.fs.read_result`), target identity
/// from the echoed fields. `Ok` carries the full file contents;
/// `Err` carries an `FsError` variant.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fs.read_result")]
pub enum ReadResult {
    Ok {
        namespace: String,
        path: String,
        bytes: Vec<u8>,
    },
    Err {
        namespace: String,
        path: String,
        error: FsError,
    },
}

/// `aether.fs.write` — request the substrate write `bytes` to
/// `namespace://path`. v1's local-file adapter stages to a
/// temporary sibling and `rename`s on success so a crash
/// mid-write leaves either the old contents or the new, never a
/// torn file. Reply: `WriteResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fs.write")]
pub struct Write {
    pub namespace: String,
    pub path: String,
    pub bytes: Vec<u8>,
}

/// Reply to `Write`. Both arms echo `namespace` + `path` for
/// correlation; the request's `bytes` field is *not* echoed so the
/// reply payload stays small even when the write was megabytes
/// (correlation needs the identity of the write, not its contents).
/// `Err` carries an `FsError` — `Forbidden` for read-only
/// namespaces (e.g. `assets://`), `AdapterError` for disk-full /
/// permission / rename failures.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fs.write_result")]
pub enum WriteResult {
    Ok {
        namespace: String,
        path: String,
    },
    Err {
        namespace: String,
        path: String,
        error: FsError,
    },
}

/// Destination address for `aether.fs.copy`: a logical namespace
/// path the substrate resolves through the write adapter registry.
/// Only writable namespaces (`save`, `config`) accept a copy; a
/// read-only namespace (`assets`) replies `Forbidden` and an unknown
/// namespace replies `UnknownNamespace`. `path` is relative to the
/// namespace root — `..` and leading `/` are rejected at the adapter
/// boundary as `Forbidden`, the same rule that governs `aether.fs.write`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
pub struct NamespaceAddr {
    pub namespace: String,
    pub path: String,
}

/// `aether.fs.copy` — copy a file from a raw host filesystem path
/// (`from`) into a writable namespace address (`to`). `from` is an
/// absolute host path the substrate reads directly — it is not
/// namespace-scoped and carries the same trust level as `config_path`
/// / `binary_path` used elsewhere on the substrate. `to` is a
/// namespace-address struct; the write sandbox applies on the `to`
/// side: a read-only or unknown namespace replies with `Forbidden` /
/// `UnknownNamespace`. Reply: `CopyResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fs.copy")]
pub struct Copy {
    pub from: String,
    pub to: NamespaceAddr,
}

/// Reply to `Copy`. Both arms echo `from` + `to` for correlation;
/// no bytes are echoed so the reply stays small regardless of file
/// size. `Err` carries an `FsError` — `NotFound` if `from` is absent
/// on the host, `Forbidden` for a read-only destination namespace or
/// a `to.path` that contains `..` / a leading `/`, `UnknownNamespace`
/// if `to.namespace` was not registered.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fs.copy_result")]
pub enum CopyResult {
    Ok {
        from: String,
        to: NamespaceAddr,
    },
    Err {
        from: String,
        to: NamespaceAddr,
        error: FsError,
    },
}

/// `aether.fs.delete` — request the substrate remove a file.
/// Missing files surface as `NotFound` (not silent success) so
/// callers that care about the distinction can tell; callers
/// that don't ignore it. Reply: `DeleteResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fs.delete")]
pub struct Delete {
    pub namespace: String,
    pub path: String,
}

/// Reply to `Delete`. Both arms echo `namespace` + `path` for
/// correlation. `Ok` on successful removal; `Err` on any
/// adapter-reported failure, including `NotFound` for a file that
/// wasn't there to delete.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fs.delete_result")]
pub enum DeleteResult {
    Ok {
        namespace: String,
        path: String,
    },
    Err {
        namespace: String,
        path: String,
        error: FsError,
    },
}

/// `aether.fs.list` — enumerate entries under `prefix` in
/// `namespace`. Shallow (no recursion) and prefix-filtered —
/// callers that want a tree walk paginate themselves. Empty
/// `prefix` lists the namespace root. Reply: `ListResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fs.list")]
pub struct List {
    pub namespace: String,
    pub prefix: String,
}

/// Reply to `List`. Both arms echo the originating `namespace` +
/// `prefix` for correlation. `Ok` carries the matching entry
/// names — bare file/dir names, not fully-qualified paths — so the
/// caller composes `{prefix}{entry}` when turning an entry back
/// into a read. Empty `entries` means "namespace exists, nothing
/// matched"; `Err { UnknownNamespace }` means the namespace itself
/// wasn't registered.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fs.list_result")]
pub enum ListResult {
    Ok {
        namespace: String,
        prefix: String,
        entries: Vec<String>,
    },
    Err {
        namespace: String,
        prefix: String,
        error: FsError,
    },
}

// `aether.fs.fetch` — the fs actor's transform-pipeline verb (issue
// 2132). Reads a file through an ordered transform pipeline and
// replies with the folded output bytes. An empty transform list
// short-circuits to the raw file bytes. Three supporting schema types
// carry the structured error cases:
//
//   - `FsFoldError` — chain-validation errors (unknown id, non-linear
//     arity, kind mismatch between adjacent transforms).
//   - `FsTransformError` — runtime invocation errors (decode failure,
//     arity mismatch, output overflow) from a single transform stage.
//   - `FsFetchError` — the outer error envelope: file I/O failure,
//     chain validation failure, single-stage invocation failure, or a
//     transform that panicked.

/// Structured chain-validation error for `aether.fs.fetch`. Returned
/// before any transform runs — a `Fold` reply is always a logic bug in
/// the caller's chain construction, not a runtime data error.
///
/// `at_index` is 0-based into the `transforms` list the caller
/// supplied; `expected` / `found` are `KindId`s for `KindMismatch`
/// so the caller can surface the exact type-mismatch without re-
/// inspecting the chain.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum FsFoldError {
    /// No transform with this `TransformId` is registered in the
    /// link-time inventory.
    UnknownTransform(TransformId),
    /// The transform at `at_index` has more than one input slot —
    /// it cannot sit in a linear fold where only one input is
    /// available.
    NonLinearArity { at_index: u64, arity: u64 },
    /// The output kind of the transform at `at_index - 1` does not
    /// match the input kind of the transform at `at_index`.
    KindMismatch {
        at_index: u64,
        expected: KindId,
        found: KindId,
    },
}

/// Structured runtime-invocation error for a single transform stage
/// in `aether.fs.fetch`. Returned when the transform's thunk itself
/// fails — decode, arity, or output overflow — after the chain was
/// already validated.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum FsTransformError {
    /// One input slice didn't decode against its declared input kind.
    /// `slot` is the 0-based slot index.
    InputDecode { slot: u64 },
    /// The number of supplied input slices didn't match the transform's
    /// declared input arity.
    InputArity { expected: u64, actual: u64 },
    /// The encoded output exceeded the executor's output-byte cap.
    OutputOverflow { limit: u64, actual: u64 },
}

/// Structured failure reason for `aether.fs.fetch_result::Err`.
///
/// - `Fs` — the underlying file read failed; the inner `FsError`
///   carries the same variants as `aether.fs.read`.
/// - `Fold` — the transform chain failed validation before any
///   compute ran; the inner `FsFoldError` names the exact rule
///   violated.
/// - `Transform` — a single stage's thunk returned an error during
///   inline execution.
/// - `Panicked` — a transform function panicked; the message is the
///   best-effort string extracted from the panic payload.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum FsFetchError {
    Fs(FsError),
    Fold(FsFoldError),
    Transform(FsTransformError),
    Panicked(String),
}

/// `aether.fs.fetch` — read a file through the fs namespace adapters
/// and run an ordered transform pipeline over its bytes, replying with
/// the folded output. An empty `transforms` list returns the raw file
/// bytes immediately (`output_kind: None`). Mailed to the `"aether.fs"`
/// mailbox; reply lands via `reply_mail` as `FsFetchResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fs.fetch")]
pub struct FsFetch {
    pub namespace: String,
    pub path: String,
    /// Ordered list of transforms to apply. Each `TransformId` names
    /// a link-time `#[transform]` entry (ADR-0048); the chain is
    /// validated for linear composition before any compute runs.
    pub transforms: Vec<TransformId>,
}

/// Reply to `FsFetch`. Both arms echo `namespace` + `path` for
/// correlation. `Ok` carries the folded output bytes (`data`) and
/// the `output_kind` of the last transform (`None` when `transforms`
/// was empty, i.e. a raw-read). `Err` carries a structured
/// `FsFetchError`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fs.fetch_result")]
pub enum FsFetchResult {
    Ok {
        namespace: String,
        path: String,
        /// `None` when the transform list was empty (raw read);
        /// `Some(k)` is the output kind of the last transform in
        /// the chain.
        output_kind: Option<KindId>,
        /// Wire-encoded output: raw file bytes when `output_kind` is
        /// `None`, or the last transform's encoded output value.
        data: Vec<u8>,
    },
    Err {
        namespace: String,
        path: String,
        error: FsFetchError,
    },
}
