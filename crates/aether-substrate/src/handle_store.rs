// Wire-encode: `usize → u32` narrowings encode handle-cache byte
// lengths into the postcard varint slots described in the wire
// format below; `u32 → u64` widenings move handle ids into the
// 64-bit id slot. Both are part of the load-bearing wire layout.
#![allow(clippy::cast_lossless, clippy::cast_possible_truncation)]
// `HandleStore` Mutex guards are intentionally held across read-then-
// update sequences (lookup + refcount mutation, eviction scan + drop)
// — releasing the guard mid-sequence opens a TOCTOU window where
// another caller could mutate the store between the read and the
// dependent action.
#![allow(clippy::significant_drop_tightening)]

//! ADR-0045 typed-handle store and ref-walking dispatch hook.
//!
//! The substrate keeps a refcounted, byte-addressed cache of handle
//! values keyed by 64-bit handle id. Components publish a value into
//! the store and pass `Ref::Handle { id, kind_id }` on the wire instead
//! of the inline value; the substrate resolves the handle on dispatch
//! and substitutes the inline bytes before delivering the mail.
//!
//! Wire format (ADR-0045 §1, inline arm revised by ADR-0100):
//! - Inline arm: discriminant 0 + `varint(len)` + `K::encode_into_bytes`
//!   (`len` bytes) — the kind's own codec image (cast or postcard),
//!   an opaque length-delimited blob the walker skips by length.
//! - Handle arm: discriminant 1 + `varint(id)` + `varint(kind_id)`.
//!
//! Resolution is structural: the walker reads the schema and skips
//! through the payload bytes, splicing inline-discriminant + cached
//! bytes at every Handle position. Mail addressed to an unresolved
//! handle parks under that handle's id; the next put-and-resolve
//! drains the queue and re-routes through the mailer.
//!
//! The store enforces a soft byte budget (`max_bytes`, configurable
//! via `AETHER_HANDLE_STORE_MAX_BYTES`, default 256 MB). Eviction is
//! LRU among entries with `refcount == 0 && !pinned`; pinned and
//! refcounted entries stay regardless of pressure (a pinned-only
//! store at the cap rejects inserts with `EvictionFailed`).
//!
//! v1 scope (PR 2 of Phase 1): substrate-side store + walker, hooked
//! into `Mailer::push` between recipient lookup and dispatch. Host-fn
//! shims for component-side publish/release land in PR 3.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::{Error as IoError, ErrorKind, Write as _};
use std::mem;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::ConfigError;
use crate::mail::Mail;
use crate::mail::registry::Registry;
use aether_data::{EnumVariant, Primitive, SchemaType};
use aether_data::{HandleId, KindId};
use std::env;

pub mod meta;

use meta::{
    HandleMeta, INDEX_FORMAT_VERSION, IndexEntry, IndexSnapshot, SCHEMA_VERSION, TransformOrigin,
};

/// Default byte cap for the handle store.
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024 * 1024;

/// Env var that overrides `DEFAULT_MAX_BYTES`. Read once at boot
/// (`SubstrateBoot::build`) and parsed as a `usize` of bytes; absent
/// or unparseable values fall back to the default.
pub const ENV_MAX_BYTES: &str = "AETHER_HANDLE_STORE_MAX_BYTES";

/// Env var pointing at the on-disk persistence root (ADR-0049 §2).
/// Absent or empty falls through to `dirs::data_dir()/aether/handles`.
pub const ENV_PERSIST_DIR: &str = "AETHER_HANDLE_STORE_DIR";

/// Env var that disables on-disk persistence outright. Set to `1` by
/// the `TestBench` harness + CI so unit tests don't leak entries into
/// the user's data dir.
pub const ENV_PERSIST_DISABLE: &str = "AETHER_HANDLE_STORE_PERSIST_DISABLE";

/// On-disk layout version directory under [`ENV_PERSIST_DIR`]. The
/// `v1/` namespacing is ADR-0049 §8's forward-compatibility hook.
pub const LAYOUT_VERSION_DIR: &str = "v1";

/// Env var overriding the on-disk byte budget (ADR-0049 §5). When the
/// ledger exceeds this, the eviction tick drops refcount-0 + unpinned
/// entries oldest-first. Default [`DEFAULT_DISK_BUDGET_BYTES`].
pub const ENV_DISK_BUDGET_BYTES: &str = "AETHER_HANDLE_STORE_DISK_BUDGET_BYTES";

/// Env var overriding the eviction tick interval in seconds. Default
/// [`DEFAULT_DISK_EVICTION_TICK_SECS`].
pub const ENV_DISK_EVICTION_TICK_SECS: &str = "AETHER_HANDLE_STORE_DISK_EVICTION_TICK_SECS";

/// Default on-disk byte budget (16 GiB per ADR-0049 §3).
pub const DEFAULT_DISK_BUDGET_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// Default eviction tick interval (60s per ADR-0049 §5).
pub const DEFAULT_DISK_EVICTION_TICK_SECS: u64 = 60;

/// Tracing target shared by every persistence diagnostic.
const TARGET: &str = "aether_substrate::handle_store";

/// Resolved on-disk persistence configuration. Carried on the
/// [`HandleStore`] when persistence is enabled (desktop + headless
/// chassis); `None` on the hub chassis and on test fixtures with
/// [`ENV_PERSIST_DISABLE`] set.
#[derive(Debug, Clone)]
pub struct PersistConfig {
    /// `${AETHER_HANDLE_STORE_DIR}/v1/` — the layout-versioned root that
    /// holds `entries/`, `pinned.set`, and `lock.pid`.
    pub root: PathBuf,
    /// On-disk byte budget (ADR-0049 §5). The eviction tick drops
    /// candidates until the ledger falls below this.
    pub disk_budget_bytes: u64,
    /// Eviction tick interval in seconds.
    pub eviction_tick_secs: u64,
}

impl PersistConfig {
    /// Resolve the persistence config from the environment, or `None`
    /// when persistence is disabled (`AETHER_HANDLE_STORE_PERSIST_DISABLE=1`)
    /// or when the data dir can't be resolved and no override is set.
    ///
    /// `enabled` is the chassis's verdict: desktop + headless pass
    /// `true`; the hub passes `false` (ADR-0049 §9). A `false` chassis
    /// vote short-circuits to `None` regardless of env.
    /// Resolution runs through confique (ADR-0090) for the two numeric
    /// budget / tick knobs via the private `PersistConfigLayer`; the
    /// `enabled` chassis vote, the `ENV_PERSIST_DISABLE` short-circuit,
    /// and the root-or-`None` resolution stay hand-written because their
    /// outcome is structural (a missing data dir disables persistence
    /// entirely, not a literal default confique could hold). Behaviour is
    /// byte-identical to the prior hand-rolled reader — an unparseable
    /// budget / tick still falls back to its default. The hard-error
    /// stance (ADR-0090 §4) lands with the chassis-env validation pass.
    ///
    /// # Panics
    ///
    /// Panics only if the layer's literal defaults are themselves
    /// malformed — a programmer error caught by the
    /// `persist_config_layer_defaults_match` test, never a runtime config
    /// fault (the env values flow through total parsers).
    #[must_use]
    pub fn from_env(enabled: bool) -> Option<Self> {
        Self::resolve(enabled, None, None, None)
    }

    /// Resolve from a chassis-CLI argv overlay shadowing env (ADR-0090
    /// unit d, issue 1258). `dir` / `disable` win against
    /// `AETHER_HANDLE_STORE_DIR` / `AETHER_HANDLE_STORE_PERSIST_DISABLE`
    /// when `Some`; the numeric-knob overlay is preloaded into the
    /// confique builder so argv-set budget / tick win against
    /// `AETHER_HANDLE_STORE_*` env. `enabled` is the chassis's vote (a
    /// `false` short-circuits to `None` regardless of argv).
    ///
    /// # Panics
    ///
    /// Same as [`Self::from_env`]: only on a malformed literal default
    /// (programmer error caught by
    /// `persist_config_layer_defaults_match`).
    #[must_use]
    pub fn from_argv_then_env(
        enabled: bool,
        dir: Option<PathBuf>,
        disable: Option<bool>,
        numeric: <PersistConfigLayer as confique::Config>::Layer,
    ) -> Option<Self> {
        Self::resolve(enabled, dir, disable, Some(numeric))
    }

    /// Shared resolution path. Argv overrides (when `Some`) win against
    /// the env variable; absent argv falls through to env, then to the
    /// platform default.
    fn resolve(
        enabled: bool,
        dir_argv: Option<PathBuf>,
        disable_argv: Option<bool>,
        numeric_argv: Option<<PersistConfigLayer as confique::Config>::Layer>,
    ) -> Option<Self> {
        use confique::Config as _;

        if !enabled {
            return None;
        }
        // disable: argv wins; absent falls through to env (=`1` ⇒ true).
        let disabled =
            disable_argv.unwrap_or_else(|| env::var(ENV_PERSIST_DISABLE).is_ok_and(|v| v == "1"));
        if disabled {
            return None;
        }
        let base = if let Some(d) = dir_argv {
            d
        } else {
            match env::var(ENV_PERSIST_DIR) {
                Ok(raw) if !raw.is_empty() => PathBuf::from(raw),
                _ => {
                    let Some(data) = dirs::data_dir() else {
                        tracing::warn!(
                            target: TARGET,
                            "no data dir and no AETHER_HANDLE_STORE_DIR; persistence disabled",
                        );
                        return None;
                    };
                    data.join("aether").join("handles")
                }
            }
        };
        // Every layer field has a literal default and a total parser, so
        // the layer always resolves; a failure here would be a malformed
        // default literal (caught by `persist_config_layer_defaults_match`).
        let mut builder = PersistConfigLayer::builder();
        if let Some(argv) = numeric_argv {
            builder = builder.preloaded(argv);
        }
        let layer = builder
            .env()
            .load()
            .expect("PersistConfigLayer defaults are well-formed");
        Some(Self {
            root: base.join(LAYOUT_VERSION_DIR),
            disk_budget_bytes: layer.disk_budget_bytes,
            eviction_tick_secs: layer.eviction_tick_secs,
        })
    }

    /// The `entries/` subdirectory under the layout root.
    #[must_use]
    pub fn entries_dir(&self) -> PathBuf {
        self.root.join("entries")
    }

    /// The `pinned.set` file under the layout root.
    #[must_use]
    pub fn pinned_set_path(&self) -> PathBuf {
        self.root.join("pinned.set")
    }

    /// The `lock.pid` file under the layout root (ADR-0049 §7).
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.root.join("lock.pid")
    }

    /// The `index.bin` boot fast-path snapshot under the layout root
    /// (ADR-0049 §3). Written on graceful shutdown; loaded then deleted
    /// at boot.
    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.bin")
    }
}

/// Why acquiring the on-disk store lock failed (ADR-0049 §7).
#[derive(Debug)]
pub enum LockError {
    /// Another live substrate already holds the lock. Boot must abort.
    Held { path: PathBuf, pid: i32 },
    /// The lockfile couldn't be written (permission, disk full, etc.).
    Io { path: PathBuf, error: IoError },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Held { path, pid } => write!(
                f,
                "handle store at {} is locked by a live substrate (pid {pid}); \
                 set AETHER_HANDLE_STORE_DIR to a different path or terminate that process",
                path.display(),
            ),
            Self::Io { path, error } => {
                write!(
                    f,
                    "failed to write handle store lock {}: {error}",
                    path.display()
                )
            }
        }
    }
}

impl StdError for LockError {}

/// RAII guard that deletes `lock.pid` on graceful shutdown. SIGKILL
/// bypasses `Drop`; the stale-lock reclamation path handles that case
/// on the next boot.
#[derive(Debug)]
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Whether `pid` names a live process. Unix: `kill(pid, 0)` returns 0
/// for a live process, `ESRCH` for a dead one, `EPERM` for a live one
/// we can't signal (still counts as alive). Non-Unix: conservatively
/// reports `false` so the lock is always reclaimable (substrate on
/// Windows is deferred per ADR-0049 §7).
#[cfg(unix)]
fn is_pid_alive(pid: i32) -> bool {
    // SAFETY: `kill` with signal 0 performs the error checks without
    // sending a signal. No memory is touched.
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return true;
    }
    // errno == EPERM means the process exists but we lack permission.
    IoError::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: i32) -> bool {
    false
}

/// A handle known to be on disk but not (necessarily) materialized in
/// memory. Populated by the boot scan (issue #985) from the `.meta`
/// sidecars; lets a cache-miss lookup find the `.bin` without re-reading
/// the meta.
#[derive(Debug, Clone)]
pub struct DiskEntry {
    pub kind_id: KindId,
    pub bytes_len: u32,
    pub pinned: bool,
    pub created_at: u64,
}

/// Read-only snapshot of the store for `describe_handles` (ADR-0049
/// §10). Built by [`HandleStore::inspect`] under a single read-lock
/// hold so the caller can serialize without keeping the store locked.
#[derive(Debug, Clone)]
pub struct HandleStoreSnapshot {
    pub total_entries: usize,
    pub in_memory_entries: usize,
    pub on_disk_entries: usize,
    pub pinned_entries: usize,
    pub in_memory_bytes: u64,
    pub on_disk_bytes: u64,
    pub on_disk_budget_bytes: u64,
    pub top_by_size: Vec<HandleSummary>,
    pub top_by_recency: Vec<HandleSummary>,
}

/// Per-handle summary line in a [`HandleStoreSnapshot`].
#[derive(Debug, Clone)]
pub struct HandleSummary {
    pub handle_id: HandleId,
    pub kind_id: KindId,
    pub bytes_len: u32,
    pub pinned: bool,
    pub refcount: u32,
    pub created_at_ms: u64,
}

/// Kind-name → current-id resolver used by the schema-evolution check
/// (ADR-0049 §6). The substrate's `Registry` implements this; the boot
/// scan calls it once per `.meta` to detect schema drift. Decoupled
/// from `Registry` directly so test fixtures can supply a synthetic
/// resolver without standing up a full registry.
pub trait KindResolver: Send + Sync {
    /// Current id for the named kind, or `None` if the kind isn't
    /// registered.
    fn id_for_name(&self, name: &str) -> Option<KindId>;
    /// Current name for the given kind id, or `None` if the id isn't
    /// registered. Used at write time to stamp `meta.kind_name`.
    fn name_for_id(&self, id: KindId) -> Option<String>;
}

impl KindResolver for Registry {
    fn id_for_name(&self, name: &str) -> Option<KindId> {
        self.kind_id(name)
    }

    fn name_for_id(&self, id: KindId) -> Option<String> {
        self.kind_name(id)
    }
}

/// Outcome of validating an on-disk `.meta` against the current kind
/// registry (ADR-0049 §6).
enum Validation {
    /// The entry's kind matches the current registry; keep it.
    Valid,
    /// The entry is stale (schema changed, kind retired, or unsupported
    /// version); drop it. Carries the reason for `engine_logs`.
    Drop(String),
}

/// Validate one `.meta` against the resolver (ADR-0049 §6). An
/// unsupported `schema_version`, an unknown kind name, or a kind-id
/// mismatch all invalidate the entry. With no resolver (test fixtures
/// that don't model schema evolution), only the version check applies.
fn validate_meta(meta: &HandleMeta, resolver: Option<&dyn KindResolver>) -> Validation {
    if meta.schema_version != SCHEMA_VERSION {
        return Validation::Drop(format!(
            "schema_version {} unsupported (current {SCHEMA_VERSION})",
            meta.schema_version,
        ));
    }
    let Some(resolver) = resolver else {
        return Validation::Valid;
    };
    match resolver.id_for_name(&meta.kind_name) {
        Some(current) if current.0 == meta.kind_id => Validation::Valid,
        Some(current) => Validation::Drop(format!(
            "kind '{}' id changed: {:016x} -> {:016x}",
            meta.kind_name, meta.kind_id, current.0,
        )),
        None => Validation::Drop(format!("kind '{}' no longer registered", meta.kind_name)),
    }
}

/// Compute the `(<bin>, <meta>)` paths for a handle id under `root`.
/// 256 prefix-shard directories keyed on the first hex byte of the id
/// (ADR-0049 §2).
#[must_use]
pub fn entry_paths(root: &Path, id: HandleId) -> (PathBuf, PathBuf) {
    let hex = format!("{:016x}", id.0);
    let dir = root.join("entries").join(&hex[0..2]);
    (
        dir.join(format!("{}.bin", &hex[2..])),
        dir.join(format!("{}.meta", &hex[2..])),
    )
}

/// Env-shaped confique layer behind the numeric knobs of
/// [`PersistConfig`] (ADR-0090). `disk_budget_bytes` and
/// `eviction_tick_secs` carry their `AETHER_HANDLE_STORE_*` values; the
/// `root` resolution stays hand-written in [`PersistConfig::from_env`]
/// (its default is structural — a missing data dir disables persistence,
/// not a literal). Public so chassis CLI overlays (ADR-0090 unit d,
/// issue 1258) can preload a `<PersistConfigLayer as
/// confique::Config>::Layer` before `.env()`; the consumed shape stays
/// `PersistConfig`.
#[derive(confique::Config)]
pub struct PersistConfigLayer {
    /// On-disk byte budget. Literal default mirrors
    /// [`DEFAULT_DISK_BUDGET_BYTES`] (16 GiB); the
    /// `persist_config_layer_defaults_match` test guards the match.
    #[config(
        env = "AETHER_HANDLE_STORE_DISK_BUDGET_BYTES",
        parse_env = parse_disk_budget_bytes,
        default = 17_179_869_184u64
    )]
    pub disk_budget_bytes: u64,
    /// Eviction tick interval in seconds. Literal default mirrors
    /// [`DEFAULT_DISK_EVICTION_TICK_SECS`] (60 s).
    #[config(
        env = "AETHER_HANDLE_STORE_DISK_EVICTION_TICK_SECS",
        parse_env = parse_eviction_tick_secs,
        default = 60u64
    )]
    pub eviction_tick_secs: u64,
}

// confique's `parse_env` contract is `fn(&str) -> Result<T, impl Error>`,
// so these total helpers carry a `Result` they never fill with `Err` — an
// unparseable value folds back to the same default as the prior
// `parse_env_u64` (the warn-on-malformed log is dropped, the disposition is
// byte-identical). The strict (erroring) variant lands with the ADR-0090 §4
// validation pass; hence the per-fn `unnecessary_wraps` allow.

/// Parse the disk byte budget; unparseable falls back to
/// [`DEFAULT_DISK_BUDGET_BYTES`]. Total — never errors.
#[allow(clippy::unnecessary_wraps)]
fn parse_disk_budget_bytes(s: &str) -> Result<u64, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_DISK_BUDGET_BYTES))
}

/// Parse the eviction tick seconds; unparseable falls back to
/// [`DEFAULT_DISK_EVICTION_TICK_SECS`]. Total — never errors.
#[allow(clippy::unnecessary_wraps)]
fn parse_eviction_tick_secs(s: &str) -> Result<u64, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_DISK_EVICTION_TICK_SECS))
}

/// Millis since the unix epoch, saturating to 0 if the clock is before
/// the epoch (unreachable in practice).
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Atomic write via tmp+rename (ADR-0041's `LocalFileAdapter` pattern):
/// stage to a sibling `.tmp-<pid>-<nonce>`, fsync it, rename over the
/// target. Creates the parent dir lazily. Returns the io error on
/// failure so the caller can log + continue (persistence is best-effort
/// per ADR-0049 §3).
fn atomic_write(target: &Path, bytes: &[u8]) -> Result<(), IoError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let nonce = now_millis();
    let pid = process::id();
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("entry");
    let tmp = target.with_file_name(format!("{file_name}.tmp-{pid}-{nonce}"));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        // fsync the tmp file so its bytes hit disk before the rename
        // publishes it (preserves ordering across a crash).
        f.sync_all()?;
    }
    match fs::rename(&tmp, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Per-entry store record. Bytes are the postcard-encoded `K` body
/// (the same shape `Ref::Inline` would carry), kept owned because the
/// walker copies them into spliced output during dispatch.
#[derive(Debug)]
struct HandleEntry {
    kind: KindId,
    bytes: Vec<u8>,
    refcount: u32,
    pinned: bool,
    /// Monotonic counter at last access; lower = older. Bumped on
    /// `put` and `get`. Wraparound at `u64::MAX` is unreachable in
    /// practice (4.6e18 dispatches ≈ 146 years at 1 GHz).
    ///
    /// Atomic so a cache-hit `get` bumps it through a shared `&Inner`
    /// under the read lock (issue #1447) instead of taking the write
    /// lock. `Relaxed` is sufficient: it is a monotone recency
    /// heuristic and eviction reads it under the exclusive write lock.
    last_access: AtomicU64,
}

/// FIFO cap on the negative cache (ADR-0049 §3). Protects high-
/// cardinality-miss pipelines from re-statting the same non-existent
/// paths every dispatch.
const NEGATIVE_CACHE_CAP: usize = 8 * 1024;

#[derive(Default)]
struct Inner {
    entries: HashMap<HandleId, HandleEntry>,
    /// Mail held back because the walker hit a missing handle id.
    /// Keyed on the missing id; drained when the matching `put` lands
    /// or when the matching parked queue is explicitly cleared.
    parked: HashMap<HandleId, VecDeque<Mail>>,
    total_bytes: usize,
    /// Monotonic source for `HandleEntry::last_access`. Atomic so a
    /// cache-hit `get` can bump it under a shared `&Inner` read lock
    /// (issue #1447). `Relaxed` is sufficient — see `bump_clock`.
    access_clock: AtomicU64,
    next_ephemeral: u64,
    /// Sparse "this handle is on disk but not in memory" index, built
    /// by the boot scan from the `.meta` sidecars (ADR-0049 §3). Lets a
    /// cache-miss `get` find the `.bin` without re-reading the meta.
    /// ~80 bytes/entry; a 25k-entry store costs ~2MB.
    disk_index: HashMap<HandleId, DiskEntry>,
    /// Bounded FIFO of "checked, confirmed not on disk" ids. Capped at
    /// [`NEGATIVE_CACHE_CAP`] with FIFO eviction.
    negative_cache: VecDeque<HandleId>,
    /// Approximate ledger of on-disk bytes (ADR-0049 §5). Bumped on each
    /// persistent write, decremented on disk delete; the eviction tick
    /// uses it to decide when to evict. Approximate (content-addressed
    /// re-writes of the same id over-count until the next boot scan
    /// reconciles).
    total_disk_bytes: u64,
}

impl Inner {
    /// Record `id` as confirmed-not-on-disk, evicting the oldest entry
    /// if the cache is at cap.
    fn note_negative(&mut self, id: HandleId) {
        if self.negative_cache.len() >= NEGATIVE_CACHE_CAP {
            self.negative_cache.pop_front();
        }
        self.negative_cache.push_back(id);
    }
}

/// Refcounted, byte-budgeted handle cache shared between mailer
/// dispatch and (in PR 3+) the host-fn shims components use to
/// publish values.
pub struct HandleStore {
    inner: RwLock<Inner>,
    max_bytes: usize,
    /// On-disk persistence config (ADR-0049). `Some` on desktop +
    /// headless chassis; `None` on hub + test fixtures. When present,
    /// [`Self::put_persistent`] mirrors the in-memory write to disk and
    /// pin/unpin rewrite `pinned.set`.
    persist: Option<PersistConfig>,
    /// `lock.pid` guard (ADR-0049 §7). Held once [`Self::acquire_lock`]
    /// succeeds; its `Drop` deletes the lockfile on graceful shutdown.
    /// `None` until acquired (or when persistence is disabled).
    lock: Mutex<Option<LockGuard>>,
    /// Kind-name → current-id resolver for the schema-evolution check
    /// (ADR-0049 §6). Supplied at construction (the substrate's
    /// `Registry`); `None` on fixtures that don't model schema drift —
    /// the boot scan then only enforces the `schema_version` check.
    kind_resolver: Option<Arc<dyn KindResolver>>,
}

/// Reasons a `put` can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutError {
    /// An entry already exists at `id` under a different kind id.
    /// Updates that match the existing kind go through; mismatches
    /// are loud because the `(id, kind)` pair is part of the wire
    /// contract — silently rebinding the same id to a new type would
    /// let a stale `Ref::Handle { kind_id }` decode against bytes
    /// that aren't shaped like its claimed type.
    KindMismatch {
        existing_kind: KindId,
        requested_kind: KindId,
    },
    /// Eviction couldn't free enough room. Every remaining entry is
    /// pinned or refcounted, so the requested insert can't fit even
    /// after dropping all evictable entries.
    EvictionFailed { needed: usize, max_bytes: usize },
}

/// Outcome of walking a payload against its schema, threaded through
/// the handle store.
#[derive(Debug)]
pub enum WalkOutcome<'a> {
    /// Every handle resolved (or the schema contained no refs at
    /// all). `payload` is `Cow::Borrowed(input)` when no substitution
    /// happened; `Cow::Owned(...)` when the walker spliced one or
    /// more handle bodies into the output.
    Resolved { payload: Cow<'a, [u8]> },
    /// Walker hit a handle id with no matching entry in the store.
    /// The mailer parks the original mail on `handle`; the next
    /// `put(handle, ...)` drains and re-routes. `kind` is the
    /// expected inner kind id, kept for diagnostic logging — the
    /// re-route walks the schema again and pulls the same id either
    /// way.
    Parked { handle: HandleId, kind: KindId },
}

/// Reasons a wire walk can fail. The mailer treats any of these as
/// "drop the mail with a warn log" — they all signal that the wire
/// payload doesn't match the descriptor the substrate has registered
/// for this kind id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkError {
    Truncated,
    InvalidBool,
    UnknownEnumDiscriminant,
    VarintOverflow,
    UnknownRefDiscriminant,
}

impl HandleStore {
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            inner: RwLock::new(Inner {
                next_ephemeral: 1,
                ..Default::default()
            }),
            max_bytes,
            persist: None,
            lock: Mutex::new(None),
            kind_resolver: None,
        }
    }

    /// Build an in-memory store with on-disk persistence wired in. The
    /// caller resolves the [`PersistConfig`] (typically via
    /// [`PersistConfig::from_env`]); the boot scan populates the disk
    /// index from the existing tree (ADR-0049 §3). No kind resolver, so
    /// the schema-evolution check (ADR-0049 §6) only enforces the
    /// `schema_version` gate — use [`Self::with_persist_validated`] to
    /// wire one.
    #[must_use]
    pub fn with_persist(max_bytes: usize, persist: Option<PersistConfig>) -> Self {
        Self::with_persist_validated(max_bytes, persist, None)
    }

    /// Build a persistent store with a kind resolver for the
    /// schema-evolution check (ADR-0049 §6). The resolver (the
    /// substrate's `Registry`) lets the boot scan detect a kind whose
    /// schema changed or was retired and drop its stale on-disk entries.
    #[must_use]
    pub fn with_persist_validated(
        max_bytes: usize,
        persist: Option<PersistConfig>,
        kind_resolver: Option<Arc<dyn KindResolver>>,
    ) -> Self {
        let store = Self {
            inner: RwLock::new(Inner {
                next_ephemeral: 1,
                ..Default::default()
            }),
            max_bytes,
            persist,
            lock: Mutex::new(None),
            kind_resolver,
        };
        // ADR-0049 §3 boot fast-path (issue #1446): try to load the
        // `index.bin` snapshot a clean shutdown left behind, collapsing
        // the per-`.meta` directory scan into one read + decode. On any
        // failure — missing, unreadable, version skew, decode error —
        // fall through to the directory walk, which stays the
        // correctness primitive (it scrubs orphans + runs the §6
        // schema-evolution check). The fast path defers that validation
        // to lazy materialization (see `lookup_from_disk`).
        if !store.load_index_bin() {
            store.restore_from_disk();
        }
        store
    }

    /// Borrow the wired persistence config, if any.
    #[must_use]
    pub fn persist_config(&self) -> Option<&PersistConfig> {
        self.persist.as_ref()
    }

    /// Build a store sized from `AETHER_HANDLE_STORE_MAX_BYTES` if
    /// set, otherwise `DEFAULT_MAX_BYTES`.
    ///
    /// ADR-0090 §4 (unit e1): an *unparseable* value is now a hard
    /// [`ConfigError`] rather than a soft warn-and-default — the
    /// soft→hard flip unit b1 deferred. An empty value is treated as
    /// unset (falls back to the default). The chassis env resolver
    /// `?`-propagates the error so a garbage budget aborts boot
    /// loudly rather than silently shrinking the cache.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnparseableKnown`] when
    /// `AETHER_HANDLE_STORE_MAX_BYTES` is set to a non-empty value
    /// that doesn't parse as `usize`.
    pub fn from_env() -> Result<Self, ConfigError> {
        let max_bytes = match env::var(ENV_MAX_BYTES) {
            Ok(raw) if raw.trim().is_empty() => DEFAULT_MAX_BYTES,
            Ok(raw) => match raw.trim().parse::<usize>() {
                Ok(n) => n,
                Err(e) => return Err(ConfigError::unparseable(ENV_MAX_BYTES, raw, e)),
            },
            Err(_) => DEFAULT_MAX_BYTES,
        };
        Ok(Self::new(max_bytes))
    }

    /// Build a store sized from the environment with on-disk
    /// persistence resolved from [`PersistConfig::from_env`]. `enabled`
    /// is the chassis verdict (desktop + headless `true`, hub `false`
    /// per ADR-0049 §9). `kind_resolver` (the substrate's `Registry`)
    /// drives the schema-evolution check (ADR-0049 §6). When persistence
    /// resolves to `Some`, the boot scan (issue #985) populates the disk
    /// index from the existing tree, dropping schema-stale entries.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnparseableKnown`] when
    /// `AETHER_HANDLE_STORE_MAX_BYTES` is set to garbage (ADR-0090
    /// §4 — via [`Self::from_env`]).
    pub fn from_env_persistent(
        enabled: bool,
        kind_resolver: Option<Arc<dyn KindResolver>>,
    ) -> Result<Self, ConfigError> {
        let max_bytes = Self::from_env()?.max_bytes;
        Ok(Self::with_persist_validated(
            max_bytes,
            PersistConfig::from_env(enabled),
            kind_resolver,
        ))
    }

    /// Mint a fresh ephemeral handle id. Pure counter today; content-
    /// addressed ids land in Phase 3. `0` is reserved as the
    /// "no-handle" sentinel — the counter starts at 1 and never
    /// returns 0.
    ///
    /// ADR-0064: the high 4 bits carry `Tag::Handle` so handle ids
    /// are bit-distinguishable from mailbox / kind ids. The counter
    /// occupies the low 60 bits — at one mint per nanosecond it
    /// wraps in ~37 years, well past any single substrate lifetime.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn next_ephemeral(&self) -> HandleId {
        let mut inner = self
            .inner
            .write()
            .expect("handle store lock poisoned; fail-fast per ADR-0063");
        let counter = inner.next_ephemeral;
        inner.next_ephemeral = inner.next_ephemeral.wrapping_add(1);
        if inner.next_ephemeral == 0 {
            inner.next_ephemeral = 1;
        }
        HandleId(aether_data::with_tag(aether_data::Tag::Handle, counter))
    }

    /// Insert (or update) a handle. The same `(id, kind)` pair can
    /// be re-put with new bytes; mismatched `kind` against an
    /// existing entry is a `KindMismatch` error. Refcount and pinned
    /// state survive a same-kind re-put — the publisher updating
    /// bytes shouldn't silently break references held by other code.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned, or if the eviction
    /// pass underflows the byte accounting — fail-fast per ADR-0063:
    /// both indicate a substrate-level invariant violation.
    pub fn put(&self, id: HandleId, kind: KindId, bytes: Vec<u8>) -> Result<(), PutError> {
        let mut inner = self
            .inner
            .write()
            .expect("handle store lock poisoned; fail-fast per ADR-0063");
        let (prior_size, refcount, pinned) = match inner.entries.get(&id) {
            Some(e) if e.kind != kind => {
                return Err(PutError::KindMismatch {
                    existing_kind: e.kind,
                    requested_kind: kind,
                });
            }
            Some(e) => (e.bytes.len(), e.refcount, e.pinned),
            None => (0, 0, false),
        };
        let needed = bytes.len();
        let projected = inner.total_bytes + needed - prior_size;
        if projected > self.max_bytes {
            evict_until_fits(&mut inner, projected - self.max_bytes, self.max_bytes, id)?;
        }
        let last_access = bump_clock(&inner);
        // Clean up the prior entry's bytes accounting (if any). The
        // eviction step skipped this id, so the entry is still in the
        // map.
        if let Some(prior) = inner.entries.remove(&id) {
            inner.total_bytes -= prior.bytes.len();
        }
        inner.total_bytes += needed;
        inner.entries.insert(
            id,
            HandleEntry {
                kind,
                bytes,
                refcount,
                pinned,
                last_access: AtomicU64::new(last_access),
            },
        );
        Ok(())
    }

    /// Insert (or update) a handle and mirror it to disk when
    /// persistence is wired (ADR-0049 §3). The in-memory `put` runs
    /// first and is authoritative for this run; the disk write is
    /// best-effort — a failure logs a warning but leaves the in-memory
    /// entry valid (the caller doesn't see a different return value).
    ///
    /// `origin` records the transform provenance, or `None` for a
    /// pinned source handle.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    // `origin` is taken by value: it's the caller's provenance record
    // handed to the store, even though the disk write only borrows it.
    #[allow(clippy::needless_pass_by_value)]
    pub fn put_persistent(
        &self,
        id: HandleId,
        kind: KindId,
        bytes: Vec<u8>,
        origin: Option<TransformOrigin>,
    ) -> Result<(), PutError> {
        // In-memory write is authoritative; clone the bytes only when
        // there's a disk target to mirror to.
        let pinned = self
            .inner
            .read()
            .expect("handle store lock poisoned; fail-fast per ADR-0063")
            .entries
            .get(&id)
            .is_some_and(|e| e.pinned);
        if let Some(cfg) = self.persist.clone() {
            let disk_bytes = bytes.clone();
            self.put(id, kind, bytes)?;
            self.write_to_disk(&cfg, id, kind, &disk_bytes, origin.as_ref(), pinned);
            Ok(())
        } else {
            self.put(id, kind, bytes)
        }
    }

    /// Write the `.bin` + `.meta` sidecar pair for `id` atomically. Best
    /// effort: a failure on either file logs and returns without
    /// touching the in-memory state.
    fn write_to_disk(
        &self,
        cfg: &PersistConfig,
        id: HandleId,
        kind: KindId,
        bytes: &[u8],
        origin: Option<&TransformOrigin>,
        pinned: bool,
    ) {
        let (bin_path, meta_path) = entry_paths(&cfg.root, id);
        // Stamp the kind's current name so the schema-evolution check
        // (ADR-0049 §6) can look it up by name on restore. Falls back to
        // the hex id when no resolver is wired (test fixtures) — a
        // synthetic name that won't match any registry entry, so such an
        // entry invalidates on the first registry-backed restore. That's
        // the correct conservative behaviour.
        let kind_name = self
            .kind_resolver
            .as_ref()
            .and_then(|r| r.name_for_id(kind))
            .unwrap_or_else(|| format!("{:016x}", kind.0));
        let meta = HandleMeta {
            schema_version: SCHEMA_VERSION,
            handle_id: id.0,
            kind_id: kind.0,
            kind_name,
            transform_origin: origin.cloned(),
            bytes_len: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
            created_at: now_millis(),
            pinned,
        };
        let meta_bytes = match postcard::to_allocvec(&meta) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: TARGET,
                    handle = %id,
                    error = %e,
                    "failed to encode HandleMeta; skipping disk persist",
                );
                return;
            }
        };
        if let Err(e) = atomic_write(&bin_path, bytes) {
            tracing::warn!(
                target: TARGET,
                handle = %id,
                path = %bin_path.display(),
                error = %e,
                "handle store .bin write failed; in-memory entry retained (best-effort persist)",
            );
            return;
        }
        if let Err(e) = atomic_write(&meta_path, &meta_bytes) {
            tracing::warn!(
                target: TARGET,
                handle = %id,
                path = %meta_path.display(),
                error = %e,
                "handle store .meta write failed; orphan .bin will be scrubbed on next boot",
            );
            return;
        }
        // Register the on-disk entry in the index + ledger so a later
        // in-memory eviction still finds it on disk and the eviction
        // tick accounts for its bytes (ADR-0049 §5). A re-write of the
        // same id over-counts the ledger until the next boot scan
        // reconciles — approximate by design.
        let mut inner = self
            .inner
            .write()
            .expect("handle store lock poisoned; fail-fast per ADR-0063");
        inner.total_disk_bytes = inner
            .total_disk_bytes
            .saturating_add(u64::from(meta.bytes_len));
        inner.disk_index.insert(
            id,
            DiskEntry {
                kind_id: kind,
                bytes_len: meta.bytes_len,
                pinned,
                created_at: meta.created_at,
            },
        );
        // The entry is now genuinely on disk — clear any negative-cache
        // shadow.
        inner.negative_cache.retain(|cached| *cached != id);
    }

    /// Rewrite `pinned.set` from the current in-memory pinned set. Cheap
    /// (one `u64` per pinned id); atomic via tmp+rename. Called from
    /// pin/unpin so the durable pinned set tracks the live set. No-op
    /// when persistence is disabled.
    fn persist_pinned_set(&self) {
        let Some(cfg) = self.persist.as_ref() else {
            return;
        };
        let mut ids: Vec<u64> = {
            let inner = self
                .inner
                .read()
                .expect("handle store lock poisoned; fail-fast per ADR-0063");
            // Union the in-memory pinned set with the on-disk pinned
            // index so a pinned disk-only entry stays in `pinned.set`.
            let mut set: HashSet<u64> = inner
                .entries
                .iter()
                .filter(|(_, e)| e.pinned)
                .map(|(id, _)| id.0)
                .collect();
            set.extend(
                inner
                    .disk_index
                    .iter()
                    .filter(|(_, e)| e.pinned)
                    .map(|(id, _)| id.0),
            );
            set.into_iter().collect()
        };
        ids.sort_unstable();
        let mut bytes = Vec::with_capacity(ids.len() * 8);
        for id in ids {
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        if let Err(e) = atomic_write(&cfg.pinned_set_path(), &bytes) {
            tracing::warn!(
                target: TARGET,
                path = %cfg.pinned_set_path().display(),
                error = %e,
                "pinned.set write failed (best-effort persist)",
            );
        }
    }

    /// Write the `index.bin` boot fast-path snapshot from the live disk
    /// index (ADR-0049 §3). Called from the chassis driver teardown on
    /// graceful shutdown — the same graceful point at which `LockGuard`
    /// removes `lock.pid`. Best-effort: an encode or write failure
    /// warn-logs and returns, leaving the next boot to fall back to the
    /// directory scan. No-op when persistence is disabled.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    pub fn snapshot_index(&self) {
        let Some(cfg) = self.persist.as_ref() else {
            return;
        };
        let entries: HashMap<u64, IndexEntry> = {
            let inner = self
                .inner
                .read()
                .expect("handle store lock poisoned; fail-fast per ADR-0063");
            inner
                .disk_index
                .iter()
                .map(|(id, e)| {
                    (
                        id.0,
                        IndexEntry {
                            kind_id: e.kind_id.0,
                            bytes_len: e.bytes_len,
                            pinned: e.pinned,
                            created_at: e.created_at,
                        },
                    )
                })
                .collect()
        };
        let count = entries.len();
        let snapshot = IndexSnapshot {
            schema_version: INDEX_FORMAT_VERSION,
            entries,
        };
        let bytes = match postcard::to_allocvec(&snapshot) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: TARGET,
                    error = %e,
                    "failed to encode index.bin snapshot; next boot falls back to the directory scan",
                );
                return;
            }
        };
        let path = cfg.index_path();
        if let Err(e) = atomic_write(&path, &bytes) {
            tracing::warn!(
                target: TARGET,
                path = %path.display(),
                error = %e,
                "index.bin snapshot write failed; next boot falls back to the directory scan",
            );
            return;
        }
        tracing::debug!(
            target: TARGET,
            indexed = count,
            "handle store index.bin snapshot written on shutdown",
        );
    }

    /// Boot fast-path (ADR-0049 §3): try to populate `disk_index` from a
    /// previously-written `index.bin` snapshot. Returns `true` when the
    /// fast path took (the index is populated and `index.bin` deleted so
    /// a later crash can't replay a stale snapshot); `false` on any
    /// failure — no persistence, missing / unreadable file, version skew,
    /// or decode error — so the caller falls back to
    /// [`Self::restore_from_disk`].
    ///
    /// On success this recomputes `total_disk_bytes` from the loaded
    /// entries' `bytes_len` sum (the same recompute the directory scan
    /// does), reconciling the approximate ledger across the restart. The
    /// §6 schema-evolution check is skipped here and deferred to
    /// [`Self::lookup_from_disk`]: the snapshot exists only after a
    /// graceful shutdown, so the tree is consistent and the index already
    /// reflects the live set.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    fn load_index_bin(&self) -> bool {
        let Some(cfg) = self.persist.as_ref() else {
            return false;
        };
        let path = cfg.index_path();
        let Ok(raw) = fs::read(&path) else {
            // Missing or unreadable — the common cold/crashed-boot case.
            // Fall back to the scan silently (a fresh store has no
            // snapshot; a crash left none).
            return false;
        };
        let snapshot = match postcard::from_bytes::<IndexSnapshot>(&raw) {
            Ok(s) if s.schema_version == INDEX_FORMAT_VERSION => s,
            Ok(s) => {
                tracing::warn!(
                    target: TARGET,
                    path = %path.display(),
                    found = s.schema_version,
                    expected = INDEX_FORMAT_VERSION,
                    "index.bin version skew; falling back to the directory scan",
                );
                return false;
            }
            Err(e) => {
                tracing::warn!(
                    target: TARGET,
                    path = %path.display(),
                    error = %e,
                    "index.bin decode failed; falling back to the directory scan",
                );
                return false;
            }
        };
        let index: HashMap<HandleId, DiskEntry> = snapshot
            .entries
            .into_iter()
            .map(|(id, e)| {
                (
                    HandleId(id),
                    DiskEntry {
                        kind_id: KindId(e.kind_id),
                        bytes_len: e.bytes_len,
                        pinned: e.pinned,
                        created_at: e.created_at,
                    },
                )
            })
            .collect();
        let count = index.len();
        let disk_bytes: u64 = index.values().map(|e| u64::from(e.bytes_len)).sum();
        {
            let mut inner = self
                .inner
                .write()
                .expect("handle store lock poisoned; fail-fast per ADR-0063");
            inner.disk_index = index;
            inner.total_disk_bytes = disk_bytes;
        }
        // Delete the snapshot so a crash before the next clean shutdown
        // can't replay a now-stale view (it is rewritten on the next
        // graceful shutdown).
        if let Err(e) = fs::remove_file(&path)
            && e.kind() != ErrorKind::NotFound
        {
            tracing::warn!(
                target: TARGET,
                path = %path.display(),
                error = %e,
                "failed to delete index.bin after load; a crash before the next clean shutdown could replay a stale snapshot",
            );
        }
        tracing::debug!(
            target: TARGET,
            indexed = count,
            "handle store boot fast-path loaded index.bin",
        );
        true
    }

    /// Boot scan (ADR-0049 §3): read `pinned.set`, walk `entries/` for
    /// `.meta` sidecars, and populate the sparse `disk_index`. Bytes are
    /// NOT eagerly loaded — disk-resident entries materialize on first
    /// access. A boot scrub deletes orphan `.bin` (no sibling meta) and
    /// orphan `.meta` (no sibling bin) so a crash mid-write doesn't leave
    /// the tree inconsistent. No-op when persistence is disabled.
    // One pass over the shard tree with inline scrub + validation arms;
    // splitting it would scatter the boot-scan invariants across helpers.
    #[allow(clippy::too_many_lines)]
    fn restore_from_disk(&self) {
        let Some(cfg) = self.persist.clone() else {
            return;
        };
        let pinned = read_pinned_set(&cfg);
        let entries_dir = cfg.entries_dir();
        let Ok(shards) = fs::read_dir(&entries_dir) else {
            // Fresh store (no entries/ yet) — nothing to scan.
            return;
        };

        let resolver = self.kind_resolver.as_deref();
        let mut index: HashMap<HandleId, DiskEntry> = HashMap::new();
        let mut orphan_bins = 0usize;
        let mut orphan_metas = 0usize;
        let mut invalidated = 0usize;
        let mut scrubbed = 0usize;

        for shard in shards.flatten() {
            let shard_path = shard.path();
            if !shard_path.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&shard_path) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                    continue;
                };
                // Only `.meta` sidecars are index entries. A `.bin`
                // without a sibling meta is an orphan, caught in the
                // second pass below; `.tmp` leftovers are swept there
                // too.
                if ext != "meta" {
                    continue;
                }
                let bin = path.with_extension("bin");
                let Some(meta) = read_meta_file(&path) else {
                    // Unreadable meta — drop it + its bin.
                    let _ = fs::remove_file(&path);
                    let _ = fs::remove_file(&bin);
                    scrubbed += 1;
                    continue;
                };
                if !bin.exists() {
                    // Orphan meta — bytes gone; the meta is unreachable.
                    // Scrub it.
                    let _ = fs::remove_file(&path);
                    orphan_metas += 1;
                    scrubbed += 1;
                    continue;
                }
                // ADR-0049 §6: schema-evolution check. A version skew,
                // retired kind, or changed kind id invalidates the entry
                // — the bytes are unsafe to decode against the current
                // schema, so drop both files. This overrides pin (a
                // pinned entry whose kind changed is still evicted — pin
                // protects against budget pressure, not against
                // correctness invalidation).
                if let Validation::Drop(reason) = validate_meta(&meta, resolver) {
                    let _ = fs::remove_file(&bin);
                    let _ = fs::remove_file(&path);
                    invalidated += 1;
                    tracing::info!(
                        target: TARGET,
                        handle = %HandleId(meta.handle_id),
                        reason = %reason,
                        "handle store entry invalidated by schema evolution; dropped",
                    );
                    continue;
                }
                let id = HandleId(meta.handle_id);
                index.insert(
                    id,
                    DiskEntry {
                        kind_id: KindId(meta.kind_id),
                        bytes_len: meta.bytes_len,
                        pinned: meta.pinned || pinned.contains(&id),
                        created_at: meta.created_at,
                    },
                );
            }
            // Second pass: scrub orphan bins (bin without sibling meta).
            if let Ok(files) = fs::read_dir(&shard_path) {
                for file in files.flatten() {
                    let path = file.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("bin") {
                        let meta = path.with_extension("meta");
                        if !meta.exists() {
                            let _ = fs::remove_file(&path);
                            orphan_bins += 1;
                            scrubbed += 1;
                        }
                    }
                    // Sweep leftover tmp files from interrupted writes.
                    if let Some(name) = path.file_name().and_then(|n| n.to_str())
                        && name.contains(".tmp-")
                    {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
        }

        let count = index.len();
        let disk_bytes: u64 = index.values().map(|e| u64::from(e.bytes_len)).sum();
        {
            let mut inner = self
                .inner
                .write()
                .expect("handle store lock poisoned; fail-fast per ADR-0063");
            inner.disk_index = index;
            inner.total_disk_bytes = disk_bytes;
        }
        if scrubbed > 0 || invalidated > 0 {
            tracing::info!(
                target: TARGET,
                indexed = count,
                orphan_bins,
                orphan_metas,
                invalidated,
                "handle store boot scan complete; scrubbed inconsistent + schema-stale entries",
            );
        } else {
            tracing::debug!(
                target: TARGET,
                indexed = count,
                "handle store boot scan complete",
            );
        }
    }

    /// Resolve `id` from disk on an in-memory cache miss (ADR-0049 §3).
    /// Materializes the bytes into the in-memory store with refcount 0
    /// and returns them. A `.bin` that the index expected but that's
    /// missing is treated as corruption: the index entry is dropped and
    /// the id lands in the negative cache. Returns `None` when there's
    /// no persistence, no index hit, or the id is negatively cached.
    ///
    /// The §6 schema-evolution check (ADR-0049) the boot scan runs
    /// eagerly is deferred to this cold materialization for entries that
    /// arrived via the `index.bin` fast path (issue #1446, which skips
    /// the scan): the sibling `.meta` is read and validated against the
    /// current kind registry, and a stale entry (kind id changed /
    /// retired since the snapshot, or a version skew) is dropped at
    /// access time — both files deleted, the ledger decremented, the id
    /// negative-cached. An unreadable `.meta` skips the check and
    /// materializes from the index entry (scan-path entries were already
    /// validated at boot, so re-reading the `.meta` there only confirms a
    /// match).
    fn lookup_from_disk(&self, id: HandleId) -> Option<(KindId, Vec<u8>)> {
        let cfg = self.persist.as_ref()?;
        // Read the index entry + negative-cache state under a read lock.
        let disk_entry = {
            let inner = self
                .inner
                .read()
                .expect("handle store lock poisoned; fail-fast per ADR-0063");
            if inner.negative_cache.contains(&id) {
                return None;
            }
            inner.disk_index.get(&id).cloned()?
        };
        let (bin_path, meta_path) = entry_paths(&cfg.root, id);
        // ADR-0049 §6 deferred schema-evolution check (issue #1446 fast
        // path). A readable `.meta` is validated against the current
        // registry; a `Drop` verdict invalidates the entry here rather
        // than at boot.
        if let Some(meta) = read_meta_file(&meta_path)
            && let Validation::Drop(reason) = validate_meta(&meta, self.kind_resolver.as_deref())
        {
            let _ = fs::remove_file(&bin_path);
            let _ = fs::remove_file(&meta_path);
            let mut inner = self
                .inner
                .write()
                .expect("handle store lock poisoned; fail-fast per ADR-0063");
            inner.total_disk_bytes = inner
                .total_disk_bytes
                .saturating_sub(u64::from(disk_entry.bytes_len));
            inner.disk_index.remove(&id);
            inner.note_negative(id);
            tracing::info!(
                target: TARGET,
                handle = %id,
                reason = %reason,
                "handle store entry invalidated by schema evolution on materialization; dropped",
            );
            return None;
        }
        let Ok(bytes) = fs::read(&bin_path) else {
            // `.bin` missing but the index said it was there —
            // corruption. Treat as a miss + remember it.
            let mut inner = self
                .inner
                .write()
                .expect("handle store lock poisoned; fail-fast per ADR-0063");
            inner.disk_index.remove(&id);
            inner.note_negative(id);
            return None;
        };
        let mut inner = self
            .inner
            .write()
            .expect("handle store lock poisoned; fail-fast per ADR-0063");
        let last_access = bump_clock(&inner);
        inner.total_bytes += bytes.len();
        inner.entries.insert(
            id,
            HandleEntry {
                kind: disk_entry.kind_id,
                bytes: bytes.clone(),
                refcount: 0,
                pinned: disk_entry.pinned,
                last_access: AtomicU64::new(last_access),
            },
        );
        Some((disk_entry.kind_id, bytes))
    }

    /// Current approximate on-disk byte ledger (ADR-0049 §5). Test +
    /// observability accessor.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    #[must_use]
    pub fn disk_bytes(&self) -> u64 {
        self.inner
            .read()
            .expect("handle store lock poisoned; fail-fast per ADR-0063")
            .total_disk_bytes
    }

    /// Run one disk-eviction pass synchronously (ADR-0049 §5). Public so
    /// the eviction thread + tests can trigger it. When over budget,
    /// evicts disk entries that are refcount-0 in memory AND unpinned,
    /// oldest-`created_at` first, two-phase (`.bin` then `.meta`), until
    /// the ledger drops below the budget or no candidates remain. No-op
    /// when persistence is disabled.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    pub fn run_disk_eviction(&self) {
        let Some(cfg) = self.persist.clone() else {
            return;
        };
        // Snapshot candidates under a read lock, then mutate under a
        // write lock per victim — the fs delete happens between, off the
        // lock.
        let (over_budget, mut candidates) = {
            let inner = self
                .inner
                .read()
                .expect("handle store lock poisoned; fail-fast per ADR-0063");
            if inner.total_disk_bytes <= cfg.disk_budget_bytes {
                return;
            }
            // refcount-0 in memory (or not in memory at all) AND unpinned.
            let candidates: Vec<(HandleId, u64, u32)> = inner
                .disk_index
                .iter()
                .filter(|(id, e)| {
                    !e.pinned && inner.entries.get(id).is_none_or(|m| m.refcount == 0)
                })
                .map(|(id, e)| (*id, e.created_at, e.bytes_len))
                .collect();
            (inner.total_disk_bytes, candidates)
        };
        // Oldest first.
        candidates.sort_by_key(|(_, created_at, _)| *created_at);

        let mut freed = 0u64;
        let budget = cfg.disk_budget_bytes;
        let mut evicted = 0usize;
        for (id, _, bytes_len) in candidates {
            if over_budget.saturating_sub(freed) <= budget {
                break;
            }
            let (bin_path, meta_path) = entry_paths(&cfg.root, id);
            // Phase A: drop the bytes.
            if let Err(e) = fs::remove_file(&bin_path)
                && e.kind() != ErrorKind::NotFound
            {
                tracing::warn!(
                    target: TARGET,
                    handle = %id,
                    path = %bin_path.display(),
                    error = %e,
                    "eviction phase A (.bin) failed; retrying next tick",
                );
                continue;
            }
            // Phase B: drop the index entry.
            if let Err(e) = fs::remove_file(&meta_path)
                && e.kind() != ErrorKind::NotFound
            {
                tracing::warn!(
                    target: TARGET,
                    handle = %id,
                    path = %meta_path.display(),
                    error = %e,
                    "eviction phase B (.meta) failed; orphan .bin already gone, will retry",
                );
            }
            // Update the ledger + index.
            {
                let mut inner = self
                    .inner
                    .write()
                    .expect("handle store lock poisoned; fail-fast per ADR-0063");
                inner.total_disk_bytes =
                    inner.total_disk_bytes.saturating_sub(u64::from(bytes_len));
                inner.disk_index.remove(&id);
            }
            freed = freed.saturating_add(u64::from(bytes_len));
            evicted += 1;
        }
        if evicted > 0 {
            tracing::info!(
                target: TARGET,
                evicted,
                freed_bytes = freed,
                budget,
                "handle store disk eviction pass complete",
            );
        }
    }

    /// Acquire the on-disk store lock (ADR-0049 §7). Writes `lock.pid`
    /// with this process's PID after checking that any existing lock is
    /// stale. A live conflicting lock returns [`LockError::Held`] so the
    /// caller can abort boot with a clear error. No-op (returns `Ok`)
    /// when persistence is disabled.
    ///
    /// On success the store holds a lock guard whose `Drop` deletes the
    /// lockfile on graceful shutdown.
    ///
    /// # Panics
    /// Panics if the lock mutex is poisoned — fail-fast per ADR-0063.
    pub fn acquire_lock(&self) -> Result<(), LockError> {
        let Some(cfg) = self.persist.as_ref() else {
            return Ok(());
        };
        let path = cfg.lock_path();
        // Inspect any existing lock.
        if let Ok(raw) = fs::read_to_string(&path) {
            match raw.trim().parse::<i32>() {
                Ok(pid) if pid > 0 && is_pid_alive(pid) => {
                    return Err(LockError::Held { path, pid });
                }
                Ok(pid) => {
                    tracing::warn!(
                        target: TARGET,
                        path = %path.display(),
                        stale_pid = pid,
                        "reclaiming stale handle store lock from a dead process",
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        target: TARGET,
                        path = %path.display(),
                        "lock.pid holds garbage; reclaiming as stale",
                    );
                }
            }
        }
        // Write our PID atomically.
        let pid = process::id();
        atomic_write(&path, pid.to_string().as_bytes()).map_err(|error| LockError::Io {
            path: path.clone(),
            error,
        })?;
        *self
            .lock
            .lock()
            .expect("handle store lock mutex poisoned; fail-fast per ADR-0063") =
            Some(LockGuard { path });
        Ok(())
    }

    /// Snapshot the store state for `describe_handles` (ADR-0049 §10).
    /// Takes the read lock once, copies the summary out, releases it
    /// before the caller serializes. `max` caps the top-N lists.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    #[must_use]
    pub fn inspect(&self, max: usize) -> HandleStoreSnapshot {
        let inner = self
            .inner
            .read()
            .expect("handle store lock poisoned; fail-fast per ADR-0063");
        let in_memory_entries = inner.entries.len();
        let in_memory_bytes = inner.total_bytes as u64;
        // On-disk entry set is the disk index; in-memory-only entries
        // (not yet persisted, e.g. ephemeral sources) are counted under
        // in_memory. Total is the union of ids.
        let on_disk_entries = inner.disk_index.len();
        let mut all_ids: HashSet<HandleId> = inner.entries.keys().copied().collect();
        all_ids.extend(inner.disk_index.keys().copied());

        // Build summaries from the union, preferring in-memory data
        // (it has refcount + live pinned state) and falling back to the
        // disk index.
        let mut summaries: Vec<HandleSummary> = all_ids
            .into_iter()
            .map(|id| {
                let mem = inner.entries.get(&id);
                let disk = inner.disk_index.get(&id);
                let kind_id = mem.map_or_else(|| disk.map_or(KindId(0), |d| d.kind_id), |m| m.kind);
                let bytes_len = mem.map_or_else(
                    || disk.map_or(0u32, |d| d.bytes_len),
                    |m| u32::try_from(m.bytes.len()).unwrap_or(u32::MAX),
                );
                let pinned = mem
                    .map(|m| m.pinned)
                    .or_else(|| disk.map(|d| d.pinned))
                    .unwrap_or(false);
                let refcount = mem.map_or(0, |m| m.refcount);
                let created_at = disk.map_or(0, |d| d.created_at);
                HandleSummary {
                    handle_id: id,
                    kind_id,
                    bytes_len,
                    pinned,
                    refcount,
                    created_at_ms: created_at,
                }
            })
            .collect();

        let pinned_entries = summaries.iter().filter(|s| s.pinned).count();
        let total_entries = summaries.len();

        // top_by_size: descending bytes_len.
        let mut by_size = summaries.clone();
        by_size.sort_by(|a, b| {
            b.bytes_len
                .cmp(&a.bytes_len)
                .then(a.handle_id.0.cmp(&b.handle_id.0))
        });
        by_size.truncate(max);

        // top_by_recency: descending created_at.
        summaries.sort_by(|a, b| {
            b.created_at_ms
                .cmp(&a.created_at_ms)
                .then(a.handle_id.0.cmp(&b.handle_id.0))
        });
        summaries.truncate(max);

        let (on_disk_bytes, on_disk_budget_bytes) = (
            inner.total_disk_bytes,
            self.persist.as_ref().map_or(0, |c| c.disk_budget_bytes),
        );

        HandleStoreSnapshot {
            total_entries,
            in_memory_entries,
            on_disk_entries,
            pinned_entries,
            in_memory_bytes,
            on_disk_bytes,
            on_disk_budget_bytes,
            top_by_size: by_size,
            top_by_recency: summaries,
        }
    }

    /// Spawn the background eviction tick (ADR-0049 §5). The thread owns
    /// a [`std::sync::Weak`] reference to avoid a reference cycle; on the
    /// store's last `Arc` dropping, the weak fails to upgrade and the
    /// thread exits. No-op when persistence is disabled. Call once at
    /// boot after wrapping the store in an `Arc`.
    pub fn spawn_eviction_thread(self: &Arc<Self>) {
        let Some(cfg) = self.persist.clone() else {
            return;
        };
        let weak = Arc::downgrade(self);
        let tick = Duration::from_secs(cfg.eviction_tick_secs.max(1));
        // Long-lived eviction timer — spawned without a ctx, no inbound chain to
        // inherit; periodic infra, not per-handler work.
        #[allow(clippy::disallowed_methods)]
        thread::Builder::new()
            .name("handle-store-evict".to_owned())
            .spawn(move || {
                loop {
                    thread::sleep(tick);
                    let Some(store) = weak.upgrade() else {
                        // Store dropped; thread exits.
                        return;
                    };
                    store.run_disk_eviction();
                }
            })
            .ok();
    }

    /// Mark `id` as pinned: it won't be evicted under memory pressure
    /// regardless of `refcount`. Returns `false` if the id isn't in
    /// the store.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn pin(&self, id: HandleId) -> bool {
        self.set_pinned(id, true)
    }

    /// Set the pinned flag on `id` across the in-memory entry AND the
    /// on-disk index (so the eviction tick skips a pinned disk-resident
    /// entry that isn't materialized in memory, ADR-0049 §5). Rewrites
    /// `pinned.set` when either was touched. Returns `false` if `id`
    /// isn't known in either place.
    fn set_pinned(&self, id: HandleId, value: bool) -> bool {
        let found = {
            let mut inner = self
                .inner
                .write()
                .expect("handle store lock poisoned; fail-fast per ADR-0063");
            let in_mem = inner
                .entries
                .get_mut(&id)
                .map(|e| e.pinned = value)
                .is_some();
            let on_disk = inner
                .disk_index
                .get_mut(&id)
                .map(|d| d.pinned = value)
                .is_some();
            in_mem || on_disk
        };
        if found {
            self.persist_pinned_set();
        }
        found
    }

    /// Clear the pinned flag on `id`. Doesn't drop the entry; only
    /// makes it eligible for LRU eviction once `refcount == 0`.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn unpin(&self, id: HandleId) -> bool {
        self.set_pinned(id, false)
    }

    /// Increment the refcount on `id`. Returns `false` if the id isn't
    /// in the store. Saturating: held references past `u32::MAX` clamp
    /// rather than wrap, so the `dec_ref` underflow guard never trips.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn inc_ref(&self, id: HandleId) -> bool {
        let mut inner = self
            .inner
            .write()
            .expect("handle store lock poisoned; fail-fast per ADR-0063");
        if let Some(entry) = inner.entries.get_mut(&id) {
            entry.refcount = entry.refcount.saturating_add(1);
            true
        } else {
            false
        }
    }

    /// Decrement the refcount on `id`. Returns `false` if the id isn't
    /// in the store. Saturating at zero: a double-drop doesn't
    /// underflow.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn dec_ref(&self, id: HandleId) -> bool {
        let mut inner = self
            .inner
            .write()
            .expect("handle store lock poisoned; fail-fast per ADR-0063");
        if let Some(entry) = inner.entries.get_mut(&id) {
            entry.refcount = entry.refcount.saturating_sub(1);
            true
        } else {
            false
        }
    }

    /// Look up an entry. Returns `(kind, bytes_clone)` so the
    /// caller can drop the lock before extending its output buffer.
    /// Bumps `last_access` so dispatch usage protects an entry from
    /// LRU eviction.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn get(&self, id: HandleId) -> Option<(KindId, Vec<u8>)> {
        {
            // Cache hit runs under the read lock: concurrent gets no
            // longer serialize against one another (issue #1447). The
            // recency bump goes through the shared `&Inner` because both
            // the clock source and `last_access` are atomic.
            let inner = self
                .inner
                .read()
                .expect("handle store lock poisoned; fail-fast per ADR-0063");
            if let Some(entry) = inner.entries.get(&id) {
                let access = bump_clock(&inner);
                entry.last_access.store(access, Ordering::Relaxed);
                return Some((entry.kind, entry.bytes.clone()));
            }
        }
        // In-memory miss: fall through to the on-disk store (no-op when
        // persistence is disabled or the id isn't indexed).
        self.lookup_from_disk(id)
    }

    /// Park a `Mail` under `handle_id`. The mailer calls this when
    /// `walk_and_resolve` returns `Parked`. The mail stays in the
    /// queue until a matching `put` or until the engine shuts down.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn park(&self, handle_id: HandleId, mail: Mail) {
        let mut inner = self
            .inner
            .write()
            .expect("handle store lock poisoned; fail-fast per ADR-0063");
        inner.parked.entry(handle_id).or_default().push_back(mail);
    }

    /// Drain the parked queue under `id`. Called by the mailer's
    /// resolve path so the freshly-resolved handle's parked mail
    /// re-routes through `route_mail` (re-walks the payload, possibly
    /// parks again on a different missing id, dispatches if fully
    /// resolved). Returns the drained mails in FIFO order; the
    /// `HashMap` entry itself is removed so a subsequent `parked_count`
    /// returns 0.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn take_parked(&self, id: HandleId) -> Vec<Mail> {
        let mut inner = self
            .inner
            .write()
            .expect("handle store lock poisoned; fail-fast per ADR-0063");
        inner.parked.remove(&id).map_or_else(Vec::new, Into::into)
    }

    /// Drain every parked queue and return all held mails, flattened in
    /// FIFO order within each handle's queue (cross-handle order is
    /// unspecified). The `parked` map is replaced with an empty one so
    /// every subsequent [`Self::parked_count`] call returns 0.
    ///
    /// This is the teardown-only counterpart to [`Self::take_parked`],
    /// which drains a single handle's queue for live replay. Call it
    /// from the terminal owner of parked mail (the `Mailer`'s `Drop`)
    /// once routing can no longer replay any parked entry.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn drain_all_parked(&self) -> Vec<Mail> {
        let mut inner = self
            .inner
            .write()
            .expect("handle store lock poisoned; fail-fast per ADR-0063");
        mem::take(&mut inner.parked)
            .into_values()
            .flatten()
            .collect()
    }

    /// Sum of `bytes.len()` across every entry in the store.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn total_bytes(&self) -> usize {
        self.inner
            .read()
            .expect("handle store lock poisoned; fail-fast per ADR-0063")
            .total_bytes
    }

    /// Count of stored entries (parked-mail queues not included).
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn entry_count(&self) -> usize {
        self.inner
            .read()
            .expect("handle store lock poisoned; fail-fast per ADR-0063")
            .entries
            .len()
    }

    /// Number of mails currently parked under `id`.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn parked_count(&self, id: HandleId) -> usize {
        self.inner
            .read()
            .expect("handle store lock poisoned; fail-fast per ADR-0063")
            .parked
            .get(&id)
            .map_or(0, VecDeque::len)
    }

    /// Configured byte cap; eviction kicks in once `total_bytes`
    /// would exceed this value.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// `true` if `id` is resolvable — in memory or on disk. A
    /// disk-resident entry counts: the DAG executor's pre-dispatch
    /// cache check (ADR-0048 §4) treats a disk hit as a hit so the
    /// transform short-circuits and `get` materializes the bytes
    /// (ADR-0049 §3).
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn contains(&self, id: HandleId) -> bool {
        let inner = self
            .inner
            .read()
            .expect("handle store lock poisoned; fail-fast per ADR-0063");
        inner.entries.contains_key(&id) || inner.disk_index.contains_key(&id)
    }

    /// `true` if `id` is currently materialized in the in-memory store
    /// (ignores the on-disk index). Test + introspection helper for
    /// asserting lazy-materialization behaviour.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    pub fn contains_in_memory(&self, id: HandleId) -> bool {
        self.inner
            .read()
            .expect("handle store lock poisoned; fail-fast per ADR-0063")
            .entries
            .contains_key(&id)
    }

    /// Count of entries in the on-disk index (disk-resident handles
    /// discovered by the boot scan, ADR-0049 §3). Test + observability
    /// helper.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    pub fn disk_index_len(&self) -> usize {
        self.inner
            .read()
            .expect("handle store lock poisoned; fail-fast per ADR-0063")
            .disk_index
            .len()
    }
}

/// Read `pinned.set` into a set of handle ids. The file is a flat
/// little-endian `u64` array. Missing / unreadable / malformed (length
/// not a multiple of 8) → empty set with a warn for the malformed case.
fn read_pinned_set(cfg: &PersistConfig) -> HashSet<HandleId> {
    let path = cfg.pinned_set_path();
    let Ok(raw) = fs::read(&path) else {
        return HashSet::new();
    };
    if raw.len() % 8 != 0 {
        tracing::warn!(
            target: TARGET,
            path = %path.display(),
            len = raw.len(),
            "pinned.set length not a multiple of 8; ignoring",
        );
        return HashSet::new();
    }
    raw.chunks_exact(8)
        .map(|c| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(c);
            HandleId(u64::from_le_bytes(bytes))
        })
        .collect()
}

/// Read + decode a `.meta` sidecar. Returns `None` on read or decode
/// failure (the boot scan treats that as a corrupt entry to scrub).
fn read_meta_file(path: &Path) -> Option<HandleMeta> {
    let raw = fs::read(path).ok()?;
    postcard::from_bytes::<HandleMeta>(&raw).ok()
}

/// Advance the access clock and return the new value. Takes `&Inner`
/// (not `&mut`) so a cache-hit `get` can bump recency under the read
/// lock (issue #1447); the write-lock callers (`put`,
/// `lookup_from_disk`) coerce their `&mut Inner` here for free.
///
/// `Relaxed` is sufficient: the clock is a monotone recency heuristic,
/// no other state is ordered against it, and eviction reads each
/// `last_access` under the exclusive write lock (which establishes the
/// happens-before barrier against read-lock-released bumps). The `+ 1`
/// recovers the post-increment value from `fetch_add`'s pre-increment
/// return; wraparound at `u64::MAX` is unreachable in practice (see
/// `HandleEntry::last_access`).
fn bump_clock(inner: &Inner) -> u64 {
    inner.access_clock.fetch_add(1, Ordering::Relaxed) + 1
}

/// Evict LRU entries until at least `need_to_free` bytes have been
/// dropped (or no more eligible entries remain). Pinned entries and
/// entries with `refcount > 0` are never touched, even if that means
/// the cap stays violated. `skip` excludes the slot the caller is
/// about to replace — its bytes are accounted as "already going
/// away" by the caller, so re-evicting it would double-count.
fn evict_until_fits(
    inner: &mut Inner,
    need_to_free: usize,
    max_bytes: usize,
    skip: HandleId,
) -> Result<(), PutError> {
    let mut candidates: Vec<(HandleId, u64, usize)> = inner
        .entries
        .iter()
        .filter(|(id, e)| **id != skip && e.refcount == 0 && !e.pinned)
        .map(|(id, e)| (*id, e.last_access.load(Ordering::Relaxed), e.bytes.len()))
        .collect();
    candidates.sort_by_key(|(_, last_access, _)| *last_access);

    let mut freed = 0usize;
    let mut evict_ids = Vec::new();
    for (id, _, sz) in candidates {
        if freed >= need_to_free {
            break;
        }
        evict_ids.push(id);
        freed += sz;
    }
    if freed < need_to_free {
        return Err(PutError::EvictionFailed {
            needed: need_to_free,
            max_bytes,
        });
    }
    for id in evict_ids {
        if let Some(e) = inner.entries.remove(&id) {
            inner.total_bytes -= e.bytes.len();
        }
    }
    Ok(())
}

/// True if any node anywhere in `schema` is `SchemaType::Ref`.
/// The mailer uses this as the fast-path predicate: kinds without
/// any refs skip the walker entirely and the original payload bytes
/// flow through unchanged.
#[must_use]
pub fn schema_contains_ref(schema: &SchemaType) -> bool {
    match schema {
        SchemaType::Ref(_) => true,
        SchemaType::Unit
        | SchemaType::Bool
        | SchemaType::Scalar(_)
        | SchemaType::String
        | SchemaType::Bytes
        // ADR-0065: typed-id leaves carry no nested fields and so
        // can never embed a `Ref`.
        | SchemaType::TypeId(_) => false,
        SchemaType::Option(inner) | SchemaType::Vec(inner) => schema_contains_ref(inner),
        SchemaType::Array { element, .. } => schema_contains_ref(element),
        SchemaType::Struct { fields, .. } => fields.iter().any(|f| schema_contains_ref(&f.ty)),
        SchemaType::Enum { variants } => variants.iter().any(|v| match v {
            EnumVariant::Unit { .. } => false,
            EnumVariant::Tuple { fields, .. } => fields.iter().any(schema_contains_ref),
            EnumVariant::Struct { fields, .. } => fields.iter().any(|f| schema_contains_ref(&f.ty)),
        }),
        // Issue #232: keys are restricted to `String`/integer/`Bool`
        // (none of which can carry a `Ref`), but the codec rejects
        // those defensively rather than the type system, so be
        // conservative and walk both sides.
        SchemaType::Map { key, value } => schema_contains_ref(key) || schema_contains_ref(value),
    }
}

/// Walk `payload` against `schema`, splicing every `Ref::Handle`
/// into its `Ref::Inline` form by looking up the cached bytes in
/// `store`. See `WalkOutcome` for the two terminal states.
pub fn walk_and_resolve<'a>(
    schema: &SchemaType,
    payload: &'a [u8],
    store: &HandleStore,
) -> Result<WalkOutcome<'a>, WalkError> {
    if !schema_contains_ref(schema) {
        return Ok(WalkOutcome::Resolved {
            payload: Cow::Borrowed(payload),
        });
    }
    let mut state = State {
        input: payload,
        pos: 0,
        out: Vec::new(),
        prefix_end: 0,
        out_initialised: false,
    };
    if let Some(parked) = walk(schema, &mut state, store)? {
        return Ok(WalkOutcome::Parked {
            handle: parked.0,
            kind: parked.1,
        });
    }
    let payload = state.finalize();
    Ok(WalkOutcome::Resolved { payload })
}

/// Walker state: tracks input, current position, and a lazily-built
/// output buffer used only when at least one substitution happens.
/// `prefix_end` is the byte index in `input` whose preceding bytes
/// have been flushed into `out`. Until the first substitution,
/// `out_initialised` stays false and `out` stays empty.
struct State<'a> {
    input: &'a [u8],
    pos: usize,
    out: Vec<u8>,
    prefix_end: usize,
    out_initialised: bool,
}

impl<'a> State<'a> {
    fn flush_up_to(&mut self, end: usize) {
        if !self.out_initialised {
            self.out.reserve(self.input.len());
            self.out_initialised = true;
        }
        self.out
            .extend_from_slice(&self.input[self.prefix_end..end]);
        self.prefix_end = end;
    }

    fn finalize(mut self) -> Cow<'a, [u8]> {
        if !self.out_initialised {
            return Cow::Borrowed(self.input);
        }
        self.out.extend_from_slice(&self.input[self.prefix_end..]);
        Cow::Owned(self.out)
    }

    fn read_byte(&mut self) -> Result<u8, WalkError> {
        if self.pos >= self.input.len() {
            return Err(WalkError::Truncated);
        }
        let b = self.input[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn read_varint(&mut self) -> Result<u64, WalkError> {
        let mut n: u64 = 0;
        let mut shift: u32 = 0;
        for _ in 0..10 {
            let b = self.read_byte()?;
            n |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(n);
            }
            shift += 7;
        }
        Err(WalkError::VarintOverflow)
    }

    fn skip_n(&mut self, n: usize) -> Result<(), WalkError> {
        if self.pos + n > self.input.len() {
            return Err(WalkError::Truncated);
        }
        self.pos += n;
        Ok(())
    }

    fn skip_varint(&mut self) -> Result<(), WalkError> {
        for _ in 0..10 {
            let b = self.read_byte()?;
            if b & 0x80 == 0 {
                return Ok(());
            }
        }
        Err(WalkError::VarintOverflow)
    }
}

/// Walk one `schema` node, advancing `state.pos` past its postcard
/// wire. Returns `Ok(Some((handle, kind)))` to signal "park on this
/// handle", `Ok(None)` for fully-walked, `Err(...)` for malformed
/// wire.
// One match arm per `SchemaType` variant; extracting per-type helpers
// would force per-arm `&mut State<'_>` plumbing without saving
// readability.
#[allow(clippy::too_many_lines)]
fn walk(
    schema: &SchemaType,
    state: &mut State<'_>,
    store: &HandleStore,
) -> Result<Option<(HandleId, KindId)>, WalkError> {
    match schema {
        SchemaType::Unit => Ok(None),
        SchemaType::Bool => {
            let b = state.read_byte()?;
            if b > 1 {
                return Err(WalkError::InvalidBool);
            }
            Ok(None)
        }
        SchemaType::Scalar(p) => {
            skip_primitive_postcard(state, *p)?;
            Ok(None)
        }
        SchemaType::String | SchemaType::Bytes => {
            let len = state.read_varint()? as usize;
            state.skip_n(len)?;
            Ok(None)
        }
        SchemaType::Option(inner) => {
            let tag = state.read_byte()?;
            match tag {
                0 => Ok(None),
                1 => walk(inner, state, store),
                _ => Err(WalkError::InvalidBool),
            }
        }
        SchemaType::Vec(inner) => {
            let len = state.read_varint()? as usize;
            for _ in 0..len {
                if let Some(parked) = walk(inner, state, store)? {
                    return Ok(Some(parked));
                }
            }
            Ok(None)
        }
        SchemaType::Array { element, len } => {
            for _ in 0..*len {
                if let Some(parked) = walk(element, state, store)? {
                    return Ok(Some(parked));
                }
            }
            Ok(None)
        }
        SchemaType::Struct { fields, .. } => {
            // Postcard wire encodes a struct as concatenated field
            // bytes regardless of `repr_c`. The walker is only
            // invoked on postcard kinds (cast-shaped kinds skip the
            // walker via the fast path), so descending into each
            // field as postcard is correct.
            for f in fields.iter() {
                if let Some(parked) = walk(&f.ty, state, store)? {
                    return Ok(Some(parked));
                }
            }
            Ok(None)
        }
        SchemaType::Enum { variants } => {
            let disc = state.read_varint()? as u32;
            let variant = variants
                .iter()
                .find(|v| v.discriminant() == disc)
                .ok_or(WalkError::UnknownEnumDiscriminant)?;
            match variant {
                EnumVariant::Unit { .. } => Ok(None),
                EnumVariant::Tuple { fields, .. } => {
                    for ty in fields.iter() {
                        if let Some(parked) = walk(ty, state, store)? {
                            return Ok(Some(parked));
                        }
                    }
                    Ok(None)
                }
                EnumVariant::Struct { fields, .. } => {
                    for f in fields.iter() {
                        if let Some(parked) = walk(&f.ty, state, store)? {
                            return Ok(Some(parked));
                        }
                    }
                    Ok(None)
                }
            }
        }
        SchemaType::Map { key, value } => {
            // Wire is `varint(len) + (k, v)` pairs. Same descent
            // pattern as `Vec<(K, V)>` — walk every key and every
            // value; bail out on the first `Ref::Handle` that doesn't
            // resolve. Keys can't carry `Ref`s under the v1 codec
            // rules, but the walker treats them uniformly so a
            // hand-rolled `Schema` impl that lands a `Ref` key here
            // doesn't silently corrupt the wire.
            let len = state.read_varint()? as usize;
            for _ in 0..len {
                if let Some(parked) = walk(key, state, store)? {
                    return Ok(Some(parked));
                }
                if let Some(parked) = walk(value, state, store)? {
                    return Ok(Some(parked));
                }
            }
            Ok(None)
        }
        SchemaType::Ref(inner) => {
            let ref_disc_start = state.pos;
            let disc = state.read_varint()? as u32;
            match disc {
                0 => {
                    // ADR-0100: the inline body is an opaque
                    // length-prefixed blob (`varint(len)` +
                    // `K::encode_into_bytes`) — the kind's own codec
                    // image, cast or postcard. Skip it by length rather
                    // than re-deriving `K`'s byte layout from `inner`;
                    // a cast image is not a schema-walkable postcard
                    // structure.
                    let len = state.read_varint()? as usize;
                    state.skip_n(len)?;
                    Ok(None)
                }
                1 => {
                    let id = HandleId(state.read_varint()?);
                    let kind = KindId(state.read_varint()?);
                    let after_handle = state.pos;
                    let Some((stored_kind, bytes)) = store.get(id) else {
                        return Ok(Some((id, kind)));
                    };
                    if stored_kind != kind {
                        // Diagnostic, not fatal: the wire stamp is exact
                        // for transform/`Call`-fed slots but best-effort
                        // for source-fed ones (the slot cell carries
                        // `K`'s schema, not its name — issue
                        // iamacoffeepot/aether#1047), so under
                        // structurally identical registered kinds the
                        // stamp can name a sibling of the stored kind.
                        // Resolution keys on the handle id and the
                        // consumer's field schema, so the splice below
                        // is sound either way. Rebinding — the other way
                        // the ids could disagree — is hard-errored by
                        // `put()` (`PutError::KindMismatch`) before this
                        // point.
                        tracing::warn!(
                            handle = id.0,
                            wire_kind = kind.0,
                            stored_kind = stored_kind.0,
                            "wire Ref::Handle kind stamp disagrees with stored kind; \
                             resolving by handle id (structural-collision diagnostic, \
                             issue 1047)"
                        );
                    }
                    // Recursively resolve nested refs inside the
                    // stored bytes. If any nested handle is missing,
                    // bubble up so the *outer* mail parks on that id.
                    let resolved_inner = walk_and_resolve(inner, &bytes, store)?;
                    match resolved_inner {
                        WalkOutcome::Parked { handle, kind } => Ok(Some((handle, kind))),
                        WalkOutcome::Resolved { payload } => {
                            // Splice: flush prefix, write the Inline arm
                            // (disc 0 + varint(len) + payload, ADR-0100),
                            // skip past the Handle wire bytes. The
                            // payload is the resolved inline blob — the
                            // byte image is identical whether it arrived
                            // inline or was spliced from a handle here.
                            state.flush_up_to(ref_disc_start);
                            state.out.push(0u8);
                            push_varint(&mut state.out, payload.len() as u64);
                            state.out.extend_from_slice(&payload);
                            state.prefix_end = after_handle;
                            Ok(None)
                        }
                    }
                }
                _ => Err(WalkError::UnknownRefDiscriminant),
            }
        }
        // ADR-0065: typed-id wire is a u64 varint regardless of
        // which `TYPE_ID` is set. Skip the varint; nothing to
        // resolve since typed-ids don't embed `Ref`s.
        SchemaType::TypeId(_) => {
            state.skip_varint()?;
            Ok(None)
        }
    }
}

/// Append `value` as an unsigned LEB128 varint — the same encoding
/// postcard uses for the inline arm's `varint(len)` length prefix
/// (ADR-0100). Mirror of [`State::read_varint`].
fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn skip_primitive_postcard(state: &mut State<'_>, p: Primitive) -> Result<(), WalkError> {
    match p {
        Primitive::U8 | Primitive::I8 => state.skip_n(1),
        // Multi-byte integers ride varints (with zigzag for signed).
        // The zigzag transform doesn't change byte length, so skipping
        // a varint covers both.
        Primitive::U16
        | Primitive::U32
        | Primitive::U64
        | Primitive::I16
        | Primitive::I32
        | Primitive::I64 => state.skip_varint(),
        Primitive::F32 => state.skip_n(4),
        Primitive::F64 => state.skip_n(8),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
mod tests {
    use Arc;

    use crate::mail::{Mail, MailboxId};
    use aether_data::{Kind, Ref};
    use aether_data::{NamedField, SchemaCell};

    use super::*;
    use aether_data::tagged_id;

    // ADR-0090: the confique migration is byte-identical to the prior
    // hand-rolled `parse_env_u64` reader. These exercise resolution
    // without touching process env (issue 464) — the parsers are pure,
    // and the defaults check loads the layer with no `.env()` source.

    #[test]
    fn parse_persist_numbers_soft_fall_back_to_defaults() {
        assert_eq!(parse_disk_budget_bytes("1024").unwrap(), 1024);
        assert_eq!(
            parse_disk_budget_bytes("nope").unwrap(),
            DEFAULT_DISK_BUDGET_BYTES
        );
        assert_eq!(parse_eviction_tick_secs("30").unwrap(), 30);
        assert_eq!(
            parse_eviction_tick_secs("nope").unwrap(),
            DEFAULT_DISK_EVICTION_TICK_SECS
        );
    }

    #[test]
    fn persist_config_layer_defaults_match() {
        use confique::Config as _;
        // No `.env()` source: literal defaults only, env-free. Guards the
        // layer defaults against the named consts.
        let layer = PersistConfigLayer::builder().load().expect("defaults load");
        assert_eq!(layer.disk_budget_bytes, DEFAULT_DISK_BUDGET_BYTES);
        assert_eq!(layer.eviction_tick_secs, DEFAULT_DISK_EVICTION_TICK_SECS);
    }

    // HandleStore unit tests

    #[test]
    fn put_then_get_round_trips_bytes_and_kind() {
        let store = HandleStore::new(1024);
        store
            .put(HandleId(7), KindId(100), b"hello".to_vec())
            .unwrap();
        let (kind, bytes) = store.get(HandleId(7)).expect("entry present");
        assert_eq!(kind, KindId(100));
        assert_eq!(&bytes, b"hello");
    }

    #[test]
    fn put_replacing_same_id_with_matching_kind_overwrites_bytes() {
        let store = HandleStore::new(1024);
        store
            .put(HandleId(1), KindId(100), b"old".to_vec())
            .unwrap();
        store
            .put(HandleId(1), KindId(100), b"newer".to_vec())
            .unwrap();
        let (_, bytes) = store.get(HandleId(1)).unwrap();
        assert_eq!(&bytes, b"newer");
        assert_eq!(store.entry_count(), 1);
        assert_eq!(store.total_bytes(), 5);
    }

    #[test]
    fn put_preserves_pinned_and_refcount_across_same_kind_reput() {
        // A re-put with matching kind shouldn't silently unpin or
        // zero a refcount that other code depends on. (Phase 1 has
        // no host-fns yet, but pin the contract before they land.)
        let store = HandleStore::new(1024);
        store
            .put(HandleId(1), KindId(100), b"old".to_vec())
            .unwrap();
        store.pin(HandleId(1));
        store.inc_ref(HandleId(1));
        store
            .put(HandleId(1), KindId(100), b"newer".to_vec())
            .unwrap();
        // Stays pinned (proof: an attempt to evict it under pressure
        // fails).
        store.put(HandleId(2), KindId(100), b"AA".to_vec()).unwrap();
        store.put(HandleId(3), KindId(100), b"BB".to_vec()).unwrap();
        assert!(store.contains(HandleId(1)));
    }

    #[test]
    fn put_with_mismatched_kind_id_errors() {
        let store = HandleStore::new(1024);
        store.put(HandleId(1), KindId(100), vec![1, 2, 3]).unwrap();
        let err = store.put(HandleId(1), KindId(200), vec![4]).unwrap_err();
        assert!(matches!(err, PutError::KindMismatch { .. }));
        // Original entry untouched.
        let (kind, bytes) = store.get(HandleId(1)).unwrap();
        assert_eq!(kind, KindId(100));
        assert_eq!(bytes, vec![1, 2, 3]);
    }

    #[test]
    fn next_ephemeral_starts_at_one_and_increments() {
        let store = HandleStore::new(1024);
        let a = store.next_ephemeral();
        let b = store.next_ephemeral();
        // ADR-0064: counter occupies the low 60 bits; the high 4
        // bits carry `Tag::Handle`. Strip the tag to assert on the
        // raw counter value.
        assert_eq!(tagged_id::body_of(a.0), 1);
        assert_eq!(tagged_id::body_of(b.0), 2);
        assert_eq!(tagged_id::tag_of(a.0), Some(aether_data::Tag::Handle));
        assert_ne!(a, HandleId(0));
    }

    #[test]
    fn lru_evicts_oldest_unpinned_unrefcounted_entry() {
        // Two entries that just fit, then add a third that forces
        // eviction. The least-recently-accessed must go first.
        let store = HandleStore::new(8);
        store.put(HandleId(1), KindId(0), b"AAAA".to_vec()).unwrap();
        store.put(HandleId(2), KindId(0), b"BBBB".to_vec()).unwrap();
        // Touch entry 1 so entry 2 is now the LRU.
        let _ = store.get(HandleId(1));
        // Insert entry 3 — should evict entry 2.
        store.put(HandleId(3), KindId(0), b"CCCC".to_vec()).unwrap();
        assert!(store.contains(HandleId(1)), "MRU survived");
        assert!(!store.contains(HandleId(2)), "LRU evicted");
        assert!(store.contains(HandleId(3)));
    }

    #[test]
    fn pinned_entry_skips_eviction() {
        let store = HandleStore::new(8);
        store.put(HandleId(1), KindId(0), b"AAAA".to_vec()).unwrap();
        store.put(HandleId(2), KindId(0), b"BBBB".to_vec()).unwrap();
        store.pin(HandleId(1));
        // Entry 1 is pinned, so entry 2 (the only evictable one)
        // must be dropped — even though it's MRU.
        store.put(HandleId(3), KindId(0), b"CCCC".to_vec()).unwrap();
        assert!(store.contains(HandleId(1)), "pinned entry stays");
        assert!(!store.contains(HandleId(2)));
        assert!(store.contains(HandleId(3)));
    }

    #[test]
    fn refcounted_entry_skips_eviction() {
        let store = HandleStore::new(8);
        store.put(HandleId(1), KindId(0), b"AAAA".to_vec()).unwrap();
        store.put(HandleId(2), KindId(0), b"BBBB".to_vec()).unwrap();
        store.inc_ref(HandleId(1));
        store.put(HandleId(3), KindId(0), b"CCCC".to_vec()).unwrap();
        assert!(store.contains(HandleId(1)), "refcounted entry stays");
        assert!(!store.contains(HandleId(2)));
    }

    #[test]
    fn put_fails_if_no_eligible_eviction_targets() {
        let store = HandleStore::new(8);
        store.put(HandleId(1), KindId(0), b"AAAA".to_vec()).unwrap();
        store.put(HandleId(2), KindId(0), b"BBBB".to_vec()).unwrap();
        store.pin(HandleId(1));
        store.pin(HandleId(2));
        let err = store
            .put(HandleId(3), KindId(0), b"CCCC".to_vec())
            .unwrap_err();
        assert!(matches!(err, PutError::EvictionFailed { .. }));
    }

    #[test]
    fn dec_ref_below_zero_saturates() {
        let store = HandleStore::new(64);
        store.put(HandleId(1), KindId(0), b"x".to_vec()).unwrap();
        // Calling dec_ref past zero saturates rather than underflowing.
        assert!(store.dec_ref(HandleId(1)));
        assert!(store.dec_ref(HandleId(1)));
        assert!(store.contains(HandleId(1)));
    }

    #[test]
    fn park_and_take_round_trip() {
        let store = HandleStore::new(64);
        let mail1 = Mail::new(MailboxId(0xAA), KindId(1), vec![1], 0);
        let mail2 = Mail::new(MailboxId(0xBB), KindId(2), vec![2], 0);
        store.park(HandleId(42), mail1);
        store.park(HandleId(42), mail2);
        assert_eq!(store.parked_count(HandleId(42)), 2);
        let drained = store.take_parked(HandleId(42));
        assert_eq!(drained.len(), 2);
        // FIFO: first-parked first-out.
        assert_eq!(drained[0].kind, KindId(1));
        assert_eq!(drained[1].kind, KindId(2));
        assert_eq!(store.parked_count(HandleId(42)), 0);
    }

    #[test]
    fn drain_all_parked_returns_all_queues_and_clears() {
        let store = HandleStore::new(64);
        // Park two mails under one handle and one under another.
        let m1 = Mail::new(MailboxId(0x01), KindId(10), vec![10], 0);
        let m2 = Mail::new(MailboxId(0x02), KindId(11), vec![11], 0);
        let m3 = Mail::new(MailboxId(0x03), KindId(12), vec![12], 0);
        store.park(HandleId(1), m1);
        store.park(HandleId(1), m2);
        store.park(HandleId(2), m3);
        assert_eq!(store.parked_count(HandleId(1)), 2);
        assert_eq!(store.parked_count(HandleId(2)), 1);

        let drained = store.drain_all_parked();
        assert_eq!(drained.len(), 3, "all three mails must be returned");
        // Every queue is cleared after the drain.
        assert_eq!(store.parked_count(HandleId(1)), 0);
        assert_eq!(store.parked_count(HandleId(2)), 0);
    }

    #[test]
    fn arc_shared_writes_are_visible() {
        let a = Arc::new(HandleStore::new(64));
        let b = Arc::clone(&a);
        a.put(HandleId(1), KindId(0), vec![1, 2, 3]).unwrap();
        let (_, bytes) = b.get(HandleId(1)).unwrap();
        assert_eq!(bytes, vec![1, 2, 3]);
    }

    // Walker tests — schema-driven over real Ref<K> wire

    /// Tiny postcard kind for walker tests. Kept here rather than
    /// pulling the derive macro into substrate-core's dev-deps:
    /// the derive expansion is exercised end-to-end in
    /// aether-actor-derive's tests; here we just need a payload that
    /// matches the schema we hand the walker.
    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug, Clone)]
    struct Note {
        body: String,
        seq: u32,
    }

    impl Kind for Note {
        const NAME: &'static str = "test.note";
        // Stable test sentinel — distinct from real schema-hashed kind ids.
        const ID: KindId = KindId(0xDEAD_BEEF_0002_0001);

        fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
            postcard::from_bytes(bytes).ok()
        }

        fn encode_into_bytes(&self) -> Vec<u8> {
            postcard::to_allocvec(self).expect("postcard encode to Vec is infallible")
        }
    }

    /// Postcard-shape `Struct { repr_c: false, fields }` builder
    /// shared by the test schemas in this module.
    fn postcard_struct(fields: Vec<NamedField>) -> SchemaType {
        SchemaType::Struct {
            fields: Cow::Owned(fields),
            repr_c: false,
        }
    }

    /// One-line `NamedField { name: Cow::Borrowed(name), ty }` builder.
    /// Cuts the per-field boilerplate that otherwise repeats for every
    /// field across every test schema in this module.
    fn named(name: &'static str, ty: SchemaType) -> NamedField {
        NamedField {
            name: Cow::Borrowed(name),
            ty,
        }
    }

    /// The `seq: u32` trailing field every test type in this module
    /// (`Note`, `HeldNote`, …) carries to disambiguate stored entries.
    /// Hoisted because both `note_schema` and `held_note_schema` end
    /// with this exact field — the same `seq: u32` Rust field both
    /// test structs declare.
    fn seq_field() -> NamedField {
        named("seq", SchemaType::Scalar(Primitive::U32))
    }

    fn note_schema() -> SchemaType {
        postcard_struct(vec![named("body", SchemaType::String), seq_field()])
    }

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug, Clone)]
    struct HeldNote {
        held: Ref<Note>,
        seq: u32,
    }

    impl Kind for HeldNote {
        const NAME: &'static str = "test.held_note";
        const ID: KindId = KindId(0xDEAD_BEEF_0002_0002);

        fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
            postcard::from_bytes(bytes).ok()
        }

        fn encode_into_bytes(&self) -> Vec<u8> {
            postcard::to_allocvec(self).expect("postcard encode to Vec is infallible")
        }
    }

    fn held_note_schema() -> SchemaType {
        postcard_struct(vec![
            named("held", SchemaType::Ref(SchemaCell::owned(note_schema()))),
            seq_field(),
        ])
    }

    /// Cast-shaped walker fixture with non-`f32` fields (`u16`). ADR-0100
    /// makes its inline body a raw cast image, whose `u16` bytes a
    /// postcard reader would misread — the walker must treat the inline
    /// body as an opaque length-prefixed blob, not walk it as postcard.
    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
    struct Coord {
        x: u16,
        y: u16,
    }

    impl Kind for Coord {
        const NAME: &'static str = "test.coord";
        const ID: KindId = KindId(0xDEAD_BEEF_0002_0003);

        fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
            (bytes.len() == size_of::<Self>()).then(|| bytemuck::pod_read_unaligned(bytes))
        }

        fn encode_into_bytes(&self) -> Vec<u8> {
            bytemuck::bytes_of(self).to_vec()
        }
    }

    fn coord_schema() -> SchemaType {
        SchemaType::Struct {
            fields: Cow::Owned(vec![
                named("x", SchemaType::Scalar(Primitive::U16)),
                named("y", SchemaType::Scalar(Primitive::U16)),
            ]),
            repr_c: true,
        }
    }

    #[test]
    fn schema_contains_ref_detects_top_level_ref() {
        assert!(schema_contains_ref(&SchemaType::Ref(SchemaCell::owned(
            SchemaType::Unit
        ))));
    }

    #[test]
    fn schema_contains_ref_detects_nested_ref_in_struct() {
        assert!(schema_contains_ref(&held_note_schema()));
    }

    #[test]
    fn schema_contains_ref_returns_false_for_pure_postcard_struct() {
        assert!(!schema_contains_ref(&note_schema()));
    }

    #[test]
    fn walk_no_refs_returns_borrowed() {
        let store = HandleStore::new(1024);
        let note = Note {
            body: "hi".to_string(),
            seq: 7,
        };
        let bytes = postcard::to_allocvec(&note).unwrap();
        let outcome = walk_and_resolve(&note_schema(), &bytes, &store).unwrap();
        match outcome {
            WalkOutcome::Resolved {
                payload: Cow::Borrowed(b),
            } => {
                assert_eq!(b.as_ptr(), bytes.as_ptr());
            }
            WalkOutcome::Resolved {
                payload: Cow::Owned(_),
            } => panic!("expected borrowed payload, got owned"),
            WalkOutcome::Parked { .. } => panic!("expected resolved"),
        }
    }

    #[test]
    fn walk_inline_ref_passes_through_borrowed() {
        let store = HandleStore::new(1024);
        let inner = Note {
            body: "inline".to_string(),
            seq: 9,
        };
        let outer = HeldNote {
            held: Ref::Inline(inner),
            seq: 11,
        };
        let bytes = postcard::to_allocvec(&outer).unwrap();
        let outcome = walk_and_resolve(&held_note_schema(), &bytes, &store).unwrap();
        // Inline refs cause no substitution; payload should still be
        // Cow::Borrowed.
        match outcome {
            WalkOutcome::Resolved {
                payload: Cow::Borrowed(_),
            } => {}
            WalkOutcome::Resolved {
                payload: Cow::Owned(_),
            } => panic!("inline refs shouldn't trigger substitution"),
            WalkOutcome::Parked { .. } => panic!("expected resolved"),
        }
    }

    #[test]
    fn walk_handle_ref_misses_and_parks() {
        let store = HandleStore::new(1024);
        let outer = HeldNote {
            held: Ref::handle(0xCAFE),
            seq: 11,
        };
        let bytes = postcard::to_allocvec(&outer).unwrap();
        let outcome = walk_and_resolve(&held_note_schema(), &bytes, &store).unwrap();
        match outcome {
            WalkOutcome::Parked { handle, kind } => {
                assert_eq!(handle, HandleId(0xCAFE));
                assert_eq!(kind, Note::ID);
            }
            WalkOutcome::Resolved { .. } => panic!("expected park on missing handle"),
        }
    }

    #[test]
    fn walk_handle_ref_resolves_and_substitutes() {
        let store = HandleStore::new(1024);
        let inner = Note {
            body: "stored".to_string(),
            seq: 99,
        };
        let inner_bytes = postcard::to_allocvec(&inner).unwrap();
        store.put(HandleId(0xCAFE), Note::ID, inner_bytes).unwrap();

        let outer = HeldNote {
            held: Ref::handle(0xCAFE),
            seq: 11,
        };
        let outer_bytes = postcard::to_allocvec(&outer).unwrap();

        let outcome = walk_and_resolve(&held_note_schema(), &outer_bytes, &store).unwrap();
        let resolved_bytes = match outcome {
            WalkOutcome::Resolved { payload } => payload.into_owned(),
            WalkOutcome::Parked { .. } => panic!("expected resolved"),
        };

        // The resolved payload should decode as HeldNote with
        // `held = Ref::Inline(inner)`.
        let decoded: HeldNote = postcard::from_bytes(&resolved_bytes).unwrap();
        assert_eq!(decoded.seq, 11);
        match decoded.held {
            Ref::Inline(got) => {
                assert_eq!(got.body, "stored");
                assert_eq!(got.seq, 99);
            }
            Ref::Handle { .. } => panic!("walker must replace Handle with Inline"),
        }
    }

    #[test]
    fn walk_two_handle_refs_substitutes_both() {
        // Vec<Ref<Note>> with two handles and one inline.
        let schema = SchemaType::Vec(SchemaCell::owned(SchemaType::Ref(SchemaCell::owned(
            note_schema(),
        ))));

        let store = HandleStore::new(4096);
        let stored_a = Note {
            body: "a".to_string(),
            seq: 1,
        };
        let stored_b = Note {
            body: "b".to_string(),
            seq: 2,
        };
        store
            .put(
                HandleId(1),
                Note::ID,
                postcard::to_allocvec(&stored_a).unwrap(),
            )
            .unwrap();
        store
            .put(
                HandleId(2),
                Note::ID,
                postcard::to_allocvec(&stored_b).unwrap(),
            )
            .unwrap();

        let outer: Vec<Ref<Note>> = vec![
            Ref::handle(1),
            Ref::Inline(Note {
                body: "mid".to_string(),
                seq: 5,
            }),
            Ref::handle(2),
        ];
        let bytes = postcard::to_allocvec(&outer).unwrap();
        let outcome = walk_and_resolve(&schema, &bytes, &store).unwrap();
        let resolved = match outcome {
            WalkOutcome::Resolved { payload } => payload.into_owned(),
            WalkOutcome::Parked { .. } => panic!("expected resolved"),
        };

        let decoded: Vec<Ref<Note>> = postcard::from_bytes(&resolved).unwrap();
        assert_eq!(decoded.len(), 3);
        for r in &decoded {
            assert!(r.is_inline(), "every ref should be inline after walk");
        }
    }

    #[test]
    fn walk_partial_resolve_parks_on_first_missing() {
        let schema = SchemaType::Vec(SchemaCell::owned(SchemaType::Ref(SchemaCell::owned(
            note_schema(),
        ))));
        let store = HandleStore::new(4096);
        // Only handle 1 is present.
        let stored = Note {
            body: "ok".to_string(),
            seq: 1,
        };
        store
            .put(
                HandleId(1),
                Note::ID,
                postcard::to_allocvec(&stored).unwrap(),
            )
            .unwrap();

        let outer: Vec<Ref<Note>> = vec![Ref::handle(1), Ref::handle(99)];
        let bytes = postcard::to_allocvec(&outer).unwrap();
        let outcome = walk_and_resolve(&schema, &bytes, &store).unwrap();
        match outcome {
            WalkOutcome::Parked { handle, .. } => {
                assert_eq!(handle, HandleId(99), "should park on first missing handle");
            }
            WalkOutcome::Resolved { .. } => panic!("expected park"),
        }
    }

    #[test]
    fn walk_truncated_payload_errors() {
        let store = HandleStore::new(64);
        // Truncate a HeldNote payload mid-string-length.
        let outer = HeldNote {
            held: Ref::Inline(Note {
                body: "x".to_string(),
                seq: 1,
            }),
            seq: 1,
        };
        let mut bytes = postcard::to_allocvec(&outer).unwrap();
        bytes.truncate(2);
        let err = walk_and_resolve(&held_note_schema(), &bytes, &store).unwrap_err();
        assert!(matches!(err, WalkError::Truncated));
    }

    /// Locks down the `Cow::Borrowed` fast path: a kind with no Refs in
    /// its schema must never allocate. Pin the outcome shape so a
    /// regression that always builds an Owned vec is loud.
    #[test]
    fn walk_fast_path_avoids_allocation_for_ref_free_schema() {
        let store = HandleStore::new(64);
        let bytes = postcard::to_allocvec(&Note {
            body: "x".to_string(),
            seq: 1,
        })
        .unwrap();
        let outcome = walk_and_resolve(&note_schema(), &bytes, &store).unwrap();
        match outcome {
            WalkOutcome::Resolved {
                payload: Cow::Borrowed(_),
            } => {}
            _ => panic!("ref-free kind must take the borrow path"),
        }
    }

    /// A `Ref<K>` whose stored bytes themselves contain another
    /// `Ref` should resolve recursively. Today we exercise the
    /// shallow case (stored bytes are pure-Inline `K`) since nested
    /// `Ref` wires are unusual; a deeper test belongs with PR 3 once
    /// there's a guest-side publish path that mints them.
    #[test]
    fn walk_nested_resolve_substitutes_handle_inside_handle() {
        // Outer = Ref<HeldNote>; HeldNote.held = Ref<Note>.
        // Outer wire is Handle(X), where X stores the bytes of a
        // HeldNote whose held field is Handle(Y), where Y stores the
        // inline Note bytes.
        let outer_schema = SchemaType::Ref(SchemaCell::owned(held_note_schema()));
        let store = HandleStore::new(4096);

        // Inner Note bytes go in store under handle Y.
        let inner_note = Note {
            body: "deep".to_string(),
            seq: 7,
        };
        let inner_bytes = postcard::to_allocvec(&inner_note).unwrap();
        store.put(HandleId(20), Note::ID, inner_bytes).unwrap();

        // Mid-level HeldNote, with held = Handle(Y), goes under X.
        let mid = HeldNote {
            held: Ref::handle(20),
            seq: 5,
        };
        let mid_bytes = postcard::to_allocvec(&mid).unwrap();
        // Use a synthetic kind id for HeldNote — the walker only uses
        // the kind id to validate against the wire, and the test
        // schemas don't go through registry registration.
        store.put(HandleId(10), KindId(0xBEEF), mid_bytes).unwrap();

        // Top-level wire: Ref<HeldNote>::Handle { id: 10, kind_id: 0xBEEF }.
        let top: Ref<HeldNote> = Ref::Handle {
            id: 10,
            kind_id: 0xBEEF,
        };
        let bytes = postcard::to_allocvec(&top).unwrap();
        let outcome = walk_and_resolve(&outer_schema, &bytes, &store).unwrap();
        let resolved = match outcome {
            WalkOutcome::Resolved { payload } => payload.into_owned(),
            WalkOutcome::Parked { .. } => panic!("expected resolved"),
        };
        let decoded: Ref<HeldNote> = postcard::from_bytes(&resolved).unwrap();
        match decoded {
            Ref::Inline(held) => match held.held {
                Ref::Inline(note) => {
                    assert_eq!(note.body, "deep");
                    assert_eq!(note.seq, 7);
                }
                Ref::Handle { .. } => panic!("nested ref must also be resolved"),
            },
            Ref::Handle { .. } => panic!("outer ref must be resolved"),
        }
    }

    /// ADR-0100: a handle to a non-`f32` cast kind resolves to an inline
    /// arm whose body is the raw cast image, and the spliced wire decodes
    /// on the guest path (`Ref<Coord>::decode → Kind::decode_from_bytes`)
    /// uncorrupted — the `u16` fields survive because the walker never
    /// re-interprets the cast image as postcard.
    #[test]
    fn walk_handle_ref_resolves_cast_kind_non_f32() {
        let schema = SchemaType::Ref(SchemaCell::owned(coord_schema()));
        let store = HandleStore::new(1024);

        let pt = Coord {
            x: 0x0102,
            y: 0xF0E1,
        };
        let cast_bytes = bytemuck::bytes_of(&pt).to_vec();
        store.put(HandleId(3), Coord::ID, cast_bytes).unwrap();

        let wire = postcard::to_allocvec(&Ref::<Coord>::handle(3)).unwrap();
        let outcome = walk_and_resolve(&schema, &wire, &store).unwrap();
        let resolved = match outcome {
            WalkOutcome::Resolved { payload } => payload.into_owned(),
            WalkOutcome::Parked { .. } => panic!("expected resolved"),
        };

        // The spliced inline arm carries the 4-byte cast image,
        // length-prefixed: disc 0 + varint(4) + 4 raw bytes.
        assert_eq!(resolved[0], 0, "spliced Inline discriminant");
        assert_eq!(resolved[1], 4, "varint length of the 4-byte cast image");
        assert_eq!(&resolved[2..], bytemuck::bytes_of(&pt));

        let back: Ref<Coord> = postcard::from_bytes(&resolved).unwrap();
        match back {
            Ref::Inline(got) => assert_eq!(got, pt, "u16 cast fields survive resolution"),
            Ref::Handle { .. } => panic!("walker must replace Handle with Inline"),
        }
    }

    /// Contention-sensitive `get()` concurrency checks (issue #1447).
    /// In `mod heavy` per the repo's heavy-test convention so nextest
    /// serializes them (the `::heavy::` path → `serial-heavy` group).
    /// These are an interim
    /// stand-in: #1439's handle-store stress harness supersedes them
    /// with measured before/after concurrent-`get()` throughput.
    #[allow(clippy::disallowed_methods)] // test scaffolding — threads here hold no settlement contract
    mod heavy {
        use super::*;
        use std::sync::Barrier;
        use std::sync::mpsc;

        /// Many threads hammering cache-hit `get()` on one shared
        /// `Arc<HandleStore>` must all observe the correct kind +
        /// bytes with no deadlock or panic. The cache-hit path now
        /// takes only the read lock, so these run concurrently rather
        /// than serializing through a write lock.
        #[test]
        fn concurrent_cache_hit_gets_return_correct_bytes() {
            const THREADS: usize = 16;
            const GETS_PER_THREAD: usize = 2_000;

            let store = Arc::new(HandleStore::new(4096));
            store
                .put(HandleId(1), KindId(100), b"payload".to_vec())
                .unwrap();

            // Release all workers at once to maximize overlap on the
            // shared read lock.
            let barrier = Arc::new(Barrier::new(THREADS));
            let workers: Vec<_> = (0..THREADS)
                .map(|_| {
                    let store = Arc::clone(&store);
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        barrier.wait();
                        for _ in 0..GETS_PER_THREAD {
                            let (kind, bytes) = store.get(HandleId(1)).expect("entry present");
                            assert_eq!(kind, KindId(100));
                            assert_eq!(&bytes, b"payload");
                        }
                    })
                })
                .collect();

            for w in workers {
                w.join().expect("worker thread panicked");
            }
        }

        /// A cache-hit `get()` must complete while another thread holds
        /// the store's read lock — direct proof it takes only the read
        /// lock (issue #1447). A write-locked `get` would block on the
        /// outstanding read guard and trip the timeout.
        #[test]
        fn cache_hit_get_proceeds_while_read_lock_held() {
            let store = Arc::new(HandleStore::new(1024));
            store
                .put(HandleId(1), KindId(100), b"payload".to_vec())
                .unwrap();

            // Hold the store's read lock for the duration of the
            // spawned get. Two concurrent readers are fine; a writer
            // would block here.
            let guard = store.inner.read().expect("handle store lock poisoned");

            let (tx, rx) = mpsc::channel();
            let worker_store = Arc::clone(&store);
            let worker = thread::spawn(move || {
                let got = worker_store.get(HandleId(1));
                tx.send(got).expect("send get result");
            });

            let got = rx.recv_timeout(Duration::from_secs(10)).expect(
                "cache-hit get serialized behind the held read lock — write lock on the hot path?",
            );
            drop(guard);
            worker.join().expect("worker thread panicked");

            let (kind, bytes) = got.expect("entry present");
            assert_eq!(kind, KindId(100));
            assert_eq!(&bytes, b"payload");
        }
    }

    // index.bin boot fast-path tests (issue #1446 / ADR-0049 §3).
    use std::sync::atomic::Ordering as AtomicOrdering;

    static FAST_PATH_NONCE: AtomicU64 = AtomicU64::new(0);

    fn fast_path_scratch(tag: &str) -> PathBuf {
        let pid = process::id();
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
        let n = FAST_PATH_NONCE.fetch_add(1, AtomicOrdering::Relaxed);
        let path = env::temp_dir().join(format!("aether-index-fastpath-{tag}-{pid}-{millis}-{n}"));
        fs::create_dir_all(&path).expect("scratch dir creates");
        path
    }

    fn fast_path_cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    fn fast_path_cfg(root: &Path) -> PersistConfig {
        PersistConfig {
            root: root.join("v1"),
            disk_budget_bytes: u64::MAX,
            eviction_tick_secs: 60,
        }
    }

    /// Synthetic bidirectional kind registry for the deferred
    /// schema-evolution check, mirroring the integration-test fixture.
    struct FakeKindRegistry {
        by_name: HashMap<String, KindId>,
        by_id: HashMap<KindId, String>,
    }

    impl FakeKindRegistry {
        fn new(entries: &[(&str, u64)]) -> Self {
            let mut by_name = HashMap::new();
            let mut by_id = HashMap::new();
            for (name, id) in entries {
                by_name.insert((*name).to_owned(), KindId(*id));
                by_id.insert(KindId(*id), (*name).to_owned());
            }
            Self { by_name, by_id }
        }
    }

    impl KindResolver for FakeKindRegistry {
        fn id_for_name(&self, name: &str) -> Option<KindId> {
            self.by_name.get(name).copied()
        }
        fn name_for_id(&self, id: KindId) -> Option<String> {
            self.by_id.get(&id).cloned()
        }
    }

    /// Step 3: a snapshot of a populated store writes a decodable
    /// `index.bin` whose entry set matches the live disk index.
    #[test]
    fn snapshot_index_writes_decodable_index_bin() {
        let root = fast_path_scratch("snapshot");
        let cfg = fast_path_cfg(&root);
        let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
        store
            .put_persistent(HandleId(1), KindId(7), vec![0u8; 16], None)
            .unwrap();
        store
            .put_persistent(HandleId(2), KindId(7), vec![0u8; 32], None)
            .unwrap();
        store.pin(HandleId(2));
        store.snapshot_index();

        let path = cfg.index_path();
        assert!(path.exists(), "index.bin written");
        let raw = fs::read(&path).unwrap();
        let snapshot: IndexSnapshot = postcard::from_bytes(&raw).unwrap();
        assert_eq!(snapshot.schema_version, INDEX_FORMAT_VERSION);
        assert_eq!(snapshot.entries.len(), 2);
        let e1 = snapshot.entries.get(&1).expect("id 1 in snapshot");
        assert_eq!(e1.kind_id, 7);
        assert_eq!(e1.bytes_len, 16);
        assert!(!e1.pinned);
        let e2 = snapshot.entries.get(&2).expect("id 2 in snapshot");
        assert_eq!(e2.bytes_len, 32);
        assert!(e2.pinned, "pinned flag carried into the snapshot");
        fast_path_cleanup(&root);
    }

    /// Step 4a: a fresh store loads the snapshot via the fast path —
    /// disk index populated, `index.bin` deleted post-load.
    #[test]
    fn fast_path_loads_index_bin_and_deletes_it() {
        let root = fast_path_scratch("load");
        let cfg = fast_path_cfg(&root);
        {
            let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
            for i in 0..50u64 {
                store
                    .put_persistent(HandleId(i + 1), KindId(7), vec![0u8; 8], None)
                    .unwrap();
            }
            store.snapshot_index();
        }
        assert!(cfg.index_path().exists(), "snapshot present before reboot");
        let restored = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
        assert_eq!(restored.disk_index_len(), 50);
        assert!(
            !cfg.index_path().exists(),
            "index.bin deleted after a successful fast-path load",
        );
        fast_path_cleanup(&root);
    }

    /// Step 4b: a corrupt (truncated) `index.bin` falls back to the
    /// directory scan, which still indexes from the `.meta` sidecars.
    #[test]
    fn fast_path_corrupt_index_falls_back_to_scan() {
        let root = fast_path_scratch("corrupt");
        let cfg = fast_path_cfg(&root);
        {
            let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
            for i in 0..10u64 {
                store
                    .put_persistent(HandleId(i + 1), KindId(7), vec![0u8; 8], None)
                    .unwrap();
            }
            store.snapshot_index();
        }
        // Truncate the snapshot to just its version byte — the entry map
        // is gone, so the decode fails.
        let path = cfg.index_path();
        let raw = fs::read(&path).unwrap();
        fs::write(&path, &raw[..1]).unwrap();

        let restored = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg));
        assert_eq!(
            restored.disk_index_len(),
            10,
            "scan fallback indexed from the .meta sidecars",
        );
        fast_path_cleanup(&root);
    }

    /// Step 4b: a version-skewed `index.bin` (decodes cleanly but the
    /// header doesn't match) falls back to the directory scan.
    #[test]
    fn fast_path_version_skew_falls_back_to_scan() {
        let root = fast_path_scratch("skew");
        let cfg = fast_path_cfg(&root);
        {
            let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
            for i in 0..10u64 {
                store
                    .put_persistent(HandleId(i + 1), KindId(7), vec![0u8; 8], None)
                    .unwrap();
            }
            store.snapshot_index();
        }
        // Flip the leading version byte; the rest of the postcard struct
        // stays valid, so it decodes but trips the version gate.
        let path = cfg.index_path();
        let mut raw = fs::read(&path).unwrap();
        raw[0] = INDEX_FORMAT_VERSION.wrapping_add(1);
        fs::write(&path, &raw).unwrap();

        let restored = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg));
        assert_eq!(
            restored.disk_index_len(),
            10,
            "scan fallback indexed despite the version skew",
        );
        fast_path_cleanup(&root);
    }

    /// Step 4c: an absent `index.bin` (no clean shutdown) falls through
    /// to the directory scan.
    #[test]
    fn fast_path_absent_index_uses_scan() {
        let root = fast_path_scratch("absent");
        let cfg = fast_path_cfg(&root);
        {
            let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
            for i in 0..10u64 {
                store
                    .put_persistent(HandleId(i + 1), KindId(7), vec![0u8; 8], None)
                    .unwrap();
            }
            // No snapshot_index() — emulates a crash before clean shutdown.
        }
        assert!(!cfg.index_path().exists(), "no snapshot was written");
        let restored = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg));
        assert_eq!(restored.disk_index_len(), 10);
        fast_path_cleanup(&root);
    }

    /// Step 5: the fast path skips the boot schema-evolution check
    /// (the entry survives the load), and the deferred check drops a
    /// schema-stale entry at cold materialization.
    #[test]
    fn fast_path_defers_schema_validation_to_materialization() {
        let root = fast_path_scratch("defer-validate");
        let cfg = fast_path_cfg(&root);
        let id = HandleId(0xABCD);
        {
            let resolver: Arc<dyn KindResolver> =
                Arc::new(FakeKindRegistry::new(&[("demo.kind", 0xAAAA)]));
            let store = HandleStore::with_persist_validated(
                64 * 1024 * 1024,
                Some(cfg.clone()),
                Some(resolver),
            );
            store
                .put_persistent(id, KindId(0xAAAA), b"bytes".to_vec(), None)
                .unwrap();
            store.snapshot_index();
        }
        // Reboot with the kind's id changed to 0xBBBB for the same name.
        let resolver: Arc<dyn KindResolver> =
            Arc::new(FakeKindRegistry::new(&[("demo.kind", 0xBBBB)]));
        let restored = HandleStore::with_persist_validated(
            64 * 1024 * 1024,
            Some(cfg.clone()),
            Some(resolver),
        );
        // Fast path loaded the entry WITHOUT validating it at boot.
        assert_eq!(
            restored.disk_index_len(),
            1,
            "fast path defers the schema check (entry present after load)",
        );
        // Cold materialization runs the deferred §6 check and drops it.
        assert!(restored.get(id).is_none(), "stale entry dropped on access");
        assert_eq!(restored.disk_index_len(), 0, "index entry removed");
        let (bin, meta) = entry_paths(&cfg.root, id);
        assert!(!bin.exists() && !meta.exists(), "both files deleted");
        fast_path_cleanup(&root);
    }

    /// Step 7: end-to-end — write N entries, snapshot, reboot on the
    /// same dir, and a subsequent `get` materializes from disk.
    #[test]
    fn end_to_end_snapshot_reboot_materializes() {
        let root = fast_path_scratch("e2e");
        let cfg = fast_path_cfg(&root);
        let mut expected: HashMap<HandleId, Vec<u8>> = HashMap::new();
        {
            let store = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
            for i in 0..32u64 {
                let id = HandleId(i + 1);
                let bytes = format!("payload-{i}").into_bytes();
                store
                    .put_persistent(id, KindId(7), bytes.clone(), None)
                    .unwrap();
                expected.insert(id, bytes);
            }
            store.snapshot_index();
        }
        let restored = HandleStore::with_persist(64 * 1024 * 1024, Some(cfg.clone()));
        assert_eq!(restored.disk_index_len(), expected.len());
        for (id, bytes) in &expected {
            assert!(
                !restored.contains_in_memory(*id),
                "not eagerly materialized"
            );
            let (kind, got) = restored.get(*id).expect("materializes from disk");
            assert_eq!(kind, KindId(7));
            assert_eq!(&got, bytes);
            assert!(restored.contains_in_memory(*id), "now in memory");
        }
        assert!(
            !cfg.index_path().exists(),
            "index.bin consumed by the fast-path load",
        );
        fast_path_cleanup(&root);
    }
}
