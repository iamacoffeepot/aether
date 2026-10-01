//! Every daemon object a run creates, removed on every path.
//!
//! Each container and the `/work` volume is registered the moment the daemon
//! answers its create, and [`Cleanup::finish`] removes them all: every
//! container first (a volume a container still names cannot go), then every
//! volume. A failed removal does not stop the others. The environment image
//! and the mount data and pointer volumes stay; they are rebuildable
//! derivatives of the journal, named by digest and checked before every use.
//! A freshly written mount data volume stays registered until its pointer is
//! created, so a lost pointer race or a failed write still removes it.

use std::error::Error;
use std::fmt;

use crate::runtime::engine::{ContainerId, Engine, EngineError, VolumeName};

/// The containers and volumes one run has created so far.
pub struct Cleanup<'engine> {
    engine: &'engine Engine,
    containers: Vec<ContainerId>,
    volumes: Vec<VolumeName>,
}

impl<'engine> Cleanup<'engine> {
    pub fn new(engine: &'engine Engine) -> Self {
        Self { engine, containers: Vec::new(), volumes: Vec::new() }
    }

    /// Remove `container` when the run ends.
    pub fn container(&mut self, container: ContainerId) {
        self.containers.push(container);
    }

    /// Remove `volume` when the run ends, after every container. Only the
    /// `/work` volume and a mount data volume still being written are ever
    /// registered; mount data and pointer volumes that outlive the run are
    /// rebuildable derivatives and stay.
    pub fn volume(&mut self, volume: VolumeName) {
        self.volumes.push(volume);
    }

    /// Stop removing `volume` when the run ends, once its mount pointer
    /// proves it complete.
    pub fn release(&mut self, volume: &VolumeName) {
        self.volumes.retain(|held| held != volume);
    }

    /// Remove everything registered, trying each removal whatever became of
    /// the ones before it.
    pub fn finish(self) -> Result<(), CleanupError> {
        let mut failures = Vec::new();
        for container in &self.containers {
            if let Err(error) = self.engine.remove_container(container) {
                failures.push((format!("container {container}"), error));
            }
        }
        for volume in &self.volumes {
            if let Err(error) = self.engine.remove_volume(volume) {
                failures.push((format!("volume {volume}"), error));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(CleanupError { failures })
        }
    }
}

/// Every removal that failed, naming its object.
#[derive(Debug)]
pub struct CleanupError {
    failures: Vec<(String, EngineError)>,
}

impl CleanupError {
    /// Each failed removal's object and [`EngineError::cause`], never the
    /// daemon's words.
    pub fn cause(&self) -> String {
        let removals: Vec<String> =
            self.failures.iter().map(|(object, error)| format!("removing {object}: {}", error.cause())).collect();
        removals.join("; ")
    }
}

impl fmt::Display for CleanupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, (object, error)) in self.failures.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "removing {object}: {error}")?;
        }
        Ok(())
    }
}

impl Error for CleanupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.failures.first().map(|(_, error)| error as &(dyn Error + 'static))
    }
}
