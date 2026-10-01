//! The post-build mount seam: spawn the journal owner, the bundle driver, and
//! the inspect actor.
//!
//! [`mount`] runs after `build()` seals the composed chain and before `run()`
//! blocks on the driver. [`BuiltChassis::spawn_actor`] is the production
//! embedder path — post-seal it commits through the ADR-0165 owner — and no
//! `DriverCtx` spawn exists, so this is the only seam that can birth actors
//! whose wiring needs the journal's born id.

use std::fmt::Debug;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use aether_actor::ActorRef;
use aether_bloomery_driver::{BundleDriver, DriverParams, InvocationLimit};
use aether_bloomery_journal::{Clock, Journal, JournalActor, ReadCacheBudget};
use aether_bloomery_kinds::{ClosureLimit, UnitKey};
use aether_bloomery_workspace::WorkspaceCapability;
use aether_substrate::Subname;
use aether_substrate::chassis::builder::BuiltChassis;
use aether_substrate::chassis::error::BootError;

use crate::chassis::BloomeryChassis;
use crate::inspect::{InspectActor, InspectParams};

/// The proven references `mount` took back from its three spawns: the journal
/// owner, the bundle driver it wired to that journal, and the inspect actor
/// reading through both. An embedder that drives the mounted engine in
/// process addresses each through these rather than resolving any by path.
#[derive(Debug, Clone, Copy)]
pub struct Mounted {
    /// The journal owner, `aether.bloomery.journal:<key>` for the unit's key.
    pub journal: ActorRef<JournalActor>,
    /// The bundle driver, `aether.bloomery.driver:driver`.
    pub driver: ActorRef<BundleDriver>,
    /// The inspect actor, `aether.bloomery.inspect:<key>` for the unit's key.
    pub inspect: ActorRef<InspectActor>,
}

/// What the bundle driver spawns over besides its journal: the closure limit,
/// the clock and tick period its timers fire on (ADR-0245), and how many
/// requests one bundle runs at once.
pub struct DriverSetup {
    /// The byte budget of one closure read.
    pub limit: ClosureLimit,
    /// The clock the journal stamps entries with, shared with the driver.
    pub clock: Arc<dyn Clock + Send + Sync>,
    /// How long one driver tick waits before it reads the clock.
    pub tick: Duration,
    /// How many requests one bundle has active at once.
    pub invocations: InvocationLimit,
}

/// Spawn the journal owner over `journal` with the `read_cache` budget under
/// `Subname::Named(unit)` and the bundle driver under `Subname::Named("driver")`
/// over the unit's key, the journal's born reference, the composed
/// workspace's reference, and `driver`'s limit, clock, tick, and invocation
/// limit, so the engine
/// answers as
/// `aether.bloomery.journal:<key>` and `aether.bloomery.driver:driver`, then
/// the inspect actor under `Subname::Named(unit)` over both references, so it
/// answers as `aether.bloomery.inspect:<key>`, and hand all three references
/// back as [`Mounted`]. The journal's name is already the
/// unit-root name ADR-0240 D1 gives it.
///
/// Takes the already-opened journal and lowered budgets rather than the config:
/// `BloomeryChassis::build_mounted` lowers the knobs and opens the root before
/// it stands up the substrate, so an unset or held journal root never reaches
/// this seam.
///
/// # Errors
///
/// Returns [`BootError`] when a spawn fails.
pub fn mount(
    built: &BuiltChassis<BloomeryChassis>,
    unit: &UnitKey,
    journal: Journal,
    read_cache: ReadCacheBudget,
    driver: DriverSetup,
) -> Result<Mounted, BootError> {
    let DriverSetup { limit, clock, tick, invocations } = driver;
    let journal = built
        .spawn_actor::<JournalActor>(Subname::Named(unit.as_str()), read_cache, journal)
        .finish()
        .map_err(|error| spawn_failed(&format!("aether.bloomery.journal:{unit}"), &error))?;
    let params = DriverParams {
        unit: unit.clone(),
        journal,
        workspace: built.actor_ref::<WorkspaceCapability>(),
        clock,
        tick,
        invocations,
    };
    let driver = built
        .spawn_actor::<BundleDriver>(Subname::Named("driver"), limit, params)
        .finish()
        .map_err(|error| spawn_failed("aether.bloomery.driver:driver", &error))?;
    let inspect = built
        .spawn_actor::<InspectActor>(Subname::Named(unit.as_str()), (), InspectParams { journal, driver })
        .finish()
        .map_err(|error| spawn_failed(&format!("aether.bloomery.inspect:{unit}"), &error))?;
    Ok(Mounted { journal, driver, inspect })
}

/// Wrap a spawn failure the way `impl From<wasmtime::Error> for BootError`
/// does: [`SpawnError`](aether_substrate::actor::native::spawn::SpawnError) is
/// `#[derive(Debug)]` only — no `Display`, no `std::error::Error` — so it
/// cannot be boxed into [`BootError::Other`] as-is.
fn spawn_failed(actor: &str, error: &dyn Debug) -> BootError {
    BootError::Other(Box::new(io::Error::other(format!("spawning {actor}: {error:?}"))))
}
