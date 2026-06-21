//! `aether.nfs` — filesystem actor (issue 2098).
//!
//! A single read-only [`NfsCapability`] singleton addressed at `aether.nfs`,
//! declared at chassis boot via `Builder::with_actor`. It serves the same
//! assets directory as the `aether.fs` monolith, running in parallel with no
//! write-cutover coordination required. No handles, copy, or cache yet.
//!
//! ## Design
//!
//! [`NfsCapability`] is a singleton (`Resolver = One`) holding one
//! [`LocalFileAdapter`](crate::fs::LocalFileAdapter), routing `Read`/`List`
//! against it directly — no `AdapterRegistry`, no namespace dispatch.
//! Path-normalization sandboxing (rejecting `..` and absolute prefixes) is
//! carried from ADR-0041 §2 through the adapter, unchanged.
//!
//! Sharding `aether.nfs` into per-namespace instances was explored and parked
//! (ADR-0120, overturned) — to revisit as an optimization when shard-level
//! concurrency is actually needed.

// Handler-signature kinds imported at file root so the `#[bridge(singleton)]`-
// emitted `impl HandlesKind<K> for NfsCapability {}` markers compile on
// wasm targets (where `mod native` is cfg-stripped).
use aether_kinds::{List, NfsFetch, Read};

/// Boot configuration for [`NfsCapability`]. Carried through
/// `Builder::with_actor`; `init` hands it to
/// [`LocalFileAdapter::new`](crate::fs::LocalFileAdapter::new).
///
/// `writable: false` for the assets root at this slice; a future writable
/// configuration would set `true`.
#[cfg(not(target_family = "wasm"))]
pub use native::NfsRoot;

#[aether_actor::bridge(singleton)]
mod native {
    use std::any::Any;
    use std::panic::{self, AssertUnwindSafe};
    use std::path::PathBuf;

    use aether_actor::actor;
    use aether_data::TransformError;
    use aether_kinds::{
        List, ListResult, NfsFetch, NfsFetchError, NfsFetchResult, NfsFoldError, NfsTransformError,
        Read, ReadResult,
    };
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::transform::{FoldError, TransformRegistry};

    use crate::fs::{FileAdapter, LocalFileAdapter};

    /// Boot configuration for [`NfsCapability`]. See module-level docs.
    pub struct NfsRoot {
        pub root: PathBuf,
        pub writable: bool,
    }

    /// Filesystem actor — a singleton holding one [`LocalFileAdapter`].
    ///
    /// Addressed as `aether.nfs`. Serves the assets root read-only, in
    /// parallel with the `aether.fs` monolith.
    pub struct NfsCapability {
        adapter: LocalFileAdapter,
        /// Link-time native-transform registry (ADR-0048 §2). Built once
        /// at `init`; immutable thereafter. Used by `on_fetch` to resolve
        /// and validate the caller's transform chain before running it.
        registry: TransformRegistry,
    }

    #[actor]
    impl NativeActor for NfsCapability {
        type Config = NfsRoot;

        /// Singleton namespace. Addressed as `aether.nfs`.
        const NAMESPACE: &'static str = "aether.nfs";

        fn init(config: NfsRoot, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let adapter = LocalFileAdapter::new(config.root, config.writable)
                .map_err(|e| BootError::Other(Box::new(e)))?;
            let registry = TransformRegistry::from_inventory();
            tracing::info!(
                target: "aether_substrate::nfs",
                root = %adapter.root().display(),
                transforms = registry.len(),
                "nfs capability initialized",
            );
            Ok(Self { adapter, registry })
        }

        /// Read bytes from a path relative to the configured root.
        ///
        /// Mirrors `FsCapability::on_read` but resolves against the single
        /// adapter. Path sandboxing (ADR-0041 §2) rejects `..` and
        /// leading `/` with `FsError::Forbidden`.
        ///
        /// # Agent
        /// Reply: `ReadResult`. Echoes namespace + path on both arms.
        #[handler]
        fn on_read(&self, _ctx: &mut NativeCtx<'_>, mail: Read) -> ReadResult {
            match self.adapter.read(&mail.path) {
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

        /// List entries under a path prefix relative to the configured root.
        ///
        /// Mirrors `FsCapability::on_list` but resolves against the single
        /// adapter.
        ///
        /// # Agent
        /// Reply: `ListResult`. Echoes namespace + prefix on both arms.
        #[handler]
        fn on_list(&self, _ctx: &mut NativeCtx<'_>, mail: List) -> ListResult {
            match self.adapter.list(&mail.prefix) {
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

        /// Read a file and run an ordered transform pipeline over its bytes,
        /// replying with the folded output (issue 2121).
        ///
        /// An empty `transforms` list short-circuits to the raw file bytes
        /// (`output_kind: None`). A non-empty chain is validated for linear
        /// composition (each adjacent pair must compose) before any compute
        /// runs. The fold executes synchronously on NFS's run-token; a
        /// heavy fold blocks the run-token until it returns (an off-run-token
        /// compute pool is the recorded future optimization).
        ///
        /// The whole fold runs under one `panic::catch_unwind` — a panicking
        /// transform maps to `FetchError::Panicked` rather than unwinding
        /// through the actor dispatch.
        ///
        /// # Agent
        /// Reply: `NfsFetchResult`. Echoes namespace + path on both arms.
        #[handler]
        fn on_fetch(&self, _ctx: &mut NativeCtx<'_>, mail: NfsFetch) -> NfsFetchResult {
            let bytes = match self.adapter.read(&mail.path) {
                Ok(b) => b,
                Err(e) => {
                    return NfsFetchResult::Err {
                        namespace: mail.namespace,
                        path: mail.path,
                        error: NfsFetchError::Fs(e),
                    };
                }
            };

            if mail.transforms.is_empty() {
                return NfsFetchResult::Ok {
                    namespace: mail.namespace,
                    path: mail.path,
                    output_kind: None,
                    data: bytes,
                };
            }

            let output_kind = match self.registry.validate_fold(&mail.transforms) {
                Ok(Some(k)) => k,
                Ok(None) => unreachable!("transforms is non-empty; validate_fold returns Some"),
                Err(fold_err) => {
                    return NfsFetchResult::Err {
                        namespace: mail.namespace,
                        path: mail.path,
                        error: NfsFetchError::Fold(map_fold_error(&fold_err)),
                    };
                }
            };

            let registry = &self.registry;
            let transforms = &mail.transforms;
            let fold_result = panic::catch_unwind(AssertUnwindSafe(|| {
                let mut buf = bytes;
                for &id in transforms {
                    let t = registry
                        .lookup(id)
                        .expect("validate_fold succeeded; every id is guaranteed to resolve");
                    buf = (t.invoke)(&[&buf])?;
                }
                Ok::<Vec<u8>, TransformError>(buf)
            }));

            match fold_result {
                Ok(Ok(data)) => NfsFetchResult::Ok {
                    namespace: mail.namespace,
                    path: mail.path,
                    output_kind: Some(output_kind),
                    data,
                },
                Ok(Err(transform_err)) => NfsFetchResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    error: NfsFetchError::Transform(map_transform_error(&transform_err)),
                },
                Err(payload) => NfsFetchResult::Err {
                    namespace: mail.namespace,
                    path: mail.path,
                    error: NfsFetchError::Panicked(panic_message(payload.as_ref())),
                },
            }
        }
    }

    fn map_fold_error(e: &FoldError) -> NfsFoldError {
        match e {
            FoldError::UnknownTransform(id) => NfsFoldError::UnknownTransform(*id),
            FoldError::NonLinearArity { at_index, arity } => NfsFoldError::NonLinearArity {
                at_index: *at_index as u64,
                arity: *arity as u64,
            },
            FoldError::KindMismatch {
                at_index,
                expected,
                found,
            } => NfsFoldError::KindMismatch {
                at_index: *at_index as u64,
                expected: *expected,
                found: *found,
            },
        }
    }

    fn map_transform_error(e: &TransformError) -> NfsTransformError {
        match e {
            TransformError::InputDecode { slot } => {
                NfsTransformError::InputDecode { slot: *slot as u64 }
            }
            TransformError::InputArity { expected, actual } => NfsTransformError::InputArity {
                expected: *expected as u64,
                actual: *actual as u64,
            },
            TransformError::OutputOverflow { limit, actual } => NfsTransformError::OutputOverflow {
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
    impl NfsCapability {
        /// Test-only direct constructor. Production boots through
        /// `Builder::with_actor::<NfsCapability>(config)` which calls `init`;
        /// tests that drive handlers without a full chassis hand a pre-built
        /// adapter directly.
        pub(super) fn from_adapter(adapter: LocalFileAdapter) -> Self {
            Self {
                adapter,
                registry: TransformRegistry::from_inventory(),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::fs;
        use std::path::{Path, PathBuf};
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        use aether_data::{Kind, MailId, MailboxId, SessionToken, Uuid};
        use aether_kinds::descriptors;
        use aether_kinds::trace::Nanos;
        use aether_kinds::{
            FsError, List, ListResult, NfsFetch, NfsFetchError, NfsFetchResult, NfsFoldError, Read,
            ReadResult,
        };
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::actor::native::ctx::NativeCtx;
        use aether_substrate::chassis::builder::Builder;
        use aether_substrate::handle_store::HandleStore;
        use aether_substrate::mail::mailer::Mailer;
        use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
        use aether_substrate::mail::registry::{
            InboxHandler, MailboxEntry, OwnedDispatch, Registry,
        };
        use aether_substrate::mail::{MailRef, Source, SourceAddr};
        use aether_substrate::transform::TransformRegistry;

        use std::sync::Arc;

        use crate::test_chassis::{TestChassis, cleanup, scratch_dir, test_mailer_and_rx};

        use serde::{Deserialize, Serialize};

        use aether_data::transform;

        use super::{LocalFileAdapter, NfsCapability, NfsRoot};

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
        #[kind(name = "aether.nfs.test.number")]
        struct TestNumber {
            value: u64,
            tag: u32,
        }

        /// Pure transform: double the wrapped value (`TestNumber` →
        /// `TestNumber`). The single-transform fold fixtures' compute.
        #[transform]
        fn double(x: TestNumber) -> TestNumber {
            TestNumber {
                value: x.value.wrapping_mul(2),
                tag: x.tag,
            }
        }

        /// Panicking transform — exercises the panic-is-failure path
        /// (`FetchError::Panicked`).
        #[transform]
        fn boom(_x: TestNumber) -> TestNumber {
            panic!("boom");
        }

        /// Zero-input transform (arity 0) — placing it mid-chain trips
        /// `FoldError::NonLinearArity`.
        #[transform]
        fn seed() -> TestNumber {
            TestNumber { value: 7, tag: 0 }
        }

        /// Resolve the `double` transform's global id from the link-time
        /// inventory.
        fn double_transform_id() -> aether_data::TransformId {
            transform_id_by_name("double")
        }

        /// Resolve the `boom` transform's id.
        fn boom_transform_id() -> aether_data::TransformId {
            transform_id_by_name("boom")
        }

        /// Resolve the zero-input `seed` transform's id.
        fn seed_transform_id() -> aether_data::TransformId {
            transform_id_by_name("seed")
        }

        /// Look up a registered transform's id by its fn-name tail.
        fn transform_id_by_name(tail: &str) -> aether_data::TransformId {
            let Some(entry) =
                aether_data::transforms().find(|t| t.name.ends_with(&format!("::{tail}")))
            else {
                panic!("transform `{tail}` not registered in link-time inventory");
            };
            entry.transform_id
        }

        fn scratch_root(tag: &str) -> PathBuf {
            scratch_dir("aether-nfs-cap", tag)
        }

        fn session_sender() -> Source {
            Source::to(SourceAddr::Session(SessionToken(Uuid::nil())))
        }

        /// Minimal test fixture for direct handler calls — skips the full
        /// chassis boot path. Mirrors the `TestFixture` pattern in fs.rs.
        struct TestFixture {
            nfs: NfsCapability,
            transport: Arc<NativeBinding>,
        }

        impl TestFixture {
            fn new(root: &Path) -> Self {
                let adapter = LocalFileAdapter::new(root.to_path_buf(), false)
                    .expect("test setup: read-only LocalFileAdapter constructs");
                let (mailer, _rx) = test_mailer_and_rx();
                let transport = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
                Self {
                    nfs: NfsCapability::from_adapter(adapter),
                    transport,
                }
            }

            fn ctx(&self, sender: Source) -> NativeCtx<'_> {
                NativeCtx::new(&self.transport, sender, MailId::NONE, MailId::NONE)
            }
        }

        /// Boot a `(Registry, Mailer, egress_rx)` triple with a live egress
        /// channel for integration tests that observe handler replies.
        fn fresh_substrate_with_egress() -> (Arc<Registry>, Arc<Mailer>, mpsc::Receiver<EgressEvent>)
        {
            let registry = Arc::new(Registry::new());
            for d in descriptors::all() {
                let _ = registry.register_kind_with_descriptor(d);
            }
            let (outbound, rx) = HubOutbound::attached_loopback();
            let store = Arc::new(HandleStore::new(1024 * 1024));
            let mailer =
                Arc::new(Mailer::new(Arc::clone(&registry), store).with_outbound(outbound));
            (registry, mailer, rx)
        }

        /// Drain the egress channel until a `ToSession` frame arrives, then
        /// return its payload. Panics if the 2-second deadline expires first.
        fn drain_for_reply(rx: &mpsc::Receiver<EgressEvent>, label: &str) -> Vec<u8> {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Ok(EgressEvent::ToSession { payload, .. }) = rx.try_recv() {
                    return payload;
                }
                assert!(
                    Instant::now() < deadline,
                    "reply for {label} did not arrive within 2s",
                );
                thread::sleep(Duration::from_millis(5));
            }
        }

        /// Encode `mail` as kind `K` and enqueue it to `handler` addressed to
        /// the session sender, then drain `rx` for the reply.
        fn dispatch_and_drain<K: Kind>(
            handler: &Arc<dyn InboxHandler>,
            rx: &mpsc::Receiver<EgressEvent>,
            mail: &K,
            sender: Source,
            label: &str,
        ) -> Vec<u8> {
            let bytes = mail.encode_into_bytes();
            handler.enqueue(OwnedDispatch::disarmed(
                K::ID,
                K::NAME.to_owned(),
                None,
                sender,
                MailRef::from(bytes),
                1,
                MailId::NONE,
                MailId::NONE,
                None,
                Nanos(0),
                0,
                MailboxId(0),
            ));
            drain_for_reply(rx, label)
        }

        /// Unit test: `on_read` returns seeded bytes via the direct handler
        /// path (no chassis boot, no mail dispatch).
        #[test]
        fn on_read_returns_seeded_bytes() {
            let root = scratch_root("unit-read");
            fs::write(root.join("hello.txt"), b"world").expect("test setup: seed hello.txt");
            let fix = TestFixture::new(&root);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.nfs.on_read(
                &mut ctx,
                Read {
                    namespace: "assets".to_string(),
                    path: "hello.txt".to_string(),
                },
            );
            match result {
                ReadResult::Ok {
                    namespace,
                    path,
                    bytes,
                } => {
                    assert_eq!(namespace, "assets");
                    assert_eq!(path, "hello.txt");
                    assert_eq!(bytes, b"world");
                }
                ReadResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
            }
            cleanup(&root);
        }

        /// Unit test: `on_list` returns sorted entry names.
        #[test]
        fn on_list_returns_entries() {
            let root = scratch_root("unit-list");
            fs::write(root.join("b.bin"), b"").expect("test setup: seed b.bin");
            fs::write(root.join("a.bin"), b"").expect("test setup: seed a.bin");
            let fix = TestFixture::new(&root);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.nfs.on_list(
                &mut ctx,
                List {
                    namespace: "assets".to_string(),
                    prefix: String::new(),
                },
            );
            match result {
                ListResult::Ok { entries, .. } => {
                    assert_eq!(entries, vec!["a.bin", "b.bin"]);
                }
                ListResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
            }
            cleanup(&root);
        }

        /// Unit test: `on_read` with a `..` path rejects with `Forbidden` —
        /// the ADR-0041 §2 sandbox gate, carried through `LocalFileAdapter`.
        #[test]
        fn on_read_rejects_parent_traversal() {
            let root = scratch_root("unit-sandbox");
            let fix = TestFixture::new(&root);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.nfs.on_read(
                &mut ctx,
                Read {
                    namespace: "assets".to_string(),
                    path: "../escape".to_string(),
                },
            );
            assert!(
                matches!(
                    result,
                    ReadResult::Err {
                        error: FsError::Forbidden,
                        ..
                    }
                ),
                "expected Forbidden for .. path, got {result:?}",
            );
            cleanup(&root);
        }

        /// Integration test: boot a chassis with `with_actor::<NfsCapability>`,
        /// dispatch `Read` + `List` via the mailbox, assert round-trip
        /// `ReadResult::Ok` / `ListResult::Ok`, and confirm a `..`-escaping
        /// path produces `FsError::Forbidden`. Mirrors `fs.rs:1064`.
        #[test]
        fn integration_chassis_read_list_and_sandbox() {
            let root = scratch_root("integration");
            fs::write(root.join("data.bin"), b"payload").expect("test setup: seed data.bin");
            fs::write(root.join("extra.txt"), b"").expect("test setup: seed extra.txt");

            let (registry, mailer, rx) = fresh_substrate_with_egress();
            let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<NfsCapability>(NfsRoot {
                    root: root.clone(),
                    writable: false,
                })
                .build_passive()
                .expect("nfs capability chassis boots");

            let id = registry
                .lookup("aether.nfs")
                .expect("aether.nfs mailbox registered");
            let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("entry") else {
                panic!("expected Inbox entry for aether.nfs");
            };
            let session = Source::to(SourceAddr::Session(SessionToken(Uuid::nil())));

            // Read.
            let payload = dispatch_and_drain(
                &handler,
                &rx,
                &Read {
                    namespace: "assets".to_string(),
                    path: "data.bin".to_string(),
                },
                session,
                "read",
            );
            let result = ReadResult::decode_from_bytes(&payload).expect("ReadResult decodes");
            match result {
                ReadResult::Ok { bytes, path, .. } => {
                    assert_eq!(path, "data.bin");
                    assert_eq!(bytes, b"payload");
                }
                ReadResult::Err { error, .. } => {
                    panic!("expected ReadResult::Ok, got Err({error:?})")
                }
            }

            // List.
            let payload = dispatch_and_drain(
                &handler,
                &rx,
                &List {
                    namespace: "assets".to_string(),
                    prefix: String::new(),
                },
                session,
                "list",
            );
            let result = ListResult::decode_from_bytes(&payload).expect("ListResult decodes");
            match result {
                ListResult::Ok { mut entries, .. } => {
                    entries.sort();
                    assert!(
                        entries.contains(&"data.bin".to_string()),
                        "entries missing data.bin: {entries:?}",
                    );
                    assert!(
                        entries.contains(&"extra.txt".to_string()),
                        "entries missing extra.txt: {entries:?}",
                    );
                }
                ListResult::Err { error, .. } => {
                    panic!("expected ListResult::Ok, got Err({error:?})")
                }
            }

            // Sandbox: `..` path → Forbidden.
            let payload = dispatch_and_drain(
                &handler,
                &rx,
                &Read {
                    namespace: "assets".to_string(),
                    path: "../etc/passwd".to_string(),
                },
                session,
                "sandbox",
            );
            let result = ReadResult::decode_from_bytes(&payload).expect("ReadResult decodes");
            assert!(
                matches!(
                    result,
                    ReadResult::Err {
                        error: FsError::Forbidden,
                        ..
                    }
                ),
                "expected Forbidden for .. path, got {result:?}",
            );

            drop(chassis);
            cleanup(&root);
        }

        /// Unit test: `on_fetch` with empty transforms returns raw file bytes.
        #[test]
        fn on_fetch_empty_transforms_returns_raw_bytes() {
            let root = scratch_root("fetch-raw");
            fs::write(root.join("data.bin"), b"raw payload").expect("test setup: seed data.bin");
            let fix = TestFixture::new(&root);
            let mut ctx = fix.ctx(session_sender());
            let result = fix.nfs.on_fetch(
                &mut ctx,
                NfsFetch {
                    namespace: "assets".to_string(),
                    path: "data.bin".to_string(),
                    transforms: vec![],
                },
            );
            match result {
                NfsFetchResult::Ok {
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
                NfsFetchResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
            }
            cleanup(&root);
        }

        /// Unit test: `on_fetch` with a single transform returns the folded
        /// output tagged with the transform's output `KindId`.
        ///
        /// Uses the `double` test transform (`TestNumber` → `TestNumber`).
        #[test]
        fn on_fetch_single_transform_returns_folded_output() {
            let root = scratch_root("fetch-transform");
            let input = TestNumber { value: 7, tag: 0 };
            let encoded = input.encode_into_bytes();
            fs::write(root.join("number.bin"), &encoded).expect("test setup: seed number.bin");

            let fix = TestFixture::new(&root);
            let mut ctx = fix.ctx(session_sender());
            let double_id = double_transform_id();

            let reg = TransformRegistry::from_inventory();
            let double_t = reg.lookup(double_id).expect("double registered");
            let expected_output_kind = double_t.output_kind_id;

            let result = fix.nfs.on_fetch(
                &mut ctx,
                NfsFetch {
                    namespace: "assets".to_string(),
                    path: "number.bin".to_string(),
                    transforms: vec![double_id],
                },
            );
            match result {
                NfsFetchResult::Ok {
                    output_kind, data, ..
                } => {
                    assert_eq!(
                        output_kind,
                        Some(expected_output_kind),
                        "output_kind should be double's output kind"
                    );
                    let out: TestNumber =
                        TestNumber::decode_from_bytes(&data).expect("output decodes as TestNumber");
                    assert_eq!(out.value, 14, "double(7) == 14");
                }
                NfsFetchResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
            }
            cleanup(&root);
        }

        /// Unit test: a non-composing chain returns `FetchError::Fold` before
        /// any transform runs. Seeds a file but uses two transforms whose
        /// output/input kinds don't compose.
        #[test]
        fn on_fetch_non_composing_chain_returns_fold_error() {
            let root = scratch_root("fetch-fold-err");
            fs::write(root.join("data.bin"), b"ignored").expect("test setup: seed data.bin");
            let fix = TestFixture::new(&root);
            let mut ctx = fix.ctx(session_sender());

            // `double`: TestNumber → TestNumber; `seed`: () → TestNumber.
            // `seed` takes ZERO inputs (arity 0), so placing it at index 1
            // (where one input is expected for a linear fold) should fire
            // NonLinearArity at index 1.
            let double_id = double_transform_id();
            let seed_id = seed_transform_id();

            let result = fix.nfs.on_fetch(
                &mut ctx,
                NfsFetch {
                    namespace: "assets".to_string(),
                    path: "data.bin".to_string(),
                    transforms: vec![double_id, seed_id],
                },
            );
            match result {
                NfsFetchResult::Err { error, .. } => {
                    assert!(
                        matches!(
                            error,
                            NfsFetchError::Fold(NfsFoldError::NonLinearArity { at_index: 1, .. })
                        ),
                        "expected Fold(NonLinearArity at 1), got {error:?}",
                    );
                }
                NfsFetchResult::Ok { .. } => panic!("expected Err(Fold), got Ok"),
            }
            cleanup(&root);
        }

        /// Unit test: a chain whose first transform can't decode the file's
        /// bytes returns `FetchError::Transform`. Writes arbitrary bytes that
        /// are not a valid `TestNumber` encoding, then applies `double`.
        #[test]
        fn on_fetch_transform_decode_failure_returns_transform_error() {
            let root = scratch_root("fetch-transform-err");
            // A single 0xFF byte cannot decode as TestNumber { value: u64, tag: u32 }.
            fs::write(root.join("garbage.bin"), [0xFF_u8]).expect("test setup: seed garbage.bin");
            let fix = TestFixture::new(&root);
            let mut ctx = fix.ctx(session_sender());
            let double_id = double_transform_id();

            let result = fix.nfs.on_fetch(
                &mut ctx,
                NfsFetch {
                    namespace: "assets".to_string(),
                    path: "garbage.bin".to_string(),
                    transforms: vec![double_id],
                },
            );
            match result {
                NfsFetchResult::Err { error, .. } => {
                    assert!(
                        matches!(error, NfsFetchError::Transform(_)),
                        "expected Transform error, got {error:?}",
                    );
                }
                NfsFetchResult::Ok { .. } => panic!("expected Err(Transform), got Ok"),
            }
            cleanup(&root);
        }

        /// Unit test: a panicking transform produces `FetchError::Panicked`.
        #[test]
        fn on_fetch_panicking_transform_returns_panicked_error() {
            let root = scratch_root("fetch-panic");
            let input = TestNumber { value: 1, tag: 0 };
            let encoded = input.encode_into_bytes();
            fs::write(root.join("number.bin"), &encoded).expect("test setup: seed number.bin");
            let fix = TestFixture::new(&root);
            let mut ctx = fix.ctx(session_sender());
            let boom_id = boom_transform_id();

            let result = fix.nfs.on_fetch(
                &mut ctx,
                NfsFetch {
                    namespace: "assets".to_string(),
                    path: "number.bin".to_string(),
                    transforms: vec![boom_id],
                },
            );
            match result {
                NfsFetchResult::Err { error, .. } => {
                    assert!(
                        matches!(error, NfsFetchError::Panicked(_)),
                        "expected Panicked error, got {error:?}",
                    );
                }
                NfsFetchResult::Ok { .. } => panic!("expected Err(Panicked), got Ok"),
            }
            cleanup(&root);
        }

        /// Integration test: dispatch `NfsFetch` through the `aether.nfs`
        /// mailbox and decode `NfsFetchResult`, mirroring
        /// `integration_chassis_read_list_and_sandbox`.
        #[test]
        fn integration_chassis_fetch_through_mailbox() {
            let root = scratch_root("fetch-integration");
            let input = TestNumber { value: 5, tag: 0 };
            let encoded = input.encode_into_bytes();
            fs::write(root.join("n.bin"), &encoded).expect("test setup: seed n.bin");

            let (registry, mailer, rx) = fresh_substrate_with_egress();
            let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<NfsCapability>(NfsRoot {
                    root: root.clone(),
                    writable: false,
                })
                .build_passive()
                .expect("nfs capability chassis boots");

            let id = registry
                .lookup("aether.nfs")
                .expect("aether.nfs mailbox registered");
            let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("entry") else {
                panic!("expected Inbox entry for aether.nfs");
            };
            let session = Source::to(SourceAddr::Session(SessionToken(Uuid::nil())));

            // Empty chain — should return raw bytes.
            let payload = dispatch_and_drain(
                &handler,
                &rx,
                &NfsFetch {
                    namespace: "assets".to_string(),
                    path: "n.bin".to_string(),
                    transforms: vec![],
                },
                session,
                "fetch-raw",
            );
            let result =
                NfsFetchResult::decode_from_bytes(&payload).expect("NfsFetchResult decodes");
            match result {
                NfsFetchResult::Ok {
                    path,
                    output_kind,
                    data,
                    ..
                } => {
                    assert_eq!(path, "n.bin");
                    assert!(output_kind.is_none());
                    assert_eq!(data, encoded);
                }
                NfsFetchResult::Err { error, .. } => {
                    panic!("expected Ok for empty chain, got Err({error:?})")
                }
            }

            // Single transform chain — double(5) == 10.
            let double_id = double_transform_id();
            let reg = TransformRegistry::from_inventory();
            let expected_kind = reg
                .lookup(double_id)
                .expect("double registered")
                .output_kind_id;

            let payload = dispatch_and_drain(
                &handler,
                &rx,
                &NfsFetch {
                    namespace: "assets".to_string(),
                    path: "n.bin".to_string(),
                    transforms: vec![double_id],
                },
                session,
                "fetch-double",
            );
            let result =
                NfsFetchResult::decode_from_bytes(&payload).expect("NfsFetchResult decodes");
            match result {
                NfsFetchResult::Ok {
                    output_kind, data, ..
                } => {
                    assert_eq!(output_kind, Some(expected_kind));
                    let out: TestNumber =
                        TestNumber::decode_from_bytes(&data).expect("output decodes as TestNumber");
                    assert_eq!(out.value, 10, "double(5) == 10");
                }
                NfsFetchResult::Err { error, .. } => {
                    panic!("expected Ok for double chain, got Err({error:?})")
                }
            }

            drop(chassis);
            cleanup(&root);
        }
    }
}
