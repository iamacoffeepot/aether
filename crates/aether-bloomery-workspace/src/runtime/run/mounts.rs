//! A mount's tree in the daemon: a rebuildable derivative of the journal,
//! named and labelled by the mount tree's digest.
//!
//! Each mount is two volumes. The data volume holds the tree, is named by
//! the daemon, and carries `MOUNT_LABEL` with the tree's hex. The pointer
//! volume is empty, is named `POINTER_PREFIX` plus the hex, and carries
//! `MOUNT_LABEL` with the hex and `DATA_LABEL` with the data volume's name.
//! The pointer is created only after the tree is written, so a pointer always
//! names a complete tree: a label set before the write would survive a crash
//! mid-write as a half-written volume every later run trusts.
//!
//! Before every use the pointer is inspected. A hit mounts the data volume
//! it names. A miss writes a new data volume through the helper container
//! and then creates the pointer. Two runs that miss the same digest at once
//! both write; the first pointer create wins, and the loser, answered 409,
//! re-inspects, mounts the winner's data volume, and leaves its own data
//! volume registered so per-run removal reclaims it. A data volume no
//! pointer names is a crash leftover: wasted disk, never a wrong mount. A
//! pointer whose mount label is not the cited hex is
//! `Refused(MountUnavailable)`, never overwritten.

use std::collections::BTreeMap;

use super::cleanup::Cleanup;
use super::volumes::helper_spec;
use super::{RunError, Stop, engine_failed, write_tree};
use crate::Refusal;
use crate::runtime::engine::{Engine, EngineError, VolumeName};
use crate::runtime::storage::StorageSession;
use crate::{Mount, Mounts};

/// The label a mount data volume and its pointer carry with the tree's hex.
pub const MOUNT_LABEL: &str = "aether.workspace.mount";

/// The label a mount pointer carries with its data volume's name.
pub const DATA_LABEL: &str = "aether.workspace.mount.data";

/// The prefix of a mount pointer volume's deterministic name.
pub const POINTER_PREFIX: &str = "aether-workspace-mount-";

/// Make sure the daemon holds every mount's tree and return each mount's
/// absolute path with the data volume to mount read-only, in request order.
pub fn ensure(
    engine: &Engine,
    cleanup: &mut Cleanup<'_>,
    session: &mut StorageSession,
    image: &str,
    mounts: &Mounts,
) -> Result<Vec<(String, VolumeName)>, Stop> {
    let inspected =
        mounts.as_slice().iter().map(|mount| inspect(engine, cleanup, mount)).collect::<Result<Vec<_>, _>>()?;

    write_misses(engine, cleanup, session, image, &inspected)?;

    inspected.into_iter().map(|mount| mount.resolve(engine, cleanup)).collect()
}

/// One mount once its pointer is inspected.
struct Inspected<'mounts> {
    mount: &'mounts Mount,
    /// The absolute path the mount is seen at.
    path: String,
    /// The mount tree's digest, in hex.
    hex: String,
    found: Found,
}

/// What a mount's pointer inspect found.
enum Found {
    /// The pointer names a complete data volume.
    Hit(VolumeName),
    /// No pointer: a fresh data volume, registered for removal until a
    /// pointer names it.
    Miss { pointer: VolumeName, data: VolumeName },
}

/// Inspect `mount`'s pointer, creating a fresh data volume on a miss.
fn inspect<'mounts>(
    engine: &Engine,
    cleanup: &mut Cleanup<'_>,
    mount: &'mounts Mount,
) -> Result<Inspected<'mounts>, Stop> {
    let path = format!("/{}", mount.at.as_str());
    let hex = mount.tree.digest().to_string();
    let pointer = VolumeName::new(&format!("{POINTER_PREFIX}{hex}"))
        .map_err(|error| RunError::Shape(format!("a mount pointer name for {hex} is not a volume name: {error}")))?;

    let inspected =
        engine.inspect_volume(&pointer).map_err(engine_failed(format!("inspecting mount volume {pointer}")))?;
    let found = if let Some(pointing) = inspected {
        Found::Hit(pointed(&pointing.labels, &hex)?)
    } else {
        let data = engine
            .create_volume(None, &BTreeMap::from([(MOUNT_LABEL, hex.as_str())]), &BTreeMap::new())
            .map_err(engine_failed(format!("creating the {path} volume")))?
            .name;
        cleanup.volume(data.clone());
        Found::Miss { pointer, data }
    };
    Ok(Inspected { mount, path, hex, found })
}

/// Write every missed mount's tree into its fresh data volume through one
/// helper container, created only when something missed.
fn write_misses(
    engine: &Engine,
    cleanup: &mut Cleanup<'_>,
    session: &mut StorageSession,
    image: &str,
    inspected: &[Inspected<'_>],
) -> Result<(), Stop> {
    let misses: Vec<(String, VolumeName)> = inspected
        .iter()
        .filter_map(|mount| match &mount.found {
            Found::Miss { data, .. } => Some((mount.path.clone(), data.clone())),
            Found::Hit(_) => None,
        })
        .collect();
    if misses.is_empty() {
        return Ok(());
    }

    let helper = engine.create(&helper_spec(image, &misses)).map_err(engine_failed("creating the mount helper"))?;
    cleanup.container(helper.clone());
    for mount in inspected.iter().filter(|mount| matches!(mount.found, Found::Miss { .. })) {
        write_tree(engine, session, &helper, &mount.path, &mount.mount.tree, None)?;
    }
    Ok(())
}

impl Inspected<'_> {
    /// The mount's path and the data volume to mount: a hit's as found, a
    /// miss's once its pointer names it.
    fn resolve(self, engine: &Engine, cleanup: &mut Cleanup<'_>) -> Result<(String, VolumeName), Stop> {
        let data = match self.found {
            Found::Hit(data) => data,
            Found::Miss { pointer, data } => point(engine, cleanup, &self.hex, &pointer, data)?,
        };
        Ok((self.path, data))
    }
}

/// Create `pointer` naming the written `data`, and answer the data volume
/// to mount. A lost race (409) answers the winner's data volume and leaves
/// `data` registered, so per-run removal reclaims it.
fn point(
    engine: &Engine,
    cleanup: &mut Cleanup<'_>,
    hex: &str,
    pointer: &VolumeName,
    data: VolumeName,
) -> Result<VolumeName, Stop> {
    let labels = BTreeMap::from([(MOUNT_LABEL, hex), (DATA_LABEL, data.as_str())]);
    match engine.create_volume(Some(pointer), &labels, &BTreeMap::new()) {
        Ok(_) => {
            cleanup.release(&data);
            Ok(data)
        }
        Err(EngineError::Status { status: 409, .. }) => engine
            .inspect_volume(pointer)
            .map_err(engine_failed(format!("inspecting mount volume {pointer}")))?
            .map_or_else(|| Err(Stop::refused(Refusal::MountUnavailable)), |pointing| pointed(&pointing.labels, hex)),
        Err(error) => Err(engine_failed(format!("creating mount volume {pointer}"))(error).into()),
    }
}

/// The data volume a pointer's `labels` name, refused unless they name `hex`.
fn pointed(labels: &BTreeMap<String, String>, hex: &str) -> Result<VolumeName, Stop> {
    let unavailable = || Stop::refused(Refusal::MountUnavailable);
    if labels.get(MOUNT_LABEL).map(String::as_str) != Some(hex) {
        return Err(unavailable());
    }
    labels.get(DATA_LABEL).and_then(|data| VolumeName::new(data).ok()).ok_or_else(unavailable)
}
