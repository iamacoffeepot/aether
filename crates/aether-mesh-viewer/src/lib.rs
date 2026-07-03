//! Mesh viewer crate (issue 552 stage 1.5 consolidated). Hosts both
//! the trunk types (kind structs) at the crate root and the runtime
//! `MeshViewer` in [`runtime`]. Other components and demos that need
//! to *talk to* a mesh viewer depend on this crate for the wire
//! shapes; the cdylib FFI exports the substrate loads at runtime are
//! emitted by `runtime`'s `aether_actor::export!()` invocation under
//! wasm32.
//!
//! The viewer loads a mesh file from the substrate's I/O surface
//! (ADR-0041 namespace + path) and replays it as `DrawTriangle` mail
//! every tick. It dispatches on file extension: `.dsl` runs through
//! the `aether-mesh` parser+mesher (ADR-0026 + ADR-0051) and emits
//! polygon-edge wireframes alongside filled triangles; `.obj` is parsed
//! as triangulated Wavefront geometry with no wireframe.

extern crate alloc;

use alloc::string::String;
use serde::{Deserialize, Serialize};

#[cfg(feature = "runtime")]
pub mod runtime;

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
