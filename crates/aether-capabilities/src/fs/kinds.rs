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

use serde::{Deserialize, Serialize};

// Reverse-lookup `NameEntry` submission (the marker surface below). Native
// — `inventory` doesn't link on wasm — so it rides `fs-runtime`, the same
// gate the native receive side keys on.
#[cfg(feature = "fs-runtime")]
use aether_data::MAILBOX_DOMAIN;
#[cfg(feature = "fs-runtime")]
use aether_data::name_inventory::{NameEntry, inventory};

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
    UnknownTransform(aether_data::TransformId),
    /// The transform at `at_index` has more than one input slot —
    /// it cannot sit in a linear fold where only one input is
    /// available.
    NonLinearArity { at_index: u64, arity: u64 },
    /// The output kind of the transform at `at_index - 1` does not
    /// match the input kind of the transform at `at_index`.
    KindMismatch {
        at_index: u64,
        expected: aether_data::KindId,
        found: aether_data::KindId,
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
    pub transforms: Vec<aether_data::TransformId>,
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
        output_kind: Option<aether_data::KindId>,
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

/// The `aether.fs` capability marker (ADR-0099 addressing). Always-on,
/// no heavy deps: a wasm component addressing the cap via
/// `ctx.actor::<FsCapability>()` and a transport-only chassis both
/// resolve `NAMESPACE` + the per-kind [`HandlesKind`](aether_actor::HandlesKind)
/// markers below without the substrate runtime.
///
/// Two definitions, picked by `fs-runtime`. A transport-only build sees
/// this unit stub — wasm guests never construct a cap, they only address
/// it by type, so an uninhabited marker is enough. An `fs-runtime` build
/// re-exports the state-bearing struct `runtime` defines (it carries the
/// adapter registry + the link-time transform table the receive side
/// needs). Hand-written here because the receive-side `#[actor]` block in
/// `fs/runtime.rs` is `skip_markers` — it emits only the dispatch table,
/// not these markers, so a transport consumer keeps them when the runtime
/// is gated out. (This is the transport/runtime split #2296 establishes as
/// the template for #2282–2292; it replaces what `#[bridge]` emitted off
/// the wasm target.)
#[cfg(not(feature = "fs-runtime"))]
pub struct FsCapability;
#[cfg(feature = "fs-runtime")]
pub use super::runtime::FsCapability;

impl aether_actor::Addressable for FsCapability {
    const NAMESPACE: &'static str = "aether.fs";
    type Resolver = aether_actor::One;
}

impl aether_actor::HandlesKind<Read> for FsCapability {}
impl aether_actor::HandlesKind<Write> for FsCapability {}
impl aether_actor::HandlesKind<Copy> for FsCapability {}
impl aether_actor::HandlesKind<Delete> for FsCapability {}
impl aether_actor::HandlesKind<List> for FsCapability {}
impl aether_actor::HandlesKind<FsFetch> for FsCapability {}

// ADR-0088 §3 reverse-lookup: a singleton's `NAMESPACE` *is* its mailbox
// name, so submit a `NameEntry` letting a `MailboxId` reverse to
// "aether.fs" through the static reverse map. Native-only (the
// `inventory` crate doesn't link on wasm), so it rides `fs-runtime` — the
// feature that carries the native receive side. Replaces the
// `#[bridge(singleton)]` macro-auto-emitted submission.
#[cfg(feature = "fs-runtime")]
inventory::submit! {
    NameEntry {
        domain: MAILBOX_DOMAIN,
        name: "aether.fs",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::Kind;

    /// Kind names are the wire contract — pin every `aether.fs.*`
    /// name so an accidental `#[kind(name = …)]` edit can't silently
    /// re-id a kind. Relocated from `aether-kinds`' `kind_names_are_stable`
    /// when the fs vocabulary moved here (ADR-0121).
    #[test]
    fn kind_names_are_stable() {
        assert_eq!(Read::NAME, "aether.fs.read");
        assert_eq!(ReadResult::NAME, "aether.fs.read_result");
        assert_eq!(Write::NAME, "aether.fs.write");
        assert_eq!(WriteResult::NAME, "aether.fs.write_result");
        assert_eq!(Delete::NAME, "aether.fs.delete");
        assert_eq!(DeleteResult::NAME, "aether.fs.delete_result");
        assert_eq!(List::NAME, "aether.fs.list");
        assert_eq!(ListResult::NAME, "aether.fs.list_result");
        assert_eq!(Copy::NAME, "aether.fs.copy");
        assert_eq!(CopyResult::NAME, "aether.fs.copy_result");
        assert_eq!(FsFetch::NAME, "aether.fs.fetch");
        assert_eq!(FsFetchResult::NAME, "aether.fs.fetch_result");
    }

    // Chassis-level descriptor coverage. The descriptor inventory is
    // link-time and `aether-capabilities` links into every chassis, so
    // these run where both crates link and `aether_kinds::descriptors::all()`
    // sees the fs statics — the standing guarantee that `describe_kinds`
    // still surfaces the fs family after the kinds moved out of
    // `aether-kinds` (ADR-0121). Relocated from `aether-kinds`'
    // relocated from `aether-kinds` when the fs kinds moved out (ADR-0121).
    mod descriptors {
        use super::*;
        use aether_kinds::descriptors::all;

        #[test]
        fn fs_kinds_are_in_descriptor_list() {
            let descs = all();
            let names: Vec<&str> = descs.iter().map(|d| d.name.as_str()).collect();
            assert!(names.contains(&Read::NAME));
            assert!(names.contains(&ReadResult::NAME));
            assert!(names.contains(&Write::NAME));
            assert!(names.contains(&WriteResult::NAME));
            assert!(names.contains(&Copy::NAME));
            assert!(names.contains(&CopyResult::NAME));
            assert!(names.contains(&Delete::NAME));
            assert!(names.contains(&DeleteResult::NAME));
            assert!(names.contains(&List::NAME));
            assert!(names.contains(&ListResult::NAME));
            assert!(names.contains(&FsFetch::NAME));
            assert!(names.contains(&FsFetchResult::NAME));
        }

        #[test]
        fn io_requests_are_structured_schemas() {
            // ADR-0041 §1: request kinds carry `String` namespace + path
            // (and `Vec<u8>` bytes on `Write`), so they must serialize as
            // non-cast structs. Catches an accidental `#[repr(C)]` +
            // `Pod` derive that would silently flip the wire format.
            use aether_data::SchemaType;
            let descs = all();
            for name in [Read::NAME, Write::NAME, Delete::NAME, List::NAME] {
                let d = descs
                    .iter()
                    .find(|d| d.name == name)
                    .expect("test setup: io request kind is registered in descriptor inventory");
                let SchemaType::Struct { repr_c, .. } = &d.schema else {
                    panic!("{name} should be Struct, got {:?}", d.schema);
                };
                assert!(!*repr_c, "{name} contains String/Vec, must be structured");
            }
        }

        #[test]
        fn io_results_are_enum_schemas() {
            // Each reply kind is an Ok/Err enum; `Err` wraps `FsError`,
            // `Ok` shape varies per operation.
            use aether_data::SchemaType;
            let descs = all();
            for name in [
                ReadResult::NAME,
                WriteResult::NAME,
                DeleteResult::NAME,
                ListResult::NAME,
            ] {
                let d = descs
                    .iter()
                    .find(|d| d.name == name)
                    .expect("test setup: io result kind is registered in descriptor inventory");
                assert!(
                    matches!(d.schema, SchemaType::Enum { .. }),
                    "{name} should be Enum, got {:?}",
                    d.schema
                );
            }
        }
    }

    // ADR-0041 I/O kind roundtrips. Request types carry String /
    // Vec<u8>, reply types are Ok/Err enums with the error arm
    // wrapping `FsError`. Kind codec roundtrip proves the derived
    // Serialize/Deserialize agree on the wire for each shape.
    // Relocated from `aether-kinds` when the fs vocabulary moved here.
    mod fs_roundtrips {
        use super::*;
        use aether_data::wire;

        #[test]
        fn read_request_roundtrip() {
            let r = Read {
                namespace: "save".to_string(),
                path: "slot1.bin".to_string(),
            };
            let bytes = r.encode_into_bytes();
            let back: Read =
                Read::decode_from_bytes(&bytes).expect("test setup: kind codec decodes Read");
            assert_eq!(back.namespace, r.namespace);
            assert_eq!(back.path, r.path);
        }

        #[test]
        fn read_result_ok_roundtrip_echoes_request() {
            let r = ReadResult::Ok {
                namespace: "save".to_string(),
                path: "slot.bin".to_string(),
                bytes: vec![1, 2, 3, 4],
            };
            let bytes = r.encode_into_bytes();
            let back: ReadResult = ReadResult::decode_from_bytes(&bytes)
                .expect("test setup: kind codec decodes ReadResult::Ok");
            match back {
                ReadResult::Ok {
                    namespace,
                    path,
                    bytes,
                } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(path, "slot.bin");
                    assert_eq!(bytes, vec![1, 2, 3, 4]);
                }
                ReadResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn read_result_err_roundtrip_echoes_request_and_io_error() {
            let r = ReadResult::Err {
                namespace: "save".to_string(),
                path: "ghost.bin".to_string(),
                error: FsError::NotFound,
            };
            let bytes = r.encode_into_bytes();
            let back: ReadResult = ReadResult::decode_from_bytes(&bytes)
                .expect("test setup: kind codec decodes ReadResult::Err");
            match back {
                ReadResult::Err {
                    namespace,
                    path,
                    error,
                } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(path, "ghost.bin");
                    assert_eq!(error, FsError::NotFound);
                }
                ReadResult::Ok { .. } => panic!("expected Err"),
            }
        }

        #[test]
        fn io_error_adapter_carries_payload() {
            let e = FsError::AdapterError("disk full".to_string());
            let bytes = wire::to_vec(&e).expect("test setup: wire encodes FsError");
            let back: FsError = wire::from_bytes(&bytes).expect("test setup: wire decodes FsError");
            match back {
                FsError::AdapterError(msg) => assert_eq!(msg, "disk full"),
                other => panic!("expected AdapterError, got {other:?}"),
            }
        }

        #[test]
        fn write_request_roundtrip() {
            let w = Write {
                namespace: "save".to_string(),
                path: "state.bin".to_string(),
                bytes: vec![0xde, 0xad, 0xbe, 0xef],
            };
            let bytes = w.encode_into_bytes();
            let back: Write =
                Write::decode_from_bytes(&bytes).expect("test setup: kind codec decodes Write");
            assert_eq!(back.bytes, vec![0xde, 0xad, 0xbe, 0xef]);
        }

        #[test]
        fn list_result_ok_roundtrip_echoes_namespace_and_prefix() {
            let r = ListResult::Ok {
                namespace: "save".to_string(),
                prefix: "slots/".to_string(),
                entries: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            };
            let bytes = r.encode_into_bytes();
            let back: ListResult = ListResult::decode_from_bytes(&bytes)
                .expect("test setup: kind codec decodes ListResult::Ok");
            match back {
                ListResult::Ok {
                    namespace,
                    prefix,
                    entries,
                } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(prefix, "slots/");
                    assert_eq!(entries, vec!["a", "b", "c"]);
                }
                ListResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn write_result_ok_roundtrip_echoes_path_without_bytes() {
            // Deliberately exercises the "no bytes in reply" rule:
            // WriteResult::Ok has no `bytes` field — confirming the
            // wire shape excludes the write payload.
            let r = WriteResult::Ok {
                namespace: "save".to_string(),
                path: "state.bin".to_string(),
            };
            let bytes = r.encode_into_bytes();
            let back: WriteResult = WriteResult::decode_from_bytes(&bytes)
                .expect("test setup: kind codec decodes WriteResult::Ok");
            match back {
                WriteResult::Ok { namespace, path } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(path, "state.bin");
                }
                WriteResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn delete_result_err_roundtrip_echoes_request_and_io_error() {
            let r = DeleteResult::Err {
                namespace: "save".to_string(),
                path: "ghost.bin".to_string(),
                error: FsError::NotFound,
            };
            let bytes = r.encode_into_bytes();
            let back: DeleteResult = DeleteResult::decode_from_bytes(&bytes)
                .expect("test setup: kind codec decodes DeleteResult::Err");
            match back {
                DeleteResult::Err {
                    namespace,
                    path,
                    error,
                } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(path, "ghost.bin");
                    assert_eq!(error, FsError::NotFound);
                }
                DeleteResult::Ok { .. } => panic!("expected Err"),
            }
        }
    }

    // `aether.fs.fetch` kind roundtrips. `FsFetch` carries String +
    // `Vec<TransformId>`; `FsFetchResult` is an Ok/Err enum. Both arms
    // roundtrip through the kind codec. Error arms exercise each
    // `FsFetchError` variant. Relocated from `aether-kinds`.
    mod fs_fetch_roundtrips {
        use super::*;
        use aether_data::wire;

        #[test]
        fn fs_fetch_request_roundtrip() {
            let r = FsFetch {
                namespace: "assets".to_string(),
                path: "model.glb".to_string(),
                transforms: vec![],
            };
            let bytes = r.encode_into_bytes();
            let back: FsFetch =
                FsFetch::decode_from_bytes(&bytes).expect("test setup: kind codec decodes FsFetch");
            assert_eq!(back.namespace, r.namespace);
            assert_eq!(back.path, r.path);
            assert!(back.transforms.is_empty());
        }

        #[test]
        fn fs_fetch_result_ok_raw_roundtrip() {
            let r = FsFetchResult::Ok {
                namespace: "assets".to_string(),
                path: "raw.bin".to_string(),
                output_kind: None,
                data: vec![0xDE, 0xAD, 0xBE, 0xEF],
            };
            let bytes = r.encode_into_bytes();
            let back: FsFetchResult = FsFetchResult::decode_from_bytes(&bytes)
                .expect("test setup: kind codec decodes FsFetchResult::Ok(raw)");
            match back {
                FsFetchResult::Ok {
                    namespace,
                    path,
                    output_kind,
                    data,
                } => {
                    assert_eq!(namespace, "assets");
                    assert_eq!(path, "raw.bin");
                    assert!(output_kind.is_none());
                    assert_eq!(data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
                }
                FsFetchResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn fs_fetch_result_ok_with_kind_roundtrip() {
            use aether_data::KindId;
            let r = FsFetchResult::Ok {
                namespace: "assets".to_string(),
                path: "decoded.bin".to_string(),
                output_kind: Some(KindId(0xABCD_1234_0000_0001)),
                data: vec![1, 2, 3],
            };
            let bytes = r.encode_into_bytes();
            let back: FsFetchResult = FsFetchResult::decode_from_bytes(&bytes)
                .expect("test setup: kind codec decodes FsFetchResult::Ok(with kind)");
            match back {
                FsFetchResult::Ok {
                    output_kind, data, ..
                } => {
                    assert_eq!(output_kind, Some(KindId(0xABCD_1234_0000_0001)));
                    assert_eq!(data, vec![1, 2, 3]);
                }
                FsFetchResult::Err { .. } => panic!("expected Ok"),
            }
        }

        #[test]
        fn fs_fetch_result_err_fs_roundtrip() {
            let r = FsFetchResult::Err {
                namespace: "assets".to_string(),
                path: "missing.bin".to_string(),
                error: FsFetchError::Fs(FsError::NotFound),
            };
            let bytes = r.encode_into_bytes();
            let back: FsFetchResult = FsFetchResult::decode_from_bytes(&bytes)
                .expect("test setup: kind codec decodes FsFetchResult::Err(Fs)");
            match back {
                FsFetchResult::Err {
                    namespace,
                    path,
                    error,
                } => {
                    assert_eq!(namespace, "assets");
                    assert_eq!(path, "missing.bin");
                    assert_eq!(error, FsFetchError::Fs(FsError::NotFound));
                }
                FsFetchResult::Ok { .. } => panic!("expected Err"),
            }
        }

        #[test]
        fn fs_fetch_error_fold_roundtrip() {
            use aether_data::KindId;
            let e = FsFetchError::Fold(FsFoldError::KindMismatch {
                at_index: 1,
                expected: KindId(0x1111_1111_1111_1111),
                found: KindId(0x2222_2222_2222_2222),
            });
            let bytes = wire::to_vec(&e).expect("test setup: wire encodes FsFetchError");
            let back: FsFetchError =
                wire::from_bytes(&bytes).expect("test setup: wire decodes FsFetchError");
            match back {
                FsFetchError::Fold(FsFoldError::KindMismatch {
                    at_index,
                    expected,
                    found,
                }) => {
                    assert_eq!(at_index, 1);
                    assert_eq!(expected, KindId(0x1111_1111_1111_1111));
                    assert_eq!(found, KindId(0x2222_2222_2222_2222));
                }
                other => panic!("expected Fold(KindMismatch), got {other:?}"),
            }
        }

        #[test]
        fn fs_fetch_error_panicked_roundtrip() {
            let e = FsFetchError::Panicked("the transform panicked".to_string());
            let bytes = wire::to_vec(&e).expect("test setup: wire encodes FsFetchError");
            let back: FsFetchError =
                wire::from_bytes(&bytes).expect("test setup: wire decodes FsFetchError");
            match back {
                FsFetchError::Panicked(msg) => assert_eq!(msg, "the transform panicked"),
                other => panic!("expected Panicked, got {other:?}"),
            }
        }
    }
}
