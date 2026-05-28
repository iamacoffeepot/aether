//! Shared confique-overlay glue for resolved cap configs (ADR-0090
//! unit d).
//!
//! Every cap that opted into confique (`HttpConfig`, `AudioConfig`,
//! `GeminiConfig`, `AnthropicConfig`, `NamespaceRoots`) ships a
//! mechanical `from_argv_then_env(argv) -> Self`: build the
//! cap's `*ConfigLayer` with the argv overlay preloaded, run
//! `.env().load()`, hand the loaded layer to a per-cap `from_layer`
//! mapping. Only `from_layer` actually differs across caps — the
//! builder boilerplate is identical.
//!
//! [`FromArgvThenEnv`] hoists the boilerplate into a default method.
//! Each cap impls just `from_layer` (and names its `Layer` associated
//! type); the trait's default `from_argv_then_env` is inherited
//! verbatim.
//!
//! # Why not `PersistConfig` too?
//!
//! `aether_substrate::handle_store::PersistConfig::from_argv_then_env`
//! takes four arguments (`enabled: bool`, `dir: Option<PathBuf>`,
//! `disable: Option<bool>`, `numeric: <_>::Layer`) because two of its
//! overlays are *not* confique fields — `enabled` is the chassis's
//! structural vote (a `false` short-circuits to `None` regardless of
//! env), and `dir` / `disable` interact with `dirs::data_dir()` lookup
//! and an `ENV_PERSIST_DISABLE` short-circuit that don't fit confique's
//! literal-default model. Only the numeric budget / tick knobs flow
//! through a `Layer`. Forcing it into a `(Layer) -> Self` shape would
//! mean re-deriving the structural inputs from environment reads inside
//! the trait body, which is exactly the structure the cap deliberately
//! kept hand-written. The five other caps still benefit from the trait;
//! `PersistConfig` stays inherent.

/// Build a cap config by overlaying an argv-derived partial confique
/// layer on top of the env layer (ADR-0090 unit d).
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
        let layer = <Self::Layer as confique::Config>::builder()
            .preloaded(argv)
            .env()
            .load()
            .expect("config layer defaults are well-formed");
        Self::from_layer(layer)
    }
}
