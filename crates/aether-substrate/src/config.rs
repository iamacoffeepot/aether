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
//! #[cfg_attr(feature = "native", config(default = false, parse = parse_flag))]
//! pub enabled: bool,
//! ```
//!
//! Polarity follows intent rather than a fixed keyword. An opt-in cap —
//! off until asked for — names the field `enabled`; an opt-out cap — on
//! until suppressed — names it `disabled`. Both default to `false`, so
//! the literal default always reads as the unsurprising state, and the
//! chassis maps the resolved `bool` to its structural choice at the one
//! composition site (`cfg.enabled.then_some(cfg)`). The `parse_flag`
//! helper accepts the usual `1` / `true` / `yes` / `on` spellings.
//! `PersistConfig` is the documented exception below; every other cap
//! follows this shape.
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

use std::collections::HashSet;
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::fmt::Write as _;

use confique::meta::{Expr, Field, FieldKind, LeafKind, Meta};

use crate::BootError;

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
    fn try_from_argv_then_env(
        argv: <Self::Layer as confique::Config>::Layer,
    ) -> Result<Self, ConfigError> {
        let layer = <Self::Layer as confique::Config>::builder()
            .preloaded(argv)
            .env()
            .load()
            .map_err(ConfigError::from_confique)?;
        Ok(Self::from_layer(layer))
    }
}

/// Distinguishes a confique-backed knob (one carrying a
/// `Config::META` leaf with an env key) from a hand-registered
/// `OnceLock` knob (the scheduler hot-path tuning vars, registered via
/// [`KnobRecord`] because they have no `Meta`). ADR-0090 §1: the
/// `Meta` walk is the single source of truth for confique knobs;
/// `KnobRecord` only carries the ones with no `Meta`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnobKind {
    /// A knob declared as a `#[derive(Config)]` field — its env key
    /// and default come from the layer `Meta`. `KnobRecord` is used
    /// for these only when a caller wants a uniform record alongside
    /// hand-registered ones; the canonical source is the `Meta`.
    Confique,
    /// A knob read directly from a process-global `OnceLock`
    /// (`scheduler/worker_deque.rs`, `calibrate.rs`,
    /// `lifecycle/driver.rs`) — no `Config::META`, so it must be
    /// hand-registered to join the known-key set + the `--config`
    /// dump (ADR-0090 unit b2, iamacoffeepot/aether#1255).
    HandRegistered,
}

/// A uniform, hand-registered knob record. b2 builds a
/// `&[KnobRecord]` of the scheduler hot-path tuning knobs; e2's
/// `--config` dump renders them; e1's [`KnownKeys`] folds their
/// `env_key`s into the accepted set so the unknown-`AETHER_*` sweep
/// doesn't flag them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnobRecord {
    /// The `AETHER_*` (or bare) env var this knob reads.
    pub env_key: &'static str,
    /// One-line human/agent-facing description, lifted verbatim from
    /// the getter doc-comment.
    pub doc: &'static str,
    /// The literal default, if the knob has one. `None` for adaptive
    /// / unset knobs (`time_budget`, `wake_cost_nanos`) — rendered
    /// "derived/unset" by the dump.
    pub default: Option<&'static str>,
    /// Whether this is a confique-backed or hand-registered knob.
    pub kind: KnobKind,
}

/// The set of env keys some part of the substrate config surface
/// claims — every `AETHER_*` (or registered bare) key that resolves
/// to a real knob. [`validate_env`] warns on any `AETHER_*` env var
/// absent from this set. Assembled by [`known_keys`] from the migrated
/// `*Layer` metas plus the hand-registered [`KnobRecord`] slices.
#[derive(Clone, Debug, Default)]
pub struct KnownKeys {
    keys: HashSet<&'static str>,
}

impl KnownKeys {
    /// Whether `key` is a claimed env var.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.keys.contains(key)
    }

    /// Number of distinct claimed keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no key is claimed (only true for an empty assembly).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Iterate the claimed keys (order unspecified).
    pub fn iter(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.keys.iter().copied()
    }
}

/// Walk one `confique::meta::Meta`, collecting every leaf's env key
/// into `out` (recursing `Nested` metas). Iterative work-stack rather
/// than recursion (CLAUDE.md: load-bearing tree walks cap depth) — a
/// `Meta` tree is statically bounded, but the stack keeps it uniform.
fn collect_meta_env_keys(meta: &'static Meta, out: &mut HashSet<&'static str>) {
    let mut stack: Vec<&'static Meta> = vec![meta];
    while let Some(m) = stack.pop() {
        for field in m.fields {
            match &field.kind {
                FieldKind::Leaf { env: Some(key), .. } => {
                    out.insert(key);
                }
                FieldKind::Leaf { env: None, .. } => {}
                FieldKind::Nested { meta } => stack.push(meta),
            }
        }
    }
}

/// Assemble a [`KnownKeys`] from a slice of migrated `*Layer` metas
/// (one `&Meta` per `#[derive(Config)]` cap layer) plus a slice of
/// hand-registered [`KnobRecord`]s (b2's scheduler knobs). Walks each
/// `Meta` for `Leaf { env: Some(k) }` (recursing `Nested`) and folds
/// in each record's `env_key`.
#[must_use]
pub fn known_keys(metas: &[&'static Meta], records: &[KnobRecord]) -> KnownKeys {
    let mut keys = HashSet::new();
    for meta in metas {
        collect_meta_env_keys(meta, &mut keys);
    }
    for record in records {
        keys.insert(record.env_key);
    }
    KnownKeys { keys }
}

/// Render a `confique::meta::Expr` default as a plain string (matching
/// how it would be typed in env). Best-effort for the discovery dump;
/// composite defaults (`Array` / `Map`) render in a compact debug
/// shape since they have no single env representation.
fn render_expr(expr: &Expr) -> String {
    match expr {
        Expr::Str(s) => (*s).to_owned(),
        Expr::Float(fl) => fl.to_string(),
        Expr::Integer(i) => i.to_string(),
        Expr::Bool(b) => b.to_string(),
        Expr::Array(items) => {
            let inner: Vec<String> = items.iter().map(render_expr).collect();
            format!("[{}]", inner.join(","))
        }
        Expr::Map(_) => "<map>".to_owned(),
        // `Expr` is `#[non_exhaustive]` — any future variant renders
        // as a placeholder rather than failing the dump.
        _ => "<expr>".to_owned(),
    }
}

/// One resolved row in the [`dump_config`] table.
struct DumpRow {
    key: String,
    value: String,
    source: &'static str,
    default: String,
    doc: String,
}

/// Resolve one confique leaf's discovery row: read the live env value
/// (the value the running config would resolve to) and label its
/// source as `env` (set) or `default` (unset).
fn leaf_row(env_key: &str, leaf: &LeafKind, doc: &[&'static str]) -> DumpRow {
    let default = match leaf {
        LeafKind::Required {
            default: Some(expr),
        } => render_expr(expr),
        LeafKind::Required { default: None } | LeafKind::Optional => String::new(),
    };
    let (value, source) =
        env::var(env_key).map_or_else(|_| (default.clone(), "default"), |v| (v, "env"));
    DumpRow {
        key: env_key.to_owned(),
        value,
        source,
        default,
        doc: doc.join(" ").trim().to_owned(),
    }
}

/// Walk one `Meta` into `rows`, resolving every leaf's discovery row
/// (recursing `Nested`). Iterative work-stack, same shape as
/// [`collect_meta_env_keys`].
fn collect_meta_rows(meta: &'static Meta, rows: &mut Vec<DumpRow>) {
    let mut stack: Vec<&'static Meta> = vec![meta];
    while let Some(m) = stack.pop() {
        for field in m.fields {
            let Field { doc, kind, .. } = field;
            match kind {
                FieldKind::Leaf {
                    env: Some(key),
                    kind: leaf,
                } => rows.push(leaf_row(key, leaf, doc)),
                FieldKind::Leaf { env: None, .. } => {}
                FieldKind::Nested { meta } => stack.push(meta),
            }
        }
    }
}

/// Render the `--config` discovery dump (ADR-0090 §4): walk the same
/// `Meta`-slice + `KnobRecord`-slice registry e1 assembles and e2
/// reads, printing every knob with its live source-resolved value,
/// source label (`env` / `default`), default, and doc. Confique knobs
/// come from the `Meta` walk (the single source of truth — no second
/// hand-maintained list); hand-registered knobs render their
/// `KnobRecord` directly (`source` = `env` when the var is set, else
/// `unregistered-default` since their default lives only in the
/// record). Output is a stable plaintext table.
#[must_use]
pub fn dump_config(metas: &[&'static Meta], records: &[KnobRecord]) -> String {
    let mut rows: Vec<DumpRow> = Vec::new();
    for meta in metas {
        collect_meta_rows(meta, &mut rows);
    }
    for record in records {
        let default = record.default.unwrap_or("").to_owned();
        let (value, source) =
            env::var(record.env_key).map_or_else(|_| (default.clone(), "default"), |v| (v, "env"));
        rows.push(DumpRow {
            key: record.env_key.to_owned(),
            value,
            source,
            default,
            doc: record.doc.to_owned(),
        });
    }
    rows.sort_by(|a, b| a.key.cmp(&b.key));

    let key_w = rows.iter().map(|r| r.key.len()).max().unwrap_or(3).max(3);
    let val_w = rows.iter().map(|r| r.value.len()).max().unwrap_or(5).max(5);
    let src_w = 7; // "default" is the widest source label
    let def_w = rows
        .iter()
        .map(|r| r.default.len())
        .max()
        .unwrap_or(7)
        .max(7);

    let mut out = String::new();
    let (k, v, s, d, doc) = ("KEY", "VALUE", "SOURCE", "DEFAULT", "DOC");
    let _ = writeln!(
        out,
        "{k:<key_w$}  {v:<val_w$}  {s:<src_w$}  {d:<def_w$}  {doc}"
    );
    for r in &rows {
        let (key, value, source, default, doc) = (&r.key, &r.value, r.source, &r.default, &r.doc);
        let _ = writeln!(
            out,
            "{key:<key_w$}  {value:<val_w$}  {source:<src_w$}  {default:<def_w$}  {doc}"
        );
    }
    out
}

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
}

impl ConfigError {
    /// Wrap a `confique::Error` (always an env-parse failure on the
    /// load path — defaults are validated by the cap `*_defaults_match`
    /// tests). The confique error's `Display` already names the field,
    /// key, and value.
    #[must_use]
    pub fn from_confique(err: confique::Error) -> Self {
        Self::UnparseableKnown {
            key: String::new(),
            value: None,
            source: Box::new(err),
        }
    }

    /// Build an `UnparseableKnown` from a hand-resolved env read (the
    /// handle-store `AETHER_HANDLE_STORE_MAX_BYTES` path, which parses
    /// outside confique).
    #[must_use]
    pub fn unparseable(
        key: impl Into<String>,
        value: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::UnparseableKnown {
            key: key.into(),
            value: Some(value.into()),
            source: Box::new(source),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnparseableKnown { key, value, source } => {
                if key.is_empty() {
                    write!(f, "unparseable config value: {source}")
                } else if let Some(value) = value {
                    write!(
                        f,
                        "unparseable value {value:?} for known config key {key:?}: {source}"
                    )
                } else {
                    write!(
                        f,
                        "unparseable value for known config key {key:?}: {source}"
                    )
                }
            }
        }
    }
}

impl StdError for ConfigError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::UnparseableKnown { source, .. } => Some(&**source),
        }
    }
}

impl From<ConfigError> for BootError {
    fn from(e: ConfigError) -> Self {
        Self::Other(Box::new(e))
    }
}

/// Validate the process environment against the claimed key set
/// (ADR-0090 §4). Warns (does not error) on any `AETHER_*` env var
/// not in `known` — a typo or stray var is loud but non-fatal (§4
/// rejects strict-reject: a stray CI var must not abort boot). The
/// hard-error half rides the parse path
/// ([`FromArgvThenEnv::try_from_argv_then_env`] /
/// [`HandleStore::from_env`](crate::handle_store::HandleStore::from_env)),
/// not this sweep. Run once per chassis boot after the env layers
/// load.
///
/// Bare registered keys (e.g. `GEMINI_API_KEY`, `ANTHROPIC_API_KEY`)
/// that don't carry the `AETHER_` prefix are accepted silently when
/// present in `known`; only `AETHER_*` keys are *swept* for unknowns,
/// because the substrate doesn't own the whole bare-env namespace.
///
/// # Errors
///
/// Never returns `Err` today — the signature returns
/// `Result<(), ConfigError>` so the hard-error half can join this
/// pass without a call-site change if §4 evolves.
pub fn validate_env(known: &KnownKeys) -> Result<(), ConfigError> {
    for (key, _value) in env::vars() {
        if key.starts_with("AETHER_") && !known.contains(key.as_str()) {
            tracing::warn!(
                target: "aether_substrate::config",
                env = %key,
                "unknown AETHER_ env var — not claimed by any registered config knob \
                 (typo? stale export?); ignored",
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use confique::Config as _;
    use std::num::ParseIntError;

    // Plain `#[derive(confique::Config)]` fixture (not the aether
    // `Config` derive — that emits a clap `Overlay` whose `#[arg]`
    // attrs need clap in scope, which `aether-substrate` doesn't
    // carry). This gives a real `META` + `Layer` to walk; the strict
    // `parse_env` reproduces the ADR-0090 §4 hard-error path.
    #[derive(Clone, Debug, confique::Config)]
    #[allow(dead_code)] // fields exercised via META / load, not read directly
    struct FixtureConfig {
        #[config(env = "AETHER_TEST_COUNT", parse_env = parse_count, default = 7)]
        count: u32,
        #[config(env = "AETHER_TEST_FLAG", default = false)]
        enabled: bool,
    }

    fn parse_count(raw: &str) -> Result<u32, ParseIntError> {
        raw.trim().parse()
    }

    /// A real `ParseIntError` for the `ConfigError` constructor tests
    /// (clippy forbids `unwrap_err()` — a parse of a non-number is the
    /// honest way to obtain one).
    fn an_int_error() -> ParseIntError {
        match "x".parse::<u32>() {
            Ok(_) => unreachable!("\"x\" is not a u32"),
            Err(e) => e,
        }
    }

    const FIXTURE_KNOBS: &[KnobRecord] = &[KnobRecord {
        env_key: "AETHER_FIXTURE_KNOB",
        doc: "a hand-registered fixture knob",
        default: Some("42"),
        kind: KnobKind::HandRegistered,
    }];

    fn fixture_meta() -> &'static Meta {
        &<FixtureConfig as confique::Config>::META
    }

    #[test]
    fn known_keys_collects_meta_env_keys() {
        let known = known_keys(&[fixture_meta()], &[]);
        assert!(known.contains("AETHER_TEST_COUNT"));
        assert!(known.contains("AETHER_TEST_FLAG"));
        assert_eq!(known.len(), 2);
    }

    #[test]
    fn known_keys_folds_in_hand_registered_records() {
        let known = known_keys(&[fixture_meta()], FIXTURE_KNOBS);
        assert!(known.contains("AETHER_FIXTURE_KNOB"));
        assert!(known.contains("AETHER_TEST_COUNT"));
        assert_eq!(known.len(), 3);
    }

    #[test]
    fn known_keys_rejects_unclaimed() {
        let known = known_keys(&[fixture_meta()], FIXTURE_KNOBS);
        assert!(!known.contains("AETHER_TYPO"));
    }

    #[test]
    fn dump_config_renders_meta_keys_defaults_and_docs() {
        let dump = dump_config(&[fixture_meta()], FIXTURE_KNOBS);
        // Confique knob from the Meta walk: key + default + a header.
        assert!(dump.contains("AETHER_TEST_COUNT"));
        assert!(dump.contains('7')); // the count default
        assert!(dump.contains("KEY"));
        assert!(dump.contains("SOURCE"));
        // Hand-registered knob rendered directly.
        assert!(dump.contains("AETHER_FIXTURE_KNOB"));
        assert!(dump.contains("a hand-registered fixture knob"));
    }

    #[test]
    fn dump_config_labels_env_set_value_as_env_source() {
        // SAFETY: single-threaded test; unique key set then removed.
        unsafe { env::set_var("AETHER_FIXTURE_KNOB", "99") };
        let dump = dump_config(&[], FIXTURE_KNOBS);
        // SAFETY: same scope.
        unsafe { env::remove_var("AETHER_FIXTURE_KNOB") };
        let row = dump
            .lines()
            .find(|l| l.contains("AETHER_FIXTURE_KNOB"))
            .expect("knob row present");
        assert!(
            row.contains("99"),
            "value should be the env override: {row}"
        );
        assert!(row.contains("env"), "source should be env: {row}");
    }

    #[test]
    fn validate_env_is_ok_with_empty_known_set() {
        // No assertion on the warn output (it depends on ambient env);
        // the contract is just "never errors on unknowns".
        assert!(validate_env(&KnownKeys::default()).is_ok());
    }

    #[test]
    fn config_error_display_names_key_and_value() {
        let e = ConfigError::unparseable("AETHER_HANDLE_STORE_MAX_BYTES", "lots", an_int_error());
        let msg = e.to_string();
        assert!(msg.contains("AETHER_HANDLE_STORE_MAX_BYTES"));
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
