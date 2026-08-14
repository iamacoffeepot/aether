//! The `session` pool's boot configuration (ADR-0090 derive-`Config`).

/// The executor session-pool knobs, resolved argv > env > default.
///
/// The defaults port `scripts/agent-pool.mjs`'s precedents: `cache_ttl_cutoff_mins`
/// is the `AGENT_POOL_CUTOFF_MINS = 55` age bound (just under the ~1 h
/// prompt-cache TTL), and `context_cap_tokens` is the
/// `AGENT_POOL_CONTEXT_CAP_TOKENS = 150000` context ceiling. `lease_ttl_mins`
/// bounds how long one `acquire` holds a session before lazy expiry reclaims it
/// (a crashed holder never wedges a key). `db_path` is the pool table's `SQLite`
/// file; the sentinel `":memory:"` opens a private non-durable store — the
/// default, so an unconfigured chassis boots without touching the filesystem.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_SESSION", cli_prefix = "session")]
pub struct SessionConfig {
    /// The pool-table `SQLite` path, or `":memory:"` for a private in-memory store.
    #[config(default = ":memory:")]
    pub db_path: String,
    /// Age bound in minutes — a session deposited longer ago than this is
    /// ineligible (the prompt-cache-TTL cutoff, `AGENT_POOL_CUTOFF_MINS`).
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
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            db_path: ":memory:".to_owned(),
            cache_ttl_cutoff_mins: 55,
            lease_ttl_mins: 15,
            context_cap_tokens: 150_000,
        }
    }
}

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
        if self.db_path == ":memory:" {
            SHARED_MEMORY
        } else {
            &self.db_path
        }
    }
}
