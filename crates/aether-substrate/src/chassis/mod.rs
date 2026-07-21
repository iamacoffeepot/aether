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
