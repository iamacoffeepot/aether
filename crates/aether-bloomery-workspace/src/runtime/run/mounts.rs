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
use crate::Mounts;
use crate::Refusal;
use crate::runtime::engine::{Engine, EngineError, VolumeName};
use crate::runtime::storage::StorageSession;

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
    let mut resolved: Vec<Option<(String, VolumeName)>> = Vec::with_capacity(mounts.as_slice().len());
    resolved.resize_with(mounts.as_slice().len(), || None);
    let mut pending: Vec<Pending> = Vec::new();

    for (index, mount) in mounts.as_slice().iter().enumerate() {
        let path = format!("/{}", mount.at.as_str());
        let hex = mount.tree.digest().to_string();
        let pointer = pointer_name(&hex)?;
        let labels =
            engine.volume_labels(&pointer).map_err(engine_failed(format!("inspecting mount volume {pointer}")))?;
        if let Some(labels) = labels {
            resolved[index] = Some((path, hit(&labels, &hex).map_err(|()| Stop::refused(Refusal::MountUnavailable))?));
        } else {
            let data = engine
                .create_volume(None, &BTreeMap::from([(MOUNT_LABEL, hex.as_str())]))
                .map_err(engine_failed(format!("creating the {path} volume")))?;
            cleanup.volume(data.clone());
            pending.push(Pending { index, path, hex, pointer, data });
        }
    }

    if pending.is_empty() {
        return Ok(resolved.into_iter().flatten().collect());
    }

    let helper_volumes: Vec<(String, VolumeName)> =
        pending.iter().map(|miss| (miss.path.clone(), miss.data.clone())).collect();
    let helper =
        engine.create(&helper_spec(image, &helper_volumes)).map_err(engine_failed("creating the mount helper"))?;
    cleanup.container(helper.clone());
    for miss in &pending {
        let tree = &mounts.as_slice()[miss.index].tree;
        write_tree(engine, session, &helper, &miss.path, tree)?;
    }

    for miss in &pending {
        match engine.create_volume(
            Some(miss.pointer.as_str()),
            &BTreeMap::from([(MOUNT_LABEL, miss.hex.as_str()), (DATA_LABEL, miss.data.as_str())]),
        ) {
            Ok(_) => {
                cleanup.release(&miss.data);
                resolved[miss.index] = Some((miss.path.clone(), miss.data.clone()));
            }
            Err(EngineError::Status { status: 409, .. }) => {
                resolved[miss.index] = Some((miss.path.clone(), adopted(engine, &miss.pointer, &miss.hex)?));
            }
            Err(error) => return Err(engine_failed(format!("creating mount volume {}", miss.pointer))(error).into()),
        }
    }

    Ok(resolved.into_iter().flatten().collect())
}

/// The deterministic pointer volume name for `hex`.
fn pointer_name(hex: &str) -> Result<VolumeName, Stop> {
    VolumeName::new(&format!("{POINTER_PREFIX}{hex}")).map_err(|error| {
        Stop::failed(RunError::Shape(format!("a mount pointer name for {hex} is not a volume name: {error}")))
    })
}

/// The data volume a pointer names, when its mount label equals `hex`.
fn hit(labels: &BTreeMap<String, String>, hex: &str) -> Result<VolumeName, ()> {
    if labels.get(MOUNT_LABEL) != Some(&hex.to_owned()) {
        return Err(());
    }
    labels.get(DATA_LABEL).and_then(|name| VolumeName::new(name).ok()).ok_or(())
}

/// The data volume `pointer` names, re-inspected after a lost pointer race.
fn adopted(engine: &Engine, pointer: &VolumeName, hex: &str) -> Result<VolumeName, Stop> {
    engine
        .volume_labels(pointer)
        .map_err(engine_failed(format!("inspecting mount volume {pointer}")))?
        .as_ref()
        .ok_or(())
        .and_then(|labels| hit(labels, hex))
        .map_err(|()| Stop::refused(Refusal::MountUnavailable))
}

/// A mount that missed its pointer and holds a freshly written data volume
/// waiting for its pointer.
struct Pending {
    index: usize,
    path: String,
    hex: String,
    pointer: VolumeName,
    data: VolumeName,
}
