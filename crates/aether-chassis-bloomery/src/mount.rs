//! The post-build mount seam: spawn the journal owner and the bundle driver.
//!
//! [`mount`] runs after `build()` seals the composed chain and before `run()`
//! blocks on the driver. [`BuiltChassis::spawn_actor`] is the production
//! embedder path — post-seal it commits through the ADR-0165 owner — and no
//! `DriverCtx` spawn exists, so this is the only seam that can birth actors
//! whose wiring needs the journal's born id.

use std::fmt::Debug;
use std::io;
use std::path::Path;

use aether_bloomery_driver::{BundleDriver, DriverParams};
use aether_bloomery_journal::JournalActor;
use aether_bloomery_kinds::ClosureLimit;
use aether_substrate::Subname;
use aether_substrate::chassis::builder::BuiltChassis;
use aether_substrate::chassis::error::BootError;

use crate::chassis::BloomeryChassis;

/// Spawn the journal owner under `Subname::Named("journal")` and the bundle
/// driver under `Subname::Named("driver")` over the journal's born reference, so the
/// engine answers as `aether.bloomery.journal:journal` and
/// `aether.bloomery.driver:driver`.
///
/// Takes the already-lowered pair rather than the config: `BloomeryChassis::build`
/// lowers the knobs before it stands up the substrate, so a bad journal path
/// never reaches this seam.
///
/// # Errors
///
/// Returns [`BootError`] when either spawn fails.
pub fn mount(built: &BuiltChassis<BloomeryChassis>, path: &Path, limit: ClosureLimit) -> Result<(), BootError> {
    let journal = built
        .spawn_actor::<JournalActor>(Subname::Named("journal"), path.to_path_buf(), ())
        .finish()
        .map_err(|error| spawn_failed("aether.bloomery.journal:journal", &error))?;
    let driver = built
        .spawn_actor::<BundleDriver>(Subname::Named("driver"), limit, DriverParams { journal })
        .finish()
        .map_err(|error| spawn_failed("aether.bloomery.driver:driver", &error))?;
    tracing::info!(
        journal = %path.display(),
        ?journal,
        ?driver,
        "bloomery chassis mounted the journal owner and the bundle driver",
    );
    Ok(())
}

/// Wrap a spawn failure the way `impl From<wasmtime::Error> for BootError`
/// does: [`SpawnError`](aether_substrate::actor::native::spawn::SpawnError) is
/// `#[derive(Debug)]` only — no `Display`, no `std::error::Error` — so it
/// cannot be boxed into [`BootError::Other`] as-is.
fn spawn_failed(actor: &str, error: &dyn Debug) -> BootError {
    BootError::Other(Box::new(io::Error::other(format!("spawning {actor}: {error:?}"))))
}
