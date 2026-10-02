//! The run's volumes: one for `/work`, shared by every step's container, plus
//! each mount's cached volume.
//!
//! `/work` is a named volume the daemon names, labelled
//! `aether.workspace=run`, rather than an anonymous one, because every step
//! runs in its own container and all of them must see the same `/work`. On a
//! read-only root a volume mount is also what lets `PUT …/archive` write
//! before `start`. `/work` is a cold run's only per-run volume; a warm run
//! adds its layer's (see [`super::layers`]). Mount data and pointer volumes
//! persist as rebuildable derivatives of the journal (see [`super::mounts`])
//! and are never registered for per-run removal.
//!
//! A missed mount's tree is written into its data volume through one helper
//! container from the environment image, created with each missed mount
//! volume writable and never started. Step containers then mount those
//! volumes read-only.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::cleanup::Cleanup;
use super::mounts;
use super::{Stop, engine_failed};
use crate::Mounts;
use crate::runtime::engine::{Engine, VolumeName};
use crate::runtime::storage::StorageSession;

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

/// Create the `/work` volume and make sure each mount's cached volume holds
/// its tree.
pub fn prepare(
    engine: &Engine,
    cleanup: &mut Cleanup<'_>,
    session: &mut StorageSession,
    image: &str,
    mounts: &Mounts,
) -> Result<Volumes, Stop> {
    let work = engine
        .create_volume(None, &BTreeMap::from([RUN_LABEL]), &BTreeMap::new())
        .map_err(engine_failed(format!("creating the {} volume", Volumes::WORK_PATH)))?
        .name;
    cleanup.volume(work.clone());

    Ok(Volumes { work, mounts: mounts::ensure(engine, cleanup, session, image, mounts)? })
}

/// A container that exists only to hold `volumes` writable: the missed
/// mount volumes while their trees are written, or a warm run's `/work` while
/// its output is read. Its command is a placeholder; it never starts.
pub fn helper_spec(image: &str, volumes: &[(String, VolumeName)]) -> Value {
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
