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
//! #[cfg_attr(feature = "runtime", derive(aether_substrate::Config))]
//! #[cfg_attr(
//!     feature = "runtime",
//!     config(env_prefix = "AETHER_HTTP", cli_prefix = "http")
//! )]
//! pub struct HttpConfig {
//!     #[cfg_attr(feature = "runtime", config(default = false))]
//!     pub disabled: bool,
//!     #[cfg_attr(
//!         feature = "runtime",
//!         config(default = 30_000, ms_duration)
//!     )]
//!     pub default_timeout: Duration,
//! }
//! ```
//!
//! A numeric / `Duration` / `bool` field needs no parser: confique's
//! native env deserialization trims the value, treats an empty value as
//! unset (falling back to the `default`), accepts the usual bool
//! spellings (`1` / `true` / `yes` / `0` / `false` / `no`), and
//! hard-errors on a non-empty garbage value (ADR-0090 §4). Only a
//! genuinely-custom mapping names a `parse =` function.
//!
//! The derive emits the env-shaped `*Layer`, the clap-shaped
//! `*Overlay` (next to the domain struct in the cap crate; the
//! bundle's `cli.rs` `pub use`s them), the `FromArgvThenEnv` impl,
//! and inherent `from_env()` / `from_argv_then_env(argv)` shims. Per-
//! field hints (`default`, `parse`, `env`, `cli_long`, `ms_duration`,
//! `csv_set`, `nonzero`, `layer_field`) cover the wire shapes; the
//! container `skip_from_layer` opt-out lets a cap hand-write
//! `from_layer` when its defaults are runtime-computed (the
//! `NamespaceRoots` case). `csv_set` auto-wires
//! [`parse_csv_set`] on the env side; `nonzero` coerces a resolved `0`
//! to the field default in `from_layer`.
//!
//! [`FromArgvThenEnv`] still exists as the underlying trait — the
//! derive emits an impl of it. Hand-written impls remain valid where
//! the derive doesn't fit.
//!
//! # Enable / disable convention
//!
//! A capability that is off (or on) by default carries its on/off state
//! as a single config-API `bool` field. It is resolved like every other
//! knob — through the derive, with a literal `false` default — so the
//! decision flows from one documented `AETHER_…` key (or its CLI flag),
//! never from presence-inference (a bound address, a configured path) and
//! never from a raw `env::var` read of a key the config layer already
//! owns:
//!
//! ```ignore
//! #[cfg_attr(feature = "runtime", config(default = false))]
//! pub enabled: bool,
//! ```
//!
//! Polarity follows intent rather than a fixed keyword. An opt-in cap —
//! off until asked for — names the field `enabled`; an opt-out cap — on
//! until suppressed — names it `disabled`. Both default to `false`, so
//! the literal default always reads as the unsurprising state, and the
//! chassis maps the resolved `bool` to its structural choice at the one
//! composition site (`cfg.enabled.then_some(cfg)`). confique's native
//! bool deserialization accepts the usual `1` / `true` / `yes` / `0` /
//! `false` / `no` spellings (case-insensitive, trimmed).

mod dump;
mod error;
mod knobs;
mod known_keys;
mod manifest;
mod member;
mod resolve;
mod sources;
#[cfg(test)]
mod test_fixtures;

pub use dump::dump_config;
pub use error::ConfigError;
pub use knobs::{
    DEFAULT_REGISTRY_OWNER_QUEUE_CAPACITY, DEFAULT_REGISTRY_RELAY_QUEUE_CAPACITY, RegistryQueueCapacities,
    RingCapacities, SchedulerTuning,
};
pub use known_keys::{KnobKind, KnobRecord, KnownKeys, known_keys, validate_env};
pub use manifest::ConfigManifest;
pub use member::{ConfigMember, ConfigMemberRecord};
pub use resolve::{FromArgvThenEnv, file_section, parse_csv_set};
pub use sources::{ConfigProvenance, ConfigSources, StageArgv};
