//! The shared chassis-binary entry point (ADR-0162 §Produce side, issue
//! #5734): the whole of a chassis `main`, once.
//!
//! Every chassis binary ran the same five steps — parse its CLI root, run the
//! [`run_describe_prelude`] discovery exits, open its `Chassis::Env` off the
//! parsed root, build, log that the chassis is up, and block on the driver —
//! and each spelled them out in a thirty-line `main` that differed only in
//! which two types it named. [`run_chassis_main`] is that flow; `chassis_main!`
//! is the `fn main` wrapper over it, so a chassis bin declares its chassis and
//! its CLI root and nothing else.
//!
//! The one real difference between the roots was how the env comes off the CLI:
//! the full-stack chassis resolve a [`CommonEnv`], the hub takes the raw
//! [`ConfigSources`] stack and resolves each member at the seam that consumes
//! it (ADR-0162). [`ChassisEnv`] names that step per env type rather than per
//! binary, so the shared flow covers both.

use aether_substrate::chassis::BootableChassis;
use aether_substrate::config::{ConfigError, ConfigSources};
use clap::Parser;

use crate::boot::{CommonEnv, run_describe_prelude};
use crate::cli::ChassisCli;

/// How a chassis's `Chassis::Env` is opened off its parsed CLI root — the
/// single step the chassis mains genuinely differed in.
///
/// Implemented per env type, not per chassis: every chassis whose env is a
/// [`CommonEnv`] opens it the same way, and so does every chassis that takes
/// the raw [`ConfigSources`] stack.
pub trait ChassisEnv: Sized {
    /// Open this env off the source stack the CLI root assembles
    /// (`--config` file + the root's derived argv overlays).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the `--config` file cannot be read, or when
    /// a known `AETHER_*` member (or argv overlay value) holds an unparseable
    /// value (ADR-0090 §4).
    fn from_cli(cli: impl ChassisCli) -> Result<Self, ConfigError>;
}

impl ChassisEnv for CommonEnv {
    fn from_cli(cli: impl ChassisCli) -> Result<Self, ConfigError> {
        Self::resolve(cli)
    }
}

impl ChassisEnv for ConfigSources {
    /// ADR-0162: the hub's env IS the raw source stack — every member it
    /// resolves (runtime, frame size, the base stratum, the always-bind RPC
    /// port) resolves at the seam that consumes it, so nothing is pre-resolved
    /// into an env bag.
    fn from_cli(cli: impl ChassisCli) -> Result<Self, ConfigError> {
        cli.into_sources()
    }
}

/// The shared chassis `main`: parse `Cli`, run the ADR-0162 discovery prelude
/// (`--print-config` / `--describe` print and exit before Init), open the env
/// off the parsed root, build the chassis, and block on its driver.
///
/// # Errors
///
/// Returns the first failure of the prelude, env resolution, chassis boot, or
/// the driver run.
pub fn run_chassis_main<C, Cli>() -> anyhow::Result<()>
where
    C: BootableChassis,
    C::Env: ChassisEnv,
    Cli: ChassisCli + Parser,
{
    let cli = Cli::parse();
    // ADR-0162 shared prelude: `--print-config` (ADR-0090 §4 dump) and
    // `--describe` (ADR-0115 manifest) print and exit before Init; a plain
    // invocation falls through to boot (desktop opens no winit event loop
    // until then).
    if run_describe_prelude::<C>(cli.meta())?.is_handled() {
        return Ok(());
    }

    let chassis = C::build(C::Env::from_cli(cli)?)?;
    tracing::info!(
        target: "aether_substrate::boot",
        profile = C::PROFILE,
        "chassis initialised",
    );
    chassis.run()?;
    Ok(())
}

/// Emit a chassis binary's `fn main` over [`run_chassis_main`] — the whole of a
/// chassis bin (issue #5734).
///
/// ```ignore
/// use aether_chassis::chassis_main;
/// use aether_chassis_headless::{HeadlessChassis, HeadlessCli};
///
/// chassis_main!(HeadlessChassis, HeadlessCli);
/// ```
///
/// The error type is this crate's re-exported `anyhow`, so a chassis bin needs
/// no dependency of its own for the `main` this replaces.
#[macro_export]
macro_rules! chassis_main {
    ($chassis:ty, $cli:ty $(,)?) => {
        fn main() -> $crate::anyhow::Result<()> {
            $crate::entry::run_chassis_main::<$chassis, $cli>()
        }
    };
}
