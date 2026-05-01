//! ADR-0070 Phase 3 (part 2): file I/O sink as a native capability.
//!
//! Wraps the ADR-0041 `aether.sink.io` mailbox: components mail
//! `aether.io.{read,write,delete,list}` here, the capability decodes
//! through `io::dispatch_io_mail`, drives the matching adapter, and
//! replies via `Mailer::send_reply`.
//!
//! State held by the capability: an `AdapterRegistry` mapping logical
//! namespace prefixes (`save`, `assets`, `config`) to backing adapters.
//! `boot()` builds the registry from a [`NamespaceRoots`] passed in by
//! the chassis main — either resolved from env via
//! [`NamespaceRoots::from_env`] or supplied explicitly (test-bench
//! tempdirs, embedder overrides).
//!
//! Boot error policy: per ADR-0063 fail-fast, adapter init failure
//! aborts the chassis. Pre-Phase-3-part-2 behavior was log-and-skip;
//! the capability tightens this to a typed `BootError`. Operators with
//! filesystem misconfiguration will see the error loudly at startup
//! rather than silently lose the io sink.
//!
//! Threading: single dispatcher thread, `AtomicBool` +
//! `recv_timeout(100ms)` shutdown — same shape as `HandleCapability`
//! and `LogCapability`. Adapter calls run synchronously on the
//! dispatcher thread; ADR-0041 flagged a future host-fn fast path
//! for asset-sized streaming.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::capability::{BootError, Capability, ChassisCtx, RunningCapability};
use crate::io::{self, NamespaceRoots};

/// Recipient name the io capability claims. ADR-0058 places
/// chassis-owned sinks under `aether.sink.*`.
pub const IO_SINK_NAME: &str = "aether.sink.io";

/// Polling interval for the dispatcher's shutdown check.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Native capability owning the ADR-0041 file-I/O sink. Constructor
/// takes the resolved [`NamespaceRoots`] explicitly — the chassis
/// main reads env (per issue 464) and passes the roots through.
pub struct IoCapability {
    roots: NamespaceRoots,
}

impl IoCapability {
    pub fn new(roots: NamespaceRoots) -> Self {
        Self { roots }
    }
}

/// Running handle returned by [`IoCapability::boot`]. Same shape as
/// the other dispatcher-thread capabilities.
pub struct IoRunning {
    thread: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl Capability for IoCapability {
    type Running = IoRunning;

    fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError> {
        let claim = ctx.claim_mailbox(IO_SINK_NAME)?;
        let mailer = ctx.mail_send_handle();
        let (registry, roots) = io::build_registry(self.roots).map_err(|e| {
            BootError::Other(Box::new(std::io::Error::new(
                e.kind(),
                format!("io adapter init failed: {e}"),
            )))
        })?;
        tracing::info!(
            target: "aether_substrate::io",
            save = %roots.save.display(),
            assets = %roots.assets.display(),
            config = %roots.config.display(),
            "io adapters registered",
        );

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let thread_flag = Arc::clone(&shutdown_flag);
        let receiver = claim.receiver;

        let thread = thread::Builder::new()
            .name("aether-io-sink".into())
            .spawn(move || {
                while !thread_flag.load(Ordering::Relaxed) {
                    match receiver.recv_timeout(SHUTDOWN_POLL_INTERVAL) {
                        Ok(env) => {
                            io::dispatch_io_mail(
                                &registry,
                                &mailer,
                                env.kind,
                                env.sender,
                                &env.payload,
                            );
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        Ok(IoRunning {
            thread: Some(thread),
            shutdown_flag,
        })
    }
}

impl RunningCapability for IoRunning {
    fn shutdown(self: Box<Self>) {
        let IoRunning {
            mut thread,
            shutdown_flag,
        } = *self;
        shutdown_flag.store(true, Ordering::Relaxed);
        if let Some(t) = thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::env::temp_dir;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::capability::ChassisBuilder;
    use crate::mailer::Mailer;
    use crate::registry::Registry;

    /// Same shape as `io::tests::scratch_root` — manual tempdir to
    /// avoid pulling in the `tempfile` crate. Caller cleans up via
    /// [`cleanup`] after the test asserts.
    fn scratch_root(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let nonce: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let path = temp_dir().join(format!("aether-io-cap-{tag}-{pid}-{nonce}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        let registry = Arc::new(Registry::new());
        for d in aether_kinds::descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        (registry, Arc::new(Mailer::new()))
    }

    fn roots_under(root: &Path) -> NamespaceRoots {
        let r = NamespaceRoots {
            save: root.join("save"),
            assets: root.join("assets"),
            config: root.join("config"),
        };
        std::fs::create_dir_all(&r.save).unwrap();
        std::fs::create_dir_all(&r.assets).unwrap();
        std::fs::create_dir_all(&r.config).unwrap();
        r
    }

    /// Boot the capability against a fresh tempdir; assert the sink
    /// is registered. The dispatch path itself is exercised by
    /// `io::tests` against the same `dispatch_io_mail` function the
    /// capability calls; this test validates the wiring layer.
    #[test]
    fn capability_boots_and_registers_sink() {
        let root = scratch_root("boots");
        let (registry, mailer) = fresh_substrate();
        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(IoCapability::new(roots_under(&root)))
            .build()
            .expect("io capability boots");
        assert!(
            registry.lookup(IO_SINK_NAME).is_some(),
            "sink mailbox registered"
        );
        chassis.shutdown();
        cleanup(&root);
    }

    /// Boot fails with a typed [`BootError::Other`] when the adapter
    /// registry can't be built. Provoke `LocalFileAdapter::new`
    /// failure by pointing the save root at a regular file rather
    /// than a directory.
    #[test]
    fn boot_fails_with_typed_error_when_adapter_init_fails() {
        let root = scratch_root("init-fails");
        let save_path = root.join("save_is_actually_a_file");
        std::fs::write(&save_path, b"not a dir").unwrap();
        let roots = NamespaceRoots {
            save: save_path,
            assets: root.join("assets"),
            config: root.join("config"),
        };
        std::fs::create_dir_all(&roots.assets).unwrap();
        std::fs::create_dir_all(&roots.config).unwrap();

        let (registry, mailer) = fresh_substrate();
        let err = ChassisBuilder::new(registry, mailer)
            .with(IoCapability::new(roots))
            .build()
            .expect_err("save root being a file must fail");
        assert!(matches!(err, BootError::Other(_)));
        cleanup(&root);
    }

    /// Builder rejects a duplicate claim. Same protection as the
    /// other capabilities.
    #[test]
    fn duplicate_claim_rejects_with_typed_error() {
        let root = scratch_root("collide");
        let (registry, mailer) = fresh_substrate();
        registry.register_sink(IO_SINK_NAME, Arc::new(|_, _, _, _, _, _| {}));

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(IoCapability::new(roots_under(&root)))
            .build()
            .expect_err("collision must surface as BootError");
        assert!(matches!(
            err,
            BootError::MailboxAlreadyClaimed { ref name } if name == IO_SINK_NAME
        ));
        cleanup(&root);
    }
}
