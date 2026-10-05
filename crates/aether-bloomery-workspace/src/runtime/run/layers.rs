//! Warm layers (ADR-0237 decision 11): cargo's build output kept between
//! runs as executor state, never in an output tree, the journal, or a result.
//!
//! A run is warm when the knob is on, its tree's root holds a `Cargo.lock`
//! file, and cargo's target directory is one of its scratch paths: every
//! step's merged environment names the same `CARGO_TARGET_DIR` of the form
//! `/work/<scratch>`, or none names it and `target` is a scratch path. Only a
//! scratch path is layered, so a layer's bytes can never reach the output.
//!
//! A layer is identified by the unit (the run's source path), the run key,
//! and the lock's digest, so a changed lock or environment writes a new
//! layer beside the old one and no layer a running overlay sits on is ever
//! replaced. Like a mount ([`super::mounts`]), a bottom layer is a data volume
//! the daemon names and a pointer volume named `POINTER_PREFIX` plus the
//! layer's hex, created only after the data volume is complete:
//!
//! - **Miss.** No pointer: a fresh data volume is mounted writable at the
//!   target directory, so the first run builds straight into it. Once its
//!   steps ran to their exits, whatever they exited with, the pointer is
//!   created, recording the tree the run built over, and the data volume
//!   stays. A run that ends any other way, or loses the pointer race, leaves
//!   it registered for per-run removal.
//! - **Hit.** The pointer and the data volume it names both carry the
//!   layer's hex, and the data volume's `Mountpoint` is the `lowerdir` of an
//!   `overlay` volume of the run's own, over an upper and a work volume
//!   created for the run. All three are removed with the run, so no run sees
//!   another's writes, and the bottom layer is never written again. The
//!   run's tree is uploaded with every file that differs from the layer's
//!   recorded tree stamped with the run's start, so cargo rebuilds exactly
//!   what changed since the layer was built; a pointer that records no tree,
//!   or one whose tree the run's source does not store (a journal replaced at
//!   the same path), stamps every file.
//!
//! Every layer byte is written and removed by the daemon: the steps write as
//! uid 0, and the actor may not share the daemon's host. A layer that does
//! not check out (a pointer or data volume labelled for another layer, a
//! data volume gone, a mountpoint the overlay options cannot carry) runs
//! cold with a warning: a layer never decides whether a run happens or what
//! it answers.

use std::collections::BTreeMap;

use aether_bloomery_kinds::Tree;
use aether_bloomery_tar::Stamp;
use aether_data::{Digest, Ref, hash_bytes};

use super::cleanup::Cleanup;
use super::volumes::{RUN_LABEL, Volumes};
use super::{RunError, Stop, engine_failed, storage_stop};
use crate::runtime::engine::{Engine, EngineError, Volume, VolumeName};
use crate::runtime::provision::RunKey;
use crate::runtime::storage::{SourceReader, StorageError};
use crate::{EnvVar, RunRequest};

/// The label a layer's data volume and its pointer carry with the layer's
/// hex.
pub const LAYER_LABEL: &str = "aether.workspace.layer";

/// The label a layer's data volume and its pointer carry with the digest of
/// the `Cargo.lock` it was built over.
pub const LOCK_LABEL: &str = "aether.workspace.layer.lock";

/// The label a layer's pointer carries with its data volume's name.
pub const DATA_LABEL: &str = "aether.workspace.layer.data";

/// The label a layer's pointer carries with the hex digest of the tree its
/// data volume was built over.
pub const TREE_LABEL: &str = "aether.workspace.layer.tree";

/// The length of a digest in hex.
const DIGEST_HEX_LEN: usize = 64;

/// The prefix of a layer pointer volume's deterministic name.
pub const POINTER_PREFIX: &str = "aether-workspace-layer-";

/// The domain tag every layer id's hash input starts with.
const DOMAIN: &[u8] = b"aether.workspace.layer.v1";

/// The variable that moves cargo's target directory.
const TARGET_VARIABLE: &str = "CARGO_TARGET_DIR";

/// Cargo's target directory under `/work` when nothing moves it.
const DEFAULT_TARGET: &str = "target";

/// The characters an overlay `o=` option string cannot carry in a path: its
/// option separator, its lower-directory separator, and its escape.
const OPTION_SEPARATORS: [char; 3] = [',', ':', '\\'];

/// The layer a warm run builds over: where it is mounted and what it is
/// keyed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    /// The absolute target directory, `/work/<scratch>`.
    pub at: String,
    /// The layer's id, in hex.
    hex: String,
    /// The `Cargo.lock` blob's digest, in hex.
    lock: String,
}

impl Wanted {
    /// Whether `volume` carries this layer's hex.
    fn labels(&self, volume: &Volume) -> bool {
        volume.labels.get(LAYER_LABEL) == Some(&self.hex)
    }
}

/// The layer `run` builds over, or `None` when it builds cold: `scratch`
/// holds no target directory every step agrees on, or the tree has no lock.
pub fn wanted(unit: &str, key: RunKey, run: &RunRequest, base_env: &[EnvVar], lock: Option<Digest>) -> Option<Wanted> {
    let lock = lock?;
    let at = target(run, base_env)?;
    let hex = id(unit, key, lock).to_string();
    Some(Wanted { at, hex, lock: lock.to_string() })
}

/// The absolute target directory every step's merged environment names,
/// when it is a scratch path of `run`.
fn target(run: &RunRequest, base_env: &[EnvVar]) -> Option<String> {
    let mut named = run.steps.as_slice().iter().map(|step| target_of(base_env, &step.env));
    let first = named.next()?;
    let agreed = named.all(|other| other == first);
    if !agreed {
        return None;
    }
    let default = format!("{}/{DEFAULT_TARGET}", Volumes::WORK_PATH);
    let at = first.map_or(default, str::to_owned);
    let relative = at.strip_prefix(Volumes::WORK_PATH)?.strip_prefix('/')?;
    let scratch = run.scratch.as_slice().iter().any(|path| path.as_str() == relative);
    scratch.then_some(at)
}

/// The target directory `base` overlaid by `step` names, the last setting
/// winning as it does when the step's environment is built.
fn target_of<'a>(base: &'a [EnvVar], step: &'a [EnvVar]) -> Option<&'a str> {
    base.iter().chain(step).rfind(|var| var.key() == TARGET_VARIABLE).map(EnvVar::value)
}

/// The layer id: sha256 over the domain tag, then the unit, the run key, and
/// the lock digest, each prefixed by its length as a u64 LE.
fn id(unit: &str, key: RunKey, lock: Digest) -> Digest {
    let mut input = Vec::from(DOMAIN);
    for field in [unit.as_bytes(), key.digest().as_bytes(), lock.as_bytes()] {
        input.extend_from_slice(&u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
        input.extend_from_slice(field);
    }
    hash_bytes(&input)
}

/// The volume a warm run mounts writable at its target directory.
#[derive(Debug)]
pub enum Layer {
    /// A fresh bottom layer the run builds into, marked complete by
    /// [`complete`] once its steps ran.
    Writing { wanted: Wanted, pointer: VolumeName, data: VolumeName },
    /// The run's own overlay over a complete bottom layer, and the tree that
    /// layer was built over, when its pointer records one.
    Over { wanted: Wanted, overlay: VolumeName, base: Option<Ref<Tree>> },
}

impl Layer {
    /// The absolute path the layer is mounted at.
    pub fn at(&self) -> &str {
        match self {
            Self::Writing { wanted, .. } | Self::Over { wanted, .. } => &wanted.at,
        }
    }

    /// The volume mounted there.
    pub fn volume(&self) -> &VolumeName {
        match self {
            Self::Writing { data, .. } => data,
            Self::Over { overlay, .. } => overlay,
        }
    }

    /// How the run's tree `tree` is uploaded over this layer: canonical over
    /// a fresh layer, which holds no build output a stale mtime could fool,
    /// and stamped with `mtime_secs` against the layer's base over a complete
    /// one. A recorded base the `source` does not store, or stores as another
    /// kind, is dropped with a warning, so every file is stamped: the layer
    /// outlives a journal fork, and the run's answer never depends on it.
    pub fn stamp_over(
        &self,
        source: &mut SourceReader<'_>,
        tree: &Ref<Tree>,
        mtime_secs: u64,
    ) -> Result<Option<Stamp>, Stop> {
        let Self::Over { wanted, base, .. } = self else {
            return Ok(None);
        };
        let Some(recorded) = *base else {
            return Ok(Some(Stamp { base: None, mtime_secs }));
        };
        let is_run_tree = recorded == *tree;
        if is_run_tree {
            return Ok(Some(Stamp { base: Some(recorded), mtime_secs }));
        }

        let base = match source.load::<Tree>(&recorded) {
            Ok(_) => Some(recorded),
            Err(StorageError::Missing(digest) | StorageError::OtherKind(digest)) if digest == recorded.digest() => {
                tracing::warn!(
                    target: "aether_bloomery_workspace",
                    layer = %wanted.hex,
                    base = %digest,
                    "the layer's recorded tree is not in the run's source: every file is stamped"
                );
                None
            }
            Err(error) => return Err(storage_stop("reading the layer's recorded tree", error)),
        };
        Ok(Some(Stamp { base, mtime_secs }))
    }
}

/// Make the volume `wanted` mounts: the run's overlay over a complete bottom
/// layer, or a fresh bottom layer to write. `None` runs cold.
pub fn prepare(engine: &Engine, cleanup: &mut Cleanup<'_>, wanted: Wanted) -> Result<Option<Layer>, Stop> {
    let pointer = VolumeName::new(&format!("{POINTER_PREFIX}{}", wanted.hex))
        .map_err(|error| RunError::Shape(format!("a layer pointer name is not a volume name: {error}")))?;
    let Some(pointing) = inspect(engine, &pointer)? else {
        let labels = BTreeMap::from([(LAYER_LABEL, wanted.hex.as_str()), (LOCK_LABEL, wanted.lock.as_str())]);
        let data = engine
            .create_volume(None, &labels, &BTreeMap::new())
            .map_err(engine_failed(format!("creating the {} layer volume", wanted.at)))?
            .name;
        cleanup.volume(data.clone());
        return Ok(Some(Layer::Writing { wanted, pointer, data }));
    };

    let bottom = match checked(engine, &wanted, &pointing)? {
        Ok(bottom) => bottom,
        Err(why) => {
            tracing::warn!(target: "aether_bloomery_workspace", %pointer, why, "the run builds cold: its layer does not check out");
            return Ok(None);
        }
    };
    let base = base_of(&pointing);
    overlay(engine, cleanup, wanted, &bottom, base).map(Some)
}

/// The tree a complete layer's pointer records it was built over: exactly
/// [`DIGEST_HEX_LEN`] lowercase hex digits under [`TREE_LABEL`], or `None`
/// for a pointer written before the label existed or one that is malformed.
fn base_of(pointing: &Volume) -> Option<Ref<Tree>> {
    let hex = pointing.labels.get(TREE_LABEL)?.as_bytes();
    if hex.len() != DIGEST_HEX_LEN {
        return None;
    }
    let mut bytes = [0; DIGEST_HEX_LEN / 2];
    for (byte, pair) in bytes.iter_mut().zip(hex.chunks_exact(2)) {
        *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(Ref::from_digest(Digest::from_bytes(bytes)))
}

/// The value of one lowercase hex digit.
fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

/// The volume `name`, or `None` when the daemon holds none.
fn inspect(engine: &Engine, name: &VolumeName) -> Result<Option<Volume>, Stop> {
    Ok(engine.inspect_volume(name).map_err(engine_failed(format!("inspecting layer volume {name}")))?)
}

/// The bottom layer's mountpoint when the pointer and the data volume it
/// names both carry `wanted`'s hex, or why not.
fn checked(engine: &Engine, wanted: &Wanted, pointing: &Volume) -> Result<Result<String, &'static str>, Stop> {
    let pointer_ours = wanted.labels(pointing);
    if !pointer_ours {
        return Ok(Err("its pointer is labelled for another layer"));
    }
    let Some(data) = pointing.labels.get(DATA_LABEL).and_then(|data| VolumeName::new(data).ok()) else {
        return Ok(Err("its pointer names no data volume"));
    };
    let Some(data) = inspect(engine, &data)? else {
        return Ok(Err("its data volume is gone"));
    };
    let data_ours = wanted.labels(&data);
    if !data_ours {
        return Ok(Err("its data volume is labelled for another layer"));
    }
    Ok(data.mountpoint.filter(|mountpoint| carried(mountpoint)).ok_or("its data volume has no usable mountpoint"))
}

/// Whether `mountpoint` can stand in an overlay `o=` option.
fn carried(mountpoint: &str) -> bool {
    mountpoint.starts_with('/') && !mountpoint.contains(OPTION_SEPARATORS)
}

/// Create the run's upper and work volumes and its overlay over `bottom`,
/// each registered for per-run removal.
fn overlay(
    engine: &Engine,
    cleanup: &mut Cleanup<'_>,
    wanted: Wanted,
    bottom: &str,
    base: Option<Ref<Tree>>,
) -> Result<Layer, Stop> {
    let upper = scratch_volume(engine, cleanup, &wanted, "upper")?;
    let work = scratch_volume(engine, cleanup, &wanted, "work")?;
    let options = format!("lowerdir={bottom},upperdir={upper},workdir={work}");
    let overlay = engine
        .create_volume(
            None,
            &BTreeMap::from([RUN_LABEL]),
            &BTreeMap::from([("type", "overlay"), ("device", "overlay"), ("o", options.as_str())]),
        )
        .map_err(engine_failed(format!("creating the {} overlay volume", wanted.at)))?
        .name;
    cleanup.volume(overlay.clone());
    Ok(Layer::Over { wanted, overlay, base })
}

/// Create one of the overlay's own directories as a run volume and answer
/// its mountpoint.
fn scratch_volume(engine: &Engine, cleanup: &mut Cleanup<'_>, wanted: &Wanted, role: &str) -> Result<String, Stop> {
    let created = engine
        .create_volume(None, &BTreeMap::from([RUN_LABEL]), &BTreeMap::new())
        .map_err(engine_failed(format!("creating the {} overlay {role} volume", wanted.at)))?;
    cleanup.volume(created.name.clone());
    created.mountpoint.filter(|mountpoint| carried(mountpoint)).ok_or_else(|| {
        RunError::Shape(format!("the {} overlay {role} volume has no mountpoint an overlay can name", wanted.at)).into()
    })
}

/// Mark a bottom layer the run wrote complete, once its steps ran to their
/// exits: create its pointer, recording `tree` as the tree the layer was
/// built over, and keep its data volume. A lost race or a failed create
/// leaves the data volume registered for removal; the run's answer never
/// depends on it.
pub fn complete(engine: &Engine, cleanup: &mut Cleanup<'_>, layer: &Layer, tree: &Ref<Tree>) {
    let Layer::Writing { wanted, pointer, data } = layer else {
        return;
    };
    let tree = tree.digest().to_string();
    let labels = BTreeMap::from([
        (LAYER_LABEL, wanted.hex.as_str()),
        (LOCK_LABEL, wanted.lock.as_str()),
        (DATA_LABEL, data.as_str()),
        (TREE_LABEL, tree.as_str()),
    ]);
    match engine.create_volume(Some(pointer), &labels, &BTreeMap::new()) {
        Ok(_) => cleanup.release(data),
        Err(EngineError::Status { status: 409, .. }) => {
            tracing::info!(target: "aether_bloomery_workspace", %pointer, "another run completed this layer first");
        }
        Err(error) => {
            tracing::warn!(target: "aether_bloomery_workspace", %pointer, %error, "marking the layer complete failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::error::Error;

    use aether_bloomery_kinds::Tree;
    use aether_data::{Digest, Ref, hash_bytes};

    use super::{TREE_LABEL, base_of, wanted};
    use crate::runtime::engine::{Volume, VolumeName};
    use crate::runtime::provision::RunKey;
    use crate::{EnvVar, Mounts, Network, RunRequest, Scratch, Step, Steps, ToolName, TreePath};

    type TestResult = Result<(), Box<dyn Error>>;

    fn digest(byte: u8) -> Digest {
        Digest::from_bytes([byte; 32])
    }

    fn env(vars: &[(&str, &str)]) -> Result<Vec<EnvVar>, Box<dyn Error>> {
        Ok(vars.iter().map(|&(key, value)| EnvVar::new(key, value)).collect::<Result<_, _>>()?)
    }

    /// A run of one step per entry of `steps`, each with that environment,
    /// over `scratch`.
    fn run(steps: &[&[(&str, &str)]], scratch: &[&str]) -> Result<RunRequest, Box<dyn Error>> {
        let steps = steps
            .iter()
            .map(|vars| Ok(Step { tool: ToolName::new("cargo")?, args: Vec::new(), env: env(vars)?, stdin: None }))
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        Ok(RunRequest {
            tree: Ref::<Tree>::from_digest(digest(1)),
            environment: Ref::from_digest(digest(2)),
            mounts: Mounts::new(Vec::new())?,
            steps: Steps::new(steps)?,
            scratch: Scratch::new(scratch.iter().map(|&path| TreePath::new(path)).collect::<Result<_, _>>()?)?,
            network: Network::Off,
        })
    }

    #[test]
    fn the_layer_sits_at_the_scratch_path_cargo_targets_and_nowhere_else() -> TestResult {
        // Catches a layer at a path that is not scratch (its bytes would reach the output tree) and a layer under
        // steps that disagree on the target.
        let lock = Some(digest(3));
        let key = |run: &RunRequest| RunKey::of(run);
        let moved = [("CARGO_TARGET_DIR", "/work/out")];
        let cases: [(RunRequest, &[EnvVar], Option<&str>); 6] = [
            (run(&[&[]], &["target", "tmp"])?, &[], Some("/work/target")),
            (run(&[&moved], &["out"])?, &[], Some("/work/out")),
            (run(&[&moved], &["target"])?, &[], None),
            (run(&[&[]], &["tmp"])?, &[], None),
            (run(&[&moved, &[]], &["out", "target"])?, &[], None),
            (run(&[&[("CARGO_TARGET_DIR", "/elsewhere")]], &["target"])?, &[], None),
        ];
        for (run, base, expected) in cases {
            let at = wanted("unit", key(&run), &run, base, lock).map(|wanted| wanted.at);
            assert_eq!(at.as_deref(), expected, "{run:?}");
        }

        let base = env(&moved)?;
        let over_base = run(&[&[]], &["out"])?;
        let at = wanted("unit", key(&over_base), &over_base, &base, lock).map(|wanted| wanted.at);
        assert_eq!(at.as_deref(), Some("/work/out"));
        let plain = run(&[&[]], &["target"])?;
        assert_eq!(wanted("unit", key(&plain), &plain, &[], None), None, "no lock, no layer");
        Ok(())
    }

    #[test]
    fn a_layer_differs_by_unit_and_by_lock() -> TestResult {
        // Catches a lock or a unit left out of the identity: a changed lock would reuse a layer built over another
        // dependency graph, and two units would share one.
        let plain = run(&[&[]], &["target"])?;
        let key = RunKey::of(&plain);
        let hex = |unit: &str, lock: u8| wanted(unit, key, &plain, &[], Some(digest(lock))).map(|wanted| wanted.hex);
        assert_eq!(hex("a", 3), hex("a", 3));
        assert_ne!(hex("a", 3), hex("a", 4));
        assert_ne!(hex("a", 3), hex("b", 3));
        Ok(())
    }

    #[test]
    fn a_pointer_reads_back_the_tree_it_recorded_and_no_other() -> TestResult {
        // Catches a pointer with no tree label, from before the label existed, or a malformed one, misread as a base:
        // the upload would compare against a tree the layer was not built over and leave changed files at the
        // canonical mtime. And a recorded tree that does not read back would stamp every file, losing the warmth.
        let tree = Ref::<Tree>::from_digest(hash_bytes(b"the tree the layer was built over"));
        let hex = tree.digest().to_string();
        let pointer = |label: Option<String>| -> Result<Volume, Box<dyn Error>> {
            let labels = label.map(|label| (TREE_LABEL.to_owned(), label)).into_iter().collect::<BTreeMap<_, _>>();
            Ok(Volume { name: VolumeName::new("aether-workspace-layer-0")?, labels, mountpoint: None })
        };

        assert_eq!(base_of(&pointer(Some(hex.clone()))?), Some(tree));
        let malformed = [None, Some(hex.to_uppercase()), Some(hex[1..].to_owned()), Some(format!("{}g", &hex[1..]))];
        for label in malformed {
            assert_eq!(base_of(&pointer(label.clone())?), None, "{label:?}");
        }
        Ok(())
    }
}
