//! Camera-controller wire kinds: the [`ControllerConfig`] init-config
//! shape (loaded once at instantiation, ADR-0090) and the
//! [`ControllerMode`] it selects. The controller *drives* the camera
//! component through its existing `aether.kit.camera.*` kinds
//! ([`crate::camera`]); this module holds only the controller's own
//! configuration vocabulary.
//!
//! [`ControllerConfig`] is a non-unit typed config. A bare load with no
//! `config_path` boots the compiled [`Default`] control scheme; callers can
//! still encode and pass a config to override that baseline.

use alloc::string::String;

use serde::{Deserialize, Serialize};

/// Which projection the controller drives on its target camera. The
/// controller emits `aether.kit.camera.orbit.set` in [`Orbit`](Self::Orbit)
/// and `aether.kit.camera.topdown.set` in [`Topdown`](Self::Topdown); the
/// target camera must already be in the matching mode (the camera
/// component warn-drops a mode-mismatched delta), so this pairs with
/// the camera's own mode rather than switching it.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ControllerMode {
    /// Drive the orbit camera: WASD pans the orbit `target` across the
    /// ground plane, ←/→ yaw, ↑/↓ pitch, Z/X dolly the eye distance.
    #[default]
    Orbit,
    /// Drive the top-down camera: WASD pans the ortho `center`, Z/X
    /// scale the ortho `extent` (zoom).
    Topdown,
}

/// Init-config for [`CameraController`](crate::camera::controller::CameraController):
/// which camera to drive, in which mode, and the per-tick rates and
/// clamps the keymap integrates. Every rate is expressed per tick, so
/// the control feel is tick-rate-relative like the rest of the kit.
///
/// # Agent
/// Encode one of these to the controller's `Config` shape and pass it
/// as the `config` bytes of the `aether.component.load` that
/// instantiates the controller (or `load_component`'s `config_path`).
/// Omitting config bytes boots [`ControllerConfig::default()`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.camera-controller.config")]
pub struct ControllerConfig {
    /// Name of the camera *within* the target camera component to
    /// drive — the `name` field of every emitted `aether.kit.camera.*`
    /// delta. Defaults to `"main"`, the camera component's boot camera.
    /// This is not the component's load name (the controller resolves
    /// the component instance separately); it selects which of that
    /// component's named cameras the keys steer.
    pub camera: String,
    /// Which projection to drive. Must match the target camera's actual
    /// mode; the controller does not switch modes.
    pub mode: ControllerMode,
    /// Ground-plane pan rate, world units per tick, for the WASD keys
    /// (orbit `target` / topdown `center`). Diagonals are
    /// velocity-normalized, so a diagonal covers the same ground per
    /// tick as a cardinal.
    pub pan_speed: f32,
    /// Orbit yaw rate, radians per tick, for the ←/→ keys. Unused in
    /// topdown mode.
    pub yaw_speed: f32,
    /// Orbit pitch rate, radians per tick, for the ↑/↓ keys. Unused in
    /// topdown mode.
    pub pitch_speed: f32,
    /// Per-tick multiplicative zoom rate for the Z/X keys: Z scales the
    /// controlled dimension (orbit `distance` / topdown `extent`) down
    /// by this factor, X scales it up. `1.0` disables zoom.
    pub zoom_rate: f32,
    /// Absolute clamp on the orbit pitch magnitude, radians. Keeps the
    /// eye out of the degenerate `±π/2` poles. Unused in topdown mode.
    pub pitch_limit: f32,
    /// Lower clamp on the controlled zoom dimension (orbit `distance` /
    /// topdown `extent`), world units, so a zoom-in never collapses the
    /// camera onto its target.
    pub distance_floor: f32,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            camera: String::from("main"),
            mode: ControllerMode::Orbit,
            // ~0.15 m/tick ≈ 9 m/s at 60 Hz — a brisk but controllable
            // scene-navigation pan.
            pan_speed: 0.15,
            // Gentle look rates, in the same ballpark as the mover's
            // arrow-key orbit (radians/tick).
            yaw_speed: 0.02,
            pitch_speed: 0.015,
            // 1.5% dolly per held tick — smooth zoom, ~60 ticks to halve
            // or ~1.6× the distance.
            zoom_rate: 0.985,
            // Just inside the ±π/2 pole so `look_at` never degenerates.
            pitch_limit: 1.5,
            // Never dolly closer than 1 world unit to the target.
            distance_floor: 1.0,
        }
    }
}
