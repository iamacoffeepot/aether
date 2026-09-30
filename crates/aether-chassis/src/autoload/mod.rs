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
//! short of serving forever.
//!
//! Each boot component is one `Publish` of its module to the component host,
//! then one `Spawn` per instance key (issue #7155, ADR-0241 §9) — the same
//! `aether.component` mailbox the hub's `load_component` and the substrate
//! harness send through, which is what makes the mechanism
//! chassis-agnostic. `boot_manifest.rs` and `package.rs` still call this
//! entry point "load" in their doc prose where that reads more plainly; the
//! wire shape underneath is publish-then-spawn.

mod loader;

use std::io;
use std::mem;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use aether_substrate::Subname;
use aether_substrate::actor::wasm::kind_manifest;
use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::BuiltChassis;
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::ConfigError;

use crate::boot_manifest::{self, PackedComponent};

use loader::{Autoloader, AutoloaderParams, LoadAnswer};

/// A component to auto-load on boot: its wasm bytes, optional init-config
/// bytes (ADR-0090; empty for none), the namespace it publishes and spawns
/// from, and the instance keys the loader spawns it at.
///
/// `namespace` is the manifest's `export`, else the wasm-declared default
/// (its `aether.namespace` custom section) when the manifest names none;
/// when neither is available it stays `None` until the loader resolves it
/// against the module's `Publish` reply — the sole type it binds, or a
/// failure naming every type it binds (issue #7155).
///
/// `keys` is one key for an unreplicated entry (`Some(name)`, or `None` to
/// spawn the sole instance unnamed) — an unreplicated singleton always
/// spawns this way — or N `None` (counter-keyed) instances for a
/// `replicas: N` entry; a replicated entry's instances are never named,
/// since [`expand_replicas`] refuses `name` together with `replicas`.
pub struct AutoloadComponent {
    pub wasm: Vec<u8>,
    pub config: Vec<u8>,
    pub namespace: Option<String>,
    pub keys: Vec<Option<String>>,
    /// The manifest's declared export, kept apart from `namespace` — which
    /// [`expand_replicas`]'s wasm-declared-default fallback may fill — so a
    /// boot failure names exactly what the manifest wrote
    /// ([`Self::label`]), never a derived default.
    label_export: Option<String>,
}

impl AutoloadComponent {
    /// Build a component to autoload: `namespace` selects the module's
    /// exported type to publish and spawn (its label fallback too, absent a
    /// key naming the one instance), and `keys` is the instance key list
    /// [`loader::Autoloader`] spawns it at, in order.
    #[must_use]
    pub fn new(wasm: Vec<u8>, config: Vec<u8>, namespace: Option<String>, keys: Vec<Option<String>>) -> Self {
        Self { wasm, config, label_export: namespace.clone(), namespace, keys }
    }

    /// How a boot failure names this component: its one key's name (an
    /// unreplicated entry only — a replicated entry's keys are never
    /// named), else its declared export, else `#<index>` for its position
    /// in the boot list.
    fn label(&self, index: usize) -> String {
        let name = match self.keys.as_slice() {
            [one] => one.clone(),
            _ => None,
        };
        name.or_else(|| self.label_export.clone()).unwrap_or_else(|| format!("#{index}"))
    }
}

impl From<PackedComponent> for AutoloadComponent {
    /// The unreplicated conversion with no default-namespace resolution:
    /// `namespace` is exactly the manifest's `export`, `None` staying
    /// `None` until [`expand_replicas`]'s wasm-declared-default fallback
    /// fills it. [`expand_replicas`] is every caller's entry point; this
    /// impl is the plain field mapping it builds on.
    fn from(packed: PackedComponent) -> Self {
        Self::new(packed.wasm, packed.config, packed.export, vec![packed.name])
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

/// Turn one manifest entry into its [`AutoloadComponent`] (issue 2626,
/// issue #7155), resolving the namespace it publishes and spawns from and
/// the instance keys it spawns at — the one expansion site every manifest
/// writer shares: JSON boot manifests (`AETHER_BOOT_MANIFEST`), the
/// content-addressed package depot manifest, and hand-written manifests.
/// Always returns exactly one component, in a `Vec` so its callers can
/// `.extend()` a growing list uniformly.
///
/// An entry with no `replicas` set spawns the one key its `name` names
/// (`None` spawns the sole instance unnamed). Otherwise it spawns
/// `replicas` counter-keyed (`None`) instances, sharing one wasm blob and
/// one config; `name` together with `replicas` is refused, since a
/// replicated entry's instances are never named individually.
///
/// The entry's `namespace` is its `export` when set. Otherwise the wasm's
/// declared default (its `aether.namespace` custom section) fills it when
/// present; when neither is available `namespace` stays `None` and the
/// loader resolves it later against the module's `Publish` reply — the
/// sole type it binds, or a failure naming every type it binds. This is
/// the unselected-load default a defaultless multi-export module (the
/// `xtask` package sweep, which packs every component with `export: None`)
/// relied on before this rewrite, so it still resolves the same way.
///
/// # Errors
///
/// Returns a [`ConfigError`] (ADR-0090 §4: a bad known value aborts boot
/// loudly, never a silent no-op) when `replicas` is `0`, when `name` and
/// `replicas` are both set, or — only when `export` is unset, so this
/// entry's wasm is inspected for a declared default — when that wasm can't
/// be parsed or its `aether.namespace` custom section holds invalid UTF-8.
/// An entry that names its `export` is never inspected this way, so
/// malformed wasm behind an explicit `export` still fails only later, at
/// `Publish`.
pub fn expand_replicas(packed: PackedComponent) -> Result<Vec<AutoloadComponent>, ConfigError> {
    if let Some(replicas) = packed.replicas
        && replicas == 0
    {
        return Err(ConfigError::unparseable(
            "replicas",
            "0",
            io::Error::new(io::ErrorKind::InvalidInput, "replicas must be at least 1"),
        ));
    }
    if packed.replicas.is_some() && packed.name.is_some() {
        return Err(ConfigError::unparseable(
            "replicas",
            format!("name = {:?}", packed.name),
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "`replicas` and `name` cannot both be set: a replicated entry's instances are \
                 always counter-keyed, never individually named",
            ),
        ));
    }

    let namespace = match &packed.export {
        Some(export) => Some(export.clone()),
        None => kind_manifest::read_namespace_from_bytes(&packed.wasm).map_err(|error| {
            ConfigError::unparseable(
                "export",
                "wasm-declared default namespace",
                io::Error::new(io::ErrorKind::InvalidData, error),
            )
        })?,
    };
    let keys = match packed.replicas {
        Some(replicas) => vec![None; replicas as usize],
        None => vec![packed.name.clone()],
    };

    Ok(vec![AutoloadComponent::new(packed.wasm, packed.config, namespace, keys)])
}

/// Load every boot component in list order and wait until each has answered
/// its load `Ok` (issue #6413), each within `budget` (issue #6637).
///
/// With an empty list this returns at once. Otherwise it labels the
/// components, spawns the loader at the chassis root, handing it the list and
/// the sending half of a channel, and blocks the calling (chassis) thread on
/// the receiving half, one answer per label. The loader publishes and spawns
/// one component at a time, so the components come up in manifest order and
/// the answer awaited next always belongs to the next label: a failure or a
/// timeout names its entry with no correlation map.
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
            name: None,
            export: Some("test.handler".to_owned()),
            replicas,
        }
    }

    #[test]
    fn expand_replicas_fans_out_counter_keyed_instances_with_shared_config() {
        // A 3-replica entry must yield one `AutoloadComponent` carrying 3
        // `None` (counter-keyed) instance keys and the shared wasm + config
        // bytes (one shared load spec). The bug this catches is a fan-out
        // that drops an instance or produces a named (rather than
        // counter-keyed) replica.
        let entries = expand_replicas(packed(Some(3))).expect("3 replicas expand");
        assert_eq!(entries.len(), 1, "one AutoloadComponent carries every replica's keys");
        let entry = &entries[0];
        assert_eq!(entry.keys, vec![None, None, None]);
        assert_eq!(entry.wasm, vec![0, 1, 2, 3]);
        assert_eq!(entry.config, vec![9, 9, 9]);
        assert_eq!(entry.namespace.as_deref(), Some("test.handler"));
    }

    #[test]
    fn expand_replicas_no_replicas_stays_single_named_instance() {
        // An entry with no `replicas` set must expand to exactly one
        // `AutoloadComponent` with one key — the caller's `name` — a
        // regression guard on the default (unreplicated) path.
        let mut entry = packed(None);
        entry.name = Some("handler".to_owned());
        let entries = expand_replicas(entry).expect("no replicas expands to one");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].keys, vec![Some("handler".to_owned())]);
        assert_eq!(entries[0].wasm, vec![0, 1, 2, 3]);
        assert_eq!(entries[0].config, vec![9, 9, 9]);
    }

    #[test]
    fn expand_replicas_rejects_name_with_replicas() {
        // `name` together with `replicas` is meaningless under counter-keyed
        // replicas and must abort boot loudly, not silently pick one
        // instance to name.
        let mut entry = packed(Some(2));
        entry.name = Some("handler".to_owned());
        match expand_replicas(entry) {
            Err(ConfigError::UnparseableKnown { .. }) => {}
            Err(e) => panic!("name + replicas returned the wrong config error: {e}"),
            Ok(_) => panic!("name + replicas must be an error, not a silent pick"),
        }
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
