// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload.
#![allow(clippy::needless_pass_by_value)]

//! A motor for the puppet's rig.
//!
//! One `aether.puppet.pose` per tick, each of the eight channels on its own
//! phase, so the subject is never quite still with nothing crossing the wasm
//! boundary after boot. The sibling of
//! [`aether-puppet-turntable`](https://docs.rs/aether-puppet-turntable),
//! which does the same for her eye: subscribe the frame stage, mail the
//! peer, leave the peer a state machine that only knows the pose it is in.
//!
//! # Why a component and not a script
//!
//! The same reason the turntable is one. A settled `send_mail` returns as
//! fast as the chain allows, so a sweep issued from a harness or an MCP
//! session arrives as a snap-through, and pacing it from outside substitutes
//! host round-trip jitter for the frame clock. Motion is a question about
//! the frame stage and has to be paced by it.
//!
//! # Two jobs
//!
//! [`Motion::Idle`] is the always-on motion that reads as a subject being
//! alive. [`Motion::Solo`] drives one channel and holds the other seven at
//! rest, which is how "does this channel move the right flesh, and nothing
//! else" gets asked — a question that cannot be answered while eight
//! channels move at once.
//!
//! # Pose ownership
//!
//! `aether.puppet.pose` is absolute — it replaces the puppet's whole pose —
//! so the motor states all eight channels every tick, including the ones it
//! is holding at zero. An out-of-band poke at the pose therefore survives
//! exactly one frame. That is the same accepted limit the turntable and the
//! camera controller carry, and for the same reason: the driver, not the
//! driven, is the source of truth.

mod channel;
mod kinds;

pub use channel::{Authored, Shape, per_tick, shaped};
pub use kinds::*;

use aether_actor::{ActorInitError, MailSender, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::Tick;
use aether_lifecycle::{LifecycleCapability, LifecycleMailboxExt};
use aether_puppet::Pose;

/// Channels the rig carries, which is the width of every array here.
const CHANNELS: usize = 8;

/// The clock driving a peer puppet's rig.
pub struct Idle {
    config: IdleConfig,
    /// Each channel's position in its own cycle, in `[0, 1)`. Wrapped every
    /// tick rather than accumulated — see [`channel`] for why.
    phases: [f32; CHANNELS],
    /// Phase advanced per tick, resolved once from the configured periods
    /// and cadence. Zero for every channel the current [`Motion`] does not
    /// drive, which is what makes a solo hold the other seven still without
    /// a branch in the hot path.
    steps: [f32; CHANNELS],
    /// The last pose sent, so a motor whose channels have not moved the
    /// pose off its previous value sends nothing.
    last: Option<Pose>,
    /// Ticks seen, for the sampling stride alone.
    ticks: u64,
}

#[actor]
impl WasmActor for Idle {
    type Config = IdleConfig;
    const NAMESPACE: &'static str = "aether.puppet-idle";

    fn init(config: IdleConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let steps = steps(&config);
        if steps.iter().all(|step| *step == 0.0) && config.running {
            tracing::warn!(
                target: "aether_puppet_idle",
                tick_hz = config.tick_hz,
                motion = ?config.motion,
                "idle motor resolved no live channel; the subject will hold still",
            );
        }

        Ok(Self { config, phases: [0.0; CHANNELS], steps, last: None, ticks: 0 })
    }

    /// Subscribe the frame stage. `wire` is the placement rather than `init`
    /// because `init`'s ctx cannot mail.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
    }

    /// Advance every channel one tick and restate the pose. A parked motor,
    /// and one whose pose has not changed, return without sending.
    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        if let Some(pose) = self.advance() {
            ctx.send_to_named(&self.config.target, &pose);
        }
        self.sample();
    }
}

impl Idle {
    /// Step every channel and return the pose to send, or `None` while
    /// parked or while the pose is unchanged.
    fn advance(&mut self) -> Option<Pose> {
        if !self.config.running {
            return None;
        }

        for (phase, step) in self.phases.iter_mut().zip(self.steps) {
            *phase = (*phase + step).rem_euclid(1.0);
        }

        let pose = self.pose();
        if self.last == Some(pose) {
            return None;
        }

        self.last = Some(pose);
        Some(pose)
    }

    /// The pose the current phases describe.
    ///
    /// Every channel is written every tick, including the ones held at
    /// zero, because the kind is absolute: a channel omitted from the pose
    /// is a channel commanded to rest, not a channel left alone.
    fn pose(&self) -> Pose {
        let mut pose = Pose::default();
        for channel in Channel::ALL {
            let phase = self.phases[channel.slot()];
            let degrees = match self.config.motion {
                Motion::Idle => {
                    let authored = channel.authored();
                    authored.degrees * self.config.liveliness * shaped(authored.shape, phase)
                }
                Motion::Solo if channel == self.config.channel => self.config.degrees * shaped(Shape::Sway, phase),
                Motion::Solo => 0.0,
            };
            channel.apply(&mut pose, degrees);
        }

        pose
    }

    /// Drop one stamped entry in the log ring every `log_every` ticks. Runs
    /// parked as well as running, so a held subject is measurable through
    /// the same instrument as a moving one.
    fn sample(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
        if self.config.log_every == 0 || !self.ticks.is_multiple_of(u64::from(self.config.log_every)) {
            return;
        }

        let pose = self.last.unwrap_or_default();
        tracing::info!(
            target: "aether_puppet_idle",
            tick = self.ticks,
            yaw = pose.yaw,
            pitch = pose.pitch,
            ear_flick_left = pose.ear_flick_left,
            "idle tick",
        );
    }
}

/// Per-tick phase step for every channel under the configured motion.
///
/// A channel the motion does not drive gets a zero step, which parks its
/// phase at zero and therefore its contribution at whatever the shape reads
/// there. Both shapes read zero at phase zero, so an undriven channel is a
/// channel at rest.
fn steps(config: &IdleConfig) -> [f32; CHANNELS] {
    let mut steps = [0.0; CHANNELS];
    for channel in Channel::ALL {
        steps[channel.slot()] = match config.motion {
            Motion::Idle => per_tick(channel.authored().period_seconds, config.tick_hz),
            Motion::Solo if channel == config.channel => per_tick(config.period_seconds, config.tick_hz),
            Motion::Solo => 0.0,
        };
    }

    steps
}

aether_actor::export!(Idle);

#[cfg(test)]
mod tests {
    use super::*;

    fn motor(config: IdleConfig) -> Idle {
        let steps = steps(&config);
        Idle { config, phases: [0.0; CHANNELS], steps, last: None, ticks: 0 }
    }

    /// Whether this pose has one channel off rest. Zeroing the channel and
    /// comparing reads the field without a second match arm to keep in step
    /// with [`Channel::apply`].
    fn carries(pose: &Pose, channel: Channel) -> bool {
        let mut cleared = *pose;
        channel.apply(&mut cleared, 0.0);
        cleared != *pose
    }

    #[test]
    fn parked_sends_nothing() {
        // Tripwire: the zero-mail-idle invariant, the same one the
        // turntable holds. `running: false` has to park the motor without
        // unloading it, so a parked tick produces no pose however long it
        // is left there.
        let mut parked = motor(IdleConfig { running: false, ..IdleConfig::default() });
        for _ in 0..1_000 {
            assert!(parked.advance().is_none(), "a parked motor sends nothing");
        }
    }

    #[test]
    fn a_solo_moves_one_channel_and_holds_the_other_seven() {
        // Tripwire: the whole point of `Solo`. If an undriven channel picks
        // up a step, or the shape reads nonzero at phase zero, the mode
        // stops being able to answer the question it exists for — and the
        // failure is invisible in the idle, where everything moves anyway.
        for channel in Channel::ALL {
            let config = IdleConfig { motion: Motion::Solo, channel, ..IdleConfig::default() };
            let mut solo = motor(config);

            let mut moved = [false; CHANNELS];
            for _ in 0..600 {
                let Some(pose) = solo.advance() else {
                    continue;
                };
                for other in Channel::ALL {
                    moved[other.slot()] |= carries(&pose, other);
                }
            }

            for other in Channel::ALL {
                assert_eq!(
                    moved[other.slot()],
                    other == channel,
                    "soloing {channel:?} left {other:?} at moved={}",
                    moved[other.slot()],
                );
            }
        }
    }

    #[test]
    fn an_unchanged_pose_is_not_resent() {
        // Tripwire: the resend guard. `liveliness: 0.0` is the documented
        // way to hold her still with the motor loaded, and it has to cost
        // one mail rather than one per tick forever — the puppet caches on
        // the pose, so a resend is cheap, but a mail per frame per driver
        // is the thing components are supposed to stop doing.
        let mut still = motor(IdleConfig { liveliness: 0.0, ..IdleConfig::default() });

        assert!(still.advance().is_some(), "the first pose is always news");
        for _ in 0..1_000 {
            assert!(still.advance().is_none(), "a pose that has not changed is not resent");
        }
    }

    #[test]
    fn the_idle_moves_every_channel_it_authored() {
        // Tripwire: the authored table reaching the pose. A channel whose
        // amplitude never arrives — a dropped `liveliness` multiply, a
        // `slot` collision, a shape that reads flat — leaves that flesh
        // dead while everything around it moves, which is far harder to
        // spot than a subject that does not move at all.
        let mut idle = motor(IdleConfig::default());
        let mut moved = [false; CHANNELS];
        for _ in 0..(60 * 15) {
            let Some(pose) = idle.advance() else {
                continue;
            };
            for channel in Channel::ALL {
                moved[channel.slot()] |= carries(&pose, channel);
            }
        }

        for channel in Channel::ALL {
            let authored = channel.authored();
            assert_eq!(
                moved[channel.slot()],
                authored.degrees > 0.0,
                "{channel:?} authored {}deg but moved={}",
                authored.degrees,
                moved[channel.slot()],
            );
        }
    }

    #[test]
    fn phases_wrap_instead_of_accumulating() {
        // Tripwire: the wrap, the same failure the turntable's azimuth
        // guards. A quarter of a million ticks is a little over an hour at
        // 60Hz; an accumulating phase is large enough by then that a single
        // step of about 0.0026 no longer changes it, and the subject
        // silently freezes mid-session.
        let mut idle = motor(IdleConfig::default());
        for _ in 0..250_000 {
            idle.advance();
            for phase in idle.phases {
                assert!((0.0..1.0).contains(&phase), "a phase left its cycle: {phase}");
            }
        }

        let still_moving = (0..60).any(|_| idle.advance().is_some());
        assert!(still_moving, "an hour in, the motor is still moving her");
    }
}
