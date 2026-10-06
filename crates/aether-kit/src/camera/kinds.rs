//! Camera wire kinds: the `aether.kit.camera.*` kinds a peer sends a
//! [`CameraComponent`], the config an instance is spawned with, and the
//! validated values they are built from.
//!
//! A camera has one [`Pose`] and one [`Lens`]. The scalar values are
//! validated newtypes: each has a fallible `new`, and the same check runs when
//! a value decodes, so a pose that exists holds usable numbers. The vector
//! fields ([`Pose::target`], [`Frame::bounds`], [`CameraRay::pixel`]) are
//! plain `aether-math` values; the camera refuses a non-finite one with a
//! warn.
//!
//! The view a camera publishes is not here: it is
//! [`aether_render::ViewProjection`], sent to whoever subscribed through the
//! [`aether_render::ViewSource`] protocol.
//!
//! [`CameraComponent`]: crate::camera::CameraComponent

use core::borrow::Borrow;
use core::error::Error as StdError;
use core::f32::consts::{FRAC_PI_2, FRAC_PI_3, PI};
use core::fmt;

use aether_actor::ActorPath;
use aether_data::LoadName;
use aether_math::{Aabb, Ray, Vec2, Vec3};
use aether_window::{INITIAL_WINDOW_NAME, WindowCapability, WindowInstance};

/// Why a camera value was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraValueError {
    /// A yaw that is not finite.
    YawNotFinite,
    /// A pitch that is not finite or lies outside `[-PI/2, PI/2]`.
    PitchOutOfRange,
    /// A distance that is not finite or not greater than zero.
    DistanceNotPositive,
    /// A field of view that is not finite or lies outside `(0, PI)`.
    FieldOfViewOutOfRange,
    /// An orthographic extent that is not finite or not greater than zero.
    OrthoExtentNotPositive,
    /// A pixel count of zero.
    PixelsZero,
}

impl CameraValueError {
    const fn reason(self) -> &'static str {
        match self {
            Self::YawNotFinite => "yaw-not-finite",
            Self::PitchOutOfRange => "pitch-out-of-range",
            Self::DistanceNotPositive => "distance-not-positive",
            Self::FieldOfViewOutOfRange => "field-of-view-out-of-range",
            Self::OrthoExtentNotPositive => "ortho-extent-not-positive",
            Self::PixelsZero => "pixels-zero",
        }
    }
}

impl aether_data::Invariant for CameraValueError {
    fn reason(&self) -> &'static str {
        Self::reason(*self)
    }
}

impl fmt::Display for CameraValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl StdError for CameraValueError {}

/// Rotation of the eye about the world `Y` axis through the target, in
/// radians. Finite; not wrapped, so a pose may hold any number of turns.
#[derive(Debug, Clone, Copy, PartialEq, aether_data::Storage)]
#[storage(validate)]
pub struct Yaw(f32);

impl Yaw {
    /// Accept a finite angle.
    ///
    /// # Errors
    ///
    /// [`CameraValueError::YawNotFinite`] for a NaN or infinite angle.
    pub fn new(radians: f32) -> Result<Self, CameraValueError> {
        Self::check(radians)?;
        Ok(Self(radians))
    }

    /// The angle in radians.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    // `#[storage(validate)]` calls `check(&inner)`; `Borrow` takes that
    // reference and `new`'s owned value alike.
    fn check(radians: impl Borrow<f32>) -> Result<(), CameraValueError> {
        if radians.borrow().is_finite() {
            Ok(())
        } else {
            Err(CameraValueError::YawNotFinite)
        }
    }
}

/// Tilt of the eye above or below the target, in radians, within
/// `[-PI/2, PI/2]`. A negative pitch puts the eye above the target looking
/// down; `-PI/2` (`-1.5707964` as an `f32`) looks straight down and `PI/2`
/// straight up, and both are ordinary poses.
#[derive(Debug, Clone, Copy, PartialEq, aether_data::Storage)]
#[storage(validate)]
pub struct Pitch(f32);

impl Pitch {
    /// The pitch that looks straight down on the target.
    pub const DOWN: Self = Self(-FRAC_PI_2);

    /// Accept a finite angle within `[-PI/2, PI/2]`.
    ///
    /// # Errors
    ///
    /// [`CameraValueError::PitchOutOfRange`] otherwise.
    pub fn new(radians: f32) -> Result<Self, CameraValueError> {
        Self::check(radians)?;
        Ok(Self(radians))
    }

    /// The angle in radians.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    fn check(radians: impl Borrow<f32>) -> Result<(), CameraValueError> {
        let radians = *radians.borrow();
        // A NaN is in no range.
        let within = (-FRAC_PI_2..=FRAC_PI_2).contains(&radians);
        if within {
            Ok(())
        } else {
            Err(CameraValueError::PitchOutOfRange)
        }
    }
}

/// How far the eye sits from the target, in world units. Finite and greater
/// than zero. It also sets the depth planes and, under an orthographic lens,
/// how much of the world is in view.
#[derive(Debug, Clone, Copy, PartialEq, aether_data::Storage)]
#[storage(validate)]
pub struct Distance(f32);

impl Distance {
    /// Accept a finite distance greater than zero.
    ///
    /// # Errors
    ///
    /// [`CameraValueError::DistanceNotPositive`] otherwise.
    pub fn new(units: f32) -> Result<Self, CameraValueError> {
        Self::check(units)?;
        Ok(Self(units))
    }

    /// The distance in world units.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    fn check(units: impl Borrow<f32>) -> Result<(), CameraValueError> {
        let units = *units.borrow();
        let positive = units > 0.0 && units.is_finite();
        if positive {
            Ok(())
        } else {
            Err(CameraValueError::DistanceNotPositive)
        }
    }
}

/// A perspective lens's vertical field of view, in radians, within `(0, PI)`.
#[derive(Debug, Clone, Copy, PartialEq, aether_data::Storage)]
#[storage(validate)]
pub struct FieldOfView(f32);

impl FieldOfView {
    /// Accept a finite angle within `(0, PI)`.
    ///
    /// # Errors
    ///
    /// [`CameraValueError::FieldOfViewOutOfRange`] otherwise.
    pub fn new(radians: f32) -> Result<Self, CameraValueError> {
        Self::check(radians)?;
        Ok(Self(radians))
    }

    /// The angle in radians.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    fn check(radians: impl Borrow<f32>) -> Result<(), CameraValueError> {
        let radians = *radians.borrow();
        let within = radians > 0.0 && radians < PI;
        if within {
            Ok(())
        } else {
            Err(CameraValueError::FieldOfViewOutOfRange)
        }
    }
}

/// An orthographic lens's half-height at the target per unit of
/// [`Distance`]: the visible half-height is `distance * extent`, so moving
/// the eye zooms an orthographic view as it does a perspective one. Finite
/// and greater than zero.
#[derive(Debug, Clone, Copy, PartialEq, aether_data::Storage)]
#[storage(validate)]
pub struct OrthoExtent(f32);

impl OrthoExtent {
    /// Accept a finite extent greater than zero.
    ///
    /// # Errors
    ///
    /// [`CameraValueError::OrthoExtentNotPositive`] otherwise.
    pub fn new(per_unit_distance: f32) -> Result<Self, CameraValueError> {
        Self::check(per_unit_distance)?;
        Ok(Self(per_unit_distance))
    }

    /// The half-height per unit of distance.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    fn check(per_unit_distance: impl Borrow<f32>) -> Result<(), CameraValueError> {
        let extent = *per_unit_distance.borrow();
        let positive = extent > 0.0 && extent.is_finite();
        if positive {
            Ok(())
        } else {
            Err(CameraValueError::OrthoExtentNotPositive)
        }
    }
}

/// A count of physical pixels along one side of a viewport. Not zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct Pixels(u32);

impl Pixels {
    /// Accept a count that is not zero.
    ///
    /// # Errors
    ///
    /// [`CameraValueError::PixelsZero`] for zero.
    pub fn new(count: u32) -> Result<Self, CameraValueError> {
        Self::check(count)?;
        Ok(Self(count))
    }

    /// The count.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    fn check(count: impl Borrow<u32>) -> Result<(), CameraValueError> {
        if *count.borrow() == 0 {
            Err(CameraValueError::PixelsZero)
        } else {
            Ok(())
        }
    }
}

/// `aether.kit.camera.pose` — where a camera is: the point it looks at, the
/// eye's yaw and pitch about that point, and the eye's distance from it.
///
/// One kind in three places. Sent to a camera it sets the pose, ending any
/// glide in progress. It is the reply to [`Where`]. It is a field of
/// [`CameraConfig`] and [`Glide`]. A pose whose `target` is not finite is
/// refused with a warn and changes nothing.
#[aether_data::kind(name = "aether.kit.camera.pose", copy, partial_eq, no_serde)]
pub struct Pose {
    /// The world-space point the eye looks at and turns about.
    pub target: Vec3,
    pub yaw: Yaw,
    pub pitch: Pitch,
    pub distance: Distance,
}

impl Pose {
    /// The pose a camera spawned with no [`CameraConfig::pose`] starts at: the
    /// origin from three units back, tilted slightly up at it.
    pub const BOOT: Self = Self { target: Vec3::ZERO, yaw: Yaw(0.0), pitch: Pitch(0.3), distance: Distance(3.0) };
}

/// How a camera projects view space.
#[derive(aether_data::Schema, Debug, Clone, Copy, PartialEq)]
pub enum Lens {
    /// A perspective projection with vertical field of view `fov`.
    Perspective { fov: FieldOfView },
    /// An orthographic projection whose half-height at the target is
    /// `distance * extent`. With [`Pitch::DOWN`] it is a top-down view.
    Orthographic { extent: OrthoExtent },
}

impl Lens {
    /// The lens of a camera spawned with no config: a 60° perspective.
    pub const BOOT: Self = Self::Perspective { fov: FieldOfView(FRAC_PI_3) };
}

/// The surface a camera's projection is built for.
#[derive(aether_data::Schema, Debug, Clone, PartialEq, Eq)]
pub enum Viewport {
    /// The window at this path, `aether.window/aether.window.instance:<name>`.
    /// The camera follows its size and publishes nothing until it has learned
    /// it. The path is compared, never sent to; it is typed so a config
    /// cannot name something that is not a window.
    Window(ActorPath<WindowInstance>),
    /// A target of a fixed size, whatever any window does.
    Fixed { width: Pixels, height: Pixels },
}

impl Viewport {
    /// The chassis's initial window, `main`.
    ///
    /// # Panics
    ///
    /// Never: the window's name is a valid segment and its path is two steps,
    /// under the depth and byte caps.
    #[must_use]
    pub fn initial_window() -> Self {
        let name = LoadName::new(INITIAL_WINDOW_NAME).expect("the initial window's name is a valid segment");
        let window = ActorPath::<WindowInstance>::child(&ActorPath::<WindowCapability>::root(), &name)
            .expect("a window path is two valid steps, under both caps");

        Self::Window(window)
    }
}

/// `aether.kit.camera.config` — what a camera instance is spawned with.
///
/// # Agent
/// Pass as `config` to `load_component` / `spawn` with `namespace:
/// "aether.kit.camera"` and a `key`, for example
/// `{"lens": {"Perspective": {"fov": 1.0471976}}, "viewport": {"Window":
/// "aether.window/aether.window.instance:main"}, "pose": null}`. A spawn with
/// no config gets [`CameraConfig::default`]: a 60° perspective following the
/// `main` window from [`Pose::BOOT`].
#[aether_data::kind(name = "aether.kit.camera.config", partial_eq, no_serde)]
pub struct CameraConfig {
    pub lens: Lens,
    pub viewport: Viewport,
    /// The starting pose; `None` is [`Pose::BOOT`].
    pub pose: Option<Pose>,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self { lens: Lens::BOOT, viewport: Viewport::initial_window(), pose: None }
    }
}

/// `aether.kit.camera.frame` — look at the centre of `bounds` from far enough
/// back that the whole box is in view, keeping yaw, pitch and lens. Ends any
/// glide in progress. Refused with a warn, changing nothing, for an empty,
/// zero-size or non-finite box, and while the camera does not know its
/// viewport's size.
#[aether_data::kind(name = "aether.kit.camera.frame", copy, partial_eq, no_serde)]
pub struct Frame {
    pub bounds: Aabb,
}

/// `aether.kit.camera.glide` — move from the current pose to `to` over
/// `over_millis` milliseconds, eased at both ends. A [`Pose`], a [`Frame`] or
/// another glide ends it where it stands. Zero milliseconds sets the pose at
/// once.
#[aether_data::kind(name = "aether.kit.camera.glide", copy, partial_eq, no_serde)]
pub struct Glide {
    pub to: Pose,
    pub over_millis: u32,
}

/// `aether.kit.camera.where` — ask a camera its current [`Pose`], which is the
/// reply. During a glide it is the pose the camera has reached.
#[aether_data::kind(name = "aether.kit.camera.where", copy, eq, no_serde)]
pub struct Where;

/// `aether.kit.camera.ray` — ask for the world-space ray through `pixel`, in
/// the viewport's physical pixels with the origin at its top-left corner.
/// Reply: [`CameraRayResult`].
#[aether_data::kind(name = "aether.kit.camera.ray", copy, partial_eq, no_serde)]
pub struct CameraRay {
    pub pixel: Vec2,
}

/// `aether.kit.camera.ray_result` — reply to [`CameraRay`].
#[aether_data::kind(name = "aether.kit.camera.ray_result", copy, partial_eq, no_serde)]
pub enum CameraRayResult {
    /// The ray from the near plane through the pixel, with a unit direction.
    Ok(Ray),
    /// The camera follows a window whose size it has not learned yet.
    NoViewport,
    /// The pixel is not finite, so no ray passes through it.
    NoRay,
}

#[cfg(test)]
mod tests {
    use aether_data::Kind;
    use aether_data::wire::encode_to_vec;

    use super::*;

    /// Each check's bounds are open or closed as its doc says: the poles are
    /// poses, a zero distance and a flat or straight-angle lens are not. An
    /// off-by-one in a comparison would admit a degenerate projection or
    /// refuse the top-down pose.
    #[test]
    fn value_bounds_are_open_or_closed_as_documented() {
        assert_eq!(Pitch::new(-FRAC_PI_2), Ok(Pitch::DOWN));
        assert!(Pitch::new(FRAC_PI_2).is_ok());
        assert_eq!(Pitch::new(FRAC_PI_2.next_up()), Err(CameraValueError::PitchOutOfRange));
        assert_eq!(Pitch::new(f32::NAN), Err(CameraValueError::PitchOutOfRange));

        assert_eq!(Distance::new(0.0), Err(CameraValueError::DistanceNotPositive));
        assert_eq!(Distance::new(f32::INFINITY), Err(CameraValueError::DistanceNotPositive));
        assert!(Distance::new(f32::MIN_POSITIVE).is_ok());

        assert_eq!(FieldOfView::new(0.0), Err(CameraValueError::FieldOfViewOutOfRange));
        assert_eq!(FieldOfView::new(PI), Err(CameraValueError::FieldOfViewOutOfRange));
        assert!(FieldOfView::new(PI.next_down()).is_ok());

        assert_eq!(OrthoExtent::new(-1.0), Err(CameraValueError::OrthoExtentNotPositive));
        assert_eq!(Yaw::new(f32::NEG_INFINITY), Err(CameraValueError::YawNotFinite));
        assert_eq!(Pixels::new(0), Err(CameraValueError::PixelsZero));
    }

    /// A pose that arrives as mail is checked like one built here. A newtype
    /// that lost `#[storage(validate)]` would decode a zero distance, and the
    /// camera would publish a projection with coincident depth planes.
    #[test]
    fn a_pose_with_a_zero_distance_does_not_decode() {
        let encode = |distance: f32| -> Vec<u8> {
            let mut bytes = encode_to_vec(&Vec3::new(1.0, 2.0, 3.0)).expect("encode target");
            for scalar in [0.5_f32, -0.25, distance] {
                bytes.extend(encode_to_vec(&scalar).expect("encode scalar"));
            }
            bytes
        };
        let valid = Pose {
            target: Vec3::new(1.0, 2.0, 3.0),
            yaw: Yaw::new(0.5).expect("a finite yaw"),
            pitch: Pitch::new(-0.25).expect("a pitch in range"),
            distance: Distance::new(4.0).expect("a positive distance"),
        };

        assert_eq!(valid.encode_into_bytes(), encode(4.0), "the hand-built layout matches the kind");
        assert_eq!(Pose::decode_from_bytes(&encode(4.0)), Some(valid));
        assert_eq!(Pose::decode_from_bytes(&encode(0.0)), None);
    }
}
