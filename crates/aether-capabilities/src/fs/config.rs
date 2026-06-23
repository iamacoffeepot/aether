//! Resolved filesystem-root configuration (ADR-0090). The
//! `#[derive(Config)]` layer the chassis builds from argv/env and
//! hands to `with_actor::<FsCapability>(roots)` (`registry`).

use std::fs;
use std::io;
use std::path::PathBuf;

#[cfg(feature = "fs-runtime")]
use std::error::Error;
#[cfg(feature = "fs-runtime")]
use std::fmt;

/// Resolved filesystem roots for the three ADR-0041 namespaces. The
/// chassis reads this at boot, hands each path to a `LocalFileAdapter`,
/// and registers the result in an `AdapterRegistry` keyed on the
/// namespace short name (`"save"`, `"assets"`, `"config"`).
///
/// ADR-0090 unit g (iamacoffeepot/aether#1264) escape hatch: the
/// `#[derive(aether_substrate::Config)]` emits the Layer +
/// `NamespaceRootsOverlay` + inherent `from_env` / `from_argv_then_env`
/// shims, but `#[config(skip_from_layer)]` opts the cap out of the
/// auto-generated `FromArgvThenEnv::from_layer`. The hand-written impl
/// (below) applies the runtime-computed `dirs::data_dir()` /
/// `current_exe()` / `dirs::config_dir()` fallbacks that confique
/// cannot express as literals. Per-field `env = "..."` overrides pin
/// the unprefixed `AETHER_*_DIR` env keys.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "fs-runtime", derive(aether_substrate::Config))]
#[cfg_attr(
    feature = "fs-runtime",
    config(env_prefix = "AETHER", cli_prefix = "", skip_from_layer)
)]
pub struct NamespaceRoots {
    #[cfg_attr(
        feature = "fs-runtime",
        config(env = "AETHER_SAVE_DIR", cli_long = "save-dir", parse = parse_dir)
    )]
    pub save: PathBuf,
    #[cfg_attr(
        feature = "fs-runtime",
        config(env = "AETHER_ASSETS_DIR", cli_long = "assets-dir", parse = parse_dir)
    )]
    pub assets: PathBuf,
    #[cfg_attr(
        feature = "fs-runtime",
        config(env = "AETHER_CONFIG_DIR", cli_long = "config-dir", parse = parse_dir)
    )]
    pub config: PathBuf,
}

impl NamespaceRoots {
    /// Pre-validate the configured roots: create each directory if
    /// missing, then canonicalize. Mirrors what `LocalFileAdapter::new`
    /// does inside `FsCapability::init`, but exposed so embedders
    /// can validate before building the chassis. Used by chassis
    /// builders that want to surface root-validity as a "skip the
    /// `aether.fs` cap and continue" decision rather than letting
    /// init failure abort the whole boot.
    pub fn ensure_dirs(&self) -> io::Result<()> {
        fs::create_dir_all(&self.save)?;
        fs::create_dir_all(&self.assets)?;
        fs::create_dir_all(&self.config)?;
        self.save.canonicalize()?;
        self.assets.canonicalize()?;
        self.config.canonicalize()?;
        Ok(())
    }
}

/// Hand-written `FromArgvThenEnv` impl for the `NamespaceRoots`
/// escape hatch (ADR-0090 unit g, iamacoffeepot/aether#1264). The
/// derive's `skip_from_layer` opt-out delegates `from_layer` here
/// because the defaults are *runtime-computed*
/// (`dirs::data_dir()` / `current_exe()` / `dirs::config_dir()`),
/// not literals confique can hold. Behaviour is byte-identical to
/// the prior `env_or_default` reader — an unset / empty
/// `AETHER_*_DIR` lands as `None` (the macro auto-promotes the
/// `PathBuf` domain to `Option<PathBuf>` on the Layer side when no
/// literal default is supplied), then the platform fallback
/// resolves it here.
///
/// On a platform-directory lookup failure (e.g. no `HOME`) or
/// `current_exe()` resolution failure, the fallback is
/// `temp_dir()/aether/...` so a boot always finishes even on
/// headless CI.
#[cfg(feature = "fs-runtime")]
impl aether_substrate::FromArgvThenEnv for NamespaceRoots {
    type Layer = NamespaceRootsLayer;

    fn from_layer(layer: NamespaceRootsLayer) -> Self {
        use std::env;
        use std::path::Path;
        Self {
            save: layer.save.unwrap_or_else(|| {
                dirs::data_dir()
                    .unwrap_or_else(env::temp_dir)
                    .join("aether")
                    .join("save")
            }),
            assets: layer.assets.unwrap_or_else(|| {
                env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(Path::to_path_buf))
                    .map_or_else(
                        || env::temp_dir().join("aether").join("assets"),
                        |p| p.join("assets"),
                    )
            }),
            config: layer.config.unwrap_or_else(|| {
                dirs::config_dir()
                    .unwrap_or_else(env::temp_dir)
                    .join("aether")
            }),
        }
    }
}

/// Parse a directory override. An empty string errors so confique
/// treats it as unset (preserving the prior `env_or_default`'s
/// `Ok(s) if !s.is_empty()` guard); any non-empty value is a path.
#[cfg(feature = "fs-runtime")]
pub(super) fn parse_dir(s: &str) -> Result<PathBuf, EmptyDir> {
    if s.is_empty() {
        Err(EmptyDir)
    } else {
        Ok(PathBuf::from(s))
    }
}

/// Sentinel error: an empty `AETHER_*_DIR` value, treated as unset by
/// confique's parse path (`Err` + empty → `None`).
#[cfg(feature = "fs-runtime")]
#[derive(Debug)]
pub(super) struct EmptyDir;

#[cfg(feature = "fs-runtime")]
impl fmt::Display for EmptyDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("empty directory override")
    }
}

#[cfg(feature = "fs-runtime")]
impl Error for EmptyDir {}

// ADR-0090: the confique migration is byte-identical to the prior
// `env_or_default` reader. These exercise resolution without touching
// process env (issue 464) — the parser is pure, and the defaults check
// loads the layer with no `.env()` source. Native-only because the
// `Config` derive (and `parse_dir` / the Layer) only exist under the
// `native` feature.
#[cfg(all(test, feature = "fs-runtime"))]
mod tests {
    use super::{NamespaceRootsLayer, parse_dir};
    use std::path::PathBuf;

    #[test]
    fn parse_dir_treats_empty_as_unset() {
        assert!(parse_dir("").is_err(), "empty → unset (Err → None)");
        assert_eq!(
            parse_dir("/tmp/aether-save").expect("non-empty parses to a path"),
            PathBuf::from("/tmp/aether-save")
        );
    }

    #[test]
    fn namespace_roots_layer_defaults_are_none() {
        use confique::Config as _;
        // No `.env()` source: each root has no literal default, so it
        // resolves to `None` and `from_env` applies the runtime
        // platform fallback. Env-free.
        let layer = NamespaceRootsLayer::builder()
            .load()
            .expect("defaults load");
        assert_eq!(layer.save, None);
        assert_eq!(layer.assets, None);
        assert_eq!(layer.config, None);
    }
}
