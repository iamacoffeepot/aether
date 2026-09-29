//! The import request: a digest-pinned image into a stored tree (ADR-0237
//! decision 3, amended 2026-09-25).

use aether_actor::{HeldReply, ProtocolPath};
use aether_bloomery_kinds::{ArtifactStorage, Detail, Ref, Tree};

use crate::kinds::image::ImageRef;

/// Pull `image`, export its filesystem, and decode it into a tree staged to
/// the storage `source` names.
///
/// It names an image by digest, never by tag, and never names a host path or
/// a tarball. The source is proven to cover [`ArtifactStorage`] when the mail
/// decodes, and proven live when the workspace receives it: one that is not
/// live is answered `Failed` before anything is queued.
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
    /// The pull, the export, or the decode failed.
    Failed {
        /// The bounded fault text naming the cause.
        detail: Detail,
    },
}

impl HeldReply for ImportResult {
    fn unanswered() -> Self {
        Self::Failed { detail: Detail::new("workspace capability closed before the import answered") }
    }
}
