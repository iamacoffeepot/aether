//! The ADR-0035 universal `Chassis` trait, redefined by ADR-0071.
//!
//! Each chassis binary impls it over whatever peripheral layer it has
//! — winit + wgpu for desktop, a std timer + stdio for headless, a
//! TCP listener + MCP surface for hub, an embedder-driven manual
//! loop for test-bench. The trait carries identity (`PROFILE`) and
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

use crate::capability::BootError;
use crate::chassis_builder::{BuiltChassis, DriverCapability};

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
/// **Passive chassis** (test-bench: no driver, embedder drives the
/// loop) still impl this trait so [`BuiltChassis<Self>`] /
/// `PassiveChassis<Self>` can be parameterised by the chassis kind;
/// they declare a phantom [`DriverCapability`] for `type Driver`
/// (`crate::capability::NeverDriver`) and have `fn build` error
/// pointing callers at the chassis's inherent `build_passive` —
/// the trait method is never reached on passive chassis but its
/// presence keeps the trait shape uniform across the workspace.
pub trait Chassis: Sized + 'static {
    /// Stable identifier for this chassis. Used in boot logs and
    /// wherever the chassis needs to identify itself to an observer.
    /// `"desktop"`, `"headless"`, `"hub"`, `"test-bench"`.
    ///
    /// Renamed from the ADR-0035 `KIND` const by ADR-0071 to avoid
    /// clobbering the data layer's `Kind` vocabulary.
    const PROFILE: &'static str;

    /// The driver capability that owns this chassis's main thread.
    /// Desktop's winit driver, headless's std-timer driver, hub's
    /// listener-and-MCP driver. Passive chassis (test-bench)
    /// declare [`crate::capability::NeverDriver`] here.
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
