//! Camera-controller wire kinds: the [`ControllerConfig`] init-config
//! (loaded once at instantiation, ADR-0090). The controller drives a camera
//! through the camera's own `aether.kit.camera.*` kinds
//! ([`crate::camera`]); this module holds only the controller's own
//! configuration vocabulary.
//!
//! A load with no config boots the compiled [`Default`] control scheme
//! driving `aether.kit.camera:main`.

use crate::camera::{CameraComponent, Distance, Pitch};
use aether_actor::ActorPath;

/// Init-config for [`CameraController`](crate::camera::controller::CameraController):
/// which camera to drive and the per-tick rates and clamps the keymap
/// integrates. Every rate is expressed per tick, so the control feel is
/// tick-rate-relative like the rest of the kit.
///
/// # Agent
/// Pass as `config` to `load_component` / `spawn` with `namespace:
/// "aether.kit.camera-controller"` and a `key`, for example `{"camera":
/// "aether.kit.camera:main", "pan_speed": 0.15, "yaw_speed": 0.02,
/// "pitch_speed": 0.015, "zoom_rate": 0.985, "pitch_limit": 1.5,
/// "distance_floor": 1.0}`. The camera must be live first.
#[aether_data::kind(name = "aether.kit.camera-controller.config", partial_eq, no_serde)]
pub struct ControllerConfig {
    /// The camera to drive, `aether.kit.camera:<key>`. It is sent to, so it
    /// is typed: a path whose leaf is not a kit camera does not decode.
    pub camera: ActorPath<CameraComponent>,
    /// Ground-plane pan rate, world units per tick, for the WASD keys.
    /// Diagonals are velocity-normalized, so a diagonal covers the same
    /// ground per tick as a cardinal.
    pub pan_speed: f32,
    /// Yaw rate, radians per tick, for the ←/→ keys.
    pub yaw_speed: f32,
    /// Pitch rate, radians per tick, for the ↑/↓ keys.
    pub pitch_speed: f32,
    /// Per-tick multiplicative zoom rate for the Z/X keys: Z scales the
    /// camera's distance down by this factor, X scales it up. `1.0`
    /// disables zoom.
    pub zoom_rate: f32,
    /// How far the keys may pitch the camera either way from level.
    pub pitch_limit: Pitch,
    /// The closest a zoom-in may bring the eye to the target.
    pub distance_floor: Distance,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            camera: CameraComponent::main_path(),
            // ~0.15 m/tick ≈ 9 m/s at 60 Hz — a brisk but controllable
            // scene-navigation pan.
            pan_speed: 0.15,
            // Gentle look rates (radians/tick).
            yaw_speed: 0.02,
            pitch_speed: 0.015,
            // 1.5% dolly per held tick — smooth zoom, ~60 ticks to halve
            // or ~1.6× the distance.
            zoom_rate: 0.985,
            pitch_limit: Pitch::new(1.5).expect("1.5 radians is within a quarter turn"),
            distance_floor: Distance::new(1.0).expect("one unit is a positive distance"),
        }
    }
}
