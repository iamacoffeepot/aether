//! The `store` capability's boot configuration (ADR-0090 derive-`Config`).

use std::error::Error;
use std::fmt;

use aether_bloomery::StoreClass;

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
    /// Which world this journal records — `live` (the estate's own operation)
    /// or `trial` (a calibration host's benchmark rows, ADR-0184).
    ///
    /// Empty (the default) means unset, and the chassis resolves it from the
    /// GitHub backend: naming the fixture implies a trial store, anything else
    /// implies a live one. Trial mode is one mode rather than two knobs, so a
    /// value set here must agree with that — the class exists to be *stated and
    /// checked*, never to select a world of its own.
    ///
    /// The resolved class is stamped into a fresh journal and compared against
    /// the stamp of one that already exists, so a benchmark run cannot append
    /// to live history and a live coordinator cannot resume a benchmark's
    /// journal as if it were the estate's.
    #[config(env = "AETHER_STORE_CLASS", default = "", parse = parse_store_class)]
    pub class: String,
}

impl StoreConfig {
    /// The resolved class, with the empty default reading as
    /// [`StoreClass::Live`] — the posture of every journal on disk today.
    ///
    /// # Errors
    /// The value is a name outside the vocabulary. Argv and file overlays store
    /// the knob as a string and bypass [`parse_store_class`], so this is the
    /// gate every ingress passes through.
    pub fn class(&self) -> Result<StoreClass, UnknownStoreClass> {
        if self.class.trim().is_empty() {
            return Ok(StoreClass::Live);
        }
        StoreClass::parse(&self.class).ok_or_else(|| UnknownStoreClass(self.class.clone()))
    }
}

/// Confique `parse_env` for [`StoreConfig::class`]: only `live` and `trial`
/// resolve. An unknown name is a boot fault rather than a silent `live`, which
/// would put benchmark rows in the estate's ledger on a typo.
fn parse_store_class(s: &str) -> Result<String, UnknownStoreClass> {
    StoreClass::parse(s).map(|class| class.as_str().to_owned()).ok_or_else(|| UnknownStoreClass(s.to_owned()))
}

/// Why a journal class name was refused.
#[derive(Debug)]
pub struct UnknownStoreClass(pub String);

impl fmt::Display for UnknownStoreClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown journal class {:?}; must be live or trial", self.0)
    }
}

impl Error for UnknownStoreClass {}

impl Default for StoreConfig {
    fn default() -> Self {
        Self { path: ":memory:".to_owned(), boot_replay_hold_millis: 0, class: String::new() }
    }
}
