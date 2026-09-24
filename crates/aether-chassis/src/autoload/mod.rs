//! Boot-time component autoload shared by the full-stack chassis
//! (iamacoffeepot/aether#1529, generalizing the #1520 desktop hook).
//!
//! Two channels populate a chassis env's `autoload` field: the package depot
//! boot (`crate::package::package_autoload`, decoding a content-addressed
//! `pack/manifest`) and the JSON boot-manifest reader below (the hub's
//! `spawn_substrate` path). `boot_standard` loads the list right after
//! `.build()`, one component at a time in list order, and waits for each to
//! answer its load before the RPC server binds (issue #6413), so an engine a
//! caller can reach already has every boot component live. Each wait is
//! bounded by the chassis's boot-load budget (issue #6637): a load that never
//! answers fails the boot naming its component rather than holding the engine
//! short of serving forever. The loads target
//! the generic `aether.component` mailbox — the same address the hub's
//! `load_component` and the substrate harness load through — which is what
//! makes the mechanism chassis-agnostic.

mod loader;

use std::io;
use std::mem;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use aether_kinds::{LoadComponent, replica_load_name};
use aether_substrate::Subname;
use aether_substrate::actor::wasm::kind_manifest;
use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::BuiltChassis;
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::ConfigError;

use crate::boot_manifest::{self, PackedComponent};

use loader::{Autoloader, AutoloaderParams, LoadAnswer};

/// A component to auto-load on boot — its wasm bytes, optional init-config
/// bytes (ADR-0090; empty for none), and the optional load name / export
/// selector that `aether.component.load` carries (ADR-0096). The package
/// depot boot and the JSON boot-manifest reader both feed these to the
/// chassis env's `autoload` list.
pub struct AutoloadComponent {
    pub wasm: Vec<u8>,
    pub config: Vec<u8>,
    pub name: Option<String>,
    pub export: Option<String>,
}

impl AutoloadComponent {
    /// The `aether.component.load` request that loads this component — the
    /// same request the hub's `load_component` and the substrate harness send.
    fn load_request(self) -> LoadComponent {
        LoadComponent { wasm: self.wasm, name: self.name, config: self.config, export: self.export }
    }

    /// How a boot failure names this component: its `name`, else its
    /// `export`, else `#<index>` for its position in the boot list.
    fn label(&self, index: usize) -> String {
        self.name.clone().or_else(|| self.export.clone()).unwrap_or_else(|| format!("#{index}"))
    }
}

impl From<PackedComponent> for AutoloadComponent {
    fn from(packed: PackedComponent) -> Self {
        Self { wasm: packed.wasm, config: packed.config, name: packed.name, export: packed.export }
    }
}

/// Read the boot manifest at `path` into the [`AutoloadComponent`] list
/// the chassis env's `autoload` field carries — the JSON-path-manifest twin
/// of the content-addressed package depot boot (`crate::package`). Both feed
/// the same `env.autoload` (one from a manifest of file paths, one from a
/// `pack/manifest` of hash-referenced objects), which `boot_standard` loads in
/// order after the build.
///
/// Reached from `CommonEnv::resolve` (the shared desktop / headless resolver)
/// when `AETHER_BOOT_MANIFEST` (or `--boot-manifest`) is set; the engines
/// cap injects that env var at the fork so a `spawn_substrate` carrying a
/// component list binds its RPC port only once those components are live.
///
/// # Errors
///
/// Returns a hard [`ConfigError`] (the ADR-0090 §4 "known knob, bad
/// value" path — boot aborts loudly) when the manifest or any wasm /
/// config file it names can't be read or parsed.
pub fn boot_manifest_autoload(path: &Path) -> Result<Vec<AutoloadComponent>, ConfigError> {
    let pack = boot_manifest::pack_from_manifest(path)
        .map_err(|e| ConfigError::unparseable("AETHER_BOOT_MANIFEST", path.display().to_string(), e))?;
    let mut components = Vec::with_capacity(pack.components.len());
    for packed in pack.components {
        components.extend(expand_replicas(packed)?);
    }
    Ok(components)
}

/// Fan one manifest entry's optional `replicas` count into one
/// [`AutoloadComponent`] per instance (issue 2626), so a `replicas: N`
/// entry covers every manifest writer at one expansion site: JSON boot
/// manifests (`AETHER_BOOT_MANIFEST`), the content-addressed package depot
/// manifest, and hand-written manifests.
///
/// An entry with no `replicas` set stays a single unmodified
/// `AutoloadComponent` (today's byte-identical behaviour). Otherwise each
/// instance is named by [`replica_load_name`] — replica 0 claims the bare
/// `base`, later replicas `{base}-{index}` — where `base` follows the same
/// precedence the component host itself applies when resolving a load's name
/// (`caller name > export > wasm-declared entry namespace`,
/// `aether-component`'s `handle_load` step 4), so a replicated load's derived
/// name matches what an unreplicated load of the same entry would have
/// resolved to. The bare instance is what keeps a replicated component
/// reachable through bare-type peer addressing
/// (iamacoffeepot/aether#5727); `replicas: 1` is therefore exactly an
/// unreplicated load.
///
/// # Errors
///
/// Returns a [`ConfigError`] (ADR-0090 §4: a bad known value aborts boot
/// loudly, never a silent no-op) when `replicas` is `0`, when the wasm's
/// `aether.namespace` custom section holds invalid UTF-8, or when none of
/// `name`, `export`, or a wasm-declared namespace is available to name the
/// replicas.
pub fn expand_replicas(packed: PackedComponent) -> Result<Vec<AutoloadComponent>, ConfigError> {
    let Some(replicas) = packed.replicas else {
        return Ok(vec![AutoloadComponent::from(packed)]);
    };
    if replicas == 0 {
        return Err(ConfigError::unparseable(
            "replicas",
            "0",
            io::Error::new(io::ErrorKind::InvalidInput, "replicas must be at least 1"),
        ));
    }
    let base = match packed.name.clone().or_else(|| packed.export.clone()) {
        Some(base) => base,
        None => match kind_manifest::read_namespace_from_bytes(&packed.wasm) {
            Ok(Some(declared)) => declared,
            Ok(None) => {
                return Err(ConfigError::unparseable(
                    "replicas",
                    "wasm namespace",
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "cannot determine a base name for replicas: no `name`, `export`, or \
                         wasm-declared entry namespace on this manifest entry",
                    ),
                ));
            }
            Err(e) => {
                return Err(ConfigError::unparseable(
                    "replicas",
                    "wasm namespace",
                    io::Error::new(io::ErrorKind::InvalidData, e),
                ));
            }
        },
    };
    Ok((0..replicas)
        .map(|index| AutoloadComponent {
            wasm: packed.wasm.clone(),
            config: packed.config.clone(),
            name: Some(replica_load_name(&base, index)),
            export: packed.export.clone(),
        })
        .collect())
}

/// Load every boot component in list order and wait until each has answered
/// its load `Ok` (issue #6413), each within `budget` (issue #6637).
///
/// With an empty list this returns at once. Otherwise it labels the
/// components, spawns the loader at the chassis root, handing it the list and
/// the sending half of a channel, and blocks the calling (chassis) thread on
/// the receiving half, one answer per load. The loader sends one load at a
/// time, so the components come up in manifest order and the answer awaited
/// next always belongs to the next label: a failure or a timeout names its
/// entry with no correlation map.
///
/// The chassis is handed back once every load has answered. On a timeout it
/// is leaked rather than dropped, because its teardown would wait on the load
/// still in flight.
///
/// # Errors
///
/// Returns [`BootError::Other`] when the loader cannot be spawned, when a boot
/// component fails to load (naming the entry and the component host's error),
/// when a boot component's load does not answer within `budget` (naming the
/// entry and how many loaded before it), or when the loader stops before every
/// component has answered.
pub(crate) fn load_boot_components<C: Chassis>(
    built: BuiltChassis<C>,
    components: Vec<AutoloadComponent>,
    budget: Duration,
) -> Result<BuiltChassis<C>, BootError> {
    if components.is_empty() {
        return Ok(built);
    }

    let labels: Vec<String> = components.iter().enumerate().map(|(index, component)| component.label(index)).collect();
    let (report, answers) = mpsc::channel();
    built
        .spawn_actor::<Autoloader>(Subname::Named("boot"), (), AutoloaderParams { components, report })
        .finish()
        .map_err(|error| boot_failed(format!("spawning the boot loader: {error:?}")))?;

    match await_boot_loads(&labels, &answers, budget) {
        Ok(()) => {
            tracing::info!(loaded = labels.len(), "every boot component answered its load");
            Ok(built)
        }
        Err(BootWaitError::Failed(message)) => Err(boot_failed(message)),
        Err(BootWaitError::TimedOut(message)) => {
            // The load that never answered may still hold a worker (a guest
            // `init` that never returns), and the orderly teardown joins every
            // starting actor, so dropping the chassis would hang in place of
            // the error. Leak it instead: the boot is failing and the process
            // exits on this error.
            mem::forget(built);
            Err(boot_failed(message))
        }
    }
}

/// How the boot wait stopped short of every load answering `Ok`.
enum BootWaitError {
    /// A load answered `Err`, or the loader stopped: nothing is left in
    /// flight, so the chassis tears down normally.
    Failed(String),
    /// A load did not answer within the budget and may still be running.
    TimedOut(String),
}

/// Wait for one answer per label, in order, each within `budget`, and name
/// the label the boot stopped on: its load's error,
/// its timeout with how many loaded before it, or the loader stopping while
/// it was awaited.
fn await_boot_loads(labels: &[String], answers: &Receiver<LoadAnswer>, budget: Duration) -> Result<(), BootWaitError> {
    for (loaded, label) in labels.iter().enumerate() {
        let answer = answers.recv_timeout(budget).map_err(|error| match error {
            RecvTimeoutError::Timeout => BootWaitError::TimedOut(format!(
                "boot component {label} did not answer its load within {budget:?} ({loaded} of {} loaded)",
                labels.len(),
            )),
            RecvTimeoutError::Disconnected => {
                BootWaitError::Failed(format!("the boot loader stopped while boot component {label} was loading"))
            }
        })?;
        answer.map_err(|error| BootWaitError::Failed(format!("boot component {label}: {error}")))?;
    }
    Ok(())
}

/// Box a boot-load failure message into [`BootError::Other`].
fn boot_failed(message: String) -> BootError {
    BootError::Other(Box::new(io::Error::other(message)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(replicas: Option<u32>) -> PackedComponent {
        PackedComponent {
            wasm: vec![0, 1, 2, 3],
            config: vec![9, 9, 9],
            name: Some("handler".to_owned()),
            export: None,
            replicas,
        }
    }

    #[test]
    fn expand_replicas_fans_out_named_instances_with_shared_config() {
        // A 3-replica entry must yield 3 autoload components, each carrying
        // the same wasm + config bytes (one shared load spec) but a
        // distinct name — the bare base for replica 0, `{base}-{index}`
        // after it. The bug this catches is a fan-out that drops an
        // instance or lets two instances collide on the same name.
        let entries = expand_replicas(packed(Some(3))).expect("3 replicas expand");
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries.iter().map(|entry| entry.name.clone()).collect::<Vec<_>>(),
            vec![Some("handler".to_owned()), Some("handler-1".to_owned()), Some("handler-2".to_owned())],
        );
        for entry in &entries {
            assert_eq!(entry.wasm, vec![0, 1, 2, 3]);
            assert_eq!(entry.config, vec![9, 9, 9]);
            assert_eq!(entry.export, None);
        }
    }

    #[test]
    fn expand_replicas_no_replicas_stays_single_unmodified_instance() {
        // An entry with no `replicas` set must expand to exactly the
        // one-instance `AutoloadComponent` today's `From` conversion
        // produces — a regression guard on the default (unreplicated) path.
        let entries = expand_replicas(packed(None)).expect("no replicas expands to one");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name.as_deref(), Some("handler"));
        assert_eq!(entries[0].wasm, vec![0, 1, 2, 3]);
        assert_eq!(entries[0].config, vec![9, 9, 9]);
    }

    #[test]
    fn boot_wait_names_the_load_that_never_answers() {
        // The first load answers and the second never does: the wait must
        // expire and name the stuck entry with its progress, where an
        // unbounded wait would hold the boot forever.
        let labels = vec!["first".to_owned(), "stuck".to_owned()];
        let (report, answers) = mpsc::channel();
        report.send(Ok(())).expect("receiver is alive");
        let Err(BootWaitError::TimedOut(error)) = await_boot_loads(&labels, &answers, Duration::from_millis(50)) else {
            panic!("a load that never answers must time the wait out");
        };
        assert!(error.contains("stuck"), "the error names the stuck load: {error}");
        assert!(error.contains("1 of 2"), "the error counts the loads before it: {error}");
    }

    #[test]
    fn expand_replicas_rejects_zero() {
        // `replicas: 0` is a hard config error (ADR-0090 §4), not a silent
        // no-op that loads nothing.
        match expand_replicas(packed(Some(0))) {
            Err(ConfigError::UnparseableKnown { .. }) => {}
            Err(e) => panic!("0 replicas returned the wrong config error: {e}"),
            Ok(_) => panic!("0 replicas must be an error, not a silent no-op"),
        }
    }
}
