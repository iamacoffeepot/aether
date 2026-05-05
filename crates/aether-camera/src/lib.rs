//! Camera component crate (issue 552 stage 1.5 consolidated). Hosts
//! both the trunk types (kind structs, parameter shapes) at the crate
//! root and the runtime `CameraComponent` in [`runtime`]. Other
//! components and demos that need to *talk to* a camera depend on
//! this crate for the wire shapes; the cdylib FFI exports the
//! substrate loads at runtime are emitted by `runtime`'s
//! `aether_actor::export!()` invocation under wasm32.
//!
//! `aether.camera` (the singular `view_proj` kind consumed by the
//! desktop chassis's `aether.render` mailbox per ADR-0074
//! §Decision 7) is *not* here — it's a chassis sink contract and
//! lives in `aether-kinds` alongside the other substrate primitives.

extern crate alloc;

use alloc::string::String;
use serde::{Deserialize, Serialize};

pub mod runtime;

/// Per-mode parameters for the orbit camera. Every field is
/// `Option<...>`: present → apply, absent → leave whatever the
/// camera already has. Used both for create-time initial state
/// (`CameraCreate { mode: Orbit(OrbitParams { distance: Some(5.0),
/// .. }) }` — anything left `None` falls back to the orbit
/// component's compiled defaults) and for live tweaks
/// (`CameraOrbitSet`).
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct OrbitParams {
    /// Eye radius from `target`. `0.0` collapses to the target.
    pub distance: Option<f32>,
    /// Vertical tilt (radians). Positive places the eye below the
    /// target (camera looks up); negative places it above (camera
    /// looks down). `±π/2` are degenerate.
    pub pitch: Option<f32>,
    /// Absolute yaw (radians). Auto-advance keeps ticking from
    /// this value next frame; pair with `speed: Some(0.0)` to pin.
    pub yaw: Option<f32>,
    /// Auto-rotation rate (radians per tick). `0.0` freezes;
    /// negative reverses.
    pub speed: Option<f32>,
    /// Vertical field of view (radians).
    pub fov_y_rad: Option<f32>,
    /// World-space pivot the camera orbits around.
    pub target: Option<[f32; 3]>,
}

/// Per-mode parameters for the orthographic top-down camera.
/// Same `Option<...>` semantics as `OrbitParams`: present → apply,
/// absent → keep current.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct TopdownParams {
    /// World-xy centerpoint. Z is implicit — the camera always
    /// looks down `-Z`.
    pub center: Option<[f32; 2]>,
    /// Half-height of the orthographic frustum in world units.
    /// Visible width is `extent * aspect`. Must be positive at
    /// apply time; the camera component clamps to a tiny floor.
    pub extent: Option<f32>,
}

/// Mode + initial parameters for create / mode-switch. Each
/// variant carries the full param struct for that mode; pass
/// `Default::default()` (all `None`) to take the camera
/// component's compiled defaults wholesale.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ModeInit {
    Orbit(OrbitParams),
    Topdown(TopdownParams),
}

/// `aether.camera.create` — create a new named camera in the given
/// mode. Errors if `name` is already taken; use `CameraSetMode` to
/// swap an existing camera's mode in place. Newly-created cameras
/// are not made active automatically; pair with `CameraSetActive`
/// or rely on the bootstrap `"main"` camera.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.camera.create")]
pub struct CameraCreate {
    pub name: String,
    pub mode: ModeInit,
}

/// `aether.camera.destroy` — drop a camera by name. No-op if the
/// name isn't bound. If the destroyed camera was the active one
/// the publish stream pauses (no `aether.camera` mail goes out)
/// until another camera is made active.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.camera.destroy")]
pub struct CameraDestroy {
    pub name: String,
}

/// `aether.camera.set_active` — promote the named camera to be the
/// one whose `view_proj` publishes to `"aether.render"` each
/// tick. Errors if the name isn't bound. Inactive cameras still
/// tick (orbit yaw keeps accumulating, etc.) so re-activating
/// later doesn't snap.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.camera.set_active")]
pub struct CameraSetActive {
    pub name: String,
}

/// `aether.camera.set_mode` — swap an existing camera's mode in
/// place. State for the prior mode is discarded; the new mode is
/// seeded from the supplied params + per-mode compiled defaults.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.camera.set_mode")]
pub struct CameraSetMode {
    pub name: String,
    pub mode: ModeInit,
}

/// `aether.camera.orbit.set` — apply orbit-mode field deltas to
/// the named camera. Errors silently (warn-log) if the camera is
/// in a non-orbit mode; switch with `CameraSetMode` first. Every
/// `Some` field overwrites; `None` leaves the current value
/// alone, so partial pokes (e.g. just `distance`) ride a single
/// kind without restating the rest.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.camera.orbit.set")]
pub struct CameraOrbitSet {
    pub name: String,
    pub params: OrbitParams,
}

/// `aether.camera.topdown.set` — apply topdown-mode field deltas
/// to the named camera. Same semantics as `CameraOrbitSet` but for
/// the orthographic mode's `center` / `extent`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.camera.topdown.set")]
pub struct CameraTopdownSet {
    pub name: String,
    pub params: TopdownParams,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::Kind;

    #[test]
    fn kind_names_are_stable() {
        assert_eq!(CameraCreate::NAME, "aether.camera.create");
        assert_eq!(CameraDestroy::NAME, "aether.camera.destroy");
        assert_eq!(CameraSetActive::NAME, "aether.camera.set_active");
        assert_eq!(CameraSetMode::NAME, "aether.camera.set_mode");
        assert_eq!(CameraOrbitSet::NAME, "aether.camera.orbit.set");
        assert_eq!(CameraTopdownSet::NAME, "aether.camera.topdown.set");
    }
}
