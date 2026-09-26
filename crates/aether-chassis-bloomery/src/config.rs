//! The bloomery chassis knobs: the journal root and the closure byte budget.
//!
//! [`BloomeryConfig`] is the chassis's own derive-`Config` member, resolved off
//! the source stack into [`BloomeryEnv`](crate::chassis::BloomeryEnv) and declared
//! on the builder by [`compose`](aether_substrate::chassis::BootableChassis::compose),
//! so the known-key sweep and `--print-config` list both knobs. `build` lowers
//! its value to the typed pair the journal owner and the bundle driver spawn
//! over before it stands up the substrate, so a refused knob costs no boot.

use std::io;
use std::path::PathBuf;

use aether_bloomery_kinds::ClosureLimit;
use aether_substrate::chassis::error::BootError;

/// The bloomery chassis knobs (issue #6244).
///
/// The `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `BloomeryConfigLayer`, the clap-shaped [`BloomeryOverlay`], the
/// `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` shims — mirrors
/// [`ChassisBootConfig`](aether_chassis::boot::ChassisBootConfig).
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_BLOOMERY", cli_prefix = "bloomery")]
pub struct BloomeryConfig {
    /// Journal root directory the engine opens and drives: `journal.sqlite`
    /// plus the `blobs` directory of artifact files (ADR-0220).
    ///
    /// Created when absent, but its parent directory must already exist. A
    /// path that is a regular file, such as an old single-file journal, is
    /// refused. The engine holds the root's exclusive lock while it runs, so
    /// two engines cannot share one root. Required: an unset journal refuses boot
    /// with a named error. It stays an `Option` rather than a mandatory flag so
    /// `--describe` / `--print-config` answer with no journal configured
    /// (ADR-0155 §4) — they exit in the shared prelude, before `build`.
    pub journal: Option<String>,
    /// Byte budget for one closure read handed to the bundle driver.
    ///
    /// Lowered once through [`ClosureLimit::new`] (ADR-0226 decision 4); a
    /// refused value is a boot error naming the key, never a panic.
    #[config(default = 4_294_967_296u64)]
    pub closure_limit_bytes: u64,
}

impl Default for BloomeryConfig {
    fn default() -> Self {
        // Matches the unset resolution: no journal and the ceiling limit. The
        // `mem::take` lift in `BloomeryChassis::build` needs this, and a derived
        // `Default` would leave `closure_limit_bytes: 0`, which
        // `ClosureLimit::new` refuses — the honest default is stated rather
        // than derived, the way `impl Default for RuntimeConfig` does.
        Self { journal: None, closure_limit_bytes: ClosureLimit::MAX_BYTES }
    }
}

impl BloomeryConfig {
    /// Lower the resolved knobs to the typed pair the mount seam spawns over:
    /// the journal root and the driver's closure limit. Called at the top of
    /// [`BloomeryChassis::build`](crate::BloomeryChassis), ahead of every boot
    /// side effect.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] naming `AETHER_BLOOMERY_JOURNAL` /
    /// `--bloomery-journal` when no journal is configured, or naming
    /// `AETHER_BLOOMERY_CLOSURE_LIMIT_BYTES` when the limit falls outside
    /// `ClosureLimit`'s accepted range.
    pub(crate) fn to_journal_and_limit(&self) -> Result<(PathBuf, ClosureLimit), BootError> {
        let Some(journal) = self.journal.as_deref().filter(|path| !path.is_empty()) else {
            return Err(BootError::Other(Box::new(io::Error::other(
                "the bloomery chassis needs a journal root: set AETHER_BLOOMERY_JOURNAL or pass --bloomery-journal <PATH>",
            ))));
        };
        let limit = ClosureLimit::new(self.closure_limit_bytes).map_err(|error| {
            BootError::Other(Box::new(io::Error::other(format!(
                "AETHER_BLOOMERY_CLOSURE_LIMIT_BYTES={} is not a usable closure limit: {error}",
                self.closure_limit_bytes,
            ))))
        })?;
        Ok((PathBuf::from(journal), limit))
    }
}

#[cfg(test)]
mod tests {
    use super::BloomeryConfig;
    use aether_bloomery_kinds::ClosureLimit;
    use aether_substrate::config::ConfigSources;

    #[test]
    fn default_closure_limit_is_accepted() {
        // Resolving off an empty stack must yield the declared default, and the
        // default must be a limit `ClosureLimit::new` accepts: catches the
        // derive's `default = 4_294_967_296u64` literal drifting above a lowered
        // `ClosureLimit::MAX_BYTES`, which would fail every default boot.
        let mut sources = ConfigSources::new(None);
        let mut config = sources.resolve::<BloomeryConfig>().expect("resolve off an empty stack");
        assert_eq!(config.closure_limit_bytes, ClosureLimit::MAX_BYTES);
        config.journal = Some("journal".to_owned());
        let (_, limit) = config.to_journal_and_limit().expect("the default limit lowers");
        assert_eq!(limit.get(), ClosureLimit::MAX_BYTES);
    }
}
