//! The environment's image in the daemon: a rebuildable derivative of the
//! journal, named and labelled by the environment's digest.
//!
//! The image is `aether-workspace-environment:<hex>`. Before every run its
//! label `aether.workspace.environment` must equal `<hex>`. When the daemon
//! holds no such image, the root tree streams as a filesystem tar to
//! `POST /images/create?fromSrc=-`, which applies the label, and the image is
//! inspected again. The imported image has no `Env` of its own, so every
//! variable a step sees is constructed per step.
//!
//! A daemon that answers but cannot produce the image, and an image whose
//! label does not match, are `Refused(EnvironmentUnavailable)`, the one place
//! that refusal is used. A transport failure is `Failed`.

use aether_bloomery_journal::ArtifactBatch;
use aether_bloomery_kinds::{Ref, Tree};
use aether_bloomery_tar::{EncodeError, encode};

use super::{Stop, engine_failed, upload_stop};
use crate::runtime::engine::{Engine, EngineError, UploadError};
use crate::runtime::journal::{JournalSource, SourceError};
use crate::{Environment, Refusal};

/// The repository every environment image is tagged under.
const REPOSITORY: &str = "aether-workspace-environment";

/// The label that must carry the environment's digest.
const LABEL: &str = "aether.workspace.environment";

/// Make sure the daemon holds `environment`'s image, importing `root` when it
/// does not, and return the image's reference.
pub fn ensure(
    engine: &Engine,
    batch: &ArtifactBatch,
    environment: &Ref<Environment>,
    root: &Ref<Tree>,
) -> Result<String, Stop> {
    let hex = environment.digest().to_string();
    let reference = format!("{REPOSITORY}:{hex}");
    let inspect = || engine.image_labels(&reference).map_err(engine_failed(format!("inspecting image {reference}")));

    if let Some(labels) = inspect()? {
        return labelled(labels.get(LABEL), &hex).map(|()| reference);
    }
    tracing::info!(target: "aether_workspace", image = %reference, "importing the environment image");
    engine
        .import_image(REPOSITORY, &hex, &[format!("LABEL {LABEL}={hex}")], |out| {
            encode(root, &mut JournalSource::new(batch), out)
        })
        .map_err(|error| import_stop(format!("importing image {reference}"), error))?;
    let labels = inspect()?.ok_or_else(|| Stop::refused(Refusal::EnvironmentUnavailable))?;
    labelled(labels.get(LABEL), &hex).map(|()| reference)
}

/// The label must name the environment the image is for.
fn labelled(label: Option<&String>, hex: &str) -> Result<(), Stop> {
    if label.is_some_and(|label| label == hex) {
        Ok(())
    } else {
        Err(Stop::refused(Refusal::EnvironmentUnavailable))
    }
}

/// A daemon that answered the import with a refusal cannot provide the
/// environment; every other failure maps as any tree upload's does.
fn import_stop(call: String, error: UploadError<EncodeError<SourceError>>) -> Stop {
    match error {
        UploadError::Engine(EngineError::Status { .. } | EngineError::Pull(_)) => {
            Stop::refused(Refusal::EnvironmentUnavailable)
        }
        error => upload_stop(call, error),
    }
}
