//! Staging-root configuration for the content-gen caps (ADR-0090). The
//! `#[derive(Config)]` layer the chassis resolves from argv/env and folds
//! into the resolved staging root it threads into the provider caps
//! (`with_common_caps`), so the knob is visible to the ADR-0090 `--config`
//! dump and the unknown-key sweep.

use std::path::PathBuf;

/// Content-gen staging config (ADR-0050 / ADR-0090). The one knob is the
/// override root generated artifacts stage under; unset, the chassis
/// resolves it from the already-resolved `save`-namespace root the
/// `aether.fs` cap owns (`AETHER_SAVE_DIR` → platform default), so a
/// component reads staged files back via
/// `aether.fs.read { namespace: "save", … }`.
///
/// ADR-0090 unit g (iamacoffeepot/aether#1264): the
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `ContentGenConfigLayer`, the clap-shaped `ContentGenOverlay`, the
/// `FromArgvThenEnv` impl, and the inherent `from_env` shims under
/// `feature = "runtime"`. The wasm-marker build carries only the domain
/// struct.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "runtime", derive(aether_substrate::Config))]
#[cfg_attr(feature = "runtime", config(env_prefix = "AETHER", cli_prefix = ""))]
pub struct ContentGenConfig {
    /// Directory generated artifacts are staged under.
    ///
    /// `env` pins the unprefixed `AETHER_GEN_DIR` key; unset (or empty)
    /// tracks the resolved `save`-namespace root at chassis boot.
    #[cfg_attr(feature = "runtime", config(env = "AETHER_GEN_DIR", cli_long = "gen-dir"))]
    pub gen_dir: Option<PathBuf>,
}
