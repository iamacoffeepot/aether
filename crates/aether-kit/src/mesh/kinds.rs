//! Mesh-viewer wire kinds. The actor
//! ([`crate::mesh::MeshViewer`]) loads a mesh file from the
//! substrate's I/O surface (ADR-0041 namespace + path) and replays it
//! as `DrawTriangle` mail every tick. It dispatches on file
//! extension: `.dsl` runs through the `aether-mesh` parser+mesher
//! (ADR-0026 + ADR-0051) and emits polygon-edge wireframes alongside
//! filled triangles; `.obj` is parsed as triangulated Wavefront
//! geometry with no wireframe.

use alloc::string::String;

use aether_actor::ActorPath;

use crate::camera::CameraComponent;

/// `aether.kit.mesh.config` — what the mesh viewer is spawned with.
///
/// # Agent
/// Pass as `config` to `load_component` / `spawn` with `namespace:
/// "aether.kit.mesh"`: `{"camera": "aether.kit.camera:main"}`. The camera
/// must be live first. A load with no config names `aether.kit.camera:main`.
#[aether_data::kind(name = "aether.kit.mesh.config", eq, no_serde)]
pub struct MeshViewerConfig {
    /// The camera whose eye the viewer draws DSL outlines for,
    /// `aether.kit.camera:<key>`. The viewer subscribes to its view, so the
    /// path is typed: one whose leaf is not a kit camera does not decode.
    pub camera: ActorPath<CameraComponent>,
}

impl Default for MeshViewerConfig {
    fn default() -> Self {
        Self { camera: CameraComponent::main_path() }
    }
}

/// `aether.kit.mesh.load` — instruct the mesh viewer to load and display
/// the file at `namespace://path`. The viewer dispatches on the
/// file extension: `.dsl` runs through `aether-mesh`'s parser +
/// mesher; `.obj` runs through the OBJ parser. Subsequent `Load`
/// mails replace the cached mesh. Fire-and-forget; errors surface
/// in `engine_logs`.
#[aether_data::kind(name = "aether.kit.mesh.load", no_serde)]
pub struct LoadMesh {
    /// Short namespace prefix (no `://`), e.g. `"save"`, `"assets"`.
    pub namespace: String,
    /// Relative path within the namespace. Extension picks the
    /// parser: `.dsl` or `.obj`. Other extensions are rejected.
    pub path: String,
}
