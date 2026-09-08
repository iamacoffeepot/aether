//! The `store` capability's boot configuration (ADR-0090 derive-`Config`).

/// Where the `SQLite` journal lives, resolved argv > env > default.
///
/// `path` is the on-disk database file. The sentinel `":memory:"` opens a
/// private in-memory database (a fresh non-durable store per boot) — the
/// default, so an unconfigured chassis boots without touching the filesystem;
/// a durable deployment sets `AETHER_STORE_PATH` (or `--store-path`) to a file.
/// `--github-store-path` is the same knob: both spellings resolve to this path.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_STORE", cli_prefix = "store")]
pub struct StoreConfig {
    /// The `SQLite` database path, or `":memory:"` for a private in-memory store.
    #[config(default = ":memory:")]
    pub path: String,
    /// How long the boot journal replay reply is held before it is sent,
    /// in milliseconds. `0` (the default) answers as soon as the journal is
    /// read. A fixture proving the boot window — the reads the control core
    /// refuses until its replay has folded — sets this so that window is a
    /// stated width rather than however long one fold happens to take on
    /// a loaded runner (issue 5765).
    #[config(default = 0)]
    pub boot_replay_hold_millis: u64,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self { path: ":memory:".to_owned(), boot_replay_hold_millis: 0 }
    }
}
