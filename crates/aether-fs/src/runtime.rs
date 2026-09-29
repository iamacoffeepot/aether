//! The `aether.fs` runtime half (ADR-0122 identity/runtime split). Compiled
//! only under `feature = "runtime"` (the `mod runtime;` declaration in the
//! parent carries the gate), so a transport-only build of the `FsCapability`
//! identity never names these types nor pulls `aether_substrate`. The
//! substrate-typed imports are gated once by this module rather than
//! line-by-line; the `#[actor] impl` reaches the state, ctx, and fold helpers
//! through the single `use runtime::*` glob in the parent.

// Fs-level types the state and fold helpers name.
use super::{AdapterRegistry, FsFoldError, FsTransformError};

// Identity, config builder, and handler-argument / reply kinds named by the
// moved `#[runtime] impl`. The identity in the parent resolves its own lifted
// `HandlesKind<K>` markers through `pub use kinds::*`.
use super::{
    Copy, CopyResult, Delete, DeleteResult, FileAdapter, FsCapability, FsError, FsFetch, FsFetchError, FsFetchResult,
    List, ListResult, NamespaceAddr, NamespaceRoots, Read, ReadResult, Write, WriteResult, build_registry,
};
use aether_actor::runtime;

pub use std::any::Any;
pub use std::fs;
pub use std::panic::{self, AssertUnwindSafe};
pub use std::sync::Arc;

pub use super::adapter::fs_error_from_std;
pub use aether_data::{KindId, TransformError, TransformId};
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::transform::{FoldError, TransformRegistry};

/// `aether.fs` runtime state (ADR-0041). Owns the resolved adapter
/// registry plus the link-time native-transform registry (ADR-0048 §2)
/// `on_fetch` uses to resolve and validate transform chains. The
/// dispatcher holds this as the cap's state and routes envelopes through
/// the macro-emitted `Dispatch` impl; replies return directly from the
/// `#[handler]` methods (ADR-0112). The addressing identity is the
/// distinct ZST `FsCapability`. Living in this private module keeps it
/// `pub`-enough to satisfy the `NativeActor::State` interface without
/// exposing it as crate-public API.
pub struct FsCapabilityState {
    registry: Arc<AdapterRegistry>,
    /// Link-time native-transform registry (ADR-0048 §2). Built once
    /// at `init`; immutable thereafter.
    transforms: TransformRegistry,
}

pub fn map_fold_error(e: &FoldError) -> FsFoldError {
    match e {
        FoldError::UnknownTransform(id) => FsFoldError::UnknownTransform(*id),
        FoldError::NonLinearArity { at_index, arity } => {
            FsFoldError::NonLinearArity { at_index: *at_index as u64, arity: *arity as u64 }
        }
        FoldError::KindMismatch { at_index, expected, found } => {
            FsFoldError::KindMismatch { at_index: *at_index as u64, expected: *expected, found: *found }
        }
    }
}

pub fn map_transform_error(e: &TransformError) -> FsTransformError {
    match e {
        TransformError::InputDecode { slot } => FsTransformError::InputDecode { slot: *slot as u64 },
        TransformError::InputArity { expected, actual } => {
            FsTransformError::InputArity { expected: *expected as u64, actual: *actual as u64 }
        }
        TransformError::OutputOverflow { limit, actual } => {
            FsTransformError::OutputOverflow { limit: *limit as u64, actual: *actual as u64 }
        }
    }
}

pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&'static str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_owned())
}

#[runtime]
impl NativeActor for FsCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// state-bearing struct holding the adapter + transform registries.
    type State = FsCapabilityState;

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
    fn init(roots: NamespaceRoots, _ctx: &mut NativeInitCtx<'_>) -> Result<FsCapabilityState, BootError> {
        let (registry, roots) = build_registry(roots).map_err(|e| BootError::Other(Box::new(e)))?;
        let transforms = TransformRegistry::from_inventory();
        tracing::info!(
            target: "aether_substrate::fs",
            save = %roots.save.display(),
            assets = %roots.assets.display(),
            config = %roots.config.display(),
            transforms = transforms.len(),
            "adapters registered",
        );
        Ok(FsCapabilityState { registry, transforms })
    }

    /// Read bytes from a logical namespace path.
    ///
    /// # Agent
    /// Reply: `ReadResult`. Echoes the address on both arms.
    #[handler::single]
    fn on_read(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Read) -> ReadResult {
        let bytes = state.adapter(&mail.addr).and_then(|adapter| adapter.read(&mail.addr.path));

        ReadResult::from_op(mail.addr, bytes)
    }

    /// Write bytes to a logical namespace path. Atomic via tmp+rename
    /// in the local file adapter; semantics may differ in future
    /// adapters (cloud, in-memory).
    ///
    /// # Agent
    /// Reply: `WriteResult`. Echoes the address (NOT bytes).
    #[handler::single]
    fn on_write(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Write) -> WriteResult {
        let written = state.adapter(&mail.addr).and_then(|adapter| adapter.write(&mail.addr.path, &mail.bytes));

        WriteResult::from_op(mail.addr, written)
    }

    /// Copy a file from a raw host path into a writable namespace.
    /// `mail.from` is read via `std::fs::read` directly — not through
    /// the `FileAdapter` trait — because `from` has no namespace root.
    /// No sender authorization is enforced today, so any guest that can address
    /// `aether.fs` can request a read of a host-readable path. The lexical write
    /// sandbox applies only on the `to` side: an unknown namespace →
    /// `UnknownNamespace`; a read-only namespace or a slash-separated
    /// `to.path` with `..` / leading `/` → `Forbidden`.
    ///
    /// # Agent
    /// Reply: `CopyResult`. Echoes `from` + `to` (no bytes).
    #[handler::single]
    fn on_copy(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Copy) -> CopyResult {
        let copied = state
            .adapter(&mail.to)
            .and_then(|adapter| adapter.write(&mail.to.path, &fs::read(&mail.from).map_err(fs_error_from_std)?));

        CopyResult::from_op(mail.from, mail.to, copied)
    }

    /// Delete a path under a namespace.
    ///
    /// # Agent
    /// Reply: `DeleteResult`. Echoes the address.
    #[handler::single]
    fn on_delete(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Delete) -> DeleteResult {
        let deleted = state.adapter(&mail.addr).and_then(|adapter| adapter.delete(&mail.addr.path));

        DeleteResult::from_op(mail.addr, deleted)
    }

    /// List entries under a namespace prefix.
    ///
    /// # Agent
    /// Reply: `ListResult`. Echoes the address, whose `path` is the
    /// listed prefix.
    #[handler::single]
    fn on_list(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: List) -> ListResult {
        let entries = state.adapter(&mail.addr).and_then(|adapter| adapter.list(&mail.addr.path));

        ListResult::from_op(mail.addr, entries)
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
    /// Reply: `FsFetchResult`. Echoes the address on both arms.
    #[handler::single]
    fn on_fetch(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: FsFetch) -> FsFetchResult {
        let fetched = state.fetch(&mail.addr, &mail.transforms);

        FsFetchResult::from_op(mail.addr, fetched)
    }
}

impl FsCapabilityState {
    /// Resolve one address to its adapter, or the `UnknownNamespace`
    /// failure every `aether.fs` verb reports for a namespace that was
    /// never registered.
    fn adapter(&self, addr: &NamespaceAddr) -> Result<Arc<dyn FileAdapter>, FsError> {
        self.registry.get(&addr.namespace).ok_or(FsError::UnknownNamespace)
    }

    /// Read a file and fold the transform chain over its bytes. An
    /// empty chain short-circuits to the raw bytes with no output kind.
    /// The whole fold runs under one `catch_unwind`, so a panicking
    /// transform becomes `FsFetchError::Panicked` rather than unwinding
    /// through actor dispatch.
    fn fetch(&self, addr: &NamespaceAddr, ids: &[TransformId]) -> Result<(Option<KindId>, Vec<u8>), FsFetchError> {
        let bytes = self.adapter(addr).and_then(|adapter| adapter.read(&addr.path)).map_err(FsFetchError::Fs)?;

        if ids.is_empty() {
            return Ok((None, bytes));
        }

        let output_kind = match self.transforms.validate_fold(ids) {
            Ok(Some(kind)) => kind,
            Ok(None) => unreachable!("transforms is non-empty; validate_fold returns Some"),
            Err(fold_error) => return Err(FsFetchError::Fold(map_fold_error(&fold_error))),
        };

        let transforms = &self.transforms;
        let folded = panic::catch_unwind(AssertUnwindSafe(|| {
            let mut buf = bytes;
            for &id in ids {
                let t = transforms.lookup(id).expect("validate_fold succeeded; every id is guaranteed to resolve");
                buf = (t.invoke)(&[&buf])?;
            }
            Ok::<Vec<u8>, TransformError>(buf)
        }));

        match folded {
            Ok(Ok(data)) => Ok((Some(output_kind), data)),
            Ok(Err(transform_error)) => Err(FsFetchError::Transform(map_transform_error(&transform_error))),
            Err(payload) => Err(FsFetchError::Panicked(panic_message(payload.as_ref()))),
        }
    }
}

#[cfg(all(test, feature = "runtime"))]
// These tests are deliberate embedders: they build a bare `TestChassis` via
// `Builder::new` rather than the `composed` boot seam production chassis use.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::super::FsCapability;
    use super::super::{
        Access, Copy, CopyResult, FileAdapter, FsError, FsFetch, FsFetchError, FsFetchResult, FsFoldError,
        LocalFileAdapter, NamespaceAddr, NamespaceRoots, Read, ReadResult, Write, WriteResult,
    };
    use aether_actor::{Addressable, HandlesKind};
    use aether_data::{Kind, SessionToken, Uuid, transform};
    use aether_substrate::PumpedSlot;
    use aether_substrate::chassis::builder::{Builder, PassiveChassis, ReplyTarget};
    use aether_substrate::mail::outbound::EgressEvent;
    use aether_substrate::testing::{
        TestChassis, boot_bare_test_chassis, cleanup, decode_session_reply, fresh_substrate, fresh_substrate_and_rx,
        scratch_dir,
    };
    use aether_substrate::transform::TransformRegistry;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::mpsc::Receiver;

    fn scratch_root(tag: &str) -> PathBuf {
        scratch_dir("aether-io-cap", tag)
    }

    fn roots_under(root: &Path) -> NamespaceRoots {
        let r = NamespaceRoots { save: root.join("save"), assets: root.join("assets"), config: root.join("config") };
        fs::create_dir_all(&r.save).expect("test setup: save root creates");
        fs::create_dir_all(&r.assets).expect("test setup: assets root creates");
        fs::create_dir_all(&r.config).expect("test setup: config root creates");
        r
    }

    #[test]
    fn resolve_rejects_parent_traversal() {
        let root = scratch_root("resolve-parent");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadWrite)
            .expect("test setup: LocalFileAdapter constructs on scratch root");
        assert!(matches!(a.read("../etc/passwd"), Err(FsError::Forbidden)));
        assert!(matches!(a.read("sub/../../escape"), Err(FsError::Forbidden)));
        cleanup(&root);
    }

    #[test]
    fn resolve_rejects_absolute() {
        let root = scratch_root("resolve-abs");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadWrite)
            .expect("test setup: LocalFileAdapter constructs on scratch root");
        assert!(matches!(a.read("/etc/passwd"), Err(FsError::Forbidden)));
        cleanup(&root);
    }

    #[test]
    fn resolve_permits_dot_segments() {
        let root = scratch_root("resolve-dot");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadWrite)
            .expect("test setup: LocalFileAdapter constructs on scratch root");
        assert!(matches!(a.read("./nonexistent"), Err(FsError::NotFound)));
        cleanup(&root);
    }

    #[test]
    fn read_missing_file_returns_not_found() {
        let root = scratch_root("read-missing");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadWrite)
            .expect("test setup: LocalFileAdapter constructs on scratch root");
        assert!(matches!(a.read("slot.bin"), Err(FsError::NotFound)));
        cleanup(&root);
    }

    #[test]
    fn write_creates_parent_directories() {
        let root = scratch_root("write-parents");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadWrite)
            .expect("test setup: LocalFileAdapter constructs on scratch root");
        a.write("deep/sub/dir/slot.bin", b"hi").expect("test setup: adapter writes through deep path");
        assert_eq!(a.read("deep/sub/dir/slot.bin").expect("test setup: adapter reads through deep path"), b"hi");
        cleanup(&root);
    }

    #[test]
    fn write_is_atomic_no_tmp_left_behind() {
        let root = scratch_root("write-atomic");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadWrite)
            .expect("test setup: LocalFileAdapter constructs on scratch root");
        a.write("slot.bin", &[0u8; 16]).expect("test setup: adapter accepts atomic write");
        let siblings: Vec<String> = fs::read_dir(a.root())
            .expect("test setup: adapter root is readable")
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str().map(ToString::to_string))
            .collect();
        assert!(!siblings.iter().any(|s| s.contains(".tmp-")), "unexpected tmp file left behind: {siblings:?}");
        cleanup(&root);
    }

    #[test]
    fn write_on_read_only_returns_forbidden() {
        let root = scratch_root("write-readonly");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadOnly)
            .expect("test setup: read-only LocalFileAdapter constructs on scratch root");
        assert!(matches!(a.write("x.bin", &[]), Err(FsError::Forbidden)));
        cleanup(&root);
    }

    #[test]
    fn delete_missing_returns_not_found() {
        let root = scratch_root("delete-missing");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadWrite)
            .expect("test setup: LocalFileAdapter constructs on scratch root");
        assert!(matches!(a.delete("ghost.bin"), Err(FsError::NotFound)));
        cleanup(&root);
    }

    #[test]
    fn delete_removes_file() {
        let root = scratch_root("delete-works");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadWrite)
            .expect("test setup: LocalFileAdapter constructs on scratch root");
        a.write("slot.bin", b"x").expect("test setup: adapter accepts write");
        a.delete("slot.bin").expect("test setup: adapter deletes existing file");
        assert!(matches!(a.read("slot.bin"), Err(FsError::NotFound)));
        cleanup(&root);
    }

    #[test]
    fn delete_on_read_only_returns_forbidden() {
        let root = scratch_root("delete-readonly");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadOnly)
            .expect("test setup: read-only LocalFileAdapter constructs on scratch root");
        assert!(matches!(a.delete("x.bin"), Err(FsError::Forbidden)));
        cleanup(&root);
    }

    #[test]
    fn list_empty_root_returns_empty_vec() {
        let root = scratch_root("list-empty");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadWrite)
            .expect("test setup: LocalFileAdapter constructs on scratch root");
        assert_eq!(a.list("").expect("test setup: adapter lists empty root"), Vec::<String>::new());
        cleanup(&root);
    }

    #[test]
    fn list_returns_sorted_names_at_root() {
        let root = scratch_root("list-root");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadWrite)
            .expect("test setup: LocalFileAdapter constructs on scratch root");
        a.write("c.bin", b"").expect("test setup: adapter accepts c.bin write");
        a.write("a.bin", b"").expect("test setup: adapter accepts a.bin write");
        a.write("b.bin", b"").expect("test setup: adapter accepts b.bin write");
        assert_eq!(a.list("").expect("test setup: adapter lists root"), vec!["a.bin", "b.bin", "c.bin"]);
        cleanup(&root);
    }

    #[test]
    fn list_under_subdirectory() {
        let root = scratch_root("list-sub");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadWrite)
            .expect("test setup: LocalFileAdapter constructs on scratch root");
        a.write("saves/slot1.bin", b"").expect("test setup: adapter accepts saves/slot1.bin write");
        a.write("saves/slot2.bin", b"").expect("test setup: adapter accepts saves/slot2.bin write");
        a.write("cfg/keys.toml", b"").expect("test setup: adapter accepts cfg/keys.toml write");
        let saves = a.list("saves").expect("test setup: adapter lists saves subdir");
        assert_eq!(saves, vec!["slot1.bin", "slot2.bin"]);
        cleanup(&root);
    }

    #[test]
    fn list_missing_directory_returns_not_found() {
        let root = scratch_root("list-missing");
        let a = LocalFileAdapter::new(root.clone(), Access::ReadWrite)
            .expect("test setup: LocalFileAdapter constructs on scratch root");
        assert!(matches!(a.list("nope"), Err(FsError::NotFound)));
        cleanup(&root);
    }

    /// Boot the cap against a fresh tempdir; assert the mailbox
    /// is registered.
    #[test]
    fn capability_boots_and_registers_mailbox() {
        let root = scratch_root("boots");
        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor_configured::<FsCapability>((), roots_under(&root))
            .build_passive()
            .expect("io capability boots");
        assert!(registry.lookup(FsCapability::NAMESPACE).is_some(), "io mailbox registered");
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
        fs::write(&save_path, b"not a dir").expect("test setup: write save_path as a regular file");
        let roots = NamespaceRoots { save: save_path, assets: root.join("assets"), config: root.join("config") };
        fs::create_dir_all(&roots.assets).expect("test setup: assets root creates");
        fs::create_dir_all(&roots.config).expect("test setup: config root creates");

        let (registry, mailer) = fresh_substrate();
        let result = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor_configured::<FsCapability>((), roots)
            .build_passive();
        assert!(result.is_err(), "save root being a file must fail cap init");
        cleanup(&root);
    }

    /// A real `FsCapability` booted as a pumped actor over scratch namespace
    /// roots: its own `init` builds the adapters (`save` read-write, `assets`
    /// read-only), a request is pushed to its proven reference with a session
    /// reply target, [`PumpedSlot::drain_available`] runs the production
    /// dispatch body, and the reply is decoded off the loopback egress.
    struct PumpedFs {
        root: PathBuf,
        roots: NamespaceRoots,
        rx: Receiver<EgressEvent>,
        chassis: PassiveChassis<TestChassis>,
        cap: PumpedSlot<FsCapability>,
    }

    impl PumpedFs {
        fn boot(tag: &str) -> Self {
            let root = scratch_root(tag);
            let roots = roots_under(&root);
            let (registry, mailer, rx) = fresh_substrate_and_rx();
            let chassis = boot_bare_test_chassis(&registry, &mailer);
            let (cap, _wake) =
                chassis.boot_pumped_actor::<FsCapability>(roots.clone(), ()).expect("FsCapability boots pumped");

            Self { root, roots, rx, chassis, cap }
        }

        fn request<K, R>(&mut self, mail: &K) -> R
        where
            K: Kind,
            R: Kind,
            FsCapability: HandlesKind<K>,
        {
            let session = SessionToken(Uuid::nil());
            self.chassis.send_for_reply(
                self.chassis.actor_ref::<FsCapability>(),
                mail,
                ReplyTarget::Session { session, correlation: 1 },
            );
            self.cap.drain_available();

            decode_session_reply(&self.rx)
        }

        fn copy_from_host(&mut self, from: &Path, to: NamespaceAddr) -> CopyResult {
            self.request(&Copy { from: from.to_string_lossy().into_owned(), to })
        }

        fn fetch(&mut self, path: &str, transforms: Vec<aether_data::TransformId>) -> FsFetchResult {
            self.request(&FsFetch { addr: NamespaceAddr::new("assets", path), transforms })
        }
    }

    impl Drop for PumpedFs {
        fn drop(&mut self) {
            self.cap.shutdown();
            cleanup(&self.root);
        }
    }

    /// Bug caught: a verb that does not map an unregistered namespace to
    /// `UnknownNamespace`, or that drops the request address from the error
    /// arm of its reply.
    #[test]
    fn read_of_unknown_namespace_replies_unknown_namespace_echoing_the_address() {
        let mut fsys = PumpedFs::boot("cap-ns");

        let result: ReadResult = fsys.request(&Read { addr: NamespaceAddr::new("nope", "x.bin") });

        match result {
            ReadResult::Err { addr, error: FsError::UnknownNamespace } => {
                assert_eq!(addr.namespace, "nope");
                assert_eq!(addr.path, "x.bin");
            }
            other => panic!("expected Err UnknownNamespace echoing request, got {other:?}"),
        }
    }

    /// Bug caught: `init` registering `assets` writable, or `on_write`
    /// bypassing the adapter's access check.
    #[test]
    fn write_to_read_only_namespace_replies_forbidden() {
        let mut fsys = PumpedFs::boot("cap-ro");

        let result: WriteResult =
            fsys.request(&Write { addr: NamespaceAddr::new("assets", "slot.bin"), bytes: vec![1] });

        match result {
            WriteResult::Err { addr, error: FsError::Forbidden } => assert_eq!(addr.namespace, "assets"),
            other => panic!("expected Err Forbidden, got {other:?}"),
        }
        assert!(!fsys.roots.assets.join("slot.bin").exists(), "a forbidden write must not land on disk");
    }

    /// Bug caught: `on_copy` reading `from` through a namespace adapter
    /// instead of the host filesystem, writing somewhere other than the `to`
    /// namespace root, or dropping either echoed address.
    #[test]
    fn copy_from_host_path_lands_in_the_save_namespace() {
        let mut fsys = PumpedFs::boot("cap-copy-ok");
        let src = fsys.root.join("source.bin");
        fs::write(&src, b"\x0a\x14\x1e").expect("test setup: write source file");

        let result = fsys.copy_from_host(&src, NamespaceAddr::new("save", "copied.bin"));

        match result {
            CopyResult::Ok { from, to } => {
                assert_eq!(from, src.to_string_lossy().as_ref());
                assert_eq!(to.namespace, "save");
                assert_eq!(to.path, "copied.bin");
            }
            CopyResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
        }
        assert_eq!(
            fs::read(fsys.roots.save.join("copied.bin")).expect("the copy lands under the save root"),
            vec![0x0a_u8, 0x14, 0x1e]
        );
    }

    /// Bug caught: `on_copy` resolving the destination namespace after (or
    /// without) the lookup that reports `UnknownNamespace`.
    #[test]
    fn copy_to_unknown_namespace_replies_unknown_namespace() {
        let mut fsys = PumpedFs::boot("cap-copy-unknown-ns");
        let src = fsys.root.join("source.bin");
        fs::write(&src, b"y").expect("test setup: write source file");

        let result = fsys.copy_from_host(&src, NamespaceAddr::new("nope", "data.bin"));

        assert!(
            matches!(result, CopyResult::Err { error: FsError::UnknownNamespace, .. }),
            "expected UnknownNamespace, got {result:?}",
        );
    }

    /// Bug caught: a missing host source mapped to anything but `NotFound`
    /// (the `fs_error_from_std` mapping on the `from` read).
    #[test]
    fn copy_from_missing_host_path_replies_not_found() {
        let mut fsys = PumpedFs::boot("cap-copy-missing-src");
        let src = fsys.root.join("does_not_exist.bin");

        let result = fsys.copy_from_host(&src, NamespaceAddr::new("save", "dst.bin"));

        assert!(
            matches!(result, CopyResult::Err { error: FsError::NotFound, .. }),
            "expected NotFound, got {result:?}",
        );
    }

    /// Bug caught: the `to` side of a copy escaping the namespace root, the
    /// write sandbox `on_copy` relies on the adapter for.
    #[test]
    fn copy_to_traversal_path_replies_forbidden() {
        let mut fsys = PumpedFs::boot("cap-copy-traversal");
        let src = fsys.root.join("source.bin");
        fs::write(&src, b"z").expect("test setup: write source file");

        let result = fsys.copy_from_host(&src, NamespaceAddr::new("save", "../escape"));

        assert!(
            matches!(result, CopyResult::Err { error: FsError::Forbidden, .. }),
            "expected Forbidden for traversal path, got {result:?}",
        );
        assert!(!fsys.root.join("escape").exists(), "a forbidden copy must not land outside the save root");
    }

    // `aether.fs.fetch` handler tests (issue 2132). The transform fixtures
    // (`double`, `boom`, `seed`) link only into this unit-test binary, which
    // is why these stay in-crate rather than in a harness scenario;
    // `TestNumber` is the shared input/output kind wired through `double`.

    /// Structured number kind — the fetch-fold fixtures' transform
    /// input + output. The extra `tag: u32` makes the `{ u64, u32 }`
    /// shape canonically distinct from the test vocabulary's other
    /// single-`u64` kinds so the resolved output `KindId` is unique.
    #[aether_data::kind(name = "aether.fs.test.number", copy, default, eq)]
    struct TestNumber {
        value: u64,
        tag: u32,
    }

    /// Pure transform: double the wrapped value (`TestNumber` →
    /// `TestNumber`). The single-transform fold fixtures' compute.
    #[transform]
    fn double_fs(x: TestNumber) -> TestNumber {
        TestNumber { value: x.value.wrapping_mul(2), tag: x.tag }
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
        let Some(entry) = aether_data::transforms().find(|t| t.name.ends_with(&format!("::{tail}"))) else {
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

    /// Bug caught: an empty chain run through the fold (or tagged with an
    /// output kind) instead of short-circuiting to the raw file bytes.
    #[test]
    fn fetch_with_no_transforms_replies_raw_bytes() {
        let mut fsys = PumpedFs::boot("fetch-raw");
        fs::write(fsys.roots.assets.join("data.bin"), b"raw payload").expect("test setup: seed data.bin");

        match fsys.fetch("data.bin", vec![]) {
            FsFetchResult::Ok { addr, output_kind, data } => {
                assert_eq!(addr.namespace, "assets");
                assert_eq!(addr.path, "data.bin");
                assert!(output_kind.is_none(), "empty transform list → output_kind is None");
                assert_eq!(data, b"raw payload");
            }
            FsFetchResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
        }
    }

    /// Bug caught: a namespace failure on fetch not wrapped as
    /// `FsFetchError::Fs`.
    #[test]
    fn fetch_from_unknown_namespace_replies_fs_unknown_namespace() {
        let mut fsys = PumpedFs::boot("fetch-ns-unknown");

        let result: FsFetchResult =
            fsys.request(&FsFetch { addr: NamespaceAddr::new("nope", "x.bin"), transforms: vec![] });

        assert!(
            matches!(result, FsFetchResult::Err { error: FsFetchError::Fs(FsError::UnknownNamespace), .. }),
            "expected Err(Fs(UnknownNamespace)), got {result:?}",
        );
    }

    /// Bug caught: the fold not running the transform over the file bytes,
    /// or tagging the output with anything but the chain's output kind.
    #[test]
    fn fetch_with_one_transform_replies_folded_output() {
        let mut fsys = PumpedFs::boot("fetch-transform");
        fs::write(fsys.roots.assets.join("number.bin"), TestNumber { value: 7, tag: 0 }.encode_into_bytes())
            .expect("test setup: seed number.bin");
        let double_id = double_fs_transform_id();
        let expected_output_kind =
            TransformRegistry::from_inventory().lookup(double_id).expect("double_fs registered").output_kind_id;

        match fsys.fetch("number.bin", vec![double_id]) {
            FsFetchResult::Ok { output_kind, data, .. } => {
                assert_eq!(output_kind, Some(expected_output_kind), "output_kind should be double_fs's output kind");
                let out = TestNumber::decode_from_bytes(&data).expect("output decodes as TestNumber");
                assert_eq!(out.value, 14, "double_fs(7) == 14");
            }
            FsFetchResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
        }
    }

    /// Bug caught: a non-composing chain run instead of refused, or its
    /// `FoldError` mapped to the wrong `FsFoldError` variant or index.
    #[test]
    fn fetch_with_non_composing_chain_replies_fold_error() {
        let mut fsys = PumpedFs::boot("fetch-fold-err");
        fs::write(fsys.roots.assets.join("data.bin"), b"ignored").expect("test setup: seed data.bin");

        // `seed_fs` takes zero inputs, so at index 1 of a linear fold it
        // trips NonLinearArity there.
        match fsys.fetch("data.bin", vec![double_fs_transform_id(), seed_fs_transform_id()]) {
            FsFetchResult::Err { error, .. } => assert!(
                matches!(error, FsFetchError::Fold(FsFoldError::NonLinearArity { at_index: 1, .. })),
                "expected Fold(NonLinearArity at 1), got {error:?}",
            ),
            FsFetchResult::Ok { .. } => panic!("expected Err(Fold), got Ok"),
        }
    }

    /// Bug caught: a transform's own decode failure surfaced as anything
    /// but `FsFetchError::Transform`.
    #[test]
    fn fetch_whose_transform_cannot_decode_replies_transform_error() {
        let mut fsys = PumpedFs::boot("fetch-transform-err");
        fs::write(fsys.roots.assets.join("garbage.bin"), [0xFF_u8]).expect("test setup: seed garbage.bin");

        match fsys.fetch("garbage.bin", vec![double_fs_transform_id()]) {
            FsFetchResult::Err { error, .. } => {
                assert!(matches!(error, FsFetchError::Transform(_)), "expected Transform error, got {error:?}");
            }
            FsFetchResult::Ok { .. } => panic!("expected Err(Transform), got Ok"),
        }
    }

    /// Bug caught: a panicking transform unwinding through actor dispatch
    /// instead of replying `FsFetchError::Panicked`.
    #[test]
    fn fetch_whose_transform_panics_replies_panicked() {
        let mut fsys = PumpedFs::boot("fetch-panic");
        fs::write(fsys.roots.assets.join("number.bin"), TestNumber { value: 1, tag: 0 }.encode_into_bytes())
            .expect("test setup: seed number.bin");

        match fsys.fetch("number.bin", vec![boom_fs_transform_id()]) {
            FsFetchResult::Err { error, .. } => {
                assert!(matches!(error, FsFetchError::Panicked(_)), "expected Panicked error, got {error:?}");
            }
            FsFetchResult::Ok { .. } => panic!("expected Err(Panicked), got Ok"),
        }
    }
}
