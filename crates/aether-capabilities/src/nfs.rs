//! `aether.nfs` — sharded filesystem actor (ADR-0120 floor, issue 2098).
//!
//! First slice: a single read-only `NfsInstance` addressed at
//! `aether.nfs:assets`, declared at chassis boot via
//! `Builder::with_instance`. The instance serves the same assets directory
//! as the `aether.fs` monolith, running in parallel with no write-cutover
//! coordination required. No handles, copy, or cache instance yet — later
//! focused issues under ADR-0120.
//!
//! ## Design
//!
//! [`NfsInstance`] is an [`Instanced`](aether_actor::Instanced) actor
//! (`Resolver = Many`). Each instance holds one [`LocalFileAdapter`](crate::fs::LocalFileAdapter)
//! and routes `Read`/`List` against it directly — no `AdapterRegistry`,
//! no namespace dispatch. The instance IS the namespace. Path-normalization
//! sandboxing (rejecting `..` and absolute prefixes) is carried from
//! ADR-0041 §2 through the adapter, unchanged.
//!
//! ## Boot declaration
//!
//! `with_common_caps` calls
//! `Builder::with_instance::<NfsInstance>("assets", NfsRoot { root, writable: false })`
//! (decision 6 of ADR-0120: the instance set is a chassis boot decision).
//! Individual instances are addressable as `aether.nfs:assets` (the
//! `recipient_name` path) via the ADR-0099 depth-1 subname id.

// Handler-signature kinds imported at file root so the `#[bridge(instanced)]`-
// emitted `impl HandlesKind<K> for NfsInstance {}` markers compile on
// wasm targets (where `mod native` is cfg-stripped).
use aether_kinds::{List, Read};

/// Boot configuration for one [`NfsInstance`]. Carried through
/// `Builder::with_instance`; `init` hands it to
/// [`LocalFileAdapter::new`](crate::fs::LocalFileAdapter::new).
///
/// `writable: false` for the assets instance — it is the value the chassis
/// passes at this slice; a future writable instance would set `true`.
#[cfg(not(target_family = "wasm"))]
pub use native::NfsRoot;

#[aether_actor::bridge(instanced)]
mod native {
    use std::path::PathBuf;

    use aether_actor::actor;
    use aether_kinds::{List, ListResult, Read, ReadResult};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    use crate::fs::{FileAdapter, LocalFileAdapter};

    /// Boot configuration for one [`NfsInstance`]. See module-level docs.
    pub struct NfsRoot {
        pub root: PathBuf,
        pub writable: bool,
    }

    /// Sharded filesystem actor — one [`LocalFileAdapter`] per instance.
    ///
    /// Addressed as `aether.nfs:{subname}` (e.g. `aether.nfs:assets`).
    /// The boot-declared `aether.nfs:assets` instance is the first
    /// concrete case of ADR-0120 decision 1 (sharded instances) and
    /// decision 6 (chassis boot declaration).
    pub struct NfsInstance {
        adapter: LocalFileAdapter,
    }

    #[actor]
    impl NativeActor for NfsInstance {
        type Config = NfsRoot;

        /// Instance namespace prefix (ADR-0120 decision 1). Individual
        /// instances address as `{NAMESPACE}:{subname}`.
        const NAMESPACE: &'static str = "aether.nfs";

        fn init(config: NfsRoot, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let adapter = LocalFileAdapter::new(config.root, config.writable)
                .map_err(|e| BootError::Other(Box::new(e)))?;
            tracing::info!(
                target: "aether_substrate::nfs",
                root = %adapter.root().display(),
                "nfs instance initialized",
            );
            Ok(Self { adapter })
        }

        /// Read bytes from a path relative to this instance's root.
        ///
        /// Mirrors `FsCapability::on_read` but resolves against the single
        /// instance adapter. Path sandboxing (ADR-0041 §2) rejects `..` and
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

        /// List entries under a path prefix relative to this instance's root.
        ///
        /// Mirrors `FsCapability::on_list` but resolves against the single
        /// instance adapter.
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
    }

    #[cfg(test)]
    impl NfsInstance {
        /// Test-only direct constructor. Production boots through
        /// `Builder::with_instance::<NfsInstance>(subname, config)` which
        /// calls `init`; tests that want to drive handlers without spinning
        /// up a full chassis hand a pre-built adapter directly.
        pub(super) fn from_adapter(adapter: LocalFileAdapter) -> Self {
            Self { adapter }
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
        use aether_kinds::{FsError, List, ListResult, Read, ReadResult};
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

        use std::sync::Arc;

        use crate::test_chassis::{TestChassis, cleanup, scratch_dir, test_mailer_and_rx};

        use super::{LocalFileAdapter, NfsInstance, NfsRoot};

        fn scratch_root(tag: &str) -> PathBuf {
            scratch_dir("aether-nfs-cap", tag)
        }

        fn session_sender() -> Source {
            Source::to(SourceAddr::Session(SessionToken(Uuid::nil())))
        }

        /// Minimal test fixture for direct handler calls — skips the full
        /// chassis boot path. Mirrors the `TestFixture` pattern in fs.rs.
        struct TestFixture {
            nfs: NfsInstance,
            transport: Arc<NativeBinding>,
        }

        impl TestFixture {
            fn new(root: &Path) -> Self {
                let adapter = LocalFileAdapter::new(root.to_path_buf(), false)
                    .expect("test setup: read-only LocalFileAdapter constructs");
                let (mailer, _rx) = test_mailer_and_rx();
                let transport = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
                Self {
                    nfs: NfsInstance::from_adapter(adapter),
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

        /// Integration test: boot a chassis with `with_instance::<NfsInstance>`,
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
                .with_instance::<NfsInstance>(
                    "assets",
                    NfsRoot {
                        root: root.clone(),
                        writable: false,
                    },
                )
                .build_passive()
                .expect("nfs instance chassis boots");

            let id = registry
                .lookup("aether.nfs:assets")
                .expect("aether.nfs:assets mailbox registered");
            let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("entry") else {
                panic!("expected Inbox entry for aether.nfs:assets");
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
    }
}
