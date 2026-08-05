//! The idle motor's own configuration vocabulary.
//!
//! It drives the puppet through the puppet's existing
//! [`Pose`](crate::Pose) kind, so there is no drive kind here — only
//! [`IdleConfig`] and the two enums it selects with, read once at
//! instantiation (ADR-0090). A bare load with no `config_path` boots the
//! compiled [`Default`], which is the authored idle at full strength.

use serde::{Deserialize, Serialize};

/// One of the rig's eight channels, in the order
/// [`Pose`](crate::Pose) declares them.
///
/// Named rather than indexed because the whole point of [`Motion::Solo`] is
/// to answer "does this channel move the flesh it claims to" — a question
/// asked by a reader who knows the channel by name and would have to count
/// struct fields to turn it into a number.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Channel {
    /// Turn, about the head pivot. Shared with the neck.
    #[default]
    Yaw,
    Pitch,
    Roll,
    /// The mandible, hinged below and in front of the ear canal.
    Jaw,
    /// The blade swinging out of the midline plane.
    EarFlickLeft,
    EarFlickRight,
    /// Aim rather than flap: the cup sweeping about the blade's long axis.
    EarTwistLeft,
    EarTwistRight,
}

/// What the motor is doing with the rig.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Motion {
    /// Every channel on its own authored amplitude and period, scaled by
    /// [`IdleConfig::liveliness`]. This is the always-on motion that reads
    /// as a subject being alive rather than a model being displayed.
    #[default]
    Idle,
    /// One channel alone, on [`IdleConfig::degrees`] and
    /// [`IdleConfig::period_seconds`], with the other seven held at rest.
    ///
    /// This is the verification mode. A channel that moves the wrong flesh
    /// is invisible inside the idle — eight channels moving at once is
    /// exactly the condition under which nobody can tell which one dragged
    /// the temple — so the acceptance question is asked one channel at a
    /// time, against a subject holding perfectly still everywhere else.
    Solo,
}

/// Init-config for [`Idle`](crate::Idle): which motion to run, how strongly,
/// and — in [`Motion::Solo`] — which single channel to sweep.
///
/// # Agent
/// Encode one of these to the motor's `Config` shape and pass it as the
/// `config` bytes of the `aether.component.load` that instantiates it (or
/// `load_component`'s `config_path`). Omitting config bytes boots
/// [`IdleConfig::default()`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.puppet-idle.config")]
pub struct IdleConfig {
    /// Whether the motor is engaged. `false` parks it — the motor stays
    /// loaded and sends nothing at all, so the subject holds whatever pose
    /// it was last given rather than snapping back to rest.
    pub running: bool,
    /// Tick cadence the periods are converted against. `aether.lifecycle.tick`
    /// carries no elapsed time, so a period stated in seconds needs the
    /// cadence stated alongside it; on the headless chassis this is
    /// `AETHER_TICK_HZ`.
    ///
    /// The desktop chassis ticks per frame rather than on a timer, so there
    /// the realized rate is the configured one only while the frame rate
    /// matches this — and the gap between the two is a reading of the frame
    /// rate, not an error to correct.
    pub tick_hz: f32,
    /// Idle every channel, or solo one of them.
    pub motion: Motion,
    /// Scales every channel's authored amplitude in [`Motion::Idle`].
    /// `0.0` holds her still without unloading the motor, `1.0` is the
    /// authored idle, and above that exaggerates it. Ignored in
    /// [`Motion::Solo`], which states its own amplitude.
    pub liveliness: f32,
    /// The channel [`Motion::Solo`] drives. Ignored in [`Motion::Idle`].
    pub channel: Channel,
    /// Degrees the solo channel sweeps to either side of rest. Ignored in
    /// [`Motion::Idle`].
    ///
    /// The rig clamps each channel to the arc it actually has, so a solo
    /// asking for more than that is a stress pose rather than a broken one:
    /// it parks the channel at its limit and is the intended way to reach
    /// the limit at all.
    pub degrees: f32,
    /// Seconds one full solo sweep takes, out and back. Ignored in
    /// [`Motion::Idle`].
    pub period_seconds: f32,
    /// Emit one log line every `n` ticks, carrying the tick counter and the
    /// pose sent. `0` disables it, which is the default.
    ///
    /// Per-actor log entries are stamped with a host wall-clock millisecond
    /// as they land in the ring (ADR-0081), and on a frame-driven chassis
    /// one tick is one frame — so consecutive entries read out the frame
    /// periods of a window nothing is holding back, the same way the
    /// turntable's sampling does for a turning one.
    pub log_every: u32,
}

impl Default for IdleConfig {
    fn default() -> Self {
        Self {
            running: true,
            tick_hz: 60.0,
            motion: Motion::Idle,
            liveliness: 1.0,
            channel: Channel::Yaw,
            // A slow sweep, because a solo is read rather than watched: the
            // question is which flesh moved, and that is easier to answer
            // while the channel is travelling than at either end of it.
            degrees: 12.0,
            period_seconds: 4.0,
            log_every: 0,
        }
    }
}
