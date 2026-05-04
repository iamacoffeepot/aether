//! ADR-0070 Phase 3 (part 2): file I/O sink as a native capability.
//!
//! Owns the full ADR-0041 stack — `FileAdapter` trait, `LocalFileAdapter`,
//! `AdapterRegistry`, env-driven `NamespaceRoots`, the `aether.sink.io`
//! mailbox claim, and the dispatcher thread that decodes inbound
//! `Read` / `Write` / `Delete` / `List` mail and drives the matching
//! adapter. Chassis mains resolve a [`NamespaceRoots`] (typically via
//! [`NamespaceRoots::from_env`]) and pass it to [`IoCapability::new`];
//! everything below the capability boundary is private.
//!
//! The trait deliberately stays small. Adding a backend is "impl
//! four methods" (`read` / `write` / `delete` / `list`), not
//! "refactor the sink."
//!
//! Boot error policy: per ADR-0063 fail-fast, adapter init failure
//! aborts the chassis. Pre-Phase-3-part-2 behavior was log-and-skip;
//! the capability tightens this to a typed `BootError`. Operators with
//! filesystem misconfiguration will see the error loudly at startup
//! rather than silently lose the io sink.
//!
//! Threading: single dispatcher thread on a channel-drop + join
//! lifecycle (ADR-0074 Phase 2d, mirroring
//! [`crate::capabilities::log::LogCapability`]). Adapter calls run
//! synchronously on the dispatcher thread; ADR-0041 flagged a future
//! host-fn fast path for asset-sized streaming.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::capability::{BootError, Capability, ChassisCtx, RunningCapability, SinkSender};
use crate::mail::ReplyTo;
use crate::mailer::Mailer;
use crate::native_transport::NativeTransport;
use aether_data::{Kind, KindId};
use aether_kinds::{
    Delete, DeleteResult, IoError, List, ListResult, Read, ReadResult, Write, WriteResult,
};

/// Recipient name the io capability claims. ADR-0058 places
/// chassis-owned sinks under `aether.sink.*`.
pub const IO_SINK_NAME: &str = "aether.sink.io";

/// Result shape used throughout the adapter layer. The variants of
/// `IoError` map directly onto ADR-0041 §1's reply enums, so the
/// chassis dispatcher can forward an adapter failure without
/// translation.
pub type IoResult<T> = Result<T, IoError>;

/// Storage backend for one namespace. Implementations decide what
/// `path` means against their own root — local files resolve it
/// relative to a directory, a bundled adapter might look it up in
/// an archive, a cloud adapter uses it as an object key. Path
/// normalization (rejecting `..` and absolute prefixes) is the
/// adapter's responsibility; callers hand the string through
/// unchanged from the incoming mail.
///
/// All four methods return `IoResult<_>`. Any backend-specific
/// detail the caller might want (OS errno text, HTTP status) rides
/// inside `IoError::AdapterError(String)`.
pub trait FileAdapter: Send + Sync {
    fn read(&self, path: &str) -> IoResult<Vec<u8>>;
    fn write(&self, path: &str, bytes: &[u8]) -> IoResult<()>;
    fn delete(&self, path: &str) -> IoResult<()>;
    fn list(&self, prefix: &str) -> IoResult<Vec<String>>;
}

/// Namespace → adapter table built at chassis boot. The I/O sink
/// dispatcher reads `namespace` off an incoming `Read`/`Write`/etc.
/// mail, looks up the adapter here, and either drives the call or
/// replies `IoError::UnknownNamespace`. Registration is one-shot
/// at boot; hot-swap is out of scope.
pub struct AdapterRegistry {
    adapters: HashMap<String, Arc<dyn FileAdapter>>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self {
            adapters: HashMap::new(),
        }
    }

    pub fn register(&mut self, namespace: impl Into<String>, adapter: Arc<dyn FileAdapter>) {
        self.adapters.insert(namespace.into(), adapter);
    }

    pub fn get(&self, namespace: &str) -> Option<Arc<dyn FileAdapter>> {
        self.adapters.get(namespace).map(Arc::clone)
    }

    pub fn has(&self, namespace: &str) -> bool {
        self.adapters.contains_key(namespace)
    }
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Local-filesystem adapter. One instance per namespace root; the
/// chassis boots one `LocalFileAdapter` per entry in the namespace
/// config and registers them in an `AdapterRegistry`.
///
/// **Atomic writes.** `write` stages to a sibling `*.tmp-{pid}` file
/// and `rename`s on success so a crash mid-write leaves either the
/// old contents or the new — never a torn file. Rename on
/// POSIX/Windows is atomic at the filesystem level; no application-
/// level lock needed.
///
/// **Path safety.** `resolve` rejects any `path` that contains `..`
/// segments, empty segments, or leading `/` — a component asking for
/// `save://../etc/passwd` fails with `Forbidden` before the adapter
/// touches the filesystem. `.` segments are permitted (they no-op on
/// the join). Symlink escapes from within the namespace root are not
/// defended against in v1: the substrate owns the root directory and
/// doesn't create symlinks, and adversarial writes would require a
/// pre-compromised disk state.
pub struct LocalFileAdapter {
    root: PathBuf,
    writable: bool,
}

impl LocalFileAdapter {
    /// Build an adapter rooted at `root`. The directory is created
    /// if missing so a fresh install of the engine on a machine
    /// without a pre-populated `$AETHER_SAVE_DIR` still boots. The
    /// path is canonicalized so later comparisons (including the
    /// symlink-safety check the v2 asset loader wants) work against
    /// the real filesystem location.
    pub fn new(root: PathBuf, writable: bool) -> std::io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        Ok(Self { root, writable })
    }

    /// Exposed for tests and chassis boot logging. Not a routing
    /// surface — components address by namespace name, never by root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, path: &str) -> IoResult<PathBuf> {
        if path.starts_with('/') {
            return Err(IoError::Forbidden);
        }
        // Empty path resolves to the root itself — useful for
        // `list("")` but not for `read`/`write`/`delete`, which the
        // adapter rejects downstream when the resolved path points
        // at a directory or doesn't exist.
        for component in path.split('/') {
            if component == ".." {
                return Err(IoError::Forbidden);
            }
            // Allow `.` and empty components (the latter covers
            // trailing slash and double slash as no-ops on join).
        }
        Ok(self.root.join(path))
    }
}

impl FileAdapter for LocalFileAdapter {
    fn read(&self, path: &str) -> IoResult<Vec<u8>> {
        let resolved = self.resolve(path)?;
        match std::fs::read(&resolved) {
            Ok(bytes) => Ok(bytes),
            Err(e) => Err(io_error_from_std(e)),
        }
    }

    fn write(&self, path: &str, bytes: &[u8]) -> IoResult<()> {
        if !self.writable {
            return Err(IoError::Forbidden);
        }
        let resolved = self.resolve(path)?;
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent).map_err(|e| IoError::AdapterError(e.to_string()))?;
        }
        // `.tmp-{pid}` suffix keeps concurrent writes from different
        // processes off each other; within one process, last-write-
        // wins is already the documented ADR-0041 semantic.
        let mut tmp = resolved.clone();
        let existing = tmp
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| IoError::AdapterError("non-utf8 filename".into()))?
            .to_string();
        tmp.set_file_name(format!("{existing}.tmp-{}", std::process::id()));
        std::fs::write(&tmp, bytes).map_err(|e| IoError::AdapterError(e.to_string()))?;
        match std::fs::rename(&tmp, &resolved) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Try to clean the staging file so a failed rename
                // doesn't leave litter in the namespace directory.
                let _ = std::fs::remove_file(&tmp);
                Err(IoError::AdapterError(e.to_string()))
            }
        }
    }

    fn delete(&self, path: &str) -> IoResult<()> {
        if !self.writable {
            return Err(IoError::Forbidden);
        }
        let resolved = self.resolve(path)?;
        match std::fs::remove_file(&resolved) {
            Ok(()) => Ok(()),
            Err(e) => Err(io_error_from_std(e)),
        }
    }

    fn list(&self, prefix: &str) -> IoResult<Vec<String>> {
        let resolved = self.resolve(prefix)?;
        let entries = std::fs::read_dir(&resolved).map_err(io_error_from_std)?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| IoError::AdapterError(e.to_string()))?;
            // Non-UTF8 filenames are skipped rather than surfaced as
            // an error — the namespace abstraction is string-typed,
            // so a file the wire can't name isn't reachable anyway.
            if let Some(s) = entry.file_name().to_str() {
                names.push(s.to_string());
            }
        }
        names.sort();
        Ok(names)
    }
}

fn io_error_from_std(err: std::io::Error) -> IoError {
    match err.kind() {
        ErrorKind::NotFound => IoError::NotFound,
        ErrorKind::PermissionDenied => IoError::Forbidden,
        _ => IoError::AdapterError(err.to_string()),
    }
}

/// Resolved filesystem roots for the three ADR-0041 namespaces. The
/// chassis reads this at boot, hands each path to a `LocalFileAdapter`,
/// and registers the result in an `AdapterRegistry` keyed on the
/// namespace short name (`"save"`, `"assets"`, `"config"`).
#[derive(Clone, Debug)]
pub struct NamespaceRoots {
    pub save: PathBuf,
    pub assets: PathBuf,
    pub config: PathBuf,
}

impl NamespaceRoots {
    /// Resolve each root from its env-var override, falling back to
    /// the `dirs`-crate platform default. v1 ships the env layer;
    /// ADR-0041's precedence order (CLI > env > TOML > defaults)
    /// leaves room for TOML and CLI to sit in front of this without
    /// changing the adapter or sink code.
    ///
    /// Defaults:
    /// - `save` → `data_dir()/aether/save`
    /// - `assets` → `{current_exe}/../assets`
    /// - `config` → `config_dir()/aether`
    ///
    /// If a platform directory lookup fails (e.g. no HOME) or
    /// `current_exe()` can't resolve, the fallback is `temp_dir()/aether/...`
    /// so a boot always finishes even on headless CI.
    ///
    /// Per issue 464, this is the chassis-main edge — substrate-core
    /// itself never reads env. The builder
    /// (`SubstrateBootBuilder::namespace_roots`) accepts a resolved
    /// `NamespaceRoots` directly so tests and chassis-as-library
    /// embedders can supply their own roots without process-env
    /// mutation.
    pub fn from_env() -> Self {
        let save = env_or_default("AETHER_SAVE_DIR", || {
            dirs::data_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("aether")
                .join("save")
        });
        let assets = env_or_default("AETHER_ASSETS_DIR", || {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .map(|p| p.join("assets"))
                .unwrap_or_else(|| std::env::temp_dir().join("aether").join("assets"))
        });
        let config = env_or_default("AETHER_CONFIG_DIR", || {
            dirs::config_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("aether")
        });
        Self {
            save,
            assets,
            config,
        }
    }
}

fn env_or_default(var: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
    match std::env::var(var) {
        Ok(s) if !s.is_empty() => PathBuf::from(s),
        _ => default(),
    }
}

/// Populate a fresh `AdapterRegistry` with `LocalFileAdapter`s for
/// each of the three ADR-0041 namespaces using the supplied
/// [`NamespaceRoots`]. `save` and `config` are writable; `assets` is
/// read-only. Returns the populated registry along with the roots
/// echoed back (cloned) so the chassis can log what it actually
/// wired. Propagates any `create_dir_all` / `canonicalize` failure
/// verbatim so the chassis can decide whether to fail boot.
///
/// Per issue 464, this is the explicit-config entry point — chassis
/// mains read env into a `NamespaceRoots` and pass it here. Tests
/// pass their own tempdir-backed roots directly.
pub fn build_registry(
    roots: NamespaceRoots,
) -> std::io::Result<(Arc<AdapterRegistry>, NamespaceRoots)> {
    let mut registry = AdapterRegistry::new();
    let save = Arc::new(LocalFileAdapter::new(roots.save.clone(), true)?);
    let assets = Arc::new(LocalFileAdapter::new(roots.assets.clone(), false)?);
    let config = Arc::new(LocalFileAdapter::new(roots.config.clone(), true)?);
    registry.register("save", save as Arc<dyn FileAdapter>);
    registry.register("assets", assets as Arc<dyn FileAdapter>);
    registry.register("config", config as Arc<dyn FileAdapter>);
    Ok((Arc::new(registry), roots))
}

/// Env-driven wrapper around [`build_registry`]. Resolves
/// [`NamespaceRoots::from_env`] then delegates. Kept for callers
/// that don't need to thread roots through their own config.
pub fn build_default_registry() -> std::io::Result<(Arc<AdapterRegistry>, NamespaceRoots)> {
    build_registry(NamespaceRoots::from_env())
}

/// Demultiplex one envelope's payload to the matching per-kind
/// adapter call. The dispatcher thread invokes this for each
/// envelope it receives.
///
/// The reply router is `Mailer::send_reply`, not
/// `HubOutbound::send_reply`, so session / engine-mailbox /
/// local-component replies all funnel through one path. Component-
/// originated mail (`ReplyTo::Component`) pushes the reply back
/// into the requesting component's inbox; session / engine mail
/// hands off to the hub outbound as before.
///
/// Adapter calls run synchronously on the dispatcher thread —
/// fine for save/config (KB-MB files). Asset-sized workloads
/// should not ride this path; ADR-0041 flags a future host-fn fast
/// path for zero-copy streaming reads.
fn dispatch_io_mail(
    registry: &AdapterRegistry,
    mailer: &Mailer,
    kind: KindId,
    sender: ReplyTo,
    bytes: &[u8],
) {
    // Decode-failure helper: the request couldn't be parsed, so we
    // have no namespace/path to echo. Send the reply with empty
    // strings in the echo fields — the `AdapterError` text carries
    // the decode diagnostic, and empty-string echo is a loud signal
    // that the request itself was malformed.
    match kind {
        <Read as Kind>::ID => {
            let req: Read = match postcard::from_bytes(bytes) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::io",
                        error = %e,
                        "read: decode failed, replying Err",
                    );
                    mailer.send_reply(
                        sender,
                        &ReadResult::Err {
                            namespace: String::new(),
                            path: String::new(),
                            error: IoError::AdapterError(format!("decode failed: {e}")),
                        },
                    );
                    return;
                }
            };
            let Some(adapter) = registry.get(&req.namespace) else {
                mailer.send_reply(
                    sender,
                    &ReadResult::Err {
                        namespace: req.namespace.clone(),
                        path: req.path.clone(),
                        error: IoError::UnknownNamespace,
                    },
                );
                return;
            };
            let _ = match adapter.read(&req.path) {
                Ok(bytes) => mailer.send_reply(
                    sender,
                    &ReadResult::Ok {
                        namespace: req.namespace.clone(),
                        path: req.path.clone(),
                        bytes,
                    },
                ),
                Err(error) => mailer.send_reply(
                    sender,
                    &ReadResult::Err {
                        namespace: req.namespace,
                        path: req.path,
                        error,
                    },
                ),
            };
        }
        <Write as Kind>::ID => {
            let req: Write = match postcard::from_bytes(bytes) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::io",
                        error = %e,
                        "write: decode failed, replying Err",
                    );
                    mailer.send_reply(
                        sender,
                        &WriteResult::Err {
                            namespace: String::new(),
                            path: String::new(),
                            error: IoError::AdapterError(format!("decode failed: {e}")),
                        },
                    );
                    return;
                }
            };
            let Some(adapter) = registry.get(&req.namespace) else {
                mailer.send_reply(
                    sender,
                    &WriteResult::Err {
                        namespace: req.namespace.clone(),
                        path: req.path.clone(),
                        error: IoError::UnknownNamespace,
                    },
                );
                return;
            };
            let _ = match adapter.write(&req.path, &req.bytes) {
                Ok(()) => mailer.send_reply(
                    sender,
                    &WriteResult::Ok {
                        namespace: req.namespace.clone(),
                        path: req.path.clone(),
                    },
                ),
                Err(error) => mailer.send_reply(
                    sender,
                    &WriteResult::Err {
                        namespace: req.namespace,
                        path: req.path,
                        error,
                    },
                ),
            };
        }
        <Delete as Kind>::ID => {
            let req: Delete = match postcard::from_bytes(bytes) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::io",
                        error = %e,
                        "delete: decode failed, replying Err",
                    );
                    mailer.send_reply(
                        sender,
                        &DeleteResult::Err {
                            namespace: String::new(),
                            path: String::new(),
                            error: IoError::AdapterError(format!("decode failed: {e}")),
                        },
                    );
                    return;
                }
            };
            let Some(adapter) = registry.get(&req.namespace) else {
                mailer.send_reply(
                    sender,
                    &DeleteResult::Err {
                        namespace: req.namespace.clone(),
                        path: req.path.clone(),
                        error: IoError::UnknownNamespace,
                    },
                );
                return;
            };
            let _ = match adapter.delete(&req.path) {
                Ok(()) => mailer.send_reply(
                    sender,
                    &DeleteResult::Ok {
                        namespace: req.namespace.clone(),
                        path: req.path.clone(),
                    },
                ),
                Err(error) => mailer.send_reply(
                    sender,
                    &DeleteResult::Err {
                        namespace: req.namespace,
                        path: req.path,
                        error,
                    },
                ),
            };
        }
        <List as Kind>::ID => {
            let req: List = match postcard::from_bytes(bytes) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::io",
                        error = %e,
                        "list: decode failed, replying Err",
                    );
                    mailer.send_reply(
                        sender,
                        &ListResult::Err {
                            namespace: String::new(),
                            prefix: String::new(),
                            error: IoError::AdapterError(format!("decode failed: {e}")),
                        },
                    );
                    return;
                }
            };
            let Some(adapter) = registry.get(&req.namespace) else {
                mailer.send_reply(
                    sender,
                    &ListResult::Err {
                        namespace: req.namespace.clone(),
                        prefix: req.prefix.clone(),
                        error: IoError::UnknownNamespace,
                    },
                );
                return;
            };
            let _ = match adapter.list(&req.prefix) {
                Ok(entries) => mailer.send_reply(
                    sender,
                    &ListResult::Ok {
                        namespace: req.namespace.clone(),
                        prefix: req.prefix.clone(),
                        entries,
                    },
                ),
                Err(error) => mailer.send_reply(
                    sender,
                    &ListResult::Err {
                        namespace: req.namespace,
                        prefix: req.prefix,
                        error,
                    },
                ),
            };
        }
        _ => {
            tracing::warn!(
                target: "aether_substrate::io",
                kind = %kind,
                "io sink received unknown kind — dropping",
            );
        }
    }
}

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

/// Running handle returned by [`IoCapability::boot`]. Holds the
/// dispatcher's `JoinHandle`, the [`SinkSender`] strong handle that
/// drives channel-drop shutdown, and the actor's
/// [`NativeTransport`] (kept alive for the dispatcher thread's
/// lifetime via the `Arc` clone the spawn closure holds).
pub struct IoRunning {
    thread: Option<JoinHandle<()>>,
    sink_sender: Option<SinkSender>,
    _transport: Arc<NativeTransport>,
}

impl Capability for IoCapability {
    type Running = IoRunning;

    fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError> {
        let claim = ctx.claim_mailbox_drop_on_shutdown(IO_SINK_NAME)?;
        let mailer: Arc<Mailer> = ctx.mail_send_handle();
        let mailbox_id = claim.id;
        let (registry, roots) = build_registry(self.roots).map_err(|e| {
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

        let transport = Arc::new(NativeTransport::new(Arc::clone(&mailer), mailbox_id));
        transport.install_inbox(claim.receiver);
        let dispatcher_transport = Arc::clone(&transport);

        let thread = thread::Builder::new()
            .name("aether-io-sink".into())
            .spawn(move || {
                // Channel-drop + join: pull until the sender side
                // disconnects. Worst-case shutdown latency is the
                // OS scheduler's wakeup, not a 100ms poll interval.
                while let Some(env) = dispatcher_transport.recv_blocking() {
                    dispatch_io_mail(&registry, &mailer, env.kind, env.sender, &env.payload);
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        Ok(IoRunning {
            thread: Some(thread),
            sink_sender: Some(claim.sink_sender),
            _transport: transport,
        })
    }
}

impl RunningCapability for IoRunning {
    fn shutdown(self: Box<Self>) {
        let IoRunning {
            mut thread,
            mut sink_sender,
            _transport,
        } = *self;
        // Drop the strong sender first to break the channel.
        sink_sender.take();
        if let Some(t) = thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::env::temp_dir;

    use super::*;
    use crate::capability::ChassisBuilder;
    use crate::registry::Registry;

    /// Manual tempdir helper to avoid pulling in the `tempfile`
    /// crate. Caller cleans up via [`cleanup`] after the test asserts.
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

    #[test]
    fn resolve_rejects_parent_traversal() {
        let root = scratch_root("resolve-parent");
        let a = LocalFileAdapter::new(root.clone(), true).unwrap();
        assert!(matches!(a.read("../etc/passwd"), Err(IoError::Forbidden)));
        assert!(matches!(
            a.read("sub/../../escape"),
            Err(IoError::Forbidden)
        ));
        cleanup(&root);
    }

    #[test]
    fn resolve_rejects_absolute() {
        let root = scratch_root("resolve-abs");
        let a = LocalFileAdapter::new(root.clone(), true).unwrap();
        assert!(matches!(a.read("/etc/passwd"), Err(IoError::Forbidden)));
        cleanup(&root);
    }

    #[test]
    fn resolve_permits_dot_segments() {
        // `./foo` should resolve to `root/foo`. A read of
        // `./nonexistent` should surface as `NotFound`, not
        // `Forbidden`, so the normalization doesn't over-reject.
        let root = scratch_root("resolve-dot");
        let a = LocalFileAdapter::new(root.clone(), true).unwrap();
        assert!(matches!(a.read("./nonexistent"), Err(IoError::NotFound)));
        cleanup(&root);
    }

    #[test]
    fn read_missing_file_returns_not_found() {
        let root = scratch_root("read-missing");
        let a = LocalFileAdapter::new(root.clone(), true).unwrap();
        assert!(matches!(a.read("slot.bin"), Err(IoError::NotFound)));
        cleanup(&root);
    }

    #[test]
    fn write_then_read_roundtrip() {
        let root = scratch_root("write-read");
        let a = LocalFileAdapter::new(root.clone(), true).unwrap();
        a.write("slot.bin", &[1, 2, 3, 4]).unwrap();
        assert_eq!(a.read("slot.bin").unwrap(), vec![1, 2, 3, 4]);
        cleanup(&root);
    }

    #[test]
    fn write_creates_parent_directories() {
        let root = scratch_root("write-parents");
        let a = LocalFileAdapter::new(root.clone(), true).unwrap();
        a.write("deep/sub/dir/slot.bin", b"hi").unwrap();
        assert_eq!(a.read("deep/sub/dir/slot.bin").unwrap(), b"hi");
        cleanup(&root);
    }

    #[test]
    fn write_is_atomic_no_tmp_left_behind() {
        // After a successful write, no .tmp-* sibling should be
        // visible under the target's parent directory.
        let root = scratch_root("write-atomic");
        let a = LocalFileAdapter::new(root.clone(), true).unwrap();
        a.write("slot.bin", &[0u8; 16]).unwrap();
        let siblings: Vec<String> = std::fs::read_dir(a.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
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
        let a = LocalFileAdapter::new(root.clone(), false).unwrap();
        assert!(matches!(a.write("x.bin", &[]), Err(IoError::Forbidden)));
        cleanup(&root);
    }

    #[test]
    fn delete_missing_returns_not_found() {
        let root = scratch_root("delete-missing");
        let a = LocalFileAdapter::new(root.clone(), true).unwrap();
        assert!(matches!(a.delete("ghost.bin"), Err(IoError::NotFound)));
        cleanup(&root);
    }

    #[test]
    fn delete_removes_file() {
        let root = scratch_root("delete-works");
        let a = LocalFileAdapter::new(root.clone(), true).unwrap();
        a.write("slot.bin", b"x").unwrap();
        a.delete("slot.bin").unwrap();
        assert!(matches!(a.read("slot.bin"), Err(IoError::NotFound)));
        cleanup(&root);
    }

    #[test]
    fn delete_on_read_only_returns_forbidden() {
        let root = scratch_root("delete-readonly");
        let a = LocalFileAdapter::new(root.clone(), false).unwrap();
        assert!(matches!(a.delete("x.bin"), Err(IoError::Forbidden)));
        cleanup(&root);
    }

    #[test]
    fn list_empty_root_returns_empty_vec() {
        let root = scratch_root("list-empty");
        let a = LocalFileAdapter::new(root.clone(), true).unwrap();
        assert_eq!(a.list("").unwrap(), Vec::<String>::new());
        cleanup(&root);
    }

    #[test]
    fn list_returns_sorted_names_at_root() {
        let root = scratch_root("list-root");
        let a = LocalFileAdapter::new(root.clone(), true).unwrap();
        a.write("c.bin", b"").unwrap();
        a.write("a.bin", b"").unwrap();
        a.write("b.bin", b"").unwrap();
        assert_eq!(a.list("").unwrap(), vec!["a.bin", "b.bin", "c.bin"]);
        cleanup(&root);
    }

    #[test]
    fn list_under_subdirectory() {
        let root = scratch_root("list-sub");
        let a = LocalFileAdapter::new(root.clone(), true).unwrap();
        a.write("saves/slot1.bin", b"").unwrap();
        a.write("saves/slot2.bin", b"").unwrap();
        a.write("cfg/keys.toml", b"").unwrap();
        let saves = a.list("saves").unwrap();
        assert_eq!(saves, vec!["slot1.bin", "slot2.bin"]);
        cleanup(&root);
    }

    #[test]
    fn list_missing_directory_returns_not_found() {
        let root = scratch_root("list-missing");
        let a = LocalFileAdapter::new(root.clone(), true).unwrap();
        assert!(matches!(a.list("nope"), Err(IoError::NotFound)));
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
        let adapter: Arc<dyn FileAdapter> =
            Arc::new(LocalFileAdapter::new(root.clone(), true).unwrap());
        let mut reg = AdapterRegistry::new();
        reg.register("save", adapter);
        assert!(reg.has("save"));
        assert!(reg.get("save").is_some());
        cleanup(&root);
    }

    // Sink dispatcher end-to-end: builds a real `LocalFileAdapter`
    // against a tempdir, pushes encoded mail bytes through the
    // dispatch function, and reads the outbound reply channel to
    // confirm the correct `*Result` variant was sent back. The
    // outbound is built via `crate::outbound::HubOutbound::attached_loopback` which
    // skips the TCP plumbing but keeps `send_reply`'s encode path
    // live.

    use crate::outbound::EgressEvent;
    use aether_data::{SessionToken, Uuid};
    fn build_save_only_registry(root: &Path, writable: bool) -> Arc<AdapterRegistry> {
        let adapter: Arc<dyn FileAdapter> =
            Arc::new(LocalFileAdapter::new(root.to_path_buf(), writable).unwrap());
        let mut r = AdapterRegistry::new();
        r.register("save", adapter);
        Arc::new(r)
    }

    fn session_sender() -> ReplyTo {
        ReplyTo::to(crate::mail::ReplyTarget::Session(SessionToken(Uuid::nil())))
    }

    /// Build a fully-wired `Mailer` connected to a fresh test
    /// outbound channel. Sessions/engine replies land on `rx`;
    /// component replies push into the mailer's component table,
    /// which is pre-wired with an empty registry + empty components
    /// table so `push` can route without panicking.
    fn test_mailer_and_rx() -> (Arc<Mailer>, std::sync::mpsc::Receiver<EgressEvent>) {
        use std::collections::HashMap;
        use std::sync::RwLock;

        let (outbound, rx) = crate::outbound::HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new());
        mailer.wire(
            Arc::new(Registry::new()),
            Arc::new(RwLock::new(HashMap::new())),
        );
        mailer.wire_outbound(outbound);
        (mailer, rx)
    }

    fn decode_reply<K: aether_data::Kind + serde::de::DeserializeOwned>(
        rx: &std::sync::mpsc::Receiver<EgressEvent>,
    ) -> K {
        // The test channel gets `EgressEvent::ToSession` events from
        // `send_reply`; pull the first one and decode its payload.
        let event = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        let EgressEvent::ToSession {
            kind_name, payload, ..
        } = event
        else {
            panic!("expected ToSession egress, got {event:?}");
        };
        assert_eq!(kind_name, K::NAME);
        postcard::from_bytes(&payload).unwrap()
    }

    /// Boot the capability against a fresh tempdir; assert the sink
    /// is registered.
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

    #[test]
    fn dispatch_read_ok_replies_with_bytes() {
        let root = scratch_root("dispatch-read");
        let reg = build_save_only_registry(&root, true);
        let (mailer, rx) = test_mailer_and_rx();
        reg.get("save")
            .unwrap()
            .write("slot.bin", &[9, 9, 9])
            .unwrap();
        let req = postcard::to_allocvec(&Read {
            namespace: "save".to_string(),
            path: "slot.bin".to_string(),
        })
        .unwrap();
        dispatch_io_mail(&reg, &mailer, <Read as Kind>::ID, session_sender(), &req);
        match decode_reply::<ReadResult>(&rx) {
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
    fn dispatch_read_unknown_namespace_replies_err() {
        let root = scratch_root("dispatch-ns");
        let reg = build_save_only_registry(&root, true);
        let (mailer, rx) = test_mailer_and_rx();
        let req = postcard::to_allocvec(&Read {
            namespace: "nope".to_string(),
            path: "x.bin".to_string(),
        })
        .unwrap();
        dispatch_io_mail(&reg, &mailer, <Read as Kind>::ID, session_sender(), &req);
        match decode_reply::<ReadResult>(&rx) {
            ReadResult::Err {
                namespace,
                path,
                error: IoError::UnknownNamespace,
            } => {
                // Echoed fields survive the unknown-namespace path —
                // the dispatcher pulls them from the decoded request
                // before looking up the adapter.
                assert_eq!(namespace, "nope");
                assert_eq!(path, "x.bin");
            }
            other => panic!("expected Err UnknownNamespace echoing request, got {other:?}"),
        }
        cleanup(&root);
    }

    #[test]
    fn dispatch_read_not_found_replies_err() {
        let root = scratch_root("dispatch-nf");
        let reg = build_save_only_registry(&root, true);
        let (mailer, rx) = test_mailer_and_rx();
        let req = postcard::to_allocvec(&Read {
            namespace: "save".to_string(),
            path: "ghost.bin".to_string(),
        })
        .unwrap();
        dispatch_io_mail(&reg, &mailer, <Read as Kind>::ID, session_sender(), &req);
        assert!(matches!(
            decode_reply::<ReadResult>(&rx),
            ReadResult::Err {
                error: IoError::NotFound,
                ..
            }
        ));
        cleanup(&root);
    }

    #[test]
    fn dispatch_write_ok_persists_bytes() {
        let root = scratch_root("dispatch-write");
        let reg = build_save_only_registry(&root, true);
        let (mailer, rx) = test_mailer_and_rx();
        let req = postcard::to_allocvec(&Write {
            namespace: "save".to_string(),
            path: "slot.bin".to_string(),
            bytes: vec![1, 2, 3],
        })
        .unwrap();
        dispatch_io_mail(&reg, &mailer, <Write as Kind>::ID, session_sender(), &req);
        match decode_reply::<WriteResult>(&rx) {
            WriteResult::Ok { namespace, path } => {
                assert_eq!(namespace, "save");
                assert_eq!(path, "slot.bin");
            }
            WriteResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
        }
        assert_eq!(
            reg.get("save").unwrap().read("slot.bin").unwrap(),
            vec![1, 2, 3]
        );
        cleanup(&root);
    }

    #[test]
    fn dispatch_write_read_only_namespace_replies_forbidden() {
        let root = scratch_root("dispatch-ro");
        let reg = build_save_only_registry(&root, false);
        let (mailer, rx) = test_mailer_and_rx();
        let req = postcard::to_allocvec(&Write {
            namespace: "save".to_string(),
            path: "slot.bin".to_string(),
            bytes: vec![],
        })
        .unwrap();
        dispatch_io_mail(&reg, &mailer, <Write as Kind>::ID, session_sender(), &req);
        assert!(matches!(
            decode_reply::<WriteResult>(&rx),
            WriteResult::Err {
                error: IoError::Forbidden,
                ..
            }
        ));
        cleanup(&root);
    }

    #[test]
    fn dispatch_delete_then_read_surfaces_not_found() {
        let root = scratch_root("dispatch-del");
        let reg = build_save_only_registry(&root, true);
        let (mailer, rx) = test_mailer_and_rx();
        reg.get("save").unwrap().write("x.bin", b"x").unwrap();
        let req = postcard::to_allocvec(&Delete {
            namespace: "save".to_string(),
            path: "x.bin".to_string(),
        })
        .unwrap();
        dispatch_io_mail(&reg, &mailer, <Delete as Kind>::ID, session_sender(), &req);
        match decode_reply::<DeleteResult>(&rx) {
            DeleteResult::Ok { namespace, path } => {
                assert_eq!(namespace, "save");
                assert_eq!(path, "x.bin");
            }
            DeleteResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
        }
        assert!(matches!(
            reg.get("save").unwrap().read("x.bin"),
            Err(IoError::NotFound)
        ));
        cleanup(&root);
    }

    #[test]
    fn dispatch_list_returns_sorted_entries() {
        let root = scratch_root("dispatch-list");
        let reg = build_save_only_registry(&root, true);
        let (mailer, rx) = test_mailer_and_rx();
        reg.get("save").unwrap().write("b.bin", b"").unwrap();
        reg.get("save").unwrap().write("a.bin", b"").unwrap();
        let req = postcard::to_allocvec(&List {
            namespace: "save".to_string(),
            prefix: "".to_string(),
        })
        .unwrap();
        dispatch_io_mail(&reg, &mailer, <List as Kind>::ID, session_sender(), &req);
        match decode_reply::<ListResult>(&rx) {
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

    #[test]
    fn dispatch_unknown_kind_id_does_not_reply() {
        // An unrelated kind id hitting the io sink should warn-drop,
        // not panic, and must not produce a reply (nothing for the
        // sender to be waiting on).
        let root = scratch_root("dispatch-unknown");
        let reg = build_save_only_registry(&root, true);
        let (mailer, rx) = test_mailer_and_rx();
        dispatch_io_mail(&reg, &mailer, KindId(0xdead_beef), session_sender(), &[]);
        assert!(rx.try_recv().is_err(), "unexpected reply on unknown kind");
        cleanup(&root);
    }

    /// End-to-end: a component pushes a `Read` at the io sink and
    /// receives the `ReadResult` via `Mailer::send_reply` →
    /// `Mailer::push` → its own dispatcher's `deliver`. The WAT
    /// guest records the inbound kind id at a known offset so the
    /// test can confirm the reply actually reached receive. Prior
    /// to the `ReplyTo::Component` plumbing this path silently
    /// dropped the reply at `HubOutbound::send_reply(None, ..)`.
    #[test]
    fn component_reply_roundtrip_delivers_readresult_to_originator() {
        use std::collections::HashMap;
        use std::sync::RwLock;

        use wasmtime::{Engine, Linker, Module};

        use crate::component::Component;
        use crate::ctx::SubstrateCtx;
        use crate::scheduler::{ComponentEntry, close_and_join};

        // WAT: store the inbound `kind` (param 0) lower+upper u32
        // halves at offsets 200 and 204 so the test can read back
        // the full u64 kind id after delivery.
        const WAT_RECORDS_KIND: &str = r#"
            (module
                (memory (export "memory") 1)
                (func (export "receive_p32") (param i64 i32 i32 i32 i32) (result i32)
                    i32.const 200
                    local.get 0
                    i32.wrap_i64
                    i32.store
                    i32.const 204
                    local.get 0
                    i64.const 32
                    i64.shr_u
                    i32.wrap_i64
                    i32.store
                    i32.const 0))
        "#;

        let root = scratch_root("dispatch-component-reply");
        let reg = build_save_only_registry(&root, true);
        reg.get("save")
            .unwrap()
            .write("slot.bin", &[1, 2, 3])
            .unwrap();

        // Full mailer wiring: a real `Registry` + `ComponentTable`
        // so the reply push routes into the test component's inbox.
        let registry = Arc::new(Registry::new());
        let caller_mailbox = registry.register_component("test_caller");
        let components: crate::scheduler::ComponentTable = Arc::new(RwLock::new(HashMap::new()));
        let (outbound, _outbound_rx) = crate::outbound::HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new());
        mailer.wire(Arc::clone(&registry), Arc::clone(&components));
        mailer.wire_outbound(Arc::clone(&outbound));

        // Instantiate the component. Its ctx gets the same mailer /
        // registry / outbound the sink will dispatch through.
        let engine = Engine::default();
        let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
        crate::host_fns::register(&mut linker).expect("register host fns");
        let wasm = wat::parse_str(WAT_RECORDS_KIND).expect("compile WAT");
        let module = Module::new(&engine, &wasm).expect("compile module");
        let ctx = SubstrateCtx::new(
            caller_mailbox,
            Arc::clone(&registry),
            Arc::clone(&mailer),
            Arc::clone(&outbound),
            crate::input::new_subscribers(),
        );
        let component =
            Component::instantiate(&engine, &linker, &module, ctx).expect("instantiate");
        let entry = Arc::new(ComponentEntry::spawn(
            component,
            Arc::clone(&registry),
            Arc::clone(&mailer),
            caller_mailbox,
        ));
        components
            .write()
            .unwrap()
            .insert(caller_mailbox, Arc::clone(&entry));

        // Dispatch a Read with sender = Component(caller_mailbox).
        let req = postcard::to_allocvec(&Read {
            namespace: "save".to_string(),
            path: "slot.bin".to_string(),
        })
        .unwrap();
        dispatch_io_mail(
            &reg,
            &mailer,
            <Read as Kind>::ID,
            ReplyTo::to(crate::mail::ReplyTarget::Component(caller_mailbox)),
            &req,
        );

        // Wait for the reply to reach receive.
        mailer.drain_all();

        // Recover the component and check it observed ReadResult.
        let mut component = close_and_join(entry);
        let lo = component.read_u32(200) as u64;
        let hi = component.read_u32(204) as u64;
        let observed_kind = lo | (hi << 32);
        assert_eq!(
            observed_kind,
            <ReadResult as Kind>::ID.0,
            "component received a kind id different from ReadResult",
        );

        cleanup(&root);
    }

    #[test]
    fn dispatch_malformed_payload_replies_adapter_error_with_empty_echo() {
        // Bytes that don't postcard-decode as `Read`. Dispatcher
        // must fall through to the decode-error branch and reply
        // with `IoError::AdapterError` rather than hang. Echo
        // fields are empty strings because the dispatcher has no
        // parsed request to pull them from — loud signal that the
        // request itself was malformed.
        let root = scratch_root("dispatch-mal");
        let reg = build_save_only_registry(&root, true);
        let (mailer, rx) = test_mailer_and_rx();
        dispatch_io_mail(
            &reg,
            &mailer,
            <Read as Kind>::ID,
            session_sender(),
            &[0xffu8; 4],
        );
        match decode_reply::<ReadResult>(&rx) {
            ReadResult::Err {
                namespace,
                path,
                error: IoError::AdapterError(_),
            } => {
                assert_eq!(namespace, "");
                assert_eq!(path, "");
            }
            other => panic!("expected Err AdapterError with empty echo, got {other:?}"),
        }
        cleanup(&root);
    }
}
