//! The ADR-0035 universal `Chassis` trait, redefined by ADR-0071.
//!
//! Each chassis binary impls it over whatever peripheral layer it has
//! — winit + wgpu for desktop, a std timer + stdio for headless, a
//! TCP listener + MCP surface for hub, an embedder-driven manual
//! loop for substrate-harness. The trait carries identity (`PROFILE`) and
//! the build entry point (`type Driver`, `type Env`, `fn build`)
//! that produces a [`BuiltChassis`]; the chassis instance you `run()`
//! is the [`BuiltChassis<Self>`], not a value of `Self` itself.
//!
//! ADR-0071 phase 2 renamed the previous `KIND` const to `PROFILE`
//! (avoiding the data-layer `Kind` / `KindId` / `KindShape` /
//! `KindLabels` clobber), removed the `CAPABILITIES` const + the
//! associated `ChassisCapabilities` struct, and lifted the per-
//! chassis inherent `build(env)` into a trait method backed by
//! `type Driver` + `type Env` associated types. The pre-ADR-0071
//! `fn run(self)` slot is gone — the chassis you ran was always
//! the [`BuiltChassis<C>`] anyway, and the indirection through a
//! trait method that immediately delegated added no value.

pub mod builder;
pub mod ctx;
pub mod error;
pub mod frame_loop;
pub mod inbox;
pub mod settlement;
pub mod settlement_counter;
pub mod settlement_table;

use crate::chassis::builder::{BuiltChassis, DriverCapability};
use crate::chassis::error::BootError;

// The boot ceremony (`BootableChassis` + the describe / config helpers) is
// only meaningful when the substrate runtime is linked: it stands up a
// `SubstrateBoot`, which — like the `boot` module itself — lives behind the
// `wasm` feature. A `wasm`-less consumer (e.g. the derive crate's trybuild
// fixtures compiling `aether-substrate` bare) never runs a chassis boot, so the
// ceremony surface is gated off there rather than referencing a type that isn't
// linked.
#[cfg(feature = "wasm")]
use std::collections::BTreeSet;

#[cfg(feature = "wasm")]
use aether_kinds::BinaryManifest;

#[cfg(feature = "wasm")]
use crate::SubstrateBoot;
#[cfg(feature = "wasm")]
use crate::chassis::builder::Builder;
#[cfg(feature = "wasm")]
use crate::config::{ConfigError, ConfigManifest, KnobRecord};

/// The composition contract a concrete chassis implements. Each
/// chassis declares its driver and the env-shaped config it takes
/// at build time; the trait method [`Self::build`] consumes the env
/// and returns a [`BuiltChassis<Self>`] whose [`BuiltChassis::run`]
/// blocks the calling thread on the driver loop.
///
/// `Sized + 'static` matches ADR-0071: every chassis binary picks
/// exactly one chassis at compile time (it's a unit struct), and
/// `'static` lets `BuiltChassis<Self>` / `PassiveChassis<Self>` be
/// stored in long-lived owners without lifetime gymnastics.
///
/// **Passive chassis** (substrate-harness: no driver, embedder drives the
/// loop) still impl this trait so [`BuiltChassis<Self>`] /
/// `PassiveChassis<Self>` can be parameterised by the chassis kind;
/// they declare a phantom [`DriverCapability`] for `type Driver`
/// (`crate::chassis::builder::NeverDriver`) and have `fn build` error
/// pointing callers at the chassis's inherent `build_passive` —
/// the trait method is never reached on passive chassis but its
/// presence keeps the trait shape uniform across the workspace.
pub trait Chassis: Sized + 'static {
    /// Stable identifier for this chassis. Used in boot logs and
    /// wherever the chassis needs to identify itself to an observer.
    /// `"desktop"`, `"headless"`, `"hub"`, `"substrate-harness"`.
    ///
    /// Renamed from the ADR-0035 `KIND` const by ADR-0071 to avoid
    /// clobbering the data layer's `Kind` vocabulary.
    const PROFILE: &'static str;

    /// The driver capability that owns this chassis's main thread.
    /// Desktop's winit driver, headless's std-timer driver, hub's
    /// listener-and-MCP driver. Passive chassis (substrate-harness)
    /// declare [`builder::NeverDriver`] here.
    type Driver: DriverCapability;

    /// Resolved-config bag the chassis takes at build time. Each
    /// chassis defines its own concrete shape because chassis
    /// genuinely take different inputs (desktop needs a winit
    /// `EventLoop`, headless doesn't); a uniform `ChassisEnv`
    /// trait would just push the per-chassis differences down a
    /// level.
    ///
    /// `main()` populates the env from environment variables (today)
    /// or layered config (CLI > env > TOML > defaults, future).
    type Env;

    /// Build the chassis from resolved config. Stands up substrate
    /// internals, boots passive capabilities, wires the driver, and
    /// returns the [`BuiltChassis<Self>`] whose [`BuiltChassis::run`]
    /// blocks until the driver exits.
    ///
    /// Passive chassis return an error pointing callers at the
    /// chassis's inherent `build_passive` instead — the trait method
    /// shape exists for trait uniformity, not for invocation.
    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError>;
}

/// Derive a chassis's wire-visible RPC engine name from its
/// [`Chassis::PROFILE`]. The rule is uniform across every chassis —
/// `"aether-" + PROFILE` (`desktop` → `aether-desktop`, `headless` →
/// `aether-headless`, `hub` → `aether-hub`, `bloomery` →
/// `aether-bloomery`) — so this is the single source of truth every
/// `PeerKind::Substrate { engine_name, .. }` a chassis builds reads, and
/// the name is never stated as a second literal beside the profile.
#[must_use]
pub fn engine_name<C: Chassis>() -> String {
    format!("aether-{}", C::PROFILE)
}

/// The boot-ceremony contract shared by every chassis binary that resolves
/// config and composes a capability chain (desktop, headless, hub, bloomery).
/// Layered over [`Chassis`] so passive / test chassis (substrate-harness and
/// the in-crate test chassis) — which never run this ceremony, they are driven
/// by an embedder — stay unaffected.
///
/// The three boot entry points a chassis binary exposes (`--describe`,
/// `--print-config`, and the real `build`) each run the same head: resolve the
/// env, stand up a [`SubstrateBoot`], and compose the capability chain. This
/// trait names that head's two per-chassis seams — [`Self::resolve_env`] and
/// [`Self::compose`] — plus the [`Self::residual_knobs`] the config sweep folds
/// in, so the generic [`describe_caps`] / [`config_manifest`] helpers can derive
/// every entry point from the one `compose` declaration. A chassis that changes
/// its chain edits `compose` alone; `--describe` and `--print-config` follow
/// from it and can no longer drift (the parallel-edit hazard of #3859).
#[cfg(feature = "wasm")]
pub trait BootableChassis: Chassis {
    /// Resolve this chassis's env — the config *data* a real boot takes — off
    /// the argv/env/file source stack. The single env-reading edge (ADR-0070);
    /// `--describe` and `--print-config` resolve it exactly as `build` does, so
    /// the manifests reflect the same config a boot would see.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a known `AETHER_*` var (or argv overlay
    /// value) fails its parser (ADR-0090 §4).
    fn resolve_env() -> Result<Self::Env, ConfigError>;

    /// Compose the capability chain — the single claim/build declaration
    /// (ADR-0155) both [`Chassis::build`] and the describe / config helpers run,
    /// so the manifest roster and config aggregate can never drift from what
    /// boots. Takes the boot handle by reference; `build` moves the same `boot`
    /// into the driver afterward, while the describe / config helpers drop it.
    fn compose(boot: &SubstrateBoot, env: Self::Env) -> Builder<Self>;

    /// The residual hand-registered knobs the composition-derived
    /// [`ConfigManifest`] can't own, folded into the known-keys sweep and the
    /// `--print-config` dump beside the manifest metas. Defaults to none —
    /// chassis with no config path (bloomery) inherit the empty default; the
    /// full-stack chassis override it with their per-profile residual set.
    #[must_use]
    fn residual_knobs() -> Vec<KnobRecord> {
        Vec::new()
    }
}

/// The `--describe` capability roster (ADR-0155): resolve the env, stand up a
/// [`SubstrateBoot`], compose the exact chain [`Chassis::build`] runs, then run
/// the claim-only terminal and read the claimed namespaces off the registry.
/// Stops before Init — opens no window / device / socket. The caller wraps the
/// returned set in a `BinaryManifest` with its own crate's build provenance.
///
/// # Errors
///
/// Returns [`BootError`] when env resolution, substrate boot, or the claim pass
/// fails.
#[cfg(feature = "wasm")]
pub fn describe_caps<C: BootableChassis>() -> Result<BTreeSet<String>, BootError> {
    let env = C::resolve_env().map_err(|e| BootError::Other(Box::new(e)))?;
    let boot = SubstrateBoot::build()?;
    C::compose(&boot, env).claim_namespaces()
}

/// The ADR-0156 §4 composition-derived config aggregate: resolve the env,
/// compose the exact chain [`Chassis::build`] runs, then read
/// [`Builder::config_manifest`] — the sibling of [`describe_caps`]'s claim
/// terminal. The known-keys sweep and `--print-config` dump read this walk, so a
/// chassis reports only the knobs it composes.
///
/// # Errors
///
/// Returns [`BootError`] when env resolution or substrate boot fails.
#[cfg(feature = "wasm")]
pub fn config_manifest<C: BootableChassis>() -> Result<ConfigManifest, BootError> {
    let env = C::resolve_env().map_err(|e| BootError::Other(Box::new(e)))?;
    let boot = SubstrateBoot::build()?;
    Ok(C::compose(&boot, env).config_manifest())
}

/// A chassis binary's build provenance — the `build.rs`-baked facts a
/// `--describe` [`BinaryManifest`] reports (ADR-0115): the source revision,
/// build profile, and target triple.
///
/// These are crate-local: each binary's `build.rs` bakes them, and `env!`
/// resolves only in the crate whose build script set them (ADR-0155). The
/// shared prelude therefore takes provenance as a value the binary constructs
/// with its own `env!`s rather than reading `env!` itself — this is what lets
/// the bloomery chassis, which does not depend on the `aether-chassis`
/// aggregate, route through the same prelude flow (ADR-0162): it fills a
/// `BuildProvenance` from its own crate's `build.rs` and hands it over.
#[derive(Debug, Clone)]
pub struct BuildProvenance {
    /// `git rev-parse --short HEAD`, or `"unknown"` outside a git checkout.
    pub git_sha: String,
    /// Cargo's build profile (`debug` / `release`).
    pub profile: String,
    /// Cargo's target triple (e.g. `aarch64-apple-darwin`).
    pub target: String,
}

/// Assemble a chassis binary's `--describe` [`BinaryManifest`] (ADR-0115,
/// amended by ADR-0155 and ADR-0162): the chassis profile, the mailbox
/// namespaces it claims, the config surface it accepts (composition-derived env
/// keys + derive-emitted argv overlay flags), and the caller-supplied
/// [`BuildProvenance`].
///
/// Composes the chain **once** and reads every derived field off that one
/// builder — the config aggregate ([`Builder::config_manifest`]) for the config
/// surface, then the claim terminal ([`Builder::claim_namespaces`]) for the cap
/// roster. Every set is therefore the same composition a real boot runs, so no
/// hand-maintained list can drift (ADR-0162):
///
/// - `caps` — the claim roster over the composed `with_actor` chain, driver
///   claims, and inline sinks. The [`BTreeSet`] arrives sorted; the field
///   preserves that order.
/// - `env_keys` — the composition-derived known-key set
///   ([`ConfigManifest::known_keys`]) folded with the chassis's
///   [`residual knobs`](BootableChassis::residual_knobs), sorted — exactly the
///   keys this binary's own unknown-`AETHER_*` sweep accepts.
/// - `argv_flags` — the composition-derived argv overlay surface
///   ([`ConfigManifest::argv_flags`]), already sorted — the derive-emitted
///   `--flags` this binary accepts, from the same machinery that stamps them
///   onto each overlay.
///
/// # Errors
///
/// Returns [`BootError`] when env resolution, substrate boot, or the claim pass
/// fails.
#[cfg(feature = "wasm")]
pub fn describe_manifest<C: BootableChassis>(provenance: &BuildProvenance) -> Result<BinaryManifest, BootError> {
    let env = C::resolve_env().map_err(|e| BootError::Other(Box::new(e)))?;
    let boot = SubstrateBoot::build()?;
    let builder = C::compose(&boot, env);

    let config = builder.config_manifest();
    let mut env_keys: Vec<String> = config.known_keys(&C::residual_knobs()).iter().map(str::to_owned).collect();
    env_keys.sort();
    let argv_flags: Vec<String> = config.argv_flags().into_iter().map(str::to_owned).collect();

    Ok(BinaryManifest {
        chassis: C::PROFILE.to_owned(),
        caps: builder.claim_namespaces()?.into_iter().collect(),
        git_sha: provenance.git_sha.clone(),
        profile: provenance.profile.clone(),
        target: provenance.target.clone(),
        env_keys,
        argv_flags,
    })
}

/// The `--print-config` discovery dump for a chassis (ADR-0090 §4): the
/// composition-derived config aggregate ([`config_manifest`]) plus the chassis's
/// [`residual knobs`](BootableChassis::residual_knobs). The prelude prints this
/// and exits before boot.
///
/// # Errors
///
/// Returns [`BootError`] when env resolution or substrate boot fails.
#[cfg(feature = "wasm")]
pub fn config_dump<C: BootableChassis>() -> Result<String, BootError> {
    Ok(config_manifest::<C>()?.dump(&C::residual_knobs()))
}

/// The prelude flags a chassis CLI root exposes before boot. Each names an
/// exit-before-Init discovery mode; a chassis whose CLI lacks one (bloomery has
/// no `--print-config`) passes `false` for it.
#[derive(Debug, Clone, Copy)]
pub struct PreludeFlags {
    /// `--describe` (ADR-0115): print the [`BinaryManifest`] JSON and exit.
    pub describe: bool,
    /// `--print-config` (ADR-0090 §4): print the config discovery dump and exit.
    pub print_config: bool,
}

/// Whether the prelude handled the invocation (it printed a discovery dump and
/// the binary should exit) or the binary should proceed to boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum PreludeAction {
    /// A prelude flag matched: the dump is printed, `main` should return.
    Handled,
    /// No prelude flag matched: `main` should resolve its env and boot.
    Boot,
}

impl PreludeAction {
    /// `true` when the binary should stop after the prelude rather than boot.
    #[must_use]
    pub fn is_handled(self) -> bool {
        matches!(self, Self::Handled)
    }
}

/// The shared chassis-main prelude (ADR-0162): the single flow every chassis
/// binary runs ahead of boot. It dispatches the exit-before-Init discovery
/// modes — `--print-config` (the ADR-0090 §4 config dump) and `--describe` (the
/// ADR-0115 [`BinaryManifest`]) — off the parsed [`PreludeFlags`], prints the
/// selected dump to stdout, and reports [`PreludeAction::Handled`] so the binary
/// returns before Init. With no flag set it reports [`PreludeAction::Boot`] and
/// the binary proceeds to resolve its env and run.
///
/// Both discovery modes run the same claim/compose ceremony a real boot runs
/// (ADR-0155), stopping at Claim, so the manifest and config dump can never
/// drift from what boots. `--describe` wraps the claim roster in the
/// caller-supplied [`BuildProvenance`], which is why the flow is uniform across
/// every chassis binary yet still reports each binary's own crate-local build
/// facts (ADR-0162).
///
/// # Errors
///
/// Returns [`BootError`] when env resolution, substrate boot, the claim pass, or
/// manifest serialization fails.
#[cfg(feature = "wasm")]
// The prelude owns the discovery-dump stdout every chassis bin previously
// printed inline (ADR-0090 §4 / e2): the whole point is to move that print off
// each bin, so it prints here, before the tracing subscriber is installed.
#[allow(clippy::print_stdout)]
pub fn run_chassis_prelude<C: BootableChassis>(
    flags: PreludeFlags,
    provenance: &BuildProvenance,
) -> Result<PreludeAction, BootError> {
    if flags.print_config {
        print!("{}", config_dump::<C>()?);
        return Ok(PreludeAction::Handled);
    }
    if flags.describe {
        let manifest = describe_manifest::<C>(provenance)?;
        let json = serde_json::to_string(&manifest).map_err(|e| BootError::Other(Box::new(e)))?;
        println!("{json}");
        return Ok(PreludeAction::Handled);
    }
    Ok(PreludeAction::Boot)
}

#[cfg(all(test, feature = "wasm"))]
mod prelude_tests {
    use super::{BootableChassis, BuildProvenance, Chassis, PreludeAction, PreludeFlags, run_chassis_prelude};
    use crate::SubstrateBoot;
    use crate::chassis::builder::{Builder, BuiltChassis, NeverDriver};
    use crate::chassis::error::BootError;
    use crate::config::ConfigError;

    /// A chassis whose every boot-ceremony seam panics. The no-flag prelude
    /// path must never reach `resolve_env` / `compose`, so a `run_chassis_prelude`
    /// over this chassis is safe exactly when it short-circuits to `Boot`.
    struct PanicChassis;

    impl Chassis for PanicChassis {
        const PROFILE: &'static str = "panic-test";
        type Driver = NeverDriver;
        type Env = ();
        fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
            unreachable!("PanicChassis is a prelude-dispatch fixture, never built");
        }
    }

    impl BootableChassis for PanicChassis {
        fn resolve_env() -> Result<Self::Env, ConfigError> {
            panic!("prelude ran the config ceremony with no discovery flag set");
        }
        fn compose(_boot: &SubstrateBoot, _env: Self::Env) -> Builder<Self> {
            panic!("prelude composed the capability chain with no discovery flag set");
        }
    }

    fn provenance() -> BuildProvenance {
        BuildProvenance { git_sha: "sha".to_owned(), profile: "debug".to_owned(), target: "triple".to_owned() }
    }

    #[test]
    fn no_flag_returns_boot_without_running_the_ceremony() {
        // Tripwire: the prelude must stop before Init when neither discovery
        // flag is set — it returns `Boot` and hands the boot back to the binary
        // rather than resolving env / composing (both side-effectful). Reordering
        // the ceremony ahead of the flag check fires `PanicChassis`'s seams.
        let action =
            run_chassis_prelude::<PanicChassis>(PreludeFlags { describe: false, print_config: false }, &provenance())
                .expect("no-flag prelude is infallible — it touches no substrate");
        assert_eq!(action, PreludeAction::Boot);
        assert!(!action.is_handled(), "Boot is the not-handled action");
    }
}
