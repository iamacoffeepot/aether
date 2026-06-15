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

/// `aether.corridor.load` — instruct the mesh viewer to load a
/// postcard-encoded `aether_labyrinth::CorridorGraph` (issue 1858) from
/// `namespace://path` and build a tick-indexable scrub datum over it
/// (issue 1869). The graph is a flat time-layered DAG: per-tick region
/// components (`nodes`) and directed cross-tick `Flow` / intra-tick
/// `Punch` edges (`edges`). On load the viewer derives a `ScrubIndex`
/// once (per-tick node buckets, per-node out-degree, flow adjacency, and
/// a per-node lineage id that carries region identity along flow edges)
/// so a later `Scrub` re-addresses the datum in O(1) without re-deriving.
/// A subsequent `LoadCorridor` replaces the cached graph + index; a
/// decode failure leaves the prior datum intact. Reply is
/// `aether.corridor.load_result`. Agent loop: export the `CorridorGraph`
/// bytes to a file via `aether.fs.write`, then `aether.corridor.load` it.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.corridor.load")]
pub struct LoadCorridor {
    /// Short namespace prefix (no `://`), e.g. `"save"`, `"assets"`.
    pub namespace: String,
    /// Relative path within the namespace to the postcard-encoded
    /// `CorridorGraph` bytes.
    pub path: String,
}

/// `aether.corridor.load_result` — reply to `aether.corridor.load`
/// (`LoadCorridor`). Mirrors `aether_kinds::MeshLoadResult`: echoes the
/// request's `namespace` + `path` so the caller correlates the reply to
/// its source, `ok` is the single success/failure read, `error` is
/// `Some` iff `ok` is false (read / decode failure), and `warnings`
/// carries non-fatal notes (none produced today; the shape is plumbed).
/// Whole-graph atomic-replace semantics: a failed load leaves the prior
/// cached datum intact.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.corridor.load_result")]
pub struct CorridorLoadResult {
    pub ok: bool,
    pub namespace: String,
    pub path: String,
    pub error: Option<String>,
    pub warnings: Vec<String>,
}

/// `aether.corridor.scrub` — set the viewer's per-tick scrub cursor to
/// `tick`. Fire-and-forget; the viewer clamps `tick` to `[0, ticks)`
/// (the corridor graph's tick span) and re-emits that tick's node slice
/// to `aether.render` on the next `Render` stage. The scrub is
/// user-driven time travel over a static field, decoupled from the
/// `Tick` lifecycle stage — the viewer stays `Render`-only.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.corridor.scrub")]
pub struct Scrub {
    /// The tick layer to address. Clamped to the loaded graph's tick
    /// span; a `Scrub` with no graph loaded is a no-op.
    pub tick: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::Kind;

    #[test]
    fn kind_name_is_stable() {
        assert_eq!(LoadMesh::NAME, "aether.mesh.load");
        assert_eq!(LoadCorridor::NAME, "aether.corridor.load");
        assert_eq!(CorridorLoadResult::NAME, "aether.corridor.load_result");
        assert_eq!(Scrub::NAME, "aether.corridor.scrub");
    }
}
