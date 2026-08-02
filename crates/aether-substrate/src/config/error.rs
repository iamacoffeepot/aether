//! The boot-time config fault (ADR-0090 §4) — distinct from
//! [`BootError`] so a chassis env resolver can surface a
//! config-specific error before the generic boot path.

use std::error::Error as StdError;
use std::fmt;
use std::path::PathBuf;

use crate::BootError;

/// A boot-time config fault (ADR-0090 §4). Distinct from
/// [`BootError`] so the chassis env resolvers can surface a
/// config-specific error before the generic boot path; it
/// `From`-converts into `BootError::Other`.
#[derive(Debug)]
pub enum ConfigError {
    /// A known env key (claimed by a `#[derive(Config)]` field or a
    /// hand-registered knob) carried a value the parser rejected.
    /// The soft warn-and-default fall-through is gone (ADR-0090 §4):
    /// a garbage known value aborts boot loudly. `source` carries the
    /// underlying parse error (a `confique::Error` or a cap-specific
    /// `ParseIntError`).
    UnparseableKnown {
        /// The env key (or the layer field, when confique didn't
        /// surface a key) whose value failed to parse.
        key: String,
        /// The offending raw value, when the resolver had it in hand.
        /// confique's own error already embeds the value in its
        /// `Display`, so this is `None` on the confique path.
        value: Option<String>,
        /// The underlying parse error.
        source: Box<dyn StdError + Send + Sync + 'static>,
    },
    /// An explicitly supplied chassis config file could not be read or
    /// parsed. Missing files are hard errors because the operator asked
    /// for this source.
    ConfigFile {
        /// File path supplied by `--config` or `AETHER_CONFIG_FILE`.
        path: PathBuf,
        /// The underlying read or TOML parse error.
        source: Box<dyn StdError + Send + Sync + 'static>,
    },
    /// A section inside the chassis config file was present but could
    /// not be decoded into the target config layer.
    ConfigSection {
        /// TOML section name, e.g. `http` or `scheduler`.
        section: String,
        /// The underlying TOML decode error.
        source: Box<dyn StdError + Send + Sync + 'static>,
    },
    /// ADR-0156 §5: a programmatic override was staged on the source stack whose
    /// config type `T` matches no composed member — a typo'd or orphaned
    /// override that would otherwise be silently left behind. The paired
    /// `Builder::with_actor_configured` makes this unconstructable (an override
    /// always composes its actor); this defends the `ConfigSources` bulk path
    /// (`set_override` + `with_config_sources`) a chassis test can drive. A hard
    /// boot error naming `T` (via [`ConfigSources::validate_overrides`](crate::ConfigSources::validate_overrides)).
    OrphanOverride {
        /// The `type_name` of the staged override that matches no composed member.
        type_name: String,
    },
    /// ADR-0156 §5 (issue 3872): an argv overlay layer was staged on the source
    /// stack (`ConfigSources::set_argv`, via a chassis CLI root's derived
    /// `StageArgv`) whose config type `T` no composed member ever resolves — a
    /// flag the chassis parses and stages but then silently drops. The converse
    /// of [`OrphanOverride`](Self::OrphanOverride) on the argv channel: a hard
    /// boot error naming `T` (via
    /// [`ConfigSources::validate_no_orphan_argv`](crate::ConfigSources::validate_no_orphan_argv)),
    /// so a staged-but-never-composed argv layer fails as loudly as the override
    /// channel already does.
    OrphanArgv {
        /// The `type_name` of the staged argv layer that no composed member consumed.
        type_name: String,
    },
}

impl ConfigError {
    /// Wrap a `confique::Error` (always an env-parse failure on the
    /// load path — defaults are validated by the cap `*_defaults_match`
    /// tests). The confique error's `Display` already names the field,
    /// key, and value.
    #[must_use]
    pub fn from_confique(err: confique::Error) -> Self {
        Self::UnparseableKnown { key: String::new(), value: None, source: Box::new(err) }
    }

    /// Build an `UnparseableKnown` from a hand-resolved env read (a
    /// value parsed outside confique, e.g. `AETHER_BOOT_MANIFEST`).
    #[must_use]
    pub fn unparseable(
        key: impl Into<String>,
        value: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::UnparseableKnown { key: key.into(), value: Some(value.into()), source: Box::new(source) }
    }

    /// Build a hard error for an explicitly supplied config file.
    #[must_use]
    pub fn config_file(path: impl Into<PathBuf>, source: impl StdError + Send + Sync + 'static) -> Self {
        Self::ConfigFile { path: path.into(), source: Box::new(source) }
    }

    /// Build a hard error for a malformed section in a config file.
    #[must_use]
    pub fn config_section(section: impl Into<String>, source: impl StdError + Send + Sync + 'static) -> Self {
        Self::ConfigSection { section: section.into(), source: Box::new(source) }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnparseableKnown { key, value, source } => {
                if key.is_empty() {
                    write!(f, "unparseable config value: {source}")
                } else if let Some(value) = value {
                    write!(f, "unparseable value {value:?} for known config key {key:?}: {source}")
                } else {
                    write!(f, "unparseable value for known config key {key:?}: {source}")
                }
            }
            Self::ConfigFile { path, source } => {
                let path = path.display();
                write!(f, "failed to load chassis config file {path}: {source}")
            }
            Self::ConfigSection { section, source } => {
                write!(f, "failed to parse chassis config section [{section}]: {source}")
            }
            Self::OrphanOverride { type_name } => {
                write!(
                    f,
                    "programmatic config override for `{type_name}` matches no composed member \
                     — staged as a source-stack override but no composed actor declares it as its \
                     Config (typo? removed cap? wrong type?)"
                )
            }
            Self::OrphanArgv { type_name } => {
                write!(
                    f,
                    "argv overlay staged for `{type_name}` matches no composed member \
                     — the chassis parsed and staged its flag(s) but no composed actor declares it \
                     as its Config, so the value would be silently discarded (a cap overlay \
                     flattened into a CLI root the chassis never composes? removed cap? wrong type?)"
                )
            }
        }
    }
}

impl StdError for ConfigError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::UnparseableKnown { source, .. }
            | Self::ConfigFile { source, .. }
            | Self::ConfigSection { source, .. } => Some(&**source),
            Self::OrphanOverride { .. } | Self::OrphanArgv { .. } => None,
        }
    }
}

impl From<ConfigError> for BootError {
    fn from(e: ConfigError) -> Self {
        Self::Other(Box::new(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_fixtures::{FixtureConfig, an_int_error};
    use confique::Config as _;
    use std::env;

    #[test]
    fn config_error_display_names_key_and_value() {
        let e = ConfigError::unparseable("AETHER_BOOT_MANIFEST", "lots", an_int_error());
        let msg = e.to_string();
        assert!(msg.contains("AETHER_BOOT_MANIFEST"));
        assert!(msg.contains("lots"));
    }

    #[test]
    fn config_error_converts_into_boot_error() {
        let e = ConfigError::unparseable("K", "v", an_int_error());
        let boot: BootError = e.into();
        assert!(matches!(boot, BootError::Other(_)));
    }

    #[test]
    fn confique_load_errors_on_garbage_known_value() {
        // The hard-error half (ADR-0090 §4): a garbage known env value
        // makes confique `.load()` return `Err`, which
        // `ConfigError::from_confique` wraps. Mirrors the path
        // `FromArgvThenEnv::try_from_argv_then_env` takes.
        //
        // SAFETY: single-threaded test; we set the unique key, load,
        // then remove it before any other thread could read it.
        unsafe { env::set_var("AETHER_TEST_COUNT", "not-a-number") };
        let loaded = FixtureConfig::builder().env().load();
        // SAFETY: same single-threaded scope; restoring the env.
        unsafe { env::remove_var("AETHER_TEST_COUNT") };
        let result = loaded.map_err(ConfigError::from_confique);
        assert!(matches!(result, Err(ConfigError::UnparseableKnown { .. })));
    }
}
