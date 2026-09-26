//! The post-build mount seam: spawn the journal owner and the bundle driver.
//!
//! [`mount`] runs after `build()` seals the composed chain and before `run()`
//! blocks on the driver. [`BuiltChassis::spawn_actor`] is the production
//! embedder path — post-seal it commits through the ADR-0165 owner — and no
//! `DriverCtx` spawn exists, so this is the only seam that can birth actors
//! whose wiring needs the journal's born id.

use std::fmt::Debug;
use std::io;

use aether_actor::ActorRef;
use aether_bloomery_driver::{BundleDriver, DriverParams};
use aether_bloomery_journal::{Journal, JournalActor, ReadCacheBudget};
use aether_bloomery_kinds::ClosureLimit;
use aether_substrate::Subname;
use aether_substrate::chassis::builder::BuiltChassis;
use aether_substrate::chassis::error::BootError;

use crate::chassis::BloomeryChassis;

/// The proven references `mount` took back from its two spawns: the journal
/// owner and the bundle driver it wired to that journal. An embedder that
/// drives the mounted engine in process addresses both through these rather
/// than resolving either by path.
#[derive(Debug, Clone, Copy)]
pub struct Mounted {
    /// The journal owner, `aether.bloomery.journal:journal`.
    pub journal: ActorRef<JournalActor>,
    /// The bundle driver, `aether.bloomery.driver:driver`.
    pub driver: ActorRef<BundleDriver>,
}

/// Spawn the journal owner over `journal` with the `read_cache` budget under
/// `Subname::Named("journal")` and the bundle driver under
/// `Subname::Named("driver")` over the journal's born reference, so the engine
/// answers as `aether.bloomery.journal:journal` and
/// `aether.bloomery.driver:driver`, and hand both references back as
/// [`Mounted`].
///
/// Takes the already-opened journal and lowered budgets rather than the config:
/// `BloomeryChassis::build_mounted` lowers the knobs and opens the root before
/// it stands up the substrate, so an unset or held journal root never reaches
/// this seam.
///
/// # Errors
///
/// Returns [`BootError`] when either spawn fails.
pub fn mount(
    built: &BuiltChassis<BloomeryChassis>,
    journal: Journal,
    limit: ClosureLimit,
    read_cache: ReadCacheBudget,
) -> Result<Mounted, BootError> {
    let journal = built
        .spawn_actor::<JournalActor>(Subname::Named("journal"), read_cache, journal)
        .finish()
        .map_err(|error| spawn_failed("aether.bloomery.journal:journal", &error))?;
    let driver = built
        .spawn_actor::<BundleDriver>(Subname::Named("driver"), limit, DriverParams { journal })
        .finish()
        .map_err(|error| spawn_failed("aether.bloomery.driver:driver", &error))?;
    Ok(Mounted { journal, driver })
}

/// Wrap a spawn failure the way `impl From<wasmtime::Error> for BootError`
/// does: [`SpawnError`](aether_substrate::actor::native::spawn::SpawnError) is
/// `#[derive(Debug)]` only — no `Display`, no `std::error::Error` — so it
/// cannot be boxed into [`BootError::Other`] as-is.
fn spawn_failed(actor: &str, error: &dyn Debug) -> BootError {
    BootError::Other(Box::new(io::Error::other(format!("spawning {actor}: {error:?}"))))
}
