//! The `session` pool's boot configuration (ADR-0090 derive-`Config`).

use std::path::Path;

/// The executor session-pool knobs, resolved argv > env > default.
///
/// The defaults port `scripts/agent-pool.mjs`'s precedents: `cache_ttl_cutoff_mins`
/// is the `AGENT_POOL_CUTOFF_MINS = 55` age bound (just under the ~1 h
/// prompt-cache TTL), and `context_cap_tokens` is the
/// `AGENT_POOL_CONTEXT_CAP_TOKENS = 150000` context ceiling. `lease_ttl_mins`
/// bounds how long one `acquire` holds a session before lazy expiry reclaims it
/// (a crashed holder never wedges a key). `pricing_cliff_tokens` is the
/// prompt-size cut a *dependent construct* resume must project under (#5178);
/// `200_000` is grok-4.6's measured long-context band. It bounds chains only —
/// a same-member refine resumes its own construct session at any context,
/// because the alternative there is a cold re-read of the same member.
/// `dependency_increment_tokens` is the per-link addend that projection
/// uses on a predecessor resume. `db_path` is the pool
/// table's `SQLite` file; the sentinel `":memory:"` opens a private non-durable
/// store. A chassis that resolved a durable journal path repoints the
/// unconfigured default at a file beside it — see
/// [`default_beside_journal`](SessionConfig::default_beside_journal).
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_SESSION", cli_prefix = "session")]
pub struct SessionConfig {
    /// The pool-table `SQLite` path, or `":memory:"` for a private in-memory store.
    #[config(default = ":memory:")]
    pub db_path: String,
    /// Age bound in minutes — a session deposited longer ago than this is
    /// ineligible (the prompt-cache-TTL cutoff, `AGENT_POOL_CUTOFF_MINS`). The
    /// same bound is the provider-cache warmth gate on a journaled predecessor
    /// resume (#5178).
    #[config(default = 55)]
    pub cache_ttl_cutoff_mins: u64,
    /// Lease lifetime in minutes — a lease older than this is treated as free
    /// (lazy expiry; no background sweep).
    #[config(default = 15)]
    pub lease_ttl_mins: u64,
    /// Context ceiling in tokens — a session whose terminal context exceeds this
    /// is ineligible (`AGENT_POOL_CONTEXT_CAP_TOKENS`).
    #[config(default = 150_000)]
    pub context_cap_tokens: u64,
    /// Prompt-token threshold a resumed dependent construct must project under,
    /// or it launches fresh. Chains only — a same-member refine ignores it.
    /// Default `200_000` is grok-4.6's measured long-context pricing cliff.
    #[config(default = 200_000)]
    pub pricing_cliff_tokens: u64,
    /// Tokens a dependent construct is projected to add on top of a predecessor's
    /// stored context — one graph link. Default `56_000` is the measured
    /// successor increment (#5178).
    #[config(default = 56_000)]
    pub dependency_increment_tokens: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            db_path: ":memory:".to_owned(),
            cache_ttl_cutoff_mins: 55,
            lease_ttl_mins: 15,
            context_cap_tokens: 150_000,
            pricing_cliff_tokens: 200_000,
            dependency_increment_tokens: 56_000,
        }
    }
}

/// The unconfigured `db_path` — a private, non-durable store.
const MEMORY_SENTINEL: &str = ":memory:";

/// Process-wide in-memory `SQLite` URI. A bare `:memory:` is private per
/// connection, so the mounted [`SessionPoolCapability`](super::SessionPoolCapability)
/// and the executor consumer would otherwise be two empty tables. Shared-cache
/// keeps the default non-durable while making them the same pool.
const SHARED_MEMORY: &str = "file:aether-session-pool?mode=memory&cache=shared";

impl SessionConfig {
    /// The `SQLite` path the pool and its in-process consumer both open.
    ///
    /// A configured file path is used as-is (WAL shares it). The `:memory:`
    /// default becomes the process-wide shared-memory URI so the capability
    /// and the executor see one table rather than two.
    #[must_use]
    pub fn store_path(&self) -> &str {
        if self.db_path == MEMORY_SENTINEL {
            SHARED_MEMORY
        } else {
            &self.db_path
        }
    }

    /// Point an unconfigured pool at a durable file beside the journal store.
    ///
    /// A coordinator restart must not discard every resumable session. The
    /// `:memory:` default lives exactly as long as the process, so a restart —
    /// a redeploy, a crash, an operator bounce — leaves every in-flight member's
    /// construct session unreachable, and the next lap on each of them relaunches
    /// cold and re-reads its whole member. The pool belongs to the same work the
    /// journal describes, so it gets the same lifetime: `sessions.sqlite` in the
    /// journal file's own directory.
    ///
    /// Only the untouched default is repointed. An explicitly configured
    /// `db_path` is the operator's choice, and a journal that is itself
    /// non-durable (`:memory:`, or any `SQLite` URI) has no directory to sit
    /// beside — both keep what they had.
    pub fn default_beside_journal(&mut self, journal_path: &str) {
        if self.db_path != MEMORY_SENTINEL || journal_path == MEMORY_SENTINEL || journal_path.starts_with("file:") {
            return;
        }
        let Some(parent) = Path::new(journal_path).parent() else {
            return;
        };
        self.db_path = parent.join("sessions.sqlite").to_string_lossy().into_owned();
    }
}
