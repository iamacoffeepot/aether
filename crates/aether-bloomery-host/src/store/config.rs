//! The `store` capability's boot configuration (ADR-0090 derive-`Config`).

/// Where the `SQLite` journal lives, resolved argv > env > default.
///
/// `path` is the on-disk database file. The sentinel `":memory:"` opens a
/// private in-memory database (a fresh non-durable store per boot) — the
/// default, so an unconfigured chassis boots without touching the filesystem;
/// a durable deployment sets `AETHER_STORE_PATH` (or `--store-path`) to a file.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_STORE", cli_prefix = "store")]
pub struct StoreConfig {
    /// The `SQLite` database path, or `":memory:"` for a private in-memory store.
    #[config(default = ":memory:")]
    pub path: String,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self { path: ":memory:".to_owned() }
    }
}
