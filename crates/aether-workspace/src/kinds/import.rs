//! The import request: a digest-pinned image into a stored tree (ADR-0237
//! decision 3, amended 2026-09-25).

use aether_bloomery_kinds::{Detail, Ref, Tree};

use crate::kinds::image::ImageRef;

/// Pull `image`, export its filesystem, and decode it into a stored tree.
///
/// It names an image by digest, never by tag, and never names a host path or
/// a tarball.
#[aether_data::kind(name = "aether.workspace.import", eq, no_serde)]
pub struct Import {
    /// The image to import.
    pub image: ImageRef,
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
