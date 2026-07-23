//! Mesh-viewer wire kinds. The actor
//! ([`crate::mesh::MeshViewer`]) loads a mesh file from the
//! substrate's I/O surface (ADR-0041 namespace + path) and replays it
//! as `DrawTriangle` mail every tick. It dispatches on file
//! extension: `.dsl` runs through the `aether-mesh` parser+mesher
//! (ADR-0026 + ADR-0051) and emits polygon-edge wireframes alongside
//! filled triangles; `.obj` is parsed as triangulated Wavefront
//! geometry with no wireframe.

use alloc::string::String;
use serde::{Deserialize, Serialize};

/// `aether.kit.mesh.load` — instruct the mesh viewer to load and display
/// the file at `namespace://path`. The viewer dispatches on the
/// file extension: `.dsl` runs through `aether-mesh`'s parser +
/// mesher; `.obj` runs through the OBJ parser. Subsequent `Load`
/// mails replace the cached mesh. Fire-and-forget; errors surface
/// in `engine_logs`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.mesh.load")]
pub struct LoadMesh {
    /// Short namespace prefix (no `://`), e.g. `"save"`, `"assets"`.
    pub namespace: String,
    /// Relative path within the namespace. Extension picks the
    /// parser: `.dsl` or `.obj`. Other extensions are rejected.
    pub path: String,
}
