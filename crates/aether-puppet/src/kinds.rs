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
    /// Optional directory holding the rig that poses this subject:
    /// `weights.npy` (per-vertex, per-bone, `<f4`) and `rig.txt` (the bone
    /// order, pivots and long axes).
    ///
    /// Weights are per vertex of one sculpt and carry no identity of their
    /// own, so a rig whose vertex count disagrees with the mesh is refused
    /// rather than applied to whatever turned up. Empty leaves the subject
    /// unposable, which is what [`LoadResult::Ok`]'s `bones` then reports.
    pub rig: String,
}

/// What the subject turned out to be. `bones` is `0` when no rig was asked
/// for or none was accepted — the one place a refused rig is visible to the
/// caller rather than only in the log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.load_result")]
pub enum LoadResult {
    Ok { vertices: u32, faces: u32, bones: u32 },
    Err { reason: String },
}

/// What the rig is doing, in degrees per channel.
///
/// Channel names follow the rigging reference: `ARKit` where `ARKit` has
/// an opinion, `ARKit`-style where it does not — ears being the obvious
/// gap. Everything at zero is the rest pose, and a subject at rest skins
/// nothing: the drawing extracted at load is the drawing that ships.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.puppet.pose")]
pub struct Pose {
    /// Turn, about the head pivot. Shared with the neck, so it reads as a
    /// neck carrying a head rather than a head swivelling on a post.
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
    /// The mandible, hinged below and in front of the ear canal.
    pub jaw: f32,
    /// The blade swinging out of the midline plane.
    pub ear_flick_left: f32,
    pub ear_flick_right: f32,
    /// Aim, not flap: the cup sweeping toward a sound about the blade's
    /// own long axis. Clamped to the arc an ear actually has.
    pub ear_twist_left: f32,
    pub ear_twist_right: f32,
}

impl Pose {
    /// Whether every channel is at rest, in which case the posed subject
    /// is the rest subject and nothing has to be skinned at all.
    pub fn is_rest(&self) -> bool {
        *self == Self::default()
    }
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
