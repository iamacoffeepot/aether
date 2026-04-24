//! Substrate file I/O (ADR-0041). The `FileAdapter` trait is the
//! extension point for storage backends — local filesystem today,
//! cloud / bundled archive / in-memory impls as they earn their
//! place. `AdapterRegistry` maps a logical namespace (`"save"`,
//! `"assets"`, `"config"`) to an adapter; the chassis that wires
//! the `"io"` sink dispatches requests by namespace, calls the
//! adapter, and sends the paired `*Result` reply.
//!
//! The trait deliberately stays small. Adding a backend is "impl
//! four methods" (`read` / `write` / `delete` / `list`), not
//! "refactor the sink."

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aether_kinds::{
    Delete, DeleteResult, IoError, List, ListResult, Read, ReadResult, Write, WriteResult,
};
use aether_mail::Kind;

use crate::hub_client::HubOutbound;
use crate::mail::ReplyTo;
use crate::registry::SinkHandler;

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
/// each of the three ADR-0041 namespaces. `save` and `config` are
/// writable; `assets` is read-only. Returns the populated registry
/// along with the resolved roots so the chassis can log what it
/// actually wired. Propagates any `create_dir_all` / `canonicalize`
/// failure verbatim so the chassis can decide whether to fail boot.
pub fn build_default_registry() -> std::io::Result<(Arc<AdapterRegistry>, NamespaceRoots)> {
    let roots = NamespaceRoots::from_env();
    let mut registry = AdapterRegistry::new();
    let save = Arc::new(LocalFileAdapter::new(roots.save.clone(), true)?);
    let assets = Arc::new(LocalFileAdapter::new(roots.assets.clone(), false)?);
    let config = Arc::new(LocalFileAdapter::new(roots.config.clone(), true)?);
    registry.register("save", save as Arc<dyn FileAdapter>);
    registry.register("assets", assets as Arc<dyn FileAdapter>);
    registry.register("config", config as Arc<dyn FileAdapter>);
    Ok((Arc::new(registry), roots))
}

/// Build the `"io"` sink handler. The chassis calls this at boot
/// after populating `registry` with adapters and passes the result
/// to `registry.register_sink("io", handler)`. The returned closure
/// demultiplexes by `kind_id` (Read / Write / Delete / List, all
/// postcard-decoded), drives the adapter, and replies with the
/// paired `*Result` kind via `outbound.send_reply`.
///
/// Adapter calls run synchronously on the sink dispatch thread —
/// fine for save/config (KB-MB files). Asset-sized workloads
/// should not ride this path; ADR-0041 flags a future host-fn fast
/// path for zero-copy streaming reads.
pub fn io_sink_handler(registry: Arc<AdapterRegistry>, outbound: Arc<HubOutbound>) -> SinkHandler {
    Arc::new(
        move |kind_id: u64,
              _kind_name: &str,
              _origin: Option<&str>,
              sender: ReplyTo,
              bytes: &[u8],
              _count: u32| {
            dispatch_io_mail(&registry, &outbound, kind_id, sender, bytes);
        },
    )
}

fn dispatch_io_mail(
    registry: &AdapterRegistry,
    outbound: &HubOutbound,
    kind_id: u64,
    sender: ReplyTo,
    bytes: &[u8],
) {
    if kind_id == <Read as Kind>::ID {
        let req: Read = match postcard::from_bytes(bytes) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::io",
                    error = %e,
                    "read: decode failed, replying Err",
                );
                outbound.send_reply(
                    sender,
                    &ReadResult::Err {
                        error: IoError::AdapterError(format!("decode failed: {e}")),
                    },
                );
                return;
            }
        };
        let Some(adapter) = registry.get(&req.namespace) else {
            outbound.send_reply(
                sender,
                &ReadResult::Err {
                    error: IoError::UnknownNamespace,
                },
            );
            return;
        };
        let _ = match adapter.read(&req.path) {
            Ok(bytes) => outbound.send_reply(sender, &ReadResult::Ok { bytes }),
            Err(error) => outbound.send_reply(sender, &ReadResult::Err { error }),
        };
    } else if kind_id == <Write as Kind>::ID {
        let req: Write = match postcard::from_bytes(bytes) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::io",
                    error = %e,
                    "write: decode failed, replying Err",
                );
                outbound.send_reply(
                    sender,
                    &WriteResult::Err {
                        error: IoError::AdapterError(format!("decode failed: {e}")),
                    },
                );
                return;
            }
        };
        let Some(adapter) = registry.get(&req.namespace) else {
            outbound.send_reply(
                sender,
                &WriteResult::Err {
                    error: IoError::UnknownNamespace,
                },
            );
            return;
        };
        let _ = match adapter.write(&req.path, &req.bytes) {
            Ok(()) => outbound.send_reply(sender, &WriteResult::Ok),
            Err(error) => outbound.send_reply(sender, &WriteResult::Err { error }),
        };
    } else if kind_id == <Delete as Kind>::ID {
        let req: Delete = match postcard::from_bytes(bytes) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::io",
                    error = %e,
                    "delete: decode failed, replying Err",
                );
                outbound.send_reply(
                    sender,
                    &DeleteResult::Err {
                        error: IoError::AdapterError(format!("decode failed: {e}")),
                    },
                );
                return;
            }
        };
        let Some(adapter) = registry.get(&req.namespace) else {
            outbound.send_reply(
                sender,
                &DeleteResult::Err {
                    error: IoError::UnknownNamespace,
                },
            );
            return;
        };
        let _ = match adapter.delete(&req.path) {
            Ok(()) => outbound.send_reply(sender, &DeleteResult::Ok),
            Err(error) => outbound.send_reply(sender, &DeleteResult::Err { error }),
        };
    } else if kind_id == <List as Kind>::ID {
        let req: List = match postcard::from_bytes(bytes) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::io",
                    error = %e,
                    "list: decode failed, replying Err",
                );
                outbound.send_reply(
                    sender,
                    &ListResult::Err {
                        error: IoError::AdapterError(format!("decode failed: {e}")),
                    },
                );
                return;
            }
        };
        let Some(adapter) = registry.get(&req.namespace) else {
            outbound.send_reply(
                sender,
                &ListResult::Err {
                    error: IoError::UnknownNamespace,
                },
            );
            return;
        };
        let _ = match adapter.list(&req.prefix) {
            Ok(entries) => outbound.send_reply(sender, &ListResult::Ok { entries }),
            Err(error) => outbound.send_reply(sender, &ListResult::Err { error }),
        };
    } else {
        tracing::warn!(
            target: "aether_substrate::io",
            kind_id,
            "io sink received unknown kind — dropping",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;

    fn scratch_root(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let nonce: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let path = temp_dir().join(format!("aether-io-test-{tag}-{pid}-{nonce}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
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
    // handler closure, and reads the outbound reply channel to
    // confirm the correct `*Result` variant was sent back. The
    // outbound is built via `HubOutbound::test_channel` which skips
    // the TCP plumbing but keeps `send_reply`'s encode path live.

    use crate::hub_client::HubOutbound;
    use aether_hub_protocol::{EngineToHub, SessionToken, Uuid};

    fn build_registry(root: &Path, writable: bool) -> Arc<AdapterRegistry> {
        let adapter: Arc<dyn FileAdapter> =
            Arc::new(LocalFileAdapter::new(root.to_path_buf(), writable).unwrap());
        let mut r = AdapterRegistry::new();
        r.register("save", adapter);
        Arc::new(r)
    }

    fn session_sender() -> ReplyTo {
        ReplyTo::Session(SessionToken(Uuid::nil()))
    }

    fn decode_reply<K: aether_mail::Kind + serde::de::DeserializeOwned>(
        rx: &std::sync::mpsc::Receiver<EngineToHub>,
    ) -> K {
        // The test channel gets `EngineToHub::Mail` frames from
        // `send_reply`; pull the first one and decode its payload.
        let frame = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        let EngineToHub::Mail(m) = frame else {
            panic!("expected Mail frame, got {frame:?}");
        };
        assert_eq!(m.kind_name, K::NAME);
        postcard::from_bytes(&m.payload).unwrap()
    }

    #[test]
    fn dispatch_read_ok_replies_with_bytes() {
        let root = scratch_root("dispatch-read");
        let reg = build_registry(&root, true);
        let (outbound, rx) = HubOutbound::test_channel();
        reg.get("save")
            .unwrap()
            .write("slot.bin", &[9, 9, 9])
            .unwrap();
        let handler = io_sink_handler(Arc::clone(&reg), Arc::clone(&outbound));
        let req = postcard::to_allocvec(&Read {
            namespace: "save".to_string(),
            path: "slot.bin".to_string(),
        })
        .unwrap();
        handler(
            <Read as Kind>::ID,
            Read::NAME,
            None,
            session_sender(),
            &req,
            1,
        );
        match decode_reply::<ReadResult>(&rx) {
            ReadResult::Ok { bytes } => assert_eq!(bytes, vec![9, 9, 9]),
            ReadResult::Err { error } => panic!("expected Ok, got Err({error:?})"),
        }
        cleanup(&root);
    }

    #[test]
    fn dispatch_read_unknown_namespace_replies_err() {
        let root = scratch_root("dispatch-ns");
        let reg = build_registry(&root, true);
        let (outbound, rx) = HubOutbound::test_channel();
        let handler = io_sink_handler(Arc::clone(&reg), Arc::clone(&outbound));
        let req = postcard::to_allocvec(&Read {
            namespace: "nope".to_string(),
            path: "x.bin".to_string(),
        })
        .unwrap();
        handler(
            <Read as Kind>::ID,
            Read::NAME,
            None,
            session_sender(),
            &req,
            1,
        );
        assert!(matches!(
            decode_reply::<ReadResult>(&rx),
            ReadResult::Err {
                error: IoError::UnknownNamespace
            }
        ));
        cleanup(&root);
    }

    #[test]
    fn dispatch_read_not_found_replies_err() {
        let root = scratch_root("dispatch-nf");
        let reg = build_registry(&root, true);
        let (outbound, rx) = HubOutbound::test_channel();
        let handler = io_sink_handler(Arc::clone(&reg), Arc::clone(&outbound));
        let req = postcard::to_allocvec(&Read {
            namespace: "save".to_string(),
            path: "ghost.bin".to_string(),
        })
        .unwrap();
        handler(
            <Read as Kind>::ID,
            Read::NAME,
            None,
            session_sender(),
            &req,
            1,
        );
        assert!(matches!(
            decode_reply::<ReadResult>(&rx),
            ReadResult::Err {
                error: IoError::NotFound
            }
        ));
        cleanup(&root);
    }

    #[test]
    fn dispatch_write_ok_persists_bytes() {
        let root = scratch_root("dispatch-write");
        let reg = build_registry(&root, true);
        let (outbound, rx) = HubOutbound::test_channel();
        let handler = io_sink_handler(Arc::clone(&reg), Arc::clone(&outbound));
        let req = postcard::to_allocvec(&Write {
            namespace: "save".to_string(),
            path: "slot.bin".to_string(),
            bytes: vec![1, 2, 3],
        })
        .unwrap();
        handler(
            <Write as Kind>::ID,
            Write::NAME,
            None,
            session_sender(),
            &req,
            1,
        );
        assert!(matches!(decode_reply::<WriteResult>(&rx), WriteResult::Ok));
        assert_eq!(
            reg.get("save").unwrap().read("slot.bin").unwrap(),
            vec![1, 2, 3]
        );
        cleanup(&root);
    }

    #[test]
    fn dispatch_write_read_only_namespace_replies_forbidden() {
        let root = scratch_root("dispatch-ro");
        let reg = build_registry(&root, false);
        let (outbound, rx) = HubOutbound::test_channel();
        let handler = io_sink_handler(Arc::clone(&reg), Arc::clone(&outbound));
        let req = postcard::to_allocvec(&Write {
            namespace: "save".to_string(),
            path: "slot.bin".to_string(),
            bytes: vec![],
        })
        .unwrap();
        handler(
            <Write as Kind>::ID,
            Write::NAME,
            None,
            session_sender(),
            &req,
            1,
        );
        assert!(matches!(
            decode_reply::<WriteResult>(&rx),
            WriteResult::Err {
                error: IoError::Forbidden
            }
        ));
        cleanup(&root);
    }

    #[test]
    fn dispatch_delete_then_read_surfaces_not_found() {
        let root = scratch_root("dispatch-del");
        let reg = build_registry(&root, true);
        let (outbound, rx) = HubOutbound::test_channel();
        reg.get("save").unwrap().write("x.bin", b"x").unwrap();
        let handler = io_sink_handler(Arc::clone(&reg), Arc::clone(&outbound));
        let req = postcard::to_allocvec(&Delete {
            namespace: "save".to_string(),
            path: "x.bin".to_string(),
        })
        .unwrap();
        handler(
            <Delete as Kind>::ID,
            Delete::NAME,
            None,
            session_sender(),
            &req,
            1,
        );
        assert!(matches!(
            decode_reply::<DeleteResult>(&rx),
            DeleteResult::Ok
        ));
        assert!(matches!(
            reg.get("save").unwrap().read("x.bin"),
            Err(IoError::NotFound)
        ));
        cleanup(&root);
    }

    #[test]
    fn dispatch_list_returns_sorted_entries() {
        let root = scratch_root("dispatch-list");
        let reg = build_registry(&root, true);
        let (outbound, rx) = HubOutbound::test_channel();
        reg.get("save").unwrap().write("b.bin", b"").unwrap();
        reg.get("save").unwrap().write("a.bin", b"").unwrap();
        let handler = io_sink_handler(Arc::clone(&reg), Arc::clone(&outbound));
        let req = postcard::to_allocvec(&List {
            namespace: "save".to_string(),
            prefix: "".to_string(),
        })
        .unwrap();
        handler(
            <List as Kind>::ID,
            List::NAME,
            None,
            session_sender(),
            &req,
            1,
        );
        match decode_reply::<ListResult>(&rx) {
            ListResult::Ok { entries } => {
                assert_eq!(entries, vec!["a.bin".to_string(), "b.bin".to_string()]);
            }
            ListResult::Err { error } => panic!("expected Ok, got Err({error:?})"),
        }
        cleanup(&root);
    }

    #[test]
    fn dispatch_unknown_kind_id_does_not_reply() {
        // An unrelated kind id hitting the io sink should warn-drop,
        // not panic, and must not produce a reply (nothing for the
        // sender to be waiting on).
        let root = scratch_root("dispatch-unknown");
        let reg = build_registry(&root, true);
        let (outbound, rx) = HubOutbound::test_channel();
        let handler = io_sink_handler(Arc::clone(&reg), Arc::clone(&outbound));
        handler(0xdead_beef, "some.other", None, session_sender(), &[], 1);
        assert!(rx.try_recv().is_err(), "unexpected reply on unknown kind");
        cleanup(&root);
    }

    #[test]
    fn dispatch_malformed_payload_replies_adapter_error() {
        // Bytes that don't postcard-decode as `Read`. Dispatcher
        // must fall through to the decode-error branch and reply
        // with `IoError::AdapterError` rather than hang.
        let root = scratch_root("dispatch-mal");
        let reg = build_registry(&root, true);
        let (outbound, rx) = HubOutbound::test_channel();
        let handler = io_sink_handler(Arc::clone(&reg), Arc::clone(&outbound));
        handler(
            <Read as Kind>::ID,
            Read::NAME,
            None,
            session_sender(),
            &[0xffu8; 4],
            1,
        );
        assert!(matches!(
            decode_reply::<ReadResult>(&rx),
            ReadResult::Err {
                error: IoError::AdapterError(_)
            }
        ));
        cleanup(&root);
    }
}
