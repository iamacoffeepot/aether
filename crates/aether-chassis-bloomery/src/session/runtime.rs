//! The `SQLite`-backed runtime for [`SessionPoolCapability`] — the executor
//! session-reuse pool.
//!
//! A single [`rusqlite::Connection`] in WAL mode, owned by the capability's
//! dispatcher (single writer by construction — one actor, one connection),
//! mirroring the store capability's blocking boundary
//! (`docs/guide/capability-anatomy.md`): each handler runs one short, local
//! `SQLite` transaction inline against the pool table. These are bounded
//! `SELECT` / `UPSERT` against a local file — no network — so they run on the
//! actor's serialized dispatch, not through `dispatch_blocking`.
//!
//! The table holds one pooled session per `(model, effort, task)` key: metadata
//! and the lease only, never the transcript bytes (those are content-addressed
//! in `aether.artifacts`). Eligibility ports `scripts/agent-pool.mjs`'s
//! `evaluateEligibility` — key match, head-freshness (#3422), age bound (#3264),
//! context cap, and lazy lease expiry — and deliberately omits the
//! `workspace_tree_hash` gate #3341 measured and removed.

use std::time::{SystemTime, UNIX_EPOCH};

use super::SessionPoolCapability;
use super::kinds::{Acquire, AcquireResult, LeaseToken, Release, ReleaseResult, SessionKey, SessionManifest};
use aether_actor::runtime;
use rusqlite::{Connection, OptionalExtension};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

/// A leased pooled session: the transcript digest to resume plus the acquired
/// session's own `receipt` (the resumed attempt's `parent_receipt`), under an
/// exclusive [`LeaseToken`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasedSession {
    /// The exclusive lease the acquire marked.
    pub lease: LeaseToken,
    /// The session transcript's `aether.artifacts` digest, to resume.
    pub session_bytes: String,
    /// The acquired session's own receipt — the resumed attempt's parent.
    pub parent_receipt: String,
}

/// The durable pool the capability drives. One method per transact-mail kind;
/// each is one atomic `SQLite` transaction.
pub trait SessionBackend: Send {
    /// Lease the eligible pooled session for `key`, marking it leased until
    /// `now + lease_ttl`, or `None` when no eligible session exists.
    /// `current_head_hash` is the resuming box's static-prefix hash for the
    /// #3422 freshness gate; `now` is seconds since the Unix epoch.
    fn acquire(
        &mut self,
        key: &SessionKey,
        current_head_hash: &str,
        now: u64,
    ) -> rusqlite::Result<Option<LeasedSession>>;
    /// Deposit `session_bytes` + `manifest` for `key`, unleased — upserting the
    /// one pooled session per key (a warm resume updates, a cold deposit inserts).
    fn release(&mut self, key: &SessionKey, session_bytes: &str, manifest: &SessionManifest) -> rusqlite::Result<()>;
}

/// Is a pooled session eligible to lease? Ports `agent-pool.mjs`'s
/// `evaluateEligibility` exactly: the caller has already matched the
/// `{model, effort, task}` key (the `SELECT`), so this decides the remaining
/// gates over the deposited row's fields.
///
/// Tripwire: the eligibility predicate is `head_hash` freshness (#3422) AND age
/// (#3264) AND context cap AND lease-state — and `workspace_tree_hash` is
/// deliberately NOT a gate (#3341: a resume re-derives every deciding fact on
/// the fresh checkout, and this pool's sole consumer is the
/// construct/verify/refine loop where the tree changes between attempts by
/// design). Adding a tree-hash gate here would defeat the reuse this pool exists
/// for; dropping any of the four real gates admits a stale-cache resume.
#[allow(clippy::too_many_arguments)]
fn eligible(
    head_hash: &str,
    context_tokens: u64,
    deposited_at: u64,
    leased_until: Option<u64>,
    current_head_hash: &str,
    now: u64,
    cutoff_secs: u64,
    context_cap: u64,
) -> bool {
    // #3422 head-freshness: the static head (`CLAUDE.md` + skill text) is not a
    // re-derived belief but the cached prefix a resume reuses, so a moved head
    // is a real cache miss.
    if head_hash != current_head_hash {
        return false;
    }
    // Age bound (#3264): within the prompt-cache-TTL cutoff.
    if now.saturating_sub(deposited_at) >= cutoff_secs {
        return false;
    }
    // Context cap: a session over the ceiling is retired.
    if context_tokens > context_cap {
        return false;
    }
    // Lazy lease expiry: free when unleased or the lease has aged past its TTL,
    // so a crashed holder never wedges the key (no background sweep needed).
    match leased_until {
        Some(until) if until > now => return false,
        _ => {}
    }
    // NOTE: `workspace_tree_hash` is intentionally absent from this predicate
    // (#3341) — see the doc comment's tripwire.
    true
}

/// The schema, applied idempotently on every open. One pooled session per
/// `(model, effort, task)` key; `leased_until` NULL means unleased.
const MIGRATIONS: &str = "\
CREATE TABLE IF NOT EXISTS sessions (
    model               TEXT    NOT NULL,
    effort              TEXT    NOT NULL,
    task                TEXT    NOT NULL,
    session_bytes       TEXT    NOT NULL,
    receipt             TEXT    NOT NULL,
    parent_receipt      TEXT,
    head_hash           TEXT    NOT NULL,
    context_tokens      INTEGER NOT NULL,
    workspace_tree_hash TEXT    NOT NULL,
    read_files          TEXT    NOT NULL,
    deposited_at        INTEGER NOT NULL,
    leased_until        INTEGER,
    PRIMARY KEY (model, effort, task)
);
";

/// A WAL-mode `SQLite` pool. Opening runs the migration idempotently, so
/// reopening the same file resumes against the persisted pool.
pub struct SqliteSessionStore {
    conn: Connection,
    cutoff_secs: u64,
    lease_ttl_secs: u64,
    context_cap: u64,
}

impl SqliteSessionStore {
    /// Open (or create) a pool at `path` with the eligibility knobs (in seconds
    /// / tokens). `":memory:"` opens a private, non-durable in-memory database —
    /// the same code path, used by tests and the default unconfigured chassis.
    pub fn open(path: &str, cutoff_secs: u64, lease_ttl_secs: u64, context_cap: u64) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(MIGRATIONS)?;
        Ok(Self { conn, cutoff_secs, lease_ttl_secs, context_cap })
    }
}

impl SessionBackend for SqliteSessionStore {
    fn acquire(
        &mut self,
        key: &SessionKey,
        current_head_hash: &str,
        now: u64,
    ) -> rusqlite::Result<Option<LeasedSession>> {
        // Read the one pooled session for the key, then decide eligibility over
        // its fields (mirroring `evaluateEligibility`). One transaction so the
        // read and the lease-marking `UPDATE` are atomic.
        let tx = self.conn.transaction()?;
        let row = tx
            .query_row(
                "SELECT session_bytes, receipt, head_hash, context_tokens, deposited_at, leased_until
                 FROM sessions WHERE model = ?1 AND effort = ?2 AND task = ?3",
                rusqlite::params![key.model, key.effort, key.task],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        u64::try_from(row.get::<_, i64>(3)?).unwrap_or(u64::MAX),
                        u64::try_from(row.get::<_, i64>(4)?).unwrap_or_default(),
                        row.get::<_, Option<i64>>(5)?.map(|v| u64::try_from(v).unwrap_or_default()),
                    ))
                },
            )
            .optional()?;
        let Some((session_bytes, receipt, head_hash, context_tokens, deposited_at, leased_until)) = row else {
            return Ok(None);
        };
        if !eligible(
            &head_hash,
            context_tokens,
            deposited_at,
            leased_until,
            current_head_hash,
            now,
            self.cutoff_secs,
            self.context_cap,
        ) {
            return Ok(None);
        }
        let lease_expiry = now.saturating_add(self.lease_ttl_secs);
        tx.execute(
            "UPDATE sessions SET leased_until = ?4 WHERE model = ?1 AND effort = ?2 AND task = ?3",
            rusqlite::params![key.model, key.effort, key.task, i64::try_from(lease_expiry).unwrap_or(i64::MAX)],
        )?;
        tx.commit()?;
        let lease = LeaseToken(format!("{}:{}:{}:{now}", key.model, key.effort, key.task));
        Ok(Some(LeasedSession { lease, session_bytes, parent_receipt: receipt }))
    }

    fn release(&mut self, key: &SessionKey, session_bytes: &str, manifest: &SessionManifest) -> rusqlite::Result<()> {
        // The read set is audit-only (#3341) — carried as JSON so a caller's
        // path list round-trips without a delimiter convention.
        let read_files = serde_json::to_string(&manifest.read_files).unwrap_or_else(|_| "[]".to_owned());
        // Upsert on the key, always depositing unleased (`leased_until = NULL`):
        // a warm release updates the row a resume leased, a cold release inserts.
        self.conn.execute(
            "INSERT INTO sessions
               (model, effort, task, session_bytes, receipt, parent_receipt, head_hash,
                context_tokens, workspace_tree_hash, read_files, deposited_at, leased_until)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL)
             ON CONFLICT(model, effort, task) DO UPDATE SET
               session_bytes = excluded.session_bytes,
               receipt = excluded.receipt,
               parent_receipt = excluded.parent_receipt,
               head_hash = excluded.head_hash,
               context_tokens = excluded.context_tokens,
               workspace_tree_hash = excluded.workspace_tree_hash,
               read_files = excluded.read_files,
               deposited_at = excluded.deposited_at,
               leased_until = NULL",
            rusqlite::params![
                key.model,
                key.effort,
                key.task,
                session_bytes,
                manifest.receipt,
                manifest.parent_receipt,
                manifest.head_hash,
                i64::try_from(manifest.context_tokens).unwrap_or(i64::MAX),
                manifest.workspace_tree_hash,
                read_files,
                i64::try_from(manifest.deposited_at).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(())
    }
}

/// Runtime state for [`SessionPoolCapability`]: the one durable pool the
/// dispatcher owns.
pub struct SessionPoolState {
    backend: Box<dyn SessionBackend>,
}

impl SessionPoolState {
    /// Build state over an explicit backend — the seam the handler tests drive.
    #[must_use]
    pub fn new(backend: Box<dyn SessionBackend>) -> Self {
        Self { backend }
    }
}

/// Current wall-clock time in seconds since the Unix epoch (a pre-1970 clock
/// falls back to 0). The pool's age + lease comparisons are all in this domain,
/// shared with the depositing caller's `deposited_at` (the fleet shares one
/// clock).
fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[runtime]
impl NativeActor for SessionPoolCapability {
    type State = SessionPoolState;
    type Config = super::SessionConfig;

    const NAMESPACE: &'static str = "aether.session";

    fn init(config: super::SessionConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<SessionPoolState, BootError> {
        let store = SqliteSessionStore::open(
            &config.db_path,
            config.cache_ttl_cutoff_mins.saturating_mul(60),
            config.lease_ttl_mins.saturating_mul(60),
            config.context_cap_tokens,
        )
        .map_err(|error| BootError::Other(Box::new(error)))?;
        tracing::info!(
            target: "aether_chassis_bloomery::session",
            path = %config.db_path,
            cutoff_mins = config.cache_ttl_cutoff_mins,
            "session pool opened (WAL)"
        );
        Ok(SessionPoolState { backend: Box::new(store) })
    }

    #[handler::single]
    fn on_acquire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Acquire) -> AcquireResult {
        let Acquire { key, current_head_hash } = mail;
        match state.backend.acquire(&key, &current_head_hash, now_secs()) {
            Ok(Some(LeasedSession { lease, session_bytes, parent_receipt })) => {
                AcquireResult::Leased { lease, session_bytes, parent_receipt }
            }
            Ok(None) => AcquireResult::None,
            Err(error) => AcquireResult::Err { error: error.to_string() },
        }
    }

    #[handler::single]
    fn on_release(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Release) -> ReleaseResult {
        let Release { key, lease: _, session_bytes, manifest } = mail;
        match state.backend.release(&key, &session_bytes, &manifest) {
            Ok(()) => ReleaseResult::Ok,
            Err(error) => ReleaseResult::Err { error: error.to_string() },
        }
    }
}
