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

use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::collections::HashSet;
use std::convert::Infallible;
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;

use aether_actor::log::DEFAULT_RING_CAP;
use aether_actor::trace::{DEFAULT_TRACE_RING_CAP, DEFAULT_TRACE_RING_MAX_CAP};
use confique::meta::{Expr, Field, FieldKind, LeafKind, Meta};

use crate::BootError;

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

/// The two per-actor ring capacities resolved once at chassis boot and
/// threaded down the spawn path (ADR-0081 log ring + ADR-0086 trace
/// ring). `Copy` so it rides every `Spawner` / builder seam as an
/// ordinary value — no process-global, no atomics. The chassis-bin
/// `ActorRingConfig` derive-`Config` knob lowers to this; substrate-core
/// never reads env (issue 464), so the resolution lives bundle-side and
/// only the resolved capacities reach here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingCapacities {
    /// Per-actor [`ActorLogRing`](aether_actor::log::ActorLogRing)
    /// capacity (env `AETHER_ACTOR_LOG_RING_SIZE`; default
    /// [`DEFAULT_RING_CAP`]).
    pub log: usize,
    /// Per-actor [`ActorTraceRing`](aether_actor::trace::ActorTraceRing)
    /// and chassis-host-ring *floor* capacity — the size each ring starts
    /// at (env `AETHER_ACTOR_TRACE_RING_SIZE`; default
    /// [`DEFAULT_TRACE_RING_CAP`]).
    pub trace: usize,
    /// Ceiling a saturating trace ring grows to before it resumes
    /// drop-oldest (env `AETHER_ACTOR_TRACE_RING_MAX_SIZE`; default
    /// [`DEFAULT_TRACE_RING_MAX_CAP`]). The trace ring grows geometrically
    /// from [`trace`](Self::trace) toward this; the log ring has no such
    /// ceiling (drop-oldest is its intended semantic).
    pub trace_max: usize,
}

impl Default for RingCapacities {
    fn default() -> Self {
        Self { log: DEFAULT_RING_CAP, trace: DEFAULT_TRACE_RING_CAP, trace_max: DEFAULT_TRACE_RING_MAX_CAP }
    }
}

/// The nine scheduler hot-path tuning knobs resolved once at chassis boot
/// and installed into the scheduler's process-global before the pool
/// starts (`crate::scheduler::install_tuning`). `Copy` so it rides the
/// builder seam as an ordinary value; the deep hot-path getters (the
/// worker loop, the blob-flush recruiter, the handoff-EWMA seed) read the
/// installed value rather than env. The chassis-bin `SchedulerTuningConfig`
/// derive-`Config` knob lowers to this; substrate-core never reads env
/// (issue 464), so the resolution lives bundle-side and only the resolved
/// values reach here.
///
/// Six knobs carry concrete defaults; the three adaptive knobs
/// ([`time_budget_micros`](Self::time_budget_micros),
/// [`handoff_cost_nanos`](Self::handoff_cost_nanos),
/// [`wake_cost_nanos`](Self::wake_cost_nanos)) are `Option` — `None`
/// selects the measured/derived behaviour, `Some` pins the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulerTuning {
    /// Route-to-spinner spin-window (microseconds) before a worker parks
    /// (env `AETHER_SPIN_WINDOW_USEC`; default `50`).
    pub spin_window_micros: u64,
    /// Deque-length backstop: max slots a worker keeps on its own deque
    /// before forcing a spill (env `AETHER_LOCAL_STICKY_MAX`; default
    /// `256`).
    pub local_sticky_max: usize,
    /// Keep-local time valve (microseconds): `Some` pins/disables the
    /// burst spill valve (`0` disables it), `None` derives it from the
    /// measured handoff cost (env `AETHER_LOCAL_TIME_BUDGET_US`; default
    /// `None`).
    pub time_budget_micros: Option<u64>,
    /// Whether idle workers may raid siblings' deques (peer-deque
    /// stealing); default owner-only (env `AETHER_PEER_STEAL`; default
    /// `false`).
    pub peer_steal: bool,
    /// Every-K injector backstop for keep-local chains (env
    /// `AETHER_LOCAL_CHAIN_BACKSTOP`; default `64`).
    pub local_chain_backstop: u32,
    /// Pins the cross-worker handoff-cost estimate (nanoseconds) and
    /// freezes live refinement; `None` boot-probes and live-refines (env
    /// `AETHER_HANDOFF_COST_NS`; default `None`).
    pub handoff_cost_nanos: Option<u64>,
    /// Minimum fresh-group count for a flush to broadcast-recruit siblings
    /// (env `AETHER_BLOB_RECRUIT_MIN`; default `9`).
    pub blob_recruit_min: usize,
    /// Cap on the number of sibling copies a single flush injects when
    /// recruiting (env `AETHER_BLOB_RECRUIT_MAX`; default `32`).
    pub blob_recruit_max: usize,
    /// Pins the recruit wake break-even (nanoseconds) and freezes live
    /// refinement; `None` uses the box-measured handoff cost (env
    /// `AETHER_WAKE_COST_NANOS`; default `None`).
    pub wake_cost_nanos: Option<u64>,
}

impl Default for SchedulerTuning {
    fn default() -> Self {
        Self {
            spin_window_micros: 50,
            local_sticky_max: 256,
            time_budget_micros: None,
            peer_steal: false,
            local_chain_backstop: 64,
            handoff_cost_nanos: None,
            blob_recruit_min: 9,
            blob_recruit_max: 32,
            wake_cost_nanos: None,
        }
    }
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
    /// [`ConfigSources::hermetic`], which `SubstrateHarness` uses so a member it
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
        LeafKind::Required { default: Some(expr) } => render_expr(expr),
        LeafKind::Required { default: None } | LeafKind::Optional => String::new(),
    };
    // The sanctioned ADR-0090 config machinery: this is the central env-read
    // the whole derive-`Config` system funnels through (the `#[config(env =
    // ...)]` discovery dump), the one place capabilities *should* configure
    // through rather than reading env themselves.
    #[allow(clippy::disallowed_methods)]
    let (value, source) = env::var(env_key).map_or_else(|_| (default.clone(), "default"), |v| (v, "env"));
    DumpRow { key: env_key.to_owned(), value, source, default, doc: doc.join(" ").trim().to_owned() }
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
                FieldKind::Leaf { env: Some(key), kind: leaf } => rows.push(leaf_row(key, leaf, doc)),
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
        // The sanctioned ADR-0090 config machinery: resolving a hand-registered
        // knob's live source for the `--config` discovery dump, the central
        // config-read path, not a cap reading its own env.
        #[allow(clippy::disallowed_methods)]
        let (value, source) = env::var(record.env_key).map_or_else(|_| (default.clone(), "default"), |v| (v, "env"));
        rows.push(DumpRow { key: record.env_key.to_owned(), value, source, default, doc: record.doc.to_owned() });
    }
    rows.sort_by(|a, b| a.key.cmp(&b.key));

    let key_w = rows.iter().map(|r| r.key.len()).max().unwrap_or(3).max(3);
    let val_w = rows.iter().map(|r| r.value.len()).max().unwrap_or(5).max(5);
    let src_w = 7; // "default" is the widest source label
    let def_w = rows.iter().map(|r| r.default.len()).max().unwrap_or(7).max(7);

    let mut out = String::new();
    let (k, v, s, d, doc) = ("KEY", "VALUE", "SOURCE", "DEFAULT", "DOC");
    let _ = writeln!(out, "{k:<key_w$}  {v:<val_w$}  {s:<src_w$}  {d:<def_w$}  {doc}");
    for r in &rows {
        let (key, value, source, default, doc) = (&r.key, &r.value, r.source, &r.default, &r.doc);
        let _ = writeln!(out, "{key:<key_w$}  {value:<val_w$}  {source:<src_w$}  {default:<def_w$}  {doc}");
    }
    out
}

/// One config member's compose-stage declaration (ADR-0156 §4): the TOML
/// config-file section it reads and the confique [`Meta`] carrying its
/// `AETHER_*` keys, defaults, and docs. Both fields are `'static`, so the
/// record is `Copy` and rides the [`Builder`](crate::chassis::builder::Builder)'s
/// accumulator as an ordinary value.
#[derive(Clone, Copy, Debug)]
pub struct ConfigMemberRecord {
    /// The `[section]` name this member reads from the sectioned TOML
    /// chassis config file (e.g. `"http"`, `"scheduler"`). Declared on the
    /// config type via the derive's `#[config(section = "...")]` (defaulting
    /// to `cli_prefix` where they align).
    pub section: &'static str,
    /// The confique layer `Meta` — the walk reads its leaves for the env
    /// keys, defaults, and docs.
    pub meta: &'static Meta,
    /// The config type's [`TypeId`] (ADR-0156 §5). Lets
    /// [`ConfigManifest::provenance`] correlate a member with the
    /// [`ConfigSources`] stack — the programmatic and argv layers are keyed by
    /// config `TypeId` — so the manifest can report which layer supplied each
    /// resolved member.
    pub type_id: TypeId,
}

/// ADR-0156 §4 member trait: a config type's contribution to the
/// composition-derived chassis config aggregate. `#[derive(aether_substrate::Config)]`
/// emits the impl (one member carrying the type's section + `META`). The
/// composition boundary ([`Builder::with_actor`](crate::chassis::builder::Builder::with_actor))
/// bounds a cap's `Config` by this trait, so a cap that smuggles construction
/// wiring into `Config` (a live handle, a resolved `MailboxId`) — a type the
/// derive can't apply to — stops compiling at the compose site rather than
/// drifting silently out of the aggregate.
///
/// [`members`](Self::members) is a **required method with no default** — a
/// blanket empty default would make the bound vacuous (any `impl ConfigMember
/// for T {}` one-liner would satisfy it, re-opening the exact wiring-in-`Config`
/// escape hatch the bound exists to close). Rust cannot seal a trait a derive
/// must implement downstream, so the required-method shape is the enforcement:
/// after #3849 the only hand-written impl in the workspace is `()` below
/// (`RpcServerConfig`'s pre-#3849 programmatic bridge retired — its port now
/// resolves through the source stack via the derive), everything else is
/// derive-emitted or moved its wiring to `Params` (`Config = ()`).
pub trait ConfigMember {
    /// The member records this config declares. A derive-`Config` type returns
    /// its one section + `META` record; the sanctioned `()` hand impl returns
    /// empty.
    #[must_use]
    fn members() -> Vec<ConfigMemberRecord>;

    /// ADR-0156 §5: resolve this member's value from the builder's source
    /// stack — programmatic > argv > env > file > default. The composition
    /// boundary calls this once per composed member ahead of `init`, so the
    /// builder (not the chassis) owns resolution: section identity comes from
    /// the derive declaration (never a chassis-side string), and the layer
    /// precedence is byte-identical to the pre-inversion per-cap
    /// `resolve_with_file` path. A derive-`Config` type delegates to
    /// [`ConfigSources::resolve_layered`] with its own section; the sanctioned
    /// `()` hand impl resolves trivially.
    ///
    /// A **required method with no default** for the same reason as
    /// [`members`](Self::members): a blanket default would re-open the
    /// wiring-in-`Config` escape hatch the required-method shape closes.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a known env key, argv overlay value, or
    /// config-file section holds an unparseable value (ADR-0090 §4 — the
    /// hard-error half stays a hard boot error).
    fn resolve(sources: &mut ConfigSources) -> Result<Self, ConfigError>
    where
        Self: Sized;
}

/// The configless-cap case: a cap whose `Config = ()` declares no aggregate
/// member. The one sanctioned hand impl (after #3849 retired the
/// `RpcServerConfig` bridge, it is the only one); every other non-derive
/// `Config` moved its wiring onto the `Params` channel rather than stamp an
/// empty member impl here.
impl ConfigMember for () {
    fn members() -> Vec<ConfigMemberRecord> {
        Vec::new()
    }

    fn resolve(_sources: &mut ConfigSources) -> Result<Self, ConfigError> {
        Ok(())
    }
}

/// ADR-0156 §5 (issue 3872): an argv overlay's own knowledge of which config
/// domain it stages onto the source stack. Closes the last hand-maintained edge
/// of the chassis config flow — the per-cap
/// `sources.set_argv::<HttpConfig>(http.into_layer())` block each chassis used
/// to assemble by hand (and could silently forget, discarding the operator's
/// flag).
///
/// Two derives emit the impls, so staging is derived from the declarations that
/// already exist:
///
/// - `#[derive(aether_substrate::Config)]` emits the **leaf** impl on each cap's
///   `*Overlay`, calling [`ConfigSources::set_argv`] with the domain type.
/// - `#[derive(aether_substrate::StageArgv)]` emits the **container** impl on
///   each hand-written chassis CLI root ([`crate::config`] docs), delegating to
///   every field's [`stage`](Self::stage). A field that is not stageable must
///   carry an explicit `#[stage(skip)]` or fail to compile — the hole cannot
///   reopen through the derive itself.
///
/// A chassis then stages its whole CLI in one `cli.stage(&mut sources)` call:
/// adding an overlay field to a root IS staging it, and the converse
/// [`ConfigSources::validate_no_orphan_argv`] tripwire makes a
/// staged-but-never-composed layer a hard boot error.
pub trait StageArgv {
    /// Stage this overlay's argv layer(s) onto `sources`. A leaf overlay stages
    /// its own member (`sources.set_argv::<Domain>(self.into_layer())`); a
    /// container delegates to each of its fields' `stage`.
    fn stage(self, sources: &mut ConfigSources);
}

/// The composition-derived chassis config aggregate (ADR-0156 §4): the
/// union of every composed cap's [`ConfigMember`] declaration, the driver's,
/// and the chassis-declared non-cap members (workers / ring capacities /
/// scheduler tuning / teardown budget), assembled by
/// [`Builder::config_manifest`](crate::chassis::builder::Builder::config_manifest).
/// The known-keys sweep and `--print-config` dump read this walk instead of a
/// hand-maintained registry, so a chassis knows exactly the knobs it composes
/// — headless stops "knowing" the window/audio knobs it never wires, desktop
/// stops "knowing" the headless tick knob.
#[derive(Clone, Debug, Default)]
pub struct ConfigManifest {
    members: Vec<ConfigMemberRecord>,
}

impl ConfigManifest {
    /// Assemble a manifest from a member list (the builder's accumulator
    /// plus the driver's members).
    #[must_use]
    pub fn from_members(members: Vec<ConfigMemberRecord>) -> Self {
        Self { members }
    }

    /// The declared members, in composition order.
    #[must_use]
    pub fn members(&self) -> &[ConfigMemberRecord] {
        &self.members
    }

    /// Every member's confique `Meta`, for the [`known_keys`] / [`dump_config`]
    /// walks.
    #[must_use]
    pub fn metas(&self) -> Vec<&'static Meta> {
        self.members.iter().map(|record| record.meta).collect()
    }

    /// The file-section vocabulary the composed members declare.
    pub fn sections(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.members.iter().map(|record| record.section)
    }

    /// Assemble the [`KnownKeys`] set for the unknown-`AETHER_*` sweep from
    /// this walk plus the residual hand-registered knobs (the chassis-direct
    /// records the aggregate doesn't yet own — the RPC port, frame size,
    /// runtime log/panic knobs; ADR-0156 §6 folds those in on later slices).
    #[must_use]
    pub fn known_keys(&self, records: &[KnobRecord]) -> KnownKeys {
        known_keys(&self.metas(), records)
    }

    /// Render the `--print-config` discovery dump from this walk plus the
    /// residual hand-registered knobs.
    #[must_use]
    pub fn dump(&self, records: &[KnobRecord]) -> String {
        dump_config(&self.metas(), records)
    }

    /// ADR-0156 §5: the resolved-provenance rollup — for every declared member,
    /// the highest-precedence source layer present in `sources` (which
    /// programmatic / argv / file / env / default supplied its value). Sibling
    /// to [`dump`](Self::dump)'s per-key value table: `dump` renders each
    /// knob's live value, this attributes each member to its winning layer.
    ///
    /// Reads the same stack the builder resolves cap configs against, so the
    /// rollup reflects exactly what a boot would resolve. The member's section
    /// pairs with the provenance so callers can render a `section = layer`
    /// line.
    #[must_use]
    pub fn provenance(&self, sources: &ConfigSources) -> Vec<(&'static str, ConfigProvenance)> {
        self.members
            .iter()
            .map(|record| (record.section, sources.provenance(record.type_id, record.section, record.meta)))
            .collect()
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

/// Which layer of the source stack supplied a resolved member's value
/// (ADR-0156 §5). Reported per member by [`ConfigManifest::provenance`] — the
/// highest-precedence layer present in the stack for that member, in the
/// resolution order programmatic > argv > file > env > default.
///
/// Precision note: [`Programmatic`](Self::Programmatic), [`File`](Self::File),
/// [`Env`](Self::Env), and [`Default`](Self::Default) are detected exactly (an
/// override in the stack, a present file section, a set env key, else the
/// literal default). [`Argv`](Self::Argv) is reported when a non-empty argv
/// overlay is staged for the member; an empty overlay (no flag passed) falls
/// through to the lower layers, matching how resolution treats it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigProvenance {
    /// A programmatic explicit value (`Builder::with_actor_configured`) — the
    /// top layer, how the harnesses and tests construct configs in code.
    Programmatic,
    /// A non-empty argv overlay staged for this member.
    Argv,
    /// A present `[section]` in the loaded chassis config file.
    File,
    /// At least one of the member's `AETHER_*` env keys is set in the process
    /// environment.
    Env,
    /// No source supplied a value — the member resolves to its literal
    /// defaults.
    Default,
}

impl fmt::Display for ConfigProvenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Programmatic => "programmatic",
            Self::Argv => "argv",
            Self::File => "file",
            Self::Env => "env",
            Self::Default => "default",
        };
        f.write_str(label)
    }
}

/// The builder's config source stack (ADR-0156 §5): the layers below `default`
/// that the composition boundary resolves each member against, in precedence
/// programmatic > argv > env > file > default. Assembled adjacent to
/// composition — the chassis loads the config file and stages each member's
/// argv overlay + any programmatic override — and handed to the builder, which
/// resolves every composed member off it ahead of `init`. Section identity
/// comes from each member's [`ConfigMember`] declaration, so no chassis-side
/// section string survives.
///
/// The programmatic layer is an explicit-value layer *inside* the stack rather
/// than a bypass around it: `SubstrateHarness` and unit tests stage config
/// values here (via `Builder::with_actor_configured`, which composes the actor
/// and stages its value together) and resolution short-circuits to them, so an
/// in-code construction never reads process env.
#[derive(Default)]
pub struct ConfigSources {
    file: Option<toml::Table>,
    /// ADR-0156 §5: when set, resolution runs the [hermetic](Self::hermetic)
    /// path — no env layer (and no file, forced `None` by the constructor), so
    /// a composed-but-unstaged member falls through to its compiled default
    /// rather than a stray process env var. `SubstrateHarness` sets it.
    hermetic: bool,
    /// Programmatic explicit values keyed by config `TypeId`. Boxed `dyn Any`
    /// because members are heterogeneously typed. Assembled and consumed on the
    /// one chassis-boot thread — no `Send`, matching the `Builder` itself
    /// (whose `!Send` driver-boot closure already keeps it thread-local). The
    /// parallel `override_names` keeps each override's `type_name` for the
    /// staged-but-never-composed boot error.
    overrides: HashMap<TypeId, Box<dyn Any>>,
    override_names: HashMap<TypeId, &'static str>,
    /// Per-member argv overlay layers keyed by config `TypeId`. Each holds the
    /// member's `<C::Layer as confique::Config>::Layer` produced by its
    /// `Overlay::into_layer` (a confique partial layer, not necessarily
    /// `Send`). The parallel `argv_names` keeps each staged layer's `type_name`
    /// for the staged-but-never-composed boot error, mirroring
    /// `override_names`; both are kept in lockstep by `set_argv` / `take_argv`,
    /// so a layer still present after resolution names its orphaned config type.
    argv: HashMap<TypeId, Box<dyn Any>>,
    argv_names: HashMap<TypeId, &'static str>,
}

impl ConfigSources {
    /// A source stack over the optional loaded chassis config file, with no
    /// argv overlays or programmatic overrides staged yet. Resolution runs the
    /// full stack — programmatic > argv > env > file > default.
    #[must_use]
    pub fn new(file: Option<toml::Table>) -> Self {
        Self {
            file,
            hermetic: false,
            overrides: HashMap::new(),
            override_names: HashMap::new(),
            argv: HashMap::new(),
            argv_names: HashMap::new(),
        }
    }

    /// ADR-0156 §5: a **hermetic** source stack — no env layer, no file layer.
    /// Resolution collapses to programmatic > argv > default; the process
    /// environment is never read. `SubstrateHarness` uses this so a member it
    /// composes but forgets to stage falls through to its compiled default
    /// (deterministic) rather than leaking a process env var into a test
    /// (before this inversion, harness-composed members never read env — their
    /// values were constructed directly — and this preserves that).
    #[must_use]
    pub fn hermetic() -> Self {
        Self {
            file: None,
            hermetic: true,
            overrides: HashMap::new(),
            override_names: HashMap::new(),
            argv: HashMap::new(),
            argv_names: HashMap::new(),
        }
    }

    /// Stage a programmatic explicit value for member `C` — the top layer of
    /// the stack. The internal mechanism `Builder::with_actor_configured` rides
    /// (which also composes the actor, so the override is never orphaned); a
    /// chassis test may also call it directly on a `ConfigSources` handed over
    /// via `with_config_sources`.
    pub fn set_override<C: 'static>(&mut self, value: C) {
        self.overrides.insert(TypeId::of::<C>(), Box::new(value));
        self.override_names.insert(TypeId::of::<C>(), type_name::<C>());
    }

    /// Stage member `C`'s argv overlay layer (its `Overlay::into_layer`
    /// output) — the typed argv handoff the chassis makes adjacent to
    /// composition, folded into the bulk stack that rides `Builder::with_config_sources`.
    pub fn set_argv<C: FromArgvThenEnv + 'static>(&mut self, layer: <C::Layer as confique::Config>::Layer) {
        self.argv.insert(TypeId::of::<C>(), Box::new(layer));
        self.argv_names.insert(TypeId::of::<C>(), type_name::<C>());
    }

    /// Resolve member `C` off the stack. Sugar for
    /// [`ConfigMember::resolve`] so a chassis can read a driver-only or
    /// chassis-declared member's resolved value (tick period, window mode,
    /// workers, ring capacities) from the same stack the builder resolves cap
    /// configs against — one resolution path, no chassis-side sections.
    ///
    /// # Errors
    ///
    /// Propagates [`ConfigError`] from the member's resolution.
    pub fn resolve<C: ConfigMember>(&mut self) -> Result<C, ConfigError> {
        C::resolve(self)
    }

    fn take_override<C: 'static>(&mut self) -> Option<C> {
        self.overrides.remove(&TypeId::of::<C>()).map(|boxed| *boxed.downcast::<C>().expect("override TypeId keys C"))
    }

    fn take_argv<C: FromArgvThenEnv + 'static>(&mut self) -> Option<<C::Layer as confique::Config>::Layer> {
        // Drop the name in lockstep with the layer so the orphan-argv tripwire
        // only ever sees layers no member consumed.
        self.argv_names.remove(&TypeId::of::<C>());
        self.argv.remove(&TypeId::of::<C>()).map(|boxed| *boxed.downcast().expect("argv-layer TypeId keys C"))
    }

    /// The derive-`Config` resolution path: programmatic override, else the
    /// staged argv overlay atop env atop the named file section atop the
    /// literal defaults. The derive-emitted [`ConfigMember::resolve`] calls
    /// this with the type's own section, so precedence is byte-identical to
    /// the pre-inversion `resolve_with_file::<C>(argv, file, section)`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the file section or a known env/argv value
    /// is malformed.
    pub fn resolve_layered<C: FromArgvThenEnv + 'static>(&mut self, section: &str) -> Result<C, ConfigError> {
        use confique::Layer as _;
        // Consume the staged argv layer unconditionally — even when a
        // programmatic override outranks it — so the orphan-argv tripwire
        // (which keys off layers left behind) never mistakes a legitimately
        // shadowed layer for an orphan. The override still wins; the argv value
        // is dropped, exactly as it was when the override short-circuited before.
        let argv = self.take_argv::<C>();
        if let Some(value) = self.take_override::<C>() {
            return Ok(value);
        }
        let argv = argv.unwrap_or_else(<<C::Layer as confique::Config>::Layer>::empty);
        // Hermetic mode (`SubstrateHarness`) skips both the env and the file
        // layers, so an unstaged member resolves to its compiled default.
        if self.hermetic {
            return C::try_resolve_hermetic(argv, None);
        }
        let file = match &self.file {
            Some(table) => file_section::<C>(table, section)?,
            None => None,
        };
        C::try_resolve(argv, file)
    }

    /// ADR-0156 §5: reject any staged programmatic override whose config type is
    /// not in `composed` — the set of `TypeId`s of every composed member's
    /// `Config` (each `with_actor`'s `A::Config`, each `declare_config_member`, and
    /// the driver's members). A typo'd or orphaned `with_config::<T>(value)` —
    /// staged but matching no composed member — is a hard boot error naming `T`
    /// rather than a value silently left behind. Run at build / claim time,
    /// before resolution.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::OrphanOverride`] naming the first override type
    /// that matches no composed member.
    pub fn validate_overrides(&self, composed: &HashSet<TypeId>) -> Result<(), ConfigError> {
        for (type_id, name) in &self.override_names {
            if !composed.contains(type_id) {
                return Err(ConfigError::OrphanOverride { type_name: (*name).to_owned() });
            }
        }
        Ok(())
    }

    /// ADR-0156 §5 converse tripwire (issue 3872): reject any staged argv layer
    /// that no composed member consumed. The resolve path removes each layer as
    /// its member resolves (`take_argv`), so a layer still present after boot
    /// resolution was staged for a config type no composed member resolves — a
    /// typo'd or orphaned `set_argv` (or, now, a `stage` delegating to a field
    /// whose overlay was flattened into a root the chassis never composes). The
    /// argv analogue of [`Self::validate_overrides`], failing as loudly rather
    /// than silently discarding the operator's flag (which resolution would
    /// otherwise treat as indistinguishable from the flag never being passed).
    /// Run once, after the Pass 0 resolve loop consumes every composed member's
    /// layer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::OrphanArgv`] naming the first argv layer left
    /// unconsumed.
    pub fn validate_no_orphan_argv(&self) -> Result<(), ConfigError> {
        if let Some(name) = self.argv_names.values().next() {
            return Err(ConfigError::OrphanArgv { type_name: (*name).to_owned() });
        }
        Ok(())
    }

    /// Whether a programmatic override is staged for member `C`.
    #[must_use]
    fn has_override(&self, type_id: TypeId) -> bool {
        self.overrides.contains_key(&type_id)
    }

    /// Whether an argv overlay is staged for member `C`.
    #[must_use]
    fn has_argv(&self, type_id: TypeId) -> bool {
        self.argv.contains_key(&type_id)
    }

    /// The highest-precedence source layer present for a member declaring
    /// `section` + `meta` and identified by `type_id`. Reads the stack plus
    /// live env; see [`ConfigProvenance`] for the precision contract.
    #[must_use]
    fn provenance(&self, type_id: TypeId, section: &str, meta: &'static Meta) -> ConfigProvenance {
        if self.has_override(type_id) {
            return ConfigProvenance::Programmatic;
        }
        if self.has_argv(type_id) {
            return ConfigProvenance::Argv;
        }
        if self.file.as_ref().is_some_and(|table| table.contains_key(section)) {
            return ConfigProvenance::File;
        }
        let mut keys = HashSet::new();
        collect_meta_env_keys(meta, &mut keys);
        // The sanctioned ADR-0090 config machinery: reading env to attribute a
        // resolved member's winning source layer for the discovery surface, the
        // central config-read path, not a cap reading its own knob.
        #[allow(clippy::disallowed_methods)]
        if keys.iter().any(|key| env::var_os(key).is_some_and(|value| !value.is_empty())) {
            return ConfigProvenance::Env;
        }
        ConfigProvenance::Default
    }
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
    /// boot error naming `T` (via [`ConfigSources::validate_overrides`]).
    OrphanOverride {
        /// The `type_name` of the staged override that matches no composed member.
        type_name: String,
    },
    /// ADR-0156 §5 (issue 3872): an argv overlay layer was staged on the source
    /// stack (`ConfigSources::set_argv`, via a chassis CLI root's derived
    /// `StageArgv`) whose config type `T` no composed member ever resolves — a
    /// flag the chassis parses and stages but then silently drops. The converse
    /// of [`OrphanOverride`](Self::OrphanOverride) on the argv channel: a hard
    /// boot error naming `T` (via [`ConfigSources::validate_no_orphan_argv`]),
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

/// Validate the process environment against the claimed key set
/// (ADR-0090 §4). Warns (does not error) on any `AETHER_*` env var
/// not in `known` — a typo or stray var is loud but non-fatal (§4
/// rejects strict-reject: a stray CI var must not abort boot). The
/// hard-error half rides the parse path
/// ([`FromArgvThenEnv::try_from_argv_then_env`]), not this sweep. Run
/// once per chassis boot after the env layers load.
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

    #[derive(Clone, Debug, confique::Config)]
    #[allow(dead_code)]
    struct PrecedenceConfig {
        #[config(env = "AETHER_PRECEDENCE_COUNT", default = 7)]
        count: u32,
    }

    impl FromArgvThenEnv for PrecedenceConfig {
        type Layer = Self;

        fn from_layer(layer: Self::Layer) -> Self {
            layer
        }
    }

    // A unique-keyed fixture so the hermetic env-skip test never races the
    // `AETHER_PRECEDENCE_COUNT` mutation in `try_resolve_orders_*`.
    #[derive(Clone, Debug, confique::Config)]
    #[allow(dead_code)]
    struct HermeticFixture {
        #[config(env = "AETHER_HERMETIC_FIXTURE_COUNT", default = 7)]
        count: u32,
    }

    impl FromArgvThenEnv for HermeticFixture {
        type Layer = Self;

        fn from_layer(layer: Self::Layer) -> Self {
            layer
        }
    }

    fn precedence_file_layer(value: u32) -> <PrecedenceConfig as confique::Config>::Layer {
        use confique::Layer as _;
        let mut file = <PrecedenceConfig as confique::Config>::Layer::empty();
        file.count = Some(value);
        file
    }

    fn precedence_file_table(value: u32) -> toml::Table {
        format!("[precedence]\ncount = {value}\n").parse::<toml::Table>().expect("parse precedence table")
    }

    fn parse_count(raw: &str) -> Result<u32, ParseIntError> {
        raw.trim().parse()
    }

    #[test]
    fn parse_csv_set_trims_and_drops_empties() {
        let got = parse_csv_set("a.com, b.com ,, c.com").expect("infallible");
        let want: HashSet<String> = ["a.com", "b.com", "c.com"].iter().map(|s| (*s).to_string()).collect();
        assert_eq!(got, want);
        assert!(parse_csv_set("").expect("infallible").is_empty());
        assert!(parse_csv_set("  ,  , ").expect("infallible").is_empty());
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
        let row = dump.lines().find(|l| l.contains("AETHER_FIXTURE_KNOB")).expect("knob row present");
        assert!(row.contains("99"), "value should be the env override: {row}");
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

    #[test]
    fn config_sources_programmatic_override_short_circuits_argv_and_file() {
        use confique::Layer as _;
        // Tripwire: the programmatic layer is the top of the stack — a staged
        // override wins over an argv overlay (and never touches env/file). Drifts
        // if `resolve_layered`'s override short-circuit is dropped or reordered.
        let mut sources = ConfigSources::new(Some(precedence_file_table(11)));
        let mut argv = <PrecedenceConfig as confique::Config>::Layer::empty();
        argv.count = Some(33);
        sources.set_argv::<PrecedenceConfig>(argv);
        sources.set_override(PrecedenceConfig { count: 99 });
        let resolved = sources.resolve_layered::<PrecedenceConfig>("precedence").expect("resolve");
        assert_eq!(resolved.count, 99, "programmatic override wins over argv and file");
    }

    #[test]
    fn config_sources_provenance_reports_highest_present_layer() {
        // Tripwire: per-member provenance reports the highest-precedence layer
        // present — programmatic > argv > file (each checked before the env
        // branch, so present-layer detection is deterministic regardless of
        // ambient env). Drifts if the layer-detection precedence in
        // `ConfigSources::provenance` changes. A file section is staged in each
        // case, so a higher layer must still win.
        use confique::Layer as _;
        let meta = &<PrecedenceConfig as confique::Config>::META;
        let type_id = TypeId::of::<PrecedenceConfig>();

        let file_sources = ConfigSources::new(Some(precedence_file_table(11)));
        assert_eq!(file_sources.provenance(type_id, "precedence", meta), ConfigProvenance::File);

        let mut argv_sources = ConfigSources::new(Some(precedence_file_table(11)));
        argv_sources.set_argv::<PrecedenceConfig>(<PrecedenceConfig as confique::Config>::Layer::empty());
        assert_eq!(argv_sources.provenance(type_id, "precedence", meta), ConfigProvenance::Argv);

        let mut override_sources = ConfigSources::new(Some(precedence_file_table(11)));
        override_sources.set_override(PrecedenceConfig { count: 1 });
        assert_eq!(override_sources.provenance(type_id, "precedence", meta), ConfigProvenance::Programmatic);
    }

    #[test]
    fn validate_overrides_rejects_orphan_and_accepts_composed() {
        // Tripwire: an override whose type is in the composed set validates; one
        // that is not is a hard `OrphanOverride` naming the type. Drifts if the
        // staged-but-never-composed guard is dropped or inverted.
        let mut sources = ConfigSources::new(None);
        sources.set_override(PrecedenceConfig { count: 1 });

        let mut composed: HashSet<TypeId> = HashSet::new();
        composed.insert(TypeId::of::<PrecedenceConfig>());
        assert!(sources.validate_overrides(&composed).is_ok(), "a composed override is accepted");

        let orphan = sources.validate_overrides(&HashSet::new());
        match orphan {
            Err(ConfigError::OrphanOverride { type_name }) => {
                assert!(type_name.contains("PrecedenceConfig"), "the error names the orphan type: {type_name}");
            }
            other => panic!("expected OrphanOverride, got {other:?}"),
        }
    }

    #[test]
    fn orphan_argv_layer_is_rejected_then_cleared_by_resolution() {
        use confique::Layer as _;
        // Tripwire (issue 3872): the converse of the override guard on the argv
        // channel. A staged argv layer no member consumes is a hard `OrphanArgv`
        // naming the config type — the silent-drop the ADR-0156 §5 inversion
        // closes — and resolving the member consumes the layer so the stack is
        // then clean. Hermetic so the resolve never touches env (no race).
        // Drifts if `set_argv` / `take_argv` stop tracking the name in lockstep,
        // or `validate_no_orphan_argv` stops reporting a leftover layer.
        let mut sources = ConfigSources::hermetic();
        sources.set_argv::<HermeticFixture>(<HermeticFixture as confique::Config>::Layer::empty());

        match sources.validate_no_orphan_argv() {
            Err(ConfigError::OrphanArgv { type_name }) => {
                assert!(type_name.contains("HermeticFixture"), "the error names the orphan type: {type_name}");
            }
            other => panic!("expected OrphanArgv for a staged-but-unconsumed layer, got {other:?}"),
        }

        sources.resolve_layered::<HermeticFixture>("hermetic_fixture").expect("resolves off the staged layer");
        sources.validate_no_orphan_argv().expect("no orphan once the member consumed its layer");
    }

    #[test]
    fn hermetic_resolution_ignores_env() {
        // Tripwire: hermetic resolution skips the env layer — a set env var does
        // not leak into a hermetic-stack resolve; the member falls through to its
        // compiled default. The non-hermetic path still reads env. Unique env key
        // (no race). Drifts if `try_resolve_hermetic` regains an `.env()` layer.
        // SAFETY: unique key set then removed in this test scope.
        unsafe { env::set_var("AETHER_HERMETIC_FIXTURE_COUNT", "55") };
        let hermetic = ConfigSources::hermetic().resolve_layered::<HermeticFixture>("hermetic");
        let full = ConfigSources::new(None).resolve_layered::<HermeticFixture>("hermetic");
        // SAFETY: same scope.
        unsafe { env::remove_var("AETHER_HERMETIC_FIXTURE_COUNT") };
        assert_eq!(hermetic.expect("hermetic resolve").count, 7, "hermetic skips env → default");
        assert_eq!(full.expect("full resolve").count, 55, "the full stack still reads env");
    }
}
