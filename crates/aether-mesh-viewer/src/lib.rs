//! Mesh viewer component trunk (ADR-0066). Hosts the kind types that
//! `aether-mesh-viewer-component` receives. The runtime cdylib lives in
//! `aether-mesh-viewer-component`.
//!
//! The viewer loads a mesh file from the substrate's I/O surface
//! (ADR-0041 namespace + path) and replays it as `DrawTriangle` mail
//! every tick. It dispatches on file extension: `.dsl` runs through
//! the `aether-mesh` parser+mesher (ADR-0026 + ADR-0051) and emits
//! polygon-edge wireframes alongside filled triangles; `.obj` is parsed
//! as triangulated Wavefront geometry with no wireframe.

#![no_std]

extern crate alloc;

use alloc::string::String;
use serde::{Deserialize, Serialize};

/// `aether.mesh.load` — instruct the mesh viewer to load and display
/// the file at `namespace://path`. The viewer dispatches on the
/// file extension: `.dsl` runs through `aether-mesh`'s parser +
/// mesher; `.obj` runs through the OBJ parser. Subsequent `Load`
/// mails replace the cached mesh. Fire-and-forget; errors surface
/// in `engine_logs`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mesh.load")]
pub struct LoadMesh {
    /// Short namespace prefix (no `://`), e.g. `"save"`, `"assets"`.
    pub namespace: String,
    /// Relative path within the namespace. Extension picks the
    /// parser: `.dsl` or `.obj`. Other extensions are rejected.
    pub path: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::Kind;

    #[test]
    fn kind_name_is_stable() {
        assert_eq!(LoadMesh::NAME, "aether.mesh.load");
    }
}
