//! The run's volumes: one for `/work`, shared by every step's container, and
//! one per mount.
//!
//! Each is a named volume the daemon names, labelled `aether.workspace=run`,
//! rather than an anonymous one, because every step runs in its own container
//! and all of them must see the same `/work`. On a read-only root a volume
//! mount is also what lets `PUT …/archive` write before `start`.
//!
//! A mount's tree is written into its volume through one helper container
//! from the environment image, created with every mount volume writable and
//! never started. Step containers then mount those volumes read-only.

use std::collections::BTreeMap;

use aether_bloomery_journal::ArtifactBatch;
use serde_json::{Value, json};

use super::cleanup::Cleanup;
use super::{Stop, engine_failed, write_tree};
use crate::Mounts;
use crate::runtime::engine::{Engine, VolumeName};

/// The label every container and volume a run creates carries, so an
/// operator can find what a crash left behind.
pub const RUN_LABEL: (&str, &str) = ("aether.workspace", "run");

/// The volumes one run's steps mount.
pub struct Volumes {
    /// Mounted writable at `/work` in every step.
    pub work: VolumeName,
    /// Each mount's absolute path and volume, mounted read-only in every step.
    pub mounts: Vec<(String, VolumeName)>,
}

impl Volumes {
    /// Where the run's tree is written and its output read from.
    pub const WORK_PATH: &'static str = "/work";
}

/// Create the volumes and write each mount's tree into its own.
pub fn prepare(
    engine: &Engine,
    cleanup: &mut Cleanup<'_>,
    batch: &ArtifactBatch,
    image: &str,
    mounts: &Mounts,
) -> Result<Volumes, Stop> {
    let labels = BTreeMap::from([RUN_LABEL]);
    let mut create = |purpose: &str| {
        let volume = engine.create_volume(&labels).map_err(engine_failed(format!("creating the {purpose} volume")))?;
        cleanup.volume(volume.clone());
        Ok::<_, Stop>(volume)
    };

    let work = create(Volumes::WORK_PATH)?;
    let mut volumes = Vec::with_capacity(mounts.as_slice().len());
    for mount in mounts.as_slice() {
        let path = format!("/{}", mount.at.as_str());
        let volume = create(&path)?;
        volumes.push((path, volume));
    }
    if mounts.as_slice().is_empty() {
        return Ok(Volumes { work, mounts: volumes });
    }

    let helper = engine.create(&helper_spec(image, &volumes)).map_err(engine_failed("creating the mount helper"))?;
    cleanup.container(helper.clone());
    for (mount, (path, _)) in mounts.as_slice().iter().zip(&volumes) {
        write_tree(engine, batch, &helper, path, &mount.tree)?;
    }
    Ok(Volumes { work, mounts: volumes })
}

/// A container that exists only to hold the mount volumes writable while
/// their trees are written. Its command is a placeholder; it never starts.
fn helper_spec(image: &str, volumes: &[(String, VolumeName)]) -> Value {
    let mounts: Vec<Value> = volumes.iter().map(|(path, volume)| volume_mount(volume, path, false)).collect();
    json!({
        "Image": image,
        "Cmd": ["/aether-workspace-mount-helper"],
        "Labels": { RUN_LABEL.0: RUN_LABEL.1 },
        "NetworkDisabled": true,
        "HostConfig": { "Mounts": mounts, "NetworkMode": "none" },
    })
}

/// A `HostConfig.Mounts` entry for `volume` at `path`. `NoCopy` keeps the
/// daemon from seeding an empty volume with whatever the image holds at
/// `path`, so a volume holds only the tree written into it.
pub fn volume_mount(volume: &VolumeName, path: &str, read_only: bool) -> Value {
    json!({
        "Type": "volume",
        "Source": volume.as_str(),
        "Target": path,
        "ReadOnly": read_only,
        "VolumeOptions": { "NoCopy": true },
    })
}
