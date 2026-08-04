// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload.
#![allow(clippy::needless_pass_by_value)]

//! A motor for the puppet's eye.
//!
//! One `aether.puppet.look` per tick, azimuth advanced by a constant, so the
//! subject turns at a steady rate with nothing crossing the wasm boundary
//! after boot. The `aether.kit.camera-controller` pattern — subscribe the
//! input, mail the pose to a peer, leave the peer a state machine that only
//! knows where the eye is — with a clock in place of the keyboard.
//!
//! # Why a component and not a script
//!
//! The pose could be poked from outside, and that is what a harness or an
//! MCP session does. Neither can turn the puppet at a *rate*: a settled
//! `send_mail` returns as fast as the chain allows, so a sweep issued from
//! the host arrives as a snap-through, and pacing it from outside substitutes
//! host round-trip jitter for the frame clock. A subscriber to the frame
//! stage is paced by the frame stage, which is the only clock the question
//! is about.
//!
//! # What it measures
//!
//! That makes it an instrument as much as a motor. A continuously turning
//! window is a frame with no host in it and nothing cached from the last
//! one — the puppet re-solves its visibility field whenever the eye moves —
//! so its steady-state period is the honest cost of the drawing. Set
//! [`TurntableConfig::log_every`] to sample it: entries land in the per-actor
//! log ring stamped with a host millisecond (ADR-0081), and consecutive
//! stamps are consecutive frames. Park the motor with
//! [`TurntableConfig::running`] to read the same window held.
//!
//! # Pose ownership
//!
//! `aether.puppet.look` is absolute — it replaces the puppet's whole pose —
//! so the turntable holds the elevation, distance and height it was
//! configured with and restates them every tick alongside the azimuth it
//! owns. An out-of-band poke at the puppet's pose therefore survives exactly
//! one frame. That is the same accepted limit the camera controller carries,
//! and for the same reason: the driver, not the driven, is the source of
//! truth.

mod kinds;
pub use kinds::*;

use aether_actor::{ActorInitError, MailSender, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::Tick;
use aether_lifecycle::{LifecycleCapability, LifecycleMailboxExt};
use aether_puppet::Look;

/// A full turn, in the degrees `Look` is stated in.
const FULL_TURN: f32 = 360.0;

/// The clock driving a peer puppet's eye.
pub struct Turntable {
    config: TurntableConfig,
    /// Degrees the eye has swept, wrapped into `[0, 360)` every tick rather
    /// than accumulated. An unwrapped counter loses a degree of mantissa
    /// every time it doubles, so a turntable left running overnight would
    /// visibly step rather than sweep.
    azimuth: f32,
    /// Degrees per tick, resolved once from the configured rate and cadence.
    /// Zero whenever the cadence is not a positive number, which keeps a
    /// nonsense config a still subject rather than a `NaN` pose the puppet
    /// would project into an empty frame.
    step: f32,
    /// Ticks seen, for the sampling stride alone.
    ticks: u64,
}

#[actor]
impl WasmActor for Turntable {
    type Config = TurntableConfig;
    const NAMESPACE: &'static str = "aether.puppet-turntable";

    fn init(config: TurntableConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let step = per_tick(config.degrees_per_second, config.tick_hz);
        if step == 0.0 && config.running {
            tracing::warn!(
                target: "aether_puppet_turntable",
                degrees_per_second = config.degrees_per_second,
                tick_hz = config.tick_hz,
                "turntable resolved a zero step; the subject will hold still",
            );
        }

        Ok(Self { azimuth: config.azimuth.rem_euclid(FULL_TURN), config, step, ticks: 0 })
    }

    /// Subscribe the frame stage. `wire` is the placement rather than `init`
    /// because `init`'s ctx cannot mail.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
    }

    /// Advance the sweep one tick and restate the pose. A parked turntable
    /// returns without sending, allocating, or touching the peer.
    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        if let Some(look) = self.advance() {
            ctx.send_to_named(&self.config.target, &look);
        }
        self.sample();
    }
}

impl Turntable {
    /// Step the azimuth and return the pose to send, or `None` while parked.
    fn advance(&mut self) -> Option<Look> {
        if !self.config.running {
            return None;
        }

        self.azimuth = (self.azimuth + self.step).rem_euclid(FULL_TURN);

        Some(Look {
            azimuth: self.azimuth,
            elevation: self.config.elevation,
            distance: self.config.distance,
            height: self.config.height,
        })
    }

    /// Drop one stamped entry in the log ring every `log_every` ticks. Runs
    /// parked as well as running, so the held condition is measurable through
    /// the same instrument as the turning one.
    fn sample(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
        if self.config.log_every == 0 || !self.ticks.is_multiple_of(u64::from(self.config.log_every)) {
            return;
        }

        tracing::info!(
            target: "aether_puppet_turntable",
            tick = self.ticks,
            azimuth = self.azimuth,
            "turntable tick",
        );
    }
}

/// Degrees per tick from a rate in degrees per second and a tick cadence.
///
/// A cadence that is not finite and positive resolves to a still subject:
/// dividing by it would hand the puppet an infinite or `NaN` azimuth, and a
/// `NaN` runs straight through the view matrix into a frame with nothing in
/// it — a failure that looks like the renderer breaking rather than like the
/// config being wrong.
fn per_tick(degrees_per_second: f32, tick_hz: f32) -> f32 {
    if tick_hz.is_finite() && tick_hz > 0.0 && degrees_per_second.is_finite() {
        degrees_per_second / tick_hz
    } else {
        0.0
    }
}

aether_actor::export!(Turntable);

#[cfg(test)]
mod tests {
    use super::*;

    fn turntable(config: TurntableConfig) -> Turntable {
        let step = per_tick(config.degrees_per_second, config.tick_hz);
        Turntable { azimuth: config.azimuth.rem_euclid(FULL_TURN), config, step, ticks: 0 }
    }

    #[test]
    fn parked_sends_nothing() {
        // Tripwire: the zero-mail-idle invariant. `running: false` has to
        // park the motor without unloading it, so a parked tick produces no
        // pose to send however long it is left there.
        let mut parked = turntable(TurntableConfig { running: false, ..TurntableConfig::default() });
        for _ in 0..1_000 {
            assert!(parked.advance().is_none(), "a parked turntable sends nothing");
        }
        assert!((parked.azimuth - TurntableConfig::default().azimuth).abs() < 1e-6, "and does not drift");
    }

    #[test]
    fn a_second_of_ticks_sweeps_the_configured_rate() {
        // Tripwire: the rate conversion. `degrees_per_second` is divided by
        // the cadence, not multiplied by it — inverted, a 30°/s sweep at 60 Hz
        // would spin at 1800°/s and read as a strobe rather than a turn.
        let config = TurntableConfig { azimuth: 0.0, degrees_per_second: 30.0, tick_hz: 60.0, ..Default::default() };
        let mut motor = turntable(config);
        for _ in 0..60 {
            motor.advance().expect("a running turntable sends every tick");
        }

        assert!((motor.azimuth - 30.0).abs() < 1e-3, "one second sweeps 30 degrees; got {}", motor.azimuth);
    }

    #[test]
    fn azimuth_wraps_instead_of_accumulating() {
        // Tripwire: the wrap. Ten thousand turns at a rate that does not
        // divide the circle has to land inside one turn and stay smooth —
        // an accumulator would be past 3.6 million by here, where a single
        // f32 step of 0.36 no longer changes the value at all.
        let config = TurntableConfig { azimuth: 0.0, degrees_per_second: 21.6, tick_hz: 60.0, ..Default::default() };
        let mut motor = turntable(config);
        let mut previous = 0.0;
        let mut stepped = 0;
        for _ in 0..1_000_000 {
            let look = motor.advance().expect("a running turntable sends every tick");
            assert!((0.0..FULL_TURN).contains(&look.azimuth), "azimuth stays inside one turn; got {}", look.azimuth);
            if (look.azimuth - previous).abs() > 1e-4 {
                stepped += 1;
            }
            previous = look.azimuth;
        }

        assert_eq!(stepped, 1_000_000, "every tick moves the eye; a saturated accumulator would stop moving it");
    }

    #[test]
    fn a_negative_rate_turns_the_other_way_and_still_wraps() {
        // Tripwire: the wrap is Euclidean. A plain `%` leaves a negative
        // azimuth, which is a legal angle but not one inside the stated
        // `[0, 360)` range the field documents.
        let config = TurntableConfig { azimuth: 0.0, degrees_per_second: -30.0, tick_hz: 60.0, ..Default::default() };
        let mut motor = turntable(config);
        for _ in 0..600 {
            let look = motor.advance().expect("a running turntable sends every tick");
            assert!((0.0..FULL_TURN).contains(&look.azimuth), "azimuth stays inside one turn; got {}", look.azimuth);
        }

        assert!((motor.azimuth - 60.0).abs() < 1e-3, "ten seconds back from zero lands at 60; got {}", motor.azimuth);
    }

    #[test]
    fn a_nonsense_cadence_holds_the_subject_still() {
        // Tripwire: the cadence guard. Dividing by a zero or negative
        // cadence hands the puppet an infinite azimuth, `rem_euclid` turns
        // that into `NaN`, and a `NaN` pose projects to an empty frame —
        // a config mistake that presents as the renderer failing.
        for cadence in [0.0, -60.0, f32::NAN, f32::INFINITY] {
            let config = TurntableConfig { tick_hz: cadence, ..TurntableConfig::default() };
            let mut motor = turntable(config);
            for _ in 0..10 {
                let look = motor.advance().expect("a running turntable sends every tick");
                assert!(look.azimuth.is_finite(), "cadence {cadence} produced a non-finite azimuth");
            }
            assert!(
                (motor.azimuth - TurntableConfig::default().azimuth).abs() < 1e-6,
                "cadence {cadence} moved the eye"
            );
        }
    }
}
