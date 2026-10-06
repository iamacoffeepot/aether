//! Camera-controller wire kinds: the [`ControllerConfig`] init-config
//! (loaded once at instantiation, ADR-0090) and the validated values it is
//! built from. The controller drives a camera through the camera's own
//! `aether.kit.camera.*` kinds ([`crate::camera`]); this module holds only
//! the controller's own configuration vocabulary.
//!
//! A load with no config boots the compiled [`Default`] control scheme:
//! `aether.kit.camera:main` driven by the input of the chassis's initial
//! window.

use core::borrow::Borrow;
use core::error::Error as StdError;
use core::fmt;

use aether_actor::ActorPath;
use aether_data::LoadName;
use aether_window::{INITIAL_WINDOW_NAME, WindowCapability, WindowInstance};

use crate::camera::{CameraComponent, Distance};

/// Why a controller value was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerValueError {
    /// A rate that is not finite or is below zero.
    RateNegative,
    /// A zoom step that is not finite or not greater than zero.
    ZoomStepNotPositive,
}

impl ControllerValueError {
    const fn reason(self) -> &'static str {
        match self {
            Self::RateNegative => "rate-negative",
            Self::ZoomStepNotPositive => "zoom-step-not-positive",
        }
    }
}

impl aether_data::Invariant for ControllerValueError {
    fn reason(&self) -> &'static str {
        Self::reason(*self)
    }
}

impl fmt::Display for ControllerValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl StdError for ControllerValueError {}

/// How fast an input moves the camera, in the unit its config field names.
/// Finite and not negative; zero turns that input off.
#[derive(Debug, Clone, Copy, PartialEq, aether_data::Storage)]
#[storage(validate)]
pub struct Rate(f32);

impl Rate {
    /// Accept a finite rate that is not negative.
    ///
    /// # Errors
    ///
    /// [`ControllerValueError::RateNegative`] otherwise.
    pub fn new(rate: f32) -> Result<Self, ControllerValueError> {
        Self::check(rate)?;
        Ok(Self(rate))
    }

    /// The rate.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    // `#[storage(validate)]` calls `check(&inner)`; `Borrow` takes that
    // reference and `new`'s owned value alike.
    fn check(rate: impl Borrow<f32>) -> Result<(), ControllerValueError> {
        let rate = *rate.borrow();
        let usable = rate >= 0.0 && rate.is_finite();
        if usable {
            Ok(())
        } else {
            Err(ControllerValueError::RateNegative)
        }
    }
}

/// What one wheel step multiplies the camera's distance by. Finite and
/// greater than zero: below one a step toward the scene zooms in, above one
/// it zooms out, and one turns the wheel off.
#[derive(Debug, Clone, Copy, PartialEq, aether_data::Storage)]
#[storage(validate)]
pub struct ZoomStep(f32);

impl ZoomStep {
    /// Accept a finite multiplier greater than zero.
    ///
    /// # Errors
    ///
    /// [`ControllerValueError::ZoomStepNotPositive`] otherwise.
    pub fn new(multiplier: f32) -> Result<Self, ControllerValueError> {
        Self::check(multiplier)?;
        Ok(Self(multiplier))
    }

    /// The multiplier.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    fn check(multiplier: impl Borrow<f32>) -> Result<(), ControllerValueError> {
        let multiplier = *multiplier.borrow();
        let positive = multiplier > 0.0 && multiplier.is_finite();
        if positive {
            Ok(())
        } else {
            Err(ControllerValueError::ZoomStepNotPositive)
        }
    }
}

/// Init-config for [`CameraController`](crate::camera::controller::CameraController):
/// the camera it drives, the window whose input it reads, and how fast each
/// input moves the camera. The key rates are per second and the pan rate is
/// a fraction of the camera's distance, so the feel is the same at any frame
/// rate and any scale.
///
/// # Agent
/// Pass as `config` to `load_component` / `spawn` with `namespace:
/// "aether.kit.camera-controller"` and a `key`, for example `{"camera":
/// "aether.kit.camera:main", "window":
/// "aether.window/aether.window.instance:main", "orbit_radians_per_pixel":
/// 0.005, "key_turn_radians_per_sec": 1.5, "zoom_per_wheel_step": 0.9,
/// "key_pan_distances_per_sec": 1.0, "nearest": 0.05, "farthest": 10000.0}`.
/// The camera must be live first. A config whose `nearest` is beyond its
/// `farthest` is refused at load.
#[aether_data::kind(name = "aether.kit.camera-controller.config", partial_eq, no_serde)]
pub struct ControllerConfig {
    /// The camera to drive, `aether.kit.camera:<key>`. It is sent to, so it
    /// is typed: a path whose leaf is not a kit camera does not decode.
    pub camera: ActorPath<CameraComponent>,
    /// The window whose keys, mouse and focus the controller reads,
    /// `aether.window/aether.window.instance:<name>`. Input from any other
    /// window is ignored. The path is compared, never sent to; it is typed
    /// so a config cannot name something that is not a window.
    pub window: ActorPath<WindowInstance>,
    /// How far a left-drag turns the camera, in radians per physical pixel
    /// of cursor travel: across for yaw, up and down for pitch.
    pub orbit_radians_per_pixel: Rate,
    /// How fast the Q and E keys turn the camera about its target, in
    /// radians per second.
    pub key_turn_radians_per_sec: Rate,
    /// What one wheel step multiplies the camera's distance by.
    pub zoom_per_wheel_step: ZoomStep,
    /// How fast WASD and the arrow keys move the target across the ground,
    /// in camera distances per second: at `1.0` a second of a held key moves
    /// the target as far as the eye sits from it.
    pub key_pan_distances_per_sec: Rate,
    /// The closest the wheel may bring the eye to the target.
    pub nearest: Distance,
    /// The farthest the wheel may take the eye from the target.
    pub farthest: Distance,
}

impl ControllerConfig {
    /// The chassis's initial window, `main`.
    fn initial_window() -> ActorPath<WindowInstance> {
        let name = LoadName::new(INITIAL_WINDOW_NAME).expect("the initial window's name is a valid segment");

        ActorPath::<WindowInstance>::child(&ActorPath::<WindowCapability>::root(), &name)
            .expect("a window path is two valid steps, under both caps")
    }
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            camera: CameraComponent::main_path(),
            window: Self::initial_window(),
            // 200 pixels of drag turn the camera one radian.
            orbit_radians_per_pixel: Rate(0.005),
            // A quarter turn takes about a second.
            key_turn_radians_per_sec: Rate(1.5),
            // Seven steps toward the scene halve the distance.
            zoom_per_wheel_step: ZoomStep(0.9),
            // A second of a held key crosses about the height of the view.
            key_pan_distances_per_sec: Rate(1.0),
            nearest: Distance::new(0.05).expect("a twentieth of a unit is a positive distance"),
            farthest: Distance::new(10_000.0).expect("ten thousand units is a positive distance"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each check's bounds are open or closed as its doc says. A zero rate
    /// is a switched-off input and must be accepted; a zero zoom step would
    /// ask the camera for a zero distance, and a negative rate would reverse
    /// an input the config has no field to reverse.
    #[test]
    fn value_bounds_are_open_or_closed_as_documented() {
        assert!(Rate::new(0.0).is_ok());
        assert_eq!(Rate::new(-0.001), Err(ControllerValueError::RateNegative));
        assert_eq!(Rate::new(f32::NAN), Err(ControllerValueError::RateNegative));
        assert_eq!(Rate::new(f32::INFINITY), Err(ControllerValueError::RateNegative));

        assert_eq!(ZoomStep::new(0.0), Err(ControllerValueError::ZoomStepNotPositive));
        assert_eq!(ZoomStep::new(f32::INFINITY), Err(ControllerValueError::ZoomStepNotPositive));
        assert!(ZoomStep::new(1.25).is_ok());
    }
}
