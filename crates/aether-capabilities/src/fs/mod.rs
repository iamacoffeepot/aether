//! `aether.fs` cap. Owns the full ADR-0041 stack — its mail kinds
//! ([`kinds`], ADR-0121), the [`FileAdapter`] trait + [`LocalFileAdapter`]
//! (`adapter`), the [`AdapterRegistry`] + env-driven [`NamespaceRoots`]
//! (`registry`), and the [`FsCapability`] itself. Chassis mains
//! resolve a [`NamespaceRoots`] (typically via `NamespaceRoots::from_env`)
//! and pass it through `with_actor::<FsCapability>(roots)` — `init`
//! builds the adapter registry and returns `BootError` on failure (per
//! ADR-0063 fail-fast).
//!
//! Threading: the actor dispatcher thread pulls envelopes from the
//! `aether.fs` mailbox and routes them through the macro-emitted
//! `NativeDispatch::__aether_dispatch_envelope`. Adapter calls run
//! synchronously on that thread; ADR-0041 flagged a future host-fn
//! fast path for asset-sized streaming.

pub mod kinds;

mod adapter;
mod registry;

pub use kinds::*;

pub use adapter::{FileAdapter, FsResult, LocalFileAdapter};
pub use registry::{AdapterRegistry, NamespaceRoots, build_registry};
// The `Config` derive on `NamespaceRoots` emits these sibling types in
// `registry`; chassis CLI / boot wiring addresses them through the
// `fs::` path, so re-export them here (native-only — the derive is
// feature-gated). Inherent shims (`from_env` / `from_argv_then_env` /
// `into_layer`) ride the type and need no re-export.
#[cfg(feature = "native")]
pub use registry::{NamespaceRootsLayer, NamespaceRootsOverlay};

// Handler-signature kinds resolve at file root through the `pub use
// kinds::*` re-export above (the `#[bridge]` emits `impl HandlesKind<K>
// for X {}` markers as siblings of the mod, always-on, outside the cfg
// gate).
use aether_actor::WasmActorMailbox;
#[cfg(not(target_arch = "wasm32"))]
use aether_substrate::actor::native::NativeActorMailbox;

/// Sender-side facade for actors addressed via
/// `ctx.actor::<FsCapability>()`.
///
/// Lifts the cap-shaped methods (`read(ns, path)`, `write(ns, path,
/// bytes)`, ...) one indirection above the raw
/// `.send(&Read { ns, path })` so component code stops reconstructing
/// the kind struct (and the `.into()` conversions on every field) at
/// every call site. The cap module owns receive-side
/// ([`FsCapability`]) AND send-side ([`FsMailboxExt`]) so future
/// kind additions land both surfaces in one place.
///
/// Impl'd for both transports `ctx.actor::<FsCapability>()` can
/// return:
///
/// - [`WasmActorMailbox<FsCapability>`] — always-on, for wasm-component
///   callers.
/// - [`NativeActorMailbox<'_, FsCapability>`] — native cap-to-cap
///   sends, gated on `#[cfg(not(target_arch = "wasm32"))]`.
///
/// All methods are fire-and-forget. Replies arrive as
/// `aether.fs.read_result` / `aether.fs.write_result` /
/// `aether.fs.delete_result` / `aether.fs.list_result`, correlated
/// by the echoed `namespace` + `path` (or `prefix`) per ADR-0041.
/// Synchronous `read_sync` / `write_sync` wrappers were on the
/// original issue 580 sketch — parked as a follow-up so this PR
/// stays mechanical.
///
/// The generic escape hatch is unaffected: `mailbox.send(&CustomKind { .. })`
/// still works for any `K` the cap declares via `HandlesKind<K>`,
/// since `send` is an inherent method on the underlying mailbox type.
pub trait FsMailboxExt {
    /// Mail `aether.fs.read { namespace, path }` to the cap.
    fn read(&self, namespace: &str, path: &str);

    /// Mail `aether.fs.write { namespace, path, bytes }` to the cap.
    /// The reply echoes `namespace` + `path` only (bytes are omitted
    /// from the echo so a megabyte write doesn't produce a megabyte
    /// reply).
    fn write(&self, namespace: &str, path: &str, bytes: &[u8]);

    /// Mail `aether.fs.delete { namespace, path }` to the cap.
    fn delete(&self, namespace: &str, path: &str);

    /// Mail `aether.fs.list { namespace, prefix }` to the cap. The
    /// reply enumerates entries under the prefix.
    fn list(&self, namespace: &str, prefix: &str);

    /// Mail `aether.fs.copy { from, to }` to the cap. `from` is a raw
    /// host filesystem path; `to` is a namespace-address destination. The
    /// bytes flow host → namespace inside the substrate — they never ride
    /// the wire. The reply echoes `from` + `to` without bytes, so a
    /// large-file copy produces a small ack.
    fn copy(&self, from: &str, to_namespace: &str, to_path: &str);
}

impl FsMailboxExt for WasmActorMailbox<'_, FsCapability> {
    fn read(&self, namespace: &str, path: &str) {
        self.send(&Read {
            namespace: namespace.into(),
            path: path.into(),
        });
    }
    //noinspection DuplicatedCode
    fn write(&self, namespace: &str, path: &str, bytes: &[u8]) {
        self.send(&Write {
            namespace: namespace.into(),
            path: path.into(),
            bytes: bytes.to_vec(),
        });
    }
    fn delete(&self, namespace: &str, path: &str) {
        self.send(&Delete {
            namespace: namespace.into(),
            path: path.into(),
        });
    }
    fn list(&self, namespace: &str, prefix: &str) {
        self.send(&List {
            namespace: namespace.into(),
            prefix: prefix.into(),
        });
    }
    //noinspection DuplicatedCode
    fn copy(&self, from: &str, to_namespace: &str, to_path: &str) {
        self.send(&Copy {
            from: from.into(),
            to: NamespaceAddr {
                namespace: to_namespace.into(),
                path: to_path.into(),
            },
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl FsMailboxExt for NativeActorMailbox<'_, FsCapability> {
    fn read(&self, namespace: &str, path: &str) {
        self.send(&Read {
            namespace: namespace.into(),
            path: path.into(),
        });
    }
    //noinspection DuplicatedCode
    fn write(&self, namespace: &str, path: &str, bytes: &[u8]) {
        self.send(&Write {
            namespace: namespace.into(),
            path: path.into(),
            bytes: bytes.to_vec(),
        });
    }
    fn delete(&self, namespace: &str, path: &str) {
        self.send(&Delete {
            namespace: namespace.into(),
            path: path.into(),
        });
    }
    fn list(&self, namespace: &str, prefix: &str) {
        self.send(&List {
            namespace: namespace.into(),
            prefix: prefix.into(),
        });
    }
    //noinspection DuplicatedCode
    fn copy(&self, from: &str, to_namespace: &str, to_path: &str) {
        self.send(&Copy {
            from: from.into(),
            to: NamespaceAddr {
                namespace: to_namespace.into(),
                path: to_path.into(),
            },
        });
    }
}

#[aether_actor::bridge(singleton)]
mod native {
    use std::any::Any;
    use std::fs;
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::Arc;

    use super::adapter::fs_error_from_std;
    use super::{
        AdapterRegistry, Copy, CopyResult, Delete, DeleteResult, FsError, FsFetch, FsFetchError,
        FsFetchResult, FsFoldError, FsTransformError, List, ListResult, NamespaceRoots, Read,
        ReadResult, Write, WriteResult, build_registry,
    };
    use aether_actor::actor;
    use aether_data::TransformError;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::transform::{FoldError, TransformRegistry};

    /// `aether.fs` mailbox cap. Owns the resolved adapter registry +
    /// namespace roots, plus the link-time native-transform registry
    /// (ADR-0048 §2) used by `on_fetch` to resolve and validate
    /// transform chains before running them. The dispatcher thread holds
    /// an `Arc<Self>` and routes envelopes through the macro-emitted
    /// `NativeDispatch` impl; replies are returned directly from
    /// `#[handler]` methods (ADR-0112) and dispatched through the
    /// substrate's `Mailer::send_reply`.
    pub struct FsCapability {
        registry: Arc<AdapterRegistry>,
        /// Link-time native-transform registry (ADR-0048 §2). Built once
        /// at `init`; immutable thereafter.
        transforms: TransformRegistry,
    }

    #[actor]
    impl NativeActor for FsCapability {
        /// Resolved namespace roots threaded through to `init`. Chassis
        /// mains build this via [`NamespaceRoots::from_env`] (or hand-roll
        /// for tests) and pass to `with_actor::<FsCapability>(roots)`.
        type Config = NamespaceRoots;

        /// ADR-0041 + ADR-0074 Phase 5 chassis-owned mailbox.
        const NAMESPACE: &'static str = "aether.fs";

        /// Build the adapter registry from the resolved roots. Adapter
        /// init failure surfaces as `BootError::Other(io::Error)` so
        /// chassis mains propagate via `?` to abort startup (ADR-0063
        /// fail-fast).
        fn init(roots: NamespaceRoots, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let (registry, roots) =
                build_registry(roots).map_err(|e| BootError::Other(Box::new(e)))?;
            let transforms = TransformRegistry::from_inventory();
            tracing::info!(
                target: "aether_substrate::fs",
                save = %roots.save.display(),
                assets = %roots.assets.display(),
                config = %roots.config.display(),
                transforms = transforms.len(),
                "adapters registered",
            );
            Ok(Self {
                registry,
                transforms,
            })
        }

        /// Read bytes from a logical namespace path.
        ///
        /// # Agent
        /// Reply: `ReadResult`. Echoes namespace + path on both arms.
        #[handler]
        fn on_read(&self, _ctx: &mut NativeCtx<'_>, mail: Read) -> ReadResult {
            let Some(adapter) = self.registry.get(&mail.namespace) else {
                return ReadResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    error: FsError::UnknownNamespace,
                };
            };
            match adapter.read(&mail.path) {
                Ok(bytes) => ReadResult::Ok {
                    namespace: mail.namespace,
                    path: mail.path,
                    bytes,
                },
                Err(error) => ReadResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    error,
                },
            }
        }

        /// Write bytes to a logical namespace path. Atomic via tmp+rename
        /// in the local file adapter; semantics may differ in future
        /// adapters (cloud, in-memory).
        ///
        /// # Agent
        /// Reply: `WriteResult`. Echoes namespace + path (NOT bytes).
        #[handler]
        fn on_write(&self, _ctx: &mut NativeCtx<'_>, mail: Write) -> WriteResult {
            let Some(adapter) = self.registry.get(&mail.namespace) else {
                return WriteResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    error: FsError::UnknownNamespace,
                };
            };
            match adapter.write(&mail.path, &mail.bytes) {
                Ok(()) => WriteResult::Ok {
                    namespace: mail.namespace,
                    path: mail.path,
                },
                Err(error) => WriteResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    error,
                },
            }
        }

        /// Copy a file from a raw host path into a writable namespace.
        /// `mail.from` is read via `std::fs::read` directly — not through
        /// the `FileAdapter` trait — because `from` is an absolute host path
        /// with no namespace root (the same trust model as `config_path` /
        /// `binary_path`). The write sandbox applies entirely on the `to`
        /// side: an unknown namespace → `UnknownNamespace`; a read-only
        /// namespace or a `to.path` with `..` / leading `/` → `Forbidden`.
        ///
        /// # Agent
        /// Reply: `CopyResult`. Echoes `from` + `to` (no bytes).
        #[handler]
        fn on_copy(&self, _ctx: &mut NativeCtx<'_>, mail: Copy) -> CopyResult {
            let Some(adapter) = self.registry.get(&mail.to.namespace) else {
                return CopyResult::Err {
                    from: mail.from,
                    to: mail.to,
                    error: FsError::UnknownNamespace,
                };
            };
            let bytes = match fs::read(&mail.from) {
                Ok(b) => b,
                Err(e) => {
                    return CopyResult::Err {
                        from: mail.from,
                        to: mail.to,
                        error: fs_error_from_std(e),
                    };
                }
            };
            match adapter.write(&mail.to.path, &bytes) {
                Ok(()) => CopyResult::Ok {
                    from: mail.from,
                    to: mail.to,
                },
                Err(error) => CopyResult::Err {
                    from: mail.from,
                    to: mail.to,
                    error,
                },
            }
        }

        /// Delete a path under a namespace.
        ///
        /// # Agent
        /// Reply: `DeleteResult`. Echoes namespace + path.
        #[handler]
        fn on_delete(&self, _ctx: &mut NativeCtx<'_>, mail: Delete) -> DeleteResult {
            let Some(adapter) = self.registry.get(&mail.namespace) else {
                return DeleteResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    error: FsError::UnknownNamespace,
                };
            };
            match adapter.delete(&mail.path) {
                Ok(()) => DeleteResult::Ok {
                    namespace: mail.namespace,
                    path: mail.path,
                },
                Err(error) => DeleteResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    error,
                },
            }
        }

        /// List entries under a namespace prefix.
        ///
        /// # Agent
        /// Reply: `ListResult`. Echoes namespace + prefix.
        #[handler]
        fn on_list(&self, _ctx: &mut NativeCtx<'_>, mail: List) -> ListResult {
            let Some(adapter) = self.registry.get(&mail.namespace) else {
                return ListResult::Err {
                    namespace: mail.namespace,
                    prefix: mail.prefix,
                    error: FsError::UnknownNamespace,
                };
            };
            match adapter.list(&mail.prefix) {
                Ok(entries) => ListResult::Ok {
                    namespace: mail.namespace,
                    prefix: mail.prefix,
                    entries,
                },
                Err(error) => ListResult::Err {
                    namespace: mail.namespace,
                    prefix: mail.prefix,
                    error,
                },
            }
        }

        /// Read a file from a namespace and run an ordered transform
        /// pipeline over its bytes, replying with the folded output
        /// (issue 2132).
        ///
        /// An empty `transforms` list short-circuits to the raw file
        /// bytes (`output_kind: None`). A non-empty chain is validated
        /// for linear composition before any compute runs. The fold
        /// executes synchronously on `aether.fs`'s run-token; a heavy
        /// fold blocks the run-token until it returns.
        ///
        /// The whole fold runs under one `panic::catch_unwind` — a
        /// panicking transform maps to `FsFetchError::Panicked` rather
        /// than unwinding through the actor dispatch.
        ///
        /// # Agent
        /// Reply: `FsFetchResult`. Echoes namespace + path on both arms.
        #[handler]
        fn on_fetch(&self, _ctx: &mut NativeCtx<'_>, mail: FsFetch) -> FsFetchResult {
            let Some(adapter) = self.registry.get(&mail.namespace) else {
                return FsFetchResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    error: FsFetchError::Fs(FsError::UnknownNamespace),
                };
            };

            let bytes = match adapter.read(&mail.path) {
                Ok(b) => b,
                Err(e) => {
                    return FsFetchResult::Err {
                        namespace: mail.namespace,
                        path: mail.path,
                        error: FsFetchError::Fs(e),
                    };
                }
            };

            if mail.transforms.is_empty() {
                return FsFetchResult::Ok {
                    namespace: mail.namespace,
                    path: mail.path,
                    output_kind: None,
                    data: bytes,
                };
            }

            let output_kind = match self.transforms.validate_fold(&mail.transforms) {
                Ok(Some(k)) => k,
                Ok(None) => unreachable!("transforms is non-empty; validate_fold returns Some"),
                Err(fold_err) => {
                    return FsFetchResult::Err {
                        namespace: mail.namespace,
                        path: mail.path,
                        error: FsFetchError::Fold(map_fold_error(&fold_err)),
                    };
                }
            };

            let transforms = &self.transforms;
            let ids = &mail.transforms;
            let fold_result = panic::catch_unwind(AssertUnwindSafe(|| {
                let mut buf = bytes;
                for &id in ids {
                    let t = transforms
                        .lookup(id)
                        .expect("validate_fold succeeded; every id is guaranteed to resolve");
                    buf = (t.invoke)(&[&buf])?;
                }
                Ok::<Vec<u8>, TransformError>(buf)
            }));

            match fold_result {
                Ok(Ok(data)) => FsFetchResult::Ok {
                    namespace: mail.namespace,
                    path: mail.path,
                    output_kind: Some(output_kind),
                    data,
                },
                Ok(Err(transform_err)) => FsFetchResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    error: FsFetchError::Transform(map_transform_error(&transform_err)),
                },
                Err(payload) => FsFetchResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    error: FsFetchError::Panicked(panic_message(payload.as_ref())),
                },
            }
        }
    }

    fn map_fold_error(e: &FoldError) -> FsFoldError {
        match e {
            FoldError::UnknownTransform(id) => FsFoldError::UnknownTransform(*id),
            FoldError::NonLinearArity { at_index, arity } => FsFoldError::NonLinearArity {
                at_index: *at_index as u64,
                arity: *arity as u64,
            },
            FoldError::KindMismatch {
                at_index,
                expected,
                found,
            } => FsFoldError::KindMismatch {
                at_index: *at_index as u64,
                expected: *expected,
                found: *found,
            },
        }
    }

    fn map_transform_error(e: &TransformError) -> FsTransformError {
        match e {
            TransformError::InputDecode { slot } => {
                FsTransformError::InputDecode { slot: *slot as u64 }
            }
            TransformError::InputArity { expected, actual } => FsTransformError::InputArity {
                expected: *expected as u64,
                actual: *actual as u64,
            },
            TransformError::OutputOverflow { limit, actual } => FsTransformError::OutputOverflow {
                limit: *limit as u64,
                actual: *actual as u64,
            },
        }
    }

    fn panic_message(payload: &(dyn Any + Send)) -> String {
        payload
            .downcast_ref::<&'static str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_owned())
    }

    #[cfg(test)]
    impl FsCapability {
        /// Test-only direct constructor. Production boots through
        /// `Builder::with_actor::<FsCapability>(roots)` which calls `init`;
        /// tests that want to drive handlers without spinning up a full
        /// chassis hand a pre-built registry directly.
        pub(crate) fn from_registry(registry: Arc<AdapterRegistry>) -> Self {
            Self {
                registry,
                transforms: TransformRegistry::from_inventory(),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::{
            AdapterRegistry, Copy, CopyResult, Delete, DeleteResult, FileAdapter, FsError, List,
            ListResult, LocalFileAdapter, NamespaceAddr, NamespaceRoots, Read, ReadResult, Write,
            WriteResult,
        };
        use super::{Arc, FsCapability};
        use aether_actor::Addressable;
        use aether_data::MailboxId;
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::actor::native::ctx::NativeCtx;
        use aether_substrate::chassis::builder::Builder;
        use aether_substrate::chassis::error::BootError;
        use aether_substrate::mail::Source;
        use std::path::{Path, PathBuf};

        use crate::test_chassis::{TestChassis, cleanup, fresh_substrate, scratch_dir};
        use aether_substrate::mail::SourceAddr;
        use aether_substrate::mail::registry;
        use std::fs;

        /// Test fixture that bundles the cap, a fully-wired test mailer,
        /// and a `NativeBinding` long enough for handlers to borrow.
        struct TestFixture {
            cap: FsCapability,
            transport: Arc<NativeBinding>,
        }

        impl TestFixture {
            fn new(reg: Arc<AdapterRegistry>) -> Self {
                let (mailer, _rx) = test_mailer_and_rx();
                let transport = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
                Self {
                    cap: FsCapability::from_registry(reg),
                    transport,
                }
            }

            fn ctx(&self, sender: Source) -> NativeCtx<'_> {
                NativeCtx::new(
                    &self.transport,
                    sender,
                    aether_data::MailId::NONE,
                    aether_data::MailId::NONE,
                )
            }
        }

        fn scratch_root(tag: &str) -> PathBuf {
            scratch_dir("aether-io-cap", tag)
        }

        fn roots_under(root: &Path) -> NamespaceRoots {
            let r = NamespaceRoots {
                save: root.join("save"),
                assets: root.join("assets"),
                config: root.join("config"),
            };
            fs::create_dir_all(&r.save).expect("test setup: save root creates");
            fs::create_dir_all(&r.assets).expect("test setup: assets root creates");
            fs::create_dir_all(&r.config).expect("test setup: config root creates");
            r
        }

        #[test]
        fn resolve_rejects_parent_traversal() {
            let root = scratch_root("resolve-parent");
            let a = LocalFileAdapter::new(root.clone(), true)
                .expect("test setup: LocalFileAdapter constructs on scratch root");
            assert!(matches!(a.read("../etc/passwd"), Err(FsError::Forbidden)));
            assert!(matches!(
                a.read("sub/../../escape"),
                Err(FsError::Forbidden)
            ));
            cleanup(&root);
        }

        #[test]
        fn resolve_rejects_absolute() {
            let root = scratch_root("resolve-abs");
            let a = LocalFileAdapter::new(root.clone(), true)
                .expect("test setup: LocalFileAdapter constructs on scratch root");
            assert!(matches!(a.read("/etc/passwd"), Err(FsError::Forbidden)));
            cleanup(&root);
        }

        #[test]
        fn resolve_permits_dot_segments() {
            let root = scratch_root("resolve-dot");
            let a = LocalFileAdapter::new(root.clone(), true)
                .expect("test setup: LocalFileAdapter constructs on scratch root");
            assert!(matches!(a.read("./nonexistent"), Err(FsError::NotFound)));
            cleanup(&root);
        }

        #[test]
        fn read_missing_file_returns_not_found() {
            let root = scratch_root("read-missing");
            let a = LocalFileAdapter::new(root.clone(), true)
                .expect("test setup: LocalFileAdapter constructs on scratch root");
            assert!(matches!(a.read("slot.bin"), Err(FsError::NotFound)));
            cleanup(&root);
        }

        #[test]
        fn write_then_read_roundtrip() {
            let root = scratch_root("write-read");
            let a = LocalFileAdapter::new(root.clone(), true)
                .expect("test setup: LocalFileAdapter constructs on scratch root");
            a.write("slot.bin", &[1, 2, 3, 4])
                .expect("test setup: adapter accepts write");
            assert_eq!(
                a.read("slot.bin")
                    .expect("test setup: adapter returns written bytes"),
                vec![1, 2, 3, 4]
            );
            cleanup(&root);
        }

        #[test]
        fn write_creates_parent_directories() {
            let root = scratch_root("write-parents");
            let a = LocalFileAdapter::new(root.clone(), true)
                .expect("test setup: LocalFileAdapter constructs on scratch root");
            a.write("deep/sub/dir/slot.bin", b"hi")
                .expect("test setup: adapter writes through deep path");
            assert_eq!(
                a.read("deep/sub/dir/slot.bin")
                    .expect("test setup: adapter reads through deep path"),
                b"hi"
            );
            cleanup(&root);
        }

        #[test]
        fn write_is_atomic_no_tmp_left_behind() {
            let root = scratch_root("write-atomic");
            let a = LocalFileAdapter::new(root.clone(), true)
                .expect("test setup: LocalFileAdapter constructs on scratch root");
            a.write("slot.bin", &[0u8; 16])
                .expect("test setup: adapter accepts atomic write");
            let siblings: Vec<String> = fs::read_dir(a.root())
                .expect("test setup: adapter root is readable")
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().to_str().map(ToString::to_string))
                .collect();
            assert!(
                !siblings.iter().any(|s| s.contains(".tmp-")),
                "unexpected tmp file left behind: {siblings:?}",
            );
            cleanup(&root);
        }

        #[test]
        fn write_on_read_only_returns_forbidden() {
            let root = scratch_root("write-readonly");
            let a = LocalFileAdapter::new(root.clone(), false)
                .expect("test setup: read-only LocalFileAdapter constructs on scratch root");
            assert!(matches!(a.write("x.bin", &[]), Err(FsError::Forbidden)));
            cleanup(&root);
        }

        #[test]
        fn delete_missing_returns_not_found() {
            let root = scratch_root("delete-missing");
            let a = LocalFileAdapter::new(root.clone(), true)
                .expect("test setup: LocalFileAdapter constructs on scratch root");
            assert!(matches!(a.delete("ghost.bin"), Err(FsError::NotFound)));
            cleanup(&root);
        }

        #[test]
        fn delete_removes_file() {
            let root = scratch_root("delete-works");
            let a = LocalFileAdapter::new(root.clone(), true)
                .expect("test setup: LocalFileAdapter constructs on scratch root");
            a.write("slot.bin", b"x")
                .expect("test setup: adapter accepts write");
            a.delete("slot.bin")
                .expect("test setup: adapter deletes existing file");
            assert!(matches!(a.read("slot.bin"), Err(FsError::NotFound)));
            cleanup(&root);
        }

        #[test]
        fn delete_on_read_only_returns_forbidden() {
            let root = scratch_root("delete-readonly");
            let a = LocalFileAdapter::new(root.clone(), false)
                .expect("test setup: read-only LocalFileAdapter constructs on scratch root");
            assert!(matches!(a.delete("x.bin"), Err(FsError::Forbidden)));
            cleanup(&root);
        }

        #[test]
        fn list_empty_root_returns_empty_vec() {
            let root = scratch_root("list-empty");
            let a = LocalFileAdapter::new(root.clone(), true)
                .expect("test setup: LocalFileAdapter constructs on scratch root");
            assert_eq!(
                a.list("").expect("test setup: adapter lists empty root"),
                Vec::<String>::new()
            );
            cleanup(&root);
        }

        #[test]
        fn list_returns_sorted_names_at_root() {
            let root = scratch_root("list-root");
            let a = LocalFileAdapter::new(root.clone(), true)
                .expect("test setup: LocalFileAdapter constructs on scratch root");
            a.write("c.bin", b"")
                .expect("test setup: adapter accepts c.bin write");
            a.write("a.bin", b"")
                .expect("test setup: adapter accepts a.bin write");
            a.write("b.bin", b"")
                .expect("test setup: adapter accepts b.bin write");
            assert_eq!(
                a.list("").expect("test setup: adapter lists root"),
                vec!["a.bin", "b.bin", "c.bin"]
            );
            cleanup(&root);
        }

        #[test]
        fn list_under_subdirectory() {
            let root = scratch_root("list-sub");
            let a = LocalFileAdapter::new(root.clone(), true)
                .expect("test setup: LocalFileAdapter constructs on scratch root");
            a.write("saves/slot1.bin", b"")
                .expect("test setup: adapter accepts saves/slot1.bin write");
            a.write("saves/slot2.bin", b"")
                .expect("test setup: adapter accepts saves/slot2.bin write");
            a.write("cfg/keys.toml", b"")
                .expect("test setup: adapter accepts cfg/keys.toml write");
            let saves = a
                .list("saves")
                .expect("test setup: adapter lists saves subdir");
            assert_eq!(saves, vec!["slot1.bin", "slot2.bin"]);
            cleanup(&root);
        }

        #[test]
        fn list_missing_directory_returns_not_found() {
            let root = scratch_root("list-missing");
            let a = LocalFileAdapter::new(root.clone(), true)
                .expect("test setup: LocalFileAdapter constructs on scratch root");
            assert!(matches!(a.list("nope"), Err(FsError::NotFound)));
            cleanup(&root);
        }

        #[test]
        fn registry_returns_none_for_unknown_namespace() {
            let reg = AdapterRegistry::new();
            assert!(reg.get("save").is_none());
            assert!(!reg.has("save"));
        }

        #[test]
        fn registry_registers_and_retrieves_adapter() {
            let root = scratch_root("reg-basic");
            let adapter: Arc<dyn FileAdapter> = Arc::new(
                LocalFileAdapter::new(root.clone(), true)
                    .expect("test setup: LocalFileAdapter constructs on scratch root"),
            );
            let mut reg = AdapterRegistry::new();
            reg.register("save", adapter);
            assert!(reg.has("save"));
            assert!(reg.get("save").is_some());
            cleanup(&root);
        }

        use aether_data::{SessionToken, Uuid};

        fn build_save_only_registry(root: &Path, writable: bool) -> Arc<AdapterRegistry> {
            let adapter: Arc<dyn FileAdapter> = Arc::new(
                LocalFileAdapter::new(root.to_path_buf(), writable)
                    .expect("test setup: LocalFileAdapter constructs on supplied root"),
            );
            let mut r = AdapterRegistry::new();
            r.register("save", adapter);
            Arc::new(r)
        }

        fn session_sender() -> Source {
            Source::to(SourceAddr::Session(SessionToken(Uuid::nil())))
        }

        use crate::test_chassis::test_mailer_and_rx;

        /// Boot the cap against a fresh tempdir; assert the mailbox
        /// is registered.
        #[test]
        fn capability_boots_and_registers_mailbox() {
            let root = scratch_root("boots");
            let (registry, mailer) = fresh_substrate();
            let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<FsCapability>(roots_under(&root))
                .build_passive()
                .expect("io capability boots");
            assert!(
                registry.lookup(FsCapability::NAMESPACE).is_some(),
                "io mailbox registered"
            );
            drop(chassis);
            cleanup(&root);
        }

        /// Cap init fails when the adapter registry can't be built —
        /// provoke `LocalFileAdapter::new` failure by pointing the save
        /// root at a regular file rather than a directory. `init` returns
        /// `BootError::Other(io::Error)`, the chassis builder propagates.
        #[test]
        fn cap_init_fails_when_adapter_init_fails() {
            let root = scratch_root("init-fails");
            let save_path = root.join("save_is_actually_a_file");
            fs::write(&save_path, b"not a dir")
                .expect("test setup: write save_path as a regular file");
            let roots = NamespaceRoots {
                save: save_path,
                assets: root.join("assets"),
                config: root.join("config"),
            };
            fs::create_dir_all(&roots.assets).expect("test setup: assets root creates");
            fs::create_dir_all(&roots.config).expect("test setup: config root creates");

            let (registry, mailer) = fresh_substrate();
            let result = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<FsCapability>(roots)
                .build_passive();
            assert!(result.is_err(), "save root being a file must fail cap init");
            cleanup(&root);
        }

        /// Builder rejects a duplicate claim. Same protection as the
        /// other capabilities.
        #[test]
        fn duplicate_claim_rejects_with_typed_error() {
            let root = scratch_root("collide");
            let (registry, mailer) = fresh_substrate();
            registry.register_inbox(FsCapability::NAMESPACE, registry::noop_handler());

            let err = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<FsCapability>(roots_under(&root))
                .build_passive()
                .expect_err("collision must surface as BootError");
            assert!(matches!(
                err,
                BootError::MailboxAlreadyClaimed { ref name }
                    if name == FsCapability::NAMESPACE
            ));
            cleanup(&root);
        }

        #[test]
        fn cap_read_ok_replies_with_bytes() {
            let root = scratch_root("cap-read");
            let reg = build_save_only_registry(&root, true);
            reg.get("save")
                .expect("test setup: save adapter is registered")
                .write("slot.bin", &[9, 9, 9])
                .expect("test setup: adapter accepts write");
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_read(
                &mut ctx,
                Read {
                    namespace: "save".to_string(),
                    path: "slot.bin".to_string(),
                },
            );
            match result {
                ReadResult::Ok {
                    namespace,
                    path,
                    bytes,
                } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(path, "slot.bin");
                    assert_eq!(bytes, vec![9, 9, 9]);
                }
                ReadResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
            }
            cleanup(&root);
        }

        #[test]
        fn cap_read_unknown_namespace_replies_err() {
            let root = scratch_root("cap-ns");
            let reg = build_save_only_registry(&root, true);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_read(
                &mut ctx,
                Read {
                    namespace: "nope".to_string(),
                    path: "x.bin".to_string(),
                },
            );
            match result {
                ReadResult::Err {
                    namespace,
                    path,
                    error: FsError::UnknownNamespace,
                } => {
                    assert_eq!(namespace, "nope");
                    assert_eq!(path, "x.bin");
                }
                other => panic!("expected Err UnknownNamespace echoing request, got {other:?}"),
            }
            cleanup(&root);
        }

        #[test]
        fn cap_read_not_found_replies_err() {
            let root = scratch_root("cap-nf");
            let reg = build_save_only_registry(&root, true);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_read(
                &mut ctx,
                Read {
                    namespace: "save".to_string(),
                    path: "ghost.bin".to_string(),
                },
            );
            assert!(matches!(
                result,
                ReadResult::Err {
                    error: FsError::NotFound,
                    ..
                }
            ));
            cleanup(&root);
        }

        #[test]
        fn cap_write_ok_persists_bytes() {
            let root = scratch_root("cap-write");
            let reg = build_save_only_registry(&root, true);
            let reg_clone = Arc::clone(&reg);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_write(
                &mut ctx,
                Write {
                    namespace: "save".to_string(),
                    path: "slot.bin".to_string(),
                    bytes: vec![1, 2, 3],
                },
            );
            match result {
                WriteResult::Ok { namespace, path } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(path, "slot.bin");
                }
                WriteResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
            }
            assert_eq!(
                reg_clone
                    .get("save")
                    .expect("test setup: save adapter is registered")
                    .read("slot.bin")
                    .expect("test setup: adapter reads written bytes"),
                vec![1, 2, 3]
            );
            cleanup(&root);
        }

        #[test]
        fn cap_write_read_only_namespace_replies_forbidden() {
            let root = scratch_root("cap-ro");
            let reg = build_save_only_registry(&root, false);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_write(
                &mut ctx,
                Write {
                    namespace: "save".to_string(),
                    path: "slot.bin".to_string(),
                    bytes: vec![],
                },
            );
            assert!(matches!(
                result,
                WriteResult::Err {
                    error: FsError::Forbidden,
                    ..
                }
            ));
            cleanup(&root);
        }

        #[test]
        fn cap_delete_then_read_surfaces_not_found() {
            let root = scratch_root("cap-del");
            let reg = build_save_only_registry(&root, true);
            let reg_clone = Arc::clone(&reg);
            reg.get("save")
                .expect("test setup: save adapter is registered")
                .write("x.bin", b"x")
                .expect("test setup: adapter accepts write");
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_delete(
                &mut ctx,
                Delete {
                    namespace: "save".to_string(),
                    path: "x.bin".to_string(),
                },
            );
            match result {
                DeleteResult::Ok { namespace, path } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(path, "x.bin");
                }
                DeleteResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
            }
            assert!(matches!(
                reg_clone
                    .get("save")
                    .expect("test setup: save adapter is registered")
                    .read("x.bin"),
                Err(FsError::NotFound)
            ));
            cleanup(&root);
        }

        #[test]
        fn cap_list_returns_sorted_entries() {
            let root = scratch_root("cap-list");
            let reg = build_save_only_registry(&root, true);
            reg.get("save")
                .expect("test setup: save adapter is registered")
                .write("b.bin", b"")
                .expect("test setup: adapter accepts b.bin write");
            reg.get("save")
                .expect("test setup: save adapter is registered")
                .write("a.bin", b"")
                .expect("test setup: adapter accepts a.bin write");
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_list(
                &mut ctx,
                List {
                    namespace: "save".to_string(),
                    prefix: String::new(),
                },
            );
            match result {
                ListResult::Ok {
                    namespace,
                    prefix,
                    entries,
                } => {
                    assert_eq!(namespace, "save");
                    assert_eq!(prefix, "");
                    assert_eq!(entries, vec!["a.bin".to_string(), "b.bin".to_string()]);
                }
                ListResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
            }
            cleanup(&root);
        }

        // The end-to-end "component pushes Read, dispatcher delivers
        // ReadResult to the component's receive_p32" test that lived here
        // pre-stage-2e (issue 552) reached deep into `aether_substrate`
        // privates (`Component::read_u32`, `ComponentEntry`, `host_fns`)
        // plus wasmtime + wat. With the cap extracted to its own crate
        // those internals are no longer reachable as crate-locals. The
        // path it exercised is now covered by:
        //   - `aether-scenario` declarative scenarios (they go through
        //     the same Mailer + dispatch reply machinery), and
        //   - the substrate's own `mailer` / `scheduler` unit tests for
        //     `Mailer::send_reply` → component delivery.
        // Reach for the in-bundle integration suite if a future change
        // wants the full WAT roundtrip back as targeted coverage.

        fn build_two_namespace_registry(root: &Path, save_writable: bool) -> Arc<AdapterRegistry> {
            let save_adapter: Arc<dyn FileAdapter> = Arc::new(
                LocalFileAdapter::new(root.join("save"), save_writable)
                    .expect("test setup: save LocalFileAdapter constructs"),
            );
            let assets_adapter: Arc<dyn FileAdapter> = Arc::new(
                LocalFileAdapter::new(root.join("assets"), false)
                    .expect("test setup: assets LocalFileAdapter constructs"),
            );
            let mut r = AdapterRegistry::new();
            r.register("save", save_adapter);
            r.register("assets", assets_adapter);
            Arc::new(r)
        }

        fn ensure_ns_dirs(root: &Path) {
            fs::create_dir_all(root.join("save")).expect("test setup: save dir creates");
            fs::create_dir_all(root.join("assets")).expect("test setup: assets dir creates");
        }

        #[test]
        fn cap_copy_host_to_save_roundtrip() {
            let root = scratch_root("cap-copy-ok");
            ensure_ns_dirs(&root);
            let src = root.join("source.bin");
            fs::write(&src, b"\x0a\x14\x1e").expect("test setup: write source file");
            let reg = build_save_only_registry(&root.join("save"), true);
            let reg_clone = Arc::clone(&reg);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_copy(
                &mut ctx,
                Copy {
                    from: src.to_string_lossy().into_owned(),
                    to: NamespaceAddr {
                        namespace: "save".to_string(),
                        path: "copied.bin".to_string(),
                    },
                },
            );
            match result {
                CopyResult::Ok { from, to } => {
                    assert_eq!(from, src.to_string_lossy().as_ref());
                    assert_eq!(to.namespace, "save");
                    assert_eq!(to.path, "copied.bin");
                }
                CopyResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
            }
            assert_eq!(
                reg_clone
                    .get("save")
                    .expect("test setup: save adapter is registered")
                    .read("copied.bin")
                    .expect("test setup: adapter reads copied bytes"),
                vec![0x0a_u8, 0x14, 0x1e]
            );
            cleanup(&root);
        }

        #[test]
        fn cap_copy_into_read_only_namespace_replies_forbidden() {
            let root = scratch_root("cap-copy-ro");
            ensure_ns_dirs(&root);
            let src = root.join("source.bin");
            fs::write(&src, b"x").expect("test setup: write source file");
            let reg = build_two_namespace_registry(&root, true);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_copy(
                &mut ctx,
                Copy {
                    from: src.to_string_lossy().into_owned(),
                    to: NamespaceAddr {
                        namespace: "assets".to_string(),
                        path: "data.bin".to_string(),
                    },
                },
            );
            assert!(
                matches!(
                    result,
                    CopyResult::Err {
                        error: FsError::Forbidden,
                        ..
                    }
                ),
                "expected Forbidden, got {result:?}",
            );
            cleanup(&root);
        }

        #[test]
        fn cap_copy_unknown_destination_namespace_replies_unknown_namespace() {
            let root = scratch_root("cap-copy-unknown-ns");
            ensure_ns_dirs(&root);
            let src = root.join("source.bin");
            fs::write(&src, b"y").expect("test setup: write source file");
            let reg = build_save_only_registry(&root.join("save"), true);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_copy(
                &mut ctx,
                Copy {
                    from: src.to_string_lossy().into_owned(),
                    to: NamespaceAddr {
                        namespace: "nope".to_string(),
                        path: "data.bin".to_string(),
                    },
                },
            );
            assert!(
                matches!(
                    result,
                    CopyResult::Err {
                        error: FsError::UnknownNamespace,
                        ..
                    }
                ),
                "expected UnknownNamespace, got {result:?}",
            );
            cleanup(&root);
        }

        #[test]
        fn cap_copy_missing_host_from_replies_not_found() {
            let root = scratch_root("cap-copy-missing-src");
            ensure_ns_dirs(&root);
            let reg = build_save_only_registry(&root.join("save"), true);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_copy(
                &mut ctx,
                Copy {
                    from: root
                        .join("does_not_exist.bin")
                        .to_string_lossy()
                        .into_owned(),
                    to: NamespaceAddr {
                        namespace: "save".to_string(),
                        path: "dst.bin".to_string(),
                    },
                },
            );
            assert!(
                matches!(
                    result,
                    CopyResult::Err {
                        error: FsError::NotFound,
                        ..
                    }
                ),
                "expected NotFound, got {result:?}",
            );
            cleanup(&root);
        }

        #[test]
        fn cap_copy_to_path_traversal_replies_forbidden() {
            let root = scratch_root("cap-copy-traversal");
            ensure_ns_dirs(&root);
            let src = root.join("source.bin");
            fs::write(&src, b"z").expect("test setup: write source file");
            let reg = build_save_only_registry(&root.join("save"), true);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_copy(
                &mut ctx,
                Copy {
                    from: src.to_string_lossy().into_owned(),
                    to: NamespaceAddr {
                        namespace: "save".to_string(),
                        path: "../escape".to_string(),
                    },
                },
            );
            assert!(
                matches!(
                    result,
                    CopyResult::Err {
                        error: FsError::Forbidden,
                        ..
                    }
                ),
                "expected Forbidden for traversal path, got {result:?}",
            );
            cleanup(&root);
        }

        // `aether.fs.fetch` handler tests (issue 2132). Migrated from the
        // retired `aether.nfs` capability. The transform fixtures (`double`,
        // `boom`, `seed`) are local to this test module; `TestNumber` is
        // the shared input/output kind wired through the `double` transform.

        use aether_data::transform;
        use serde::{Deserialize, Serialize};

        /// Structured number kind — the fetch-fold fixtures' transform
        /// input + output. The extra `tag: u32` makes the `{ u64, u32 }`
        /// shape canonically distinct from the test vocabulary's other
        /// single-`u64` kinds so the resolved output `KindId` is unique.
        #[derive(
            Copy,
            Clone,
            Debug,
            Default,
            PartialEq,
            Eq,
            Serialize,
            Deserialize,
            aether_data::Kind,
            aether_data::Schema,
        )]
        #[kind(name = "aether.fs.test.number")]
        struct TestNumber {
            value: u64,
            tag: u32,
        }

        /// Pure transform: double the wrapped value (`TestNumber` →
        /// `TestNumber`). The single-transform fold fixtures' compute.
        #[transform]
        fn double_fs(x: TestNumber) -> TestNumber {
            TestNumber {
                value: x.value.wrapping_mul(2),
                tag: x.tag,
            }
        }

        /// Panicking transform — exercises the panic-is-failure path
        /// (`FsFetchError::Panicked`).
        #[transform]
        fn boom_fs(_x: TestNumber) -> TestNumber {
            panic!("boom");
        }

        /// Zero-input transform (arity 0) — placing it mid-chain trips
        /// `FsFoldError::NonLinearArity`.
        #[transform]
        fn seed_fs() -> TestNumber {
            TestNumber { value: 7, tag: 0 }
        }

        fn transform_id_by_name(tail: &str) -> aether_data::TransformId {
            let Some(entry) =
                aether_data::transforms().find(|t| t.name.ends_with(&format!("::{tail}")))
            else {
                panic!("transform `{tail}` not registered in link-time inventory");
            };
            entry.transform_id
        }

        fn double_fs_transform_id() -> aether_data::TransformId {
            transform_id_by_name("double_fs")
        }

        fn boom_fs_transform_id() -> aether_data::TransformId {
            transform_id_by_name("boom_fs")
        }

        fn seed_fs_transform_id() -> aether_data::TransformId {
            transform_id_by_name("seed_fs")
        }

        use super::super::{FsFetch, FsFetchError, FsFetchResult, FsFoldError};
        use aether_data::Kind;
        use aether_substrate::transform::TransformRegistry;

        /// Unit test: `on_fetch` with empty transforms returns raw file bytes.
        #[test]
        fn on_fetch_empty_transforms_returns_raw_bytes() {
            let root = scratch_root("fetch-raw");
            let assets = root.join("assets");
            fs::create_dir_all(&assets).expect("test setup: assets dir creates");
            fs::write(assets.join("data.bin"), b"raw payload").expect("test setup: seed data.bin");
            let reg = build_two_namespace_registry(&root, true);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_fetch(
                &mut ctx,
                FsFetch {
                    namespace: "assets".to_string(),
                    path: "data.bin".to_string(),
                    transforms: vec![],
                },
            );
            match result {
                FsFetchResult::Ok {
                    namespace,
                    path,
                    output_kind,
                    data,
                } => {
                    assert_eq!(namespace, "assets");
                    assert_eq!(path, "data.bin");
                    assert!(
                        output_kind.is_none(),
                        "empty transform list → output_kind is None"
                    );
                    assert_eq!(data, b"raw payload");
                }
                FsFetchResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
            }
            cleanup(&root);
        }

        /// Unit test: `on_fetch` with an unknown namespace returns
        /// `FsFetchError::Fs(FsError::UnknownNamespace)`.
        #[test]
        fn on_fetch_unknown_namespace_returns_unknown_namespace() {
            let root = scratch_root("fetch-ns-unknown");
            let reg = build_save_only_registry(&root, true);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.cap.on_fetch(
                &mut ctx,
                FsFetch {
                    namespace: "nope".to_string(),
                    path: "x.bin".to_string(),
                    transforms: vec![],
                },
            );
            assert!(
                matches!(
                    result,
                    FsFetchResult::Err {
                        error: FsFetchError::Fs(FsError::UnknownNamespace),
                        ..
                    }
                ),
                "expected Err(Fs(UnknownNamespace)), got {result:?}",
            );
            cleanup(&root);
        }

        /// Unit test: `on_fetch` with a single transform returns the folded
        /// output tagged with the transform's output `KindId`.
        ///
        /// Uses the `double_fs` test transform (`TestNumber` → `TestNumber`).
        #[test]
        fn on_fetch_single_transform_returns_folded_output() {
            let root = scratch_root("fetch-transform");
            let assets = root.join("assets");
            fs::create_dir_all(&assets).expect("test setup: assets dir creates");
            let input = TestNumber { value: 7, tag: 0 };
            let encoded = input.encode_into_bytes();
            fs::write(assets.join("number.bin"), &encoded).expect("test setup: seed number.bin");

            let reg = build_two_namespace_registry(&root, true);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let double_id = double_fs_transform_id();

            let transform_reg = TransformRegistry::from_inventory();
            let double_t = transform_reg
                .lookup(double_id)
                .expect("double_fs registered");
            let expected_output_kind = double_t.output_kind_id;

            let result = fix.cap.on_fetch(
                &mut ctx,
                FsFetch {
                    namespace: "assets".to_string(),
                    path: "number.bin".to_string(),
                    transforms: vec![double_id],
                },
            );
            match result {
                FsFetchResult::Ok {
                    output_kind, data, ..
                } => {
                    assert_eq!(
                        output_kind,
                        Some(expected_output_kind),
                        "output_kind should be double_fs's output kind"
                    );
                    let out: TestNumber =
                        TestNumber::decode_from_bytes(&data).expect("output decodes as TestNumber");
                    assert_eq!(out.value, 14, "double_fs(7) == 14");
                }
                FsFetchResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
            }
            cleanup(&root);
        }

        /// Unit test: a non-composing chain returns `FsFetchError::Fold`
        /// before any transform runs.
        #[test]
        fn on_fetch_non_composing_chain_returns_fold_error() {
            let root = scratch_root("fetch-fold-err");
            let assets = root.join("assets");
            fs::create_dir_all(&assets).expect("test setup: assets dir creates");
            fs::write(assets.join("data.bin"), b"ignored").expect("test setup: seed data.bin");
            let reg = build_two_namespace_registry(&root, true);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());

            // `double_fs`: TestNumber → TestNumber; `seed_fs`: () → TestNumber.
            // `seed_fs` takes ZERO inputs (arity 0), so placing it at index 1
            // (where one input is expected for a linear fold) fires
            // NonLinearArity at index 1.
            let double_id = double_fs_transform_id();
            let seed_id = seed_fs_transform_id();

            let result = fix.cap.on_fetch(
                &mut ctx,
                FsFetch {
                    namespace: "assets".to_string(),
                    path: "data.bin".to_string(),
                    transforms: vec![double_id, seed_id],
                },
            );
            match result {
                FsFetchResult::Err { error, .. } => {
                    assert!(
                        matches!(
                            error,
                            FsFetchError::Fold(FsFoldError::NonLinearArity { at_index: 1, .. })
                        ),
                        "expected Fold(NonLinearArity at 1), got {error:?}",
                    );
                }
                FsFetchResult::Ok { .. } => panic!("expected Err(Fold), got Ok"),
            }
            cleanup(&root);
        }

        /// Unit test: a chain whose first transform can't decode the file's
        /// bytes returns `FsFetchError::Transform`.
        #[test]
        fn on_fetch_transform_decode_failure_returns_transform_error() {
            let root = scratch_root("fetch-transform-err");
            let assets = root.join("assets");
            fs::create_dir_all(&assets).expect("test setup: assets dir creates");
            fs::write(assets.join("garbage.bin"), [0xFF_u8]).expect("test setup: seed garbage.bin");
            let reg = build_two_namespace_registry(&root, true);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let double_id = double_fs_transform_id();

            let result = fix.cap.on_fetch(
                &mut ctx,
                FsFetch {
                    namespace: "assets".to_string(),
                    path: "garbage.bin".to_string(),
                    transforms: vec![double_id],
                },
            );
            match result {
                FsFetchResult::Err { error, .. } => {
                    assert!(
                        matches!(error, FsFetchError::Transform(_)),
                        "expected Transform error, got {error:?}",
                    );
                }
                FsFetchResult::Ok { .. } => panic!("expected Err(Transform), got Ok"),
            }
            cleanup(&root);
        }

        /// Unit test: a panicking transform produces `FsFetchError::Panicked`.
        #[test]
        fn on_fetch_panicking_transform_returns_panicked_error() {
            let root = scratch_root("fetch-panic");
            let assets = root.join("assets");
            fs::create_dir_all(&assets).expect("test setup: assets dir creates");
            let input = TestNumber { value: 1, tag: 0 };
            let encoded = input.encode_into_bytes();
            fs::write(assets.join("number.bin"), &encoded).expect("test setup: seed number.bin");
            let reg = build_two_namespace_registry(&root, true);
            let fix = TestFixture::new(reg);
            let mut ctx = fix.ctx(session_sender());
            let boom_id = boom_fs_transform_id();

            let result = fix.cap.on_fetch(
                &mut ctx,
                FsFetch {
                    namespace: "assets".to_string(),
                    path: "number.bin".to_string(),
                    transforms: vec![boom_id],
                },
            );
            match result {
                FsFetchResult::Err { error, .. } => {
                    assert!(
                        matches!(error, FsFetchError::Panicked(_)),
                        "expected Panicked error, got {error:?}",
                    );
                }
                FsFetchResult::Ok { .. } => panic!("expected Err(Panicked), got Ok"),
            }
            cleanup(&root);
        }
    }
}
