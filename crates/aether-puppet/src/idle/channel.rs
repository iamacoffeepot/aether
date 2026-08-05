//! What each channel does with time.
//!
//! A channel is a phase in `[0, 1)` and a shape that reads a value out of
//! it. The phase advances by elapsed time every tick and wraps rather than
//! accumulating, for the reason the turntable's azimuth does: an unwrapped
//! counter loses a bit of mantissa every time it doubles, so a motor left
//! running overnight steps instead of moving.
//!
//! # Why two shapes and not one
//!
//! A head sways and an ear does not. Driving all eight channels off the
//! same sine gives a subject that moves continuously everywhere at once,
//! which reads as underwater rather than alive — the ears in particular
//! become slow waving fronds. An ear is still almost all the time and then
//! flicks: fast out, slower back, then nothing. That asymmetry is most of
//! what makes the motion read as an animal, and it costs one branch.

use super::kinds::Channel;
use crate::Pose;
use core::f32::consts::TAU;

/// How a channel's phase becomes a multiplier in `[-1, 1]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    /// A sine over the whole period. Continuous motion that is never quite
    /// still, which is what a head carried on a neck actually does.
    Sway,
    /// A twitch: a fast rise, a slower cubic settle, then rest for the
    /// remainder of the period.
    Flick,
}

/// Fraction of a flick's period the movement occupies. The rest is the
/// stillness that makes the movement read as an event rather than a wave.
const FLICK_SPAN: f32 = 0.22;

/// Fraction of the movement spent rising. Small, because an ear reaches
/// its limit far faster than it comes back down, and reversing the two
/// gives a slow raise and a snap that reads as a glitch.
const FLICK_ATTACK: f32 = 0.18;

/// One channel's authored idle: how far it travels, how long its cycle
/// takes, and which shape it travels with.
///
/// These are authored animation values, not configuration. What a viewer
/// reads as "alive" is a specific set of small numbers, and exposing them
/// as knobs would invite tuning by anyone except the person watching her.
/// [`IdleConfig::liveliness`](crate::IdleConfig::liveliness) scales the
/// whole set, which is the one axis worth handing out.
pub struct Authored {
    pub degrees: f32,
    pub period_seconds: f32,
    pub shape: Shape,
}

impl Channel {
    /// Every channel, in the order [`Pose`] declares them.
    pub const ALL: [Self; 8] = [
        Self::Yaw,
        Self::Pitch,
        Self::Roll,
        Self::Jaw,
        Self::EarFlickLeft,
        Self::EarFlickRight,
        Self::EarTwistLeft,
        Self::EarTwistRight,
    ];

    /// Index into the motor's phase array. The array is positional because
    /// there are exactly eight channels and the set is closed by [`Pose`];
    /// a map would be a heap allocation per motor to hold a constant.
    #[must_use]
    pub const fn slot(self) -> usize {
        match self {
            Self::Yaw => 0,
            Self::Pitch => 1,
            Self::Roll => 2,
            Self::Jaw => 3,
            Self::EarFlickLeft => 4,
            Self::EarFlickRight => 5,
            Self::EarTwistLeft => 6,
            Self::EarTwistRight => 7,
        }
    }

    /// This channel's authored idle.
    ///
    /// The periods are deliberately unrelated to each other. Sharing one,
    /// or picking values with a common factor, lands every channel back at
    /// rest on the same beat — and a subject that returns to a neutral pose
    /// on a regular pulse reads as a machine cycling however small the
    /// amplitudes are.
    #[must_use]
    pub const fn authored(self) -> Authored {
        match self {
            Self::Yaw => Authored { degrees: 2.5, period_seconds: 11.0, shape: Shape::Sway },
            Self::Pitch => Authored { degrees: 1.5, period_seconds: 7.0, shape: Shape::Sway },
            Self::Roll => Authored { degrees: 1.2, period_seconds: 13.0, shape: Shape::Sway },
            // Held shut. A jaw that drifts open on an idle reads as chewing,
            // and there is nothing for her to chew; the channel is reachable
            // through `Motion::Solo` and through whatever drives speech.
            Self::Jaw => Authored { degrees: 0.0, period_seconds: 5.0, shape: Shape::Sway },
            // The two ears differ in both amplitude and period so they never
            // flick together. A matched pair reads as a mechanism; an
            // unmatched one reads as two ears.
            Self::EarFlickLeft => Authored { degrees: 9.0, period_seconds: 6.5, shape: Shape::Flick },
            Self::EarFlickRight => Authored { degrees: 7.0, period_seconds: 9.5, shape: Shape::Flick },
            Self::EarTwistLeft => Authored { degrees: 5.0, period_seconds: 8.5, shape: Shape::Flick },
            Self::EarTwistRight => Authored { degrees: 6.0, period_seconds: 12.5, shape: Shape::Flick },
        }
    }

    /// Write this channel's degrees into the pose, leaving the other seven
    /// alone.
    pub fn apply(self, pose: &mut Pose, degrees: f32) {
        match self {
            Self::Yaw => pose.yaw = degrees,
            Self::Pitch => pose.pitch = degrees,
            Self::Roll => pose.roll = degrees,
            Self::Jaw => pose.jaw = degrees,
            Self::EarFlickLeft => pose.ear_flick_left = degrees,
            Self::EarFlickRight => pose.ear_flick_right = degrees,
            Self::EarTwistLeft => pose.ear_twist_left = degrees,
            Self::EarTwistRight => pose.ear_twist_right = degrees,
        }
    }
}

/// The multiplier a shape reads out of a phase in `[0, 1)`.
#[must_use]
pub fn shaped(shape: Shape, phase: f32) -> f32 {
    match shape {
        Shape::Sway => (phase * TAU).sin(),
        Shape::Flick => flick(phase),
    }
}

/// The flick envelope: rise, settle, rest.
fn flick(phase: f32) -> f32 {
    if phase >= FLICK_SPAN {
        return 0.0;
    }

    let local = phase / FLICK_SPAN;
    if local < FLICK_ATTACK {
        local / FLICK_ATTACK
    } else {
        let settling = (local - FLICK_ATTACK) / (1.0 - FLICK_ATTACK);
        (1.0 - settling).powi(3)
    }
}

/// Phase advanced per elapsed second for a period stated in seconds.
///
/// A period that is not finite and positive resolves to a still channel.
/// Dividing by it would hand the puppet an infinite or `NaN` angle, and a
/// `NaN` runs straight through the skin into a frame with nothing in it — a
/// config mistake that presents as the renderer failing.
#[must_use]
pub fn per_second(period_seconds: f32) -> f32 {
    if period_seconds.is_finite() && period_seconds > 0.0 {
        1.0 / period_seconds
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_channel_writes_its_own_field() {
        // Tripwire: the `apply` mapping. Eight near-identical match arms
        // over eight near-identically named fields is where a copy-paste
        // lands, and the failure it produces is exactly the one #4339's
        // acceptance is about — a channel that visibly moves the wrong
        // flesh. Writing one channel and reading all eight catches both
        // halves: an arm that writes the wrong field, and an arm that
        // writes two.
        for channel in Channel::ALL {
            let mut pose = Pose::default();
            channel.apply(&mut pose, 1.0);

            let written: Vec<Channel> = Channel::ALL
                .into_iter()
                .filter(|other| {
                    let mut probe = Pose::default();
                    other.apply(&mut probe, 1.0);
                    probe == pose
                })
                .collect();

            assert_eq!(written, vec![channel], "{channel:?} did not write exactly its own field");
        }
    }

    #[test]
    fn a_flick_rests_for_most_of_its_period() {
        // Tripwire: the span. Drop the early return and the envelope
        // stretches across the whole period, which turns the ear from
        // something that twitches into something that waves — the exact
        // failure the two shapes exist to avoid, and one that looks
        // plausible in a still.
        let moving = (0..1_000u16).filter(|step| flick(f32::from(*step) / 1_000.0) > 1e-3).count();

        assert!(moving < 300, "a flick should be still for most of its period; moved for {moving} of 1000");
    }

    #[test]
    fn a_flick_rises_faster_than_it_settles() {
        // Tripwire: the asymmetry. Swapping attack and release is a
        // one-character edit that leaves the envelope continuous, bounded
        // and peaking in the same place — and reads as the ear being
        // dragged up and dropped, which is backwards.
        let peak = FLICK_SPAN * FLICK_ATTACK;
        let rising = peak;
        let settling = FLICK_SPAN - peak;

        assert!((flick(peak) - 1.0).abs() < 1e-5, "the envelope peaks where the attack ends");
        assert!(rising < settling, "the rise ({rising}) must be shorter than the settle ({settling})");
    }

    #[test]
    fn the_idle_channels_do_not_share_a_cycle() {
        // Tripwire: the authored periods. Two channels on the same period —
        // or on periods with a common factor — return to rest together, and
        // a subject that neutralises on a beat reads as a mechanism no
        // matter how small the motion is. The composite period is the LCM,
        // computed here rather than asserted per-pair so that editing any
        // one value is checked against all the others.
        let composite = Channel::ALL.into_iter().map(Channel::authored).filter(|authored| authored.degrees > 0.0).fold(
            1u64,
            |lcm, authored| {
                let tenths = tenths_of_a_second(authored.period_seconds);
                lcm / gcd(lcm, tenths) * tenths
            },
        );

        assert!(composite > 36_000, "the idle repeats every {}s, which is inside a sitting", composite / 10);
    }

    /// The authored periods are all small positive multiples of a tenth,
    /// so this is exact rather than a rounding policy.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "authored constants under 100s")]
    fn tenths_of_a_second(seconds: f32) -> u64 {
        (seconds * 10.0).round() as u64
    }

    fn gcd(mut a: u64, mut b: u64) -> u64 {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    }

    #[test]
    fn a_nonsense_period_holds_the_channel_still() {
        // Tripwire: the guard. Dividing by a zero or negative period hands
        // the puppet an infinite phase, which becomes `NaN` on the wrap and
        // projects to an empty frame — a config mistake that presents as
        // the renderer breaking.
        for period in [0.0, -6.5, f32::NAN, f32::INFINITY] {
            assert_eq!(per_second(period), 0.0, "period {period} produced a live step");
        }
    }
}
