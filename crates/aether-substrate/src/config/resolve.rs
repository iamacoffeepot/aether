//! The argv-then-env resolution path: the [`FromArgvThenEnv`] trait every
//! `#[derive(aether_substrate::Config)]` emits an impl of, the confique
//! `parse_env` helper that backs the `csv_set` field hint, and the config-file
//! section slice the file layer resolves through.

use std::collections::HashSet;
use std::convert::Infallible;
use std::io;

use super::error::ConfigError;

/// Split a comma-separated env value into a `HashSet`, trimming each
/// element and dropping empties. The `#[derive(Config)]` macro wires
/// this as the `parse_env` for any field carrying the `csv_set` hint
/// (the allowlist / bootstrap-path knobs), so a cap declares `csv_set`
/// and never hand-rolls the split. Total — a missing/empty value yields
/// the empty set, which confique then overrides with the field default
/// when the var is unset.
///
/// Unlike confique's own `env::parse::list_by_sep`, this trims each
/// element and drops empties, so `"a, ,b,"` is `{"a", "b"}` rather than
/// `{"a", " ", "b", ""}`.
///
/// # Errors
///
/// Never errors (the return type is [`Infallible`]); the `Result` is the
/// shape confique's `parse_env` contract requires.
pub fn parse_csv_set(s: &str) -> Result<HashSet<String>, Infallible> {
    Ok(s.split(',').map(str::trim).filter(|element| !element.is_empty()).map(str::to_string).collect())
}

/// Build a cap config by overlaying an argv-derived partial confique
/// layer on top of the env layer (ADR-0090 unit d) and, optionally, a
/// lower-priority config-file layer (ADR-0090 step 3).
///
/// The cap declares its env-shaped layer via [`Layer`] (a
/// `#[derive(confique::Config)]` struct) and its per-cap mapping via
/// [`from_layer`]. The default [`from_argv_then_env`] builds the
/// preloaded `Layer`, runs the env-plus-defaults resolution, and hands
/// off to `from_layer`. Argv-set fields win against env; unset
/// (`None`) fields fall through to env, then to the literal defaults
/// declared on `Layer`.
///
/// [`Layer`]: Self::Layer
/// [`from_layer`]: Self::from_layer
/// [`from_argv_then_env`]: Self::from_argv_then_env
pub trait FromArgvThenEnv: Sized {
    /// The env-shaped confique layer behind this config — the
    /// `#[derive(confique::Config)]` struct whose fields carry the
    /// `AETHER_*` env keys + literal defaults.
    type Layer: confique::Config;

    /// Per-cap mapping from the loaded confique layer onto the
    /// domain-shaped config struct. This is the only part that
    /// actually differs across caps (ms → `Duration`, CSV → `HashSet`,
    /// raw `Option<String>` → soft-parsed numeric, etc.).
    fn from_layer(layer: Self::Layer) -> Self;

    /// Resolve the config from a chassis-CLI argv overlay shadowing
    /// `AETHER_*` env (ADR-0090 unit d, issue 1258). Argv-set fields
    /// win; unset (`None`) fall through to env, then literal defaults.
    /// Defaulted — every cap inherits this verbatim.
    ///
    /// # Panics
    ///
    /// Panics only if the cap's layer literal defaults are themselves
    /// malformed — a programmer error caught by each cap's
    /// `*_defaults_match` test, never a runtime config fault (env
    /// values flow through total parsers).
    #[must_use]
    fn from_argv_then_env(argv: <Self::Layer as confique::Config>::Layer) -> Self {
        match Self::try_from_argv_then_env(argv) {
            Ok(this) => this,
            Err(e) => panic!("config layer resolution failed: {e}"),
        }
    }

    /// Fallible sibling of [`from_argv_then_env`](Self::from_argv_then_env):
    /// surfaces an unparseable known env value as a [`ConfigError`]
    /// rather than panicking (ADR-0090 §4 — the e1 hard-error half).
    /// The chassis env resolvers call this and `?`-propagate.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnparseableKnown`] when a known env key
    /// (or argv overlay value) fails the layer's parser — the soft
    /// `.expect()` fall-through is gone.
    fn try_from_argv_then_env(argv: <Self::Layer as confique::Config>::Layer) -> Result<Self, ConfigError> {
        Self::try_resolve(argv, None)
    }

    /// Resolve with a lower-priority config-file layer. Source order is
    /// argv > env > file > typed defaults; `None` preserves the old
    /// argv > env > defaults path byte-for-byte.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnparseableKnown`] when a known env key or
    /// preloaded layer value fails the layer's parser.
    fn try_resolve(
        argv: <Self::Layer as confique::Config>::Layer,
        file: Option<<Self::Layer as confique::Config>::Layer>,
    ) -> Result<Self, ConfigError> {
        let mut builder = <Self::Layer as confique::Config>::builder().preloaded(argv).env();
        if let Some(file) = file {
            builder = builder.preloaded(file);
        }
        let layer = builder.load().map_err(ConfigError::from_confique)?;
        Ok(Self::from_layer(layer))
    }

    /// The hermetic sibling of [`try_resolve`](Self::try_resolve): resolves
    /// with **no env layer** (ADR-0156 §5). Source order collapses to argv >
    /// file > typed defaults — the process environment never contributes. Backs
    /// [`ConfigSources::hermetic`](crate::ConfigSources::hermetic), which `SubstrateHarness` uses so a member it
    /// composes but forgets to stage falls through to its compiled default
    /// (deterministic) rather than a stray process env var (a flake factory):
    /// before the compose-then-resolve inversion, harness-composed members never
    /// read env at all (their values were constructed directly), and this keeps
    /// that property.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnparseableKnown`] when a preloaded argv/file
    /// value fails the layer's parser.
    fn try_resolve_hermetic(
        argv: <Self::Layer as confique::Config>::Layer,
        file: Option<<Self::Layer as confique::Config>::Layer>,
    ) -> Result<Self, ConfigError> {
        let mut builder = <Self::Layer as confique::Config>::builder().preloaded(argv);
        if let Some(file) = file {
            builder = builder.preloaded(file);
        }
        let layer = builder.load().map_err(ConfigError::from_confique)?;
        Ok(Self::from_layer(layer))
    }
}

/// Extract one `[section]` from a parsed chassis config file and deserialize
/// it into the target config's partial confique layer (ADR-0090 step 3). An
/// absent section is `None`; a present but malformed section is a hard boot
/// error. Moved substrate-side (ADR-0156 §5) so the builder's source stack
/// slices the file itself rather than the chassis threading a per-cap section
/// string.
///
/// # Errors
///
/// Returns [`ConfigError`] when a present section is not a table or cannot
/// deserialize into the target layer.
pub fn file_section<C: FromArgvThenEnv>(
    table: &toml::Table,
    section: &str,
) -> Result<Option<<C::Layer as confique::Config>::Layer>, ConfigError> {
    let Some(value) = table.get(section) else {
        return Ok(None);
    };
    if !matches!(value, toml::Value::Table(_)) {
        let source = io::Error::new(io::ErrorKind::InvalidData, format!("expected [{section}] to be a TOML table"));
        return Err(ConfigError::config_section(section, source));
    }
    value.clone().try_into().map(Some).map_err(|source| ConfigError::config_section(section, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_fixtures::{PrecedenceConfig, precedence_file_layer};
    use std::env;

    #[test]
    fn parse_csv_set_trims_and_drops_empties() {
        let got = parse_csv_set("a.com, b.com ,, c.com").expect("infallible");
        let want: HashSet<String> = ["a.com", "b.com", "c.com"].iter().map(|s| (*s).to_string()).collect();
        assert_eq!(got, want);
        assert!(parse_csv_set("").expect("infallible").is_empty());
        assert!(parse_csv_set("  ,  , ").expect("infallible").is_empty());
    }

    #[test]
    fn try_resolve_orders_argv_over_env_over_file() {
        use confique::Layer as _;

        // Tripwire: file layer must sit below env below argv.
        let mut argv = <PrecedenceConfig as confique::Config>::Layer::empty();
        argv.count = Some(33);

        // SAFETY: unique key set then removed in this test scope.
        unsafe { env::set_var("AETHER_PRECEDENCE_COUNT", "22") };
        let resolved =
            PrecedenceConfig::try_resolve(argv, Some(precedence_file_layer(11))).expect("argv/env/file resolve");
        assert_eq!(resolved.count, 33, "argv wins over env and file");

        let env_wins = PrecedenceConfig::try_resolve(
            <PrecedenceConfig as confique::Config>::Layer::empty(),
            Some(precedence_file_layer(11)),
        )
        .expect("env/file resolve");
        assert_eq!(env_wins.count, 22, "env wins over file");

        // SAFETY: same scope.
        unsafe { env::remove_var("AETHER_PRECEDENCE_COUNT") };
        let file_wins = PrecedenceConfig::try_resolve(
            <PrecedenceConfig as confique::Config>::Layer::empty(),
            Some(precedence_file_layer(11)),
        )
        .expect("file/default resolve");
        assert_eq!(file_wins.count, 11, "file wins over default");
    }

    #[test]
    fn file_section_absent_is_none_and_non_table_errors() {
        // Tripwire: the moved `file_section` slices a named `[section]` — an
        // absent section falls through (`None`), a present non-table section is
        // a hard boot error. Drifts if the section-extraction logic breaks.
        let table = "[precedence]\ncount = 5\n".parse::<toml::Table>().expect("parse table");
        assert!(
            file_section::<PrecedenceConfig>(&table, "absent").expect("absent ok").is_none(),
            "absent section falls through to env/defaults",
        );
        let present = file_section::<PrecedenceConfig>(&table, "precedence")
            .expect("present section decodes")
            .expect("section present");
        assert_eq!(present.count, Some(5));

        let bad = "precedence = 7\n".parse::<toml::Table>().expect("parse table");
        assert!(
            matches!(file_section::<PrecedenceConfig>(&bad, "precedence"), Err(ConfigError::ConfigSection { .. })),
            "a non-table section is a hard error",
        );
    }
}
