//! Shared confique-overlay glue for resolved cap configs (ADR-0090
//! unit d), and the trait that ADR-0090 unit g
//! (iamacoffeepot/aether#1264) plumbs the per-cap
//! `#[derive(aether_substrate::Config)]` against.
//!
//! # Preferred shape — `#[derive(aether_substrate::Config)]`
//!
//! Cap authors should reach for the derive on the resolved-config
//! struct rather than hand-writing the trio + impl:
//!
//! ```ignore
//! #[derive(Clone, Debug)]
//! #[cfg_attr(feature = "native", derive(aether_substrate::Config))]
//! #[cfg_attr(
//!     feature = "native",
//!     config(env_prefix = "AETHER_HTTP", cli_prefix = "http")
//! )]
//! pub struct HttpConfig {
//!     #[cfg_attr(feature = "native", config(default = false, parse = parse_flag))]
//!     pub disabled: bool,
//!     #[cfg_attr(
//!         feature = "native",
//!         config(default = 30_000, parse = parse_timeout_ms, ms_duration)
//!     )]
//!     pub default_timeout: Duration,
//! }
//! ```
//!
//! The derive emits the env-shaped `*Layer`, the clap-shaped
//! `*Overlay` (next to the domain struct in the cap crate; the
//! bundle's `cli.rs` `pub use`s them), the `FromArgvThenEnv` impl,
//! and inherent `from_env()` / `from_argv_then_env(argv)` shims. Per-
//! field hints (`default`, `parse`, `env`, `cli_long`, `ms_duration`,
//! `csv_set`, `layer_field`) cover the d-era wire shapes; the
//! container `skip_from_layer` opt-out lets a cap hand-write
//! `from_layer` when its defaults are runtime-computed (the
//! `NamespaceRoots` case).
//!
//! [`FromArgvThenEnv`] still exists as the underlying trait — the
//! derive emits an impl of it. Hand-written impls remain valid where
//! the derive doesn't fit.
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
//! kept hand-written. The five other caps benefit from the derive;
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
