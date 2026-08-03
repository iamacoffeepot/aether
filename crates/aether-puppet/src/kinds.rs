//! The mail shapes peers send the puppet.

use serde::{Deserialize, Serialize};

/// Point the puppet at a mesh in one of the substrate's I/O namespaces
/// (`save`, `assets`, `config`). The load is asynchronous; the cached
/// drawing is replaced atomically when the bytes arrive, so a failed load
/// leaves the previous subject on screen rather than blanking it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.load")]
pub struct Load {
    pub namespace: String,
    pub path: String,
    /// Optional path to the material field, a `uint8` cubic `.npy` baked
    /// over the same sculpt.
    ///
    /// Without it every crease is inked, and the sculpt carves a hair-strand
    /// seam exactly as deeply as it carves an eyelid — there are five times
    /// as many of them, so they bury the face. Depth cannot tell them apart;
    /// the field already knows which is which, so the drawing asks it rather
    /// than inventing a geometric proxy. Empty loads the mesh alone.
    pub labels: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.load_result")]
pub enum LoadResult {
    Ok { vertices: u32, faces: u32 },
    Err { reason: String },
}

/// Where the eye sits. Silhouettes are the zero set of `view . normal`, so
/// the renderer cannot draw a frame without knowing this — which is why the
/// puppet owns its camera rather than reading one.
///
/// `aether.view_projection` travels to `aether.render` as a directed send
/// rather than a subscribable stream, so a second actor's camera is not
/// something this one can overhear. Owning it also means she is one
/// component to boot: load her and she is on screen, framed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.look")]
pub struct Look {
    /// Degrees around her, counterclockwise from facing the camera.
    pub azimuth: f32,
    /// Degrees above the horizon.
    pub elevation: f32,
    /// Distance from the framing target, in model units.
    pub distance: f32,
    /// Height of the point the camera aims at.
    pub height: f32,
}
