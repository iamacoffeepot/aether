//! The import request: a digest-pinned image into a stored tree (ADR-0237
//! decision 3, amended 2026-09-25).

use aether_actor::{HeldReply, PathRefused, ProtocolPath};
use aether_bloomery_kinds::{ArtifactStorage, Detail, Tree};
use aether_data::Ref;

use crate::kinds::image::ImageRef;

/// Pull `image`, export its filesystem, and decode it into a tree staged to
/// the storage `source` names.
///
/// It names an image by digest, never by tag, and never names a host path or
/// a tarball. The source is proven to cover [`ArtifactStorage`] when the mail
/// decodes, and proven live when the workspace receives it: one that fails
/// either proof is answered `Err(ImportError::Source(..))` before anything
/// is queued.
#[aether_data::kind(name = "aether.workspace.import", eq, no_serde)]
pub struct Import {
    /// The image to import.
    pub image: ImageRef,
    /// Where the imported tree is staged.
    pub source: ProtocolPath<ArtifactStorage>,
}

/// The one reply to an [`Import`].
#[aether_data::kind(name = "aether.workspace.import_result", eq, no_serde)]
pub enum ImportResult {
    /// The image's filesystem, as a stored tree.
    Ok {
        /// The imported root.
        tree: Ref<Tree>,
    },
    /// Nothing was imported.
    Err(ImportError),
}

/// Why an [`Import`] imported nothing.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub enum ImportError {
    /// The pull, the export, or the decode failed.
    Failed {
        /// The bounded fault text naming the cause.
        detail: Detail,
    },
    /// The `source` did not prove (ADR-0231 §3): no route has stood at it,
    /// its route does not cover [`ArtifactStorage`], or it is not live.
    Source(PathRefused),
}

impl From<PathRefused> for ImportResult {
    fn from(refused: PathRefused) -> Self {
        Self::Err(ImportError::Source(refused))
    }
}

impl HeldReply for ImportResult {
    fn unanswered() -> Self {
        Self::Err(ImportError::Failed { detail: Detail::new("workspace capability closed before the import answered") })
    }
}
