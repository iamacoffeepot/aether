//! The import sequence (ADR-0237 decision 3, amended 2026-09-25): a
//! digest-pinned image into a stored tree, run on the actor's worker thread.
//!
//! 1. `POST /images/create` pulls the image.
//! 2. `GET /images/<ref>/json` must list the ref in `RepoDigests`.
//! 3. `POST /containers/create` creates a container from it, never started.
//! 4. `GET /containers/{id}/export` decodes under the userland rules straight
//!    into one artifact batch.
//! 5. `DELETE /containers/{id}` runs on every path once the container exists.
//! 6. The batch commits only when the decode and the removal both succeeded.
//!
//! So a failed import commits no row and leaves no container behind. Blob
//! files a failed decode already renamed stay as harmless orphans (ADR-0220).
//! The pulled image stays in the daemon's cache.

#[cfg(all(test, unix))]
mod tests;

use std::error::Error;
use std::fmt;

use aether_bloomery_journal::{AppendError, ArtifactBatch, ArtifactStore, JournalError};
use aether_bloomery_kinds::{Detail, Ref, Tree};
use aether_bloomery_tar::{DecodeError, Rules, decode};

use super::engine::{ContainerId, Engine, EngineError};
use super::journal::JournalSink;
use crate::{ImageRef, ImportResult};

/// Everything one import needs, cloned onto the worker thread per request.
#[derive(Clone)]
pub struct Importer {
    pub engine: Engine,
    pub artifacts: ArtifactStore,
    pub rules: Rules,
}

impl Importer {
    /// Run the import and answer it: the tree, or the failure's text, which is
    /// also logged.
    pub fn answer(&self, image: &ImageRef) -> ImportResult {
        match self.run(image) {
            Ok(tree) => {
                tracing::info!(target: "aether_workspace", image = image.as_str(), tree = %tree.digest(), "imported");
                ImportResult::Ok { tree }
            }
            Err(error) => {
                tracing::warn!(target: "aether_workspace", image = image.as_str(), %error, "import failed");
                ImportResult::Failed { detail: Detail::new(error.to_string()) }
            }
        }
    }

    /// Pull, check, create, export and decode, remove, then commit.
    pub fn run(&self, image: &ImageRef) -> Result<Ref<Tree>, ImportError> {
        self.engine.pull(image)?;
        if !self.engine.repo_digests(image)?.iter().any(|listed| listed == image.as_str()) {
            return Err(ImportError::NotListed(image.as_str().to_owned()));
        }
        let container = self.engine.create_container(image)?;

        let decoded = self.decode_export(&container);
        match (decoded, self.engine.remove_container(&container)) {
            (Ok((batch, tree)), Ok(())) => batch.commit().map(|()| tree).map_err(ImportError::Commit),
            (Ok(_), Err(error)) => Err(ImportError::Remove { container, error, after: None }),
            (Err(cause), Ok(())) => Err(cause),
            (Err(cause), Err(error)) => Err(ImportError::Remove { container, error, after: Some(Box::new(cause)) }),
        }
    }

    /// Stream the container's export into a fresh batch, uncommitted.
    fn decode_export(&self, container: &ContainerId) -> Result<(ArtifactBatch, Ref<Tree>), ImportError> {
        let mut batch = self.artifacts.batch().map_err(ImportError::Journal)?;
        let tree = decode(self.engine.export(container)?, &mut JournalSink::new(&mut batch), &self.rules)
            .map_err(ImportError::Decode)?;
        Ok((batch, tree))
    }
}

/// Why an import failed. Its text becomes the reply's [`Detail`].
#[derive(Debug)]
pub enum ImportError {
    /// An Engine API call failed.
    Engine(EngineError),
    /// The daemon's `RepoDigests` for the pulled image do not list the ref.
    NotListed(String),
    /// The journal would not open a batch.
    Journal(JournalError),
    /// The export is not a tree under the userland rules, or the journal
    /// refused a blob or tree while it streamed.
    Decode(DecodeError<JournalError>),
    /// The batch did not commit.
    Commit(AppendError),
    /// The container could not be removed; `after` is the failure that came
    /// before it, if any.
    Remove { container: ContainerId, error: EngineError, after: Option<Box<Self>> },
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine(error) => error.fmt(f),
            Self::NotListed(image) => write!(f, "the daemon does not list {image} in the pulled image's RepoDigests"),
            Self::Journal(error) => write!(f, "opening a journal batch: {error}"),
            Self::Decode(error) => write!(f, "decoding the export: {error}"),
            Self::Commit(error) => write!(f, "committing the imported tree: {error}"),
            Self::Remove { container, error, after: None } => write!(f, "removing container {container}: {error}"),
            Self::Remove { container, error, after: Some(cause) } => {
                write!(f, "{cause}; removing container {container} also failed: {error}")
            }
        }
    }
}

impl Error for ImportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Engine(error) | Self::Remove { error, .. } => Some(error),
            Self::Journal(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::Commit(error) => Some(error),
            Self::NotListed(_) => None,
        }
    }
}

impl From<EngineError> for ImportError {
    fn from(error: EngineError) -> Self {
        Self::Engine(error)
    }
}
