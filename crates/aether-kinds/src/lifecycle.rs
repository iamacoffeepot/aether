//! Lifecycle stage kind vocabulary. The subscription request kinds live in
//! `aether-lifecycle`, beside the capability that answers them, because
//! their subscriber is a `ProtocolPath` this crate cannot name.

// ADR-0082 lifecycle stage kinds. Most are empty signals. `Tick` carries
// the elapsed time its subscribers need to state motion in seconds rather
// than in an assumed frame cadence (issue 4470), and the running total a
// subscriber counts whole steps from.

use core::num::NonZeroU32;
use core::ops::Range;

/// Per-frame lifecycle stage (ADR-0082 §11), the one stage that carries
/// time. `delta_micros` is the game time this frame adds and
/// `elapsed_micros` is the game time since boot with this frame included, so
/// the frame covers `(elapsed_micros - delta_micros, elapsed_micros]` and
/// `delta_micros` is always the growth of `elapsed_micros`. The lifecycle
/// capability owns the total and adds each delta its driver states exactly
/// once.
///
/// Game time is what the chassis driver states: on desktop the wall time
/// between frames, each frame's share held to the driver's limit; on
/// headless the timer period for every `Tick` broadcast; in a harness the
/// stated duration. Motion subscribers integrate `delta_micros` so authored
/// seconds remain seconds when frame rate changes (issue 4470), and logic
/// that counts in whole steps reads them from [`Tick::steps`].
#[aether_data::kind(name = "aether.lifecycle.tick", copy, default, eq)]
pub struct Tick {
    pub delta_micros: u32,
    pub elapsed_micros: u64,
}

impl Tick {
    /// Elapsed time in seconds for rate/period integration.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub const fn delta_seconds(self) -> f32 {
        self.delta_micros as f32 / 1_000_000.0
    }

    /// The whole steps of `length` that complete in this frame, and the part
    /// of the next step already elapsed. Steps are numbered from 0 at boot;
    /// step `k` completes at `(k + 1) * length` and belongs to the frame whose
    /// half-open interval `(elapsed_micros - delta_micros, elapsed_micros]`
    /// holds that instant, so a boundary exactly at a frame's end belongs to
    /// that frame and one frame's steps end where the next frame's begin.
    ///
    /// Nothing but this mail and `length` enters the result: every actor that
    /// asks with the same length gets the same steps, whenever it was loaded.
    /// A hand-built `Tick` whose total is below its delta reads as a frame
    /// that began at zero.
    #[must_use]
    pub const fn steps(self, length: StepLength) -> Steps {
        let length_micros = length.micros() as u64;
        let first = self.elapsed_micros.saturating_sub(self.delta_micros as u64) / length_micros;
        Steps {
            first,
            count: self.elapsed_micros / length_micros - first,
            remainder_micros: self.elapsed_micros % length_micros,
            length,
        }
    }
}

/// The length of one logic step in microseconds, never zero. The length is
/// the game's own constant and the argument to [`Tick::steps`]; the engine
/// has no step length of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepLength(NonZeroU32);

impl StepLength {
    /// A step of `micros` microseconds, or `None` for zero.
    #[must_use]
    pub const fn from_micros(micros: u32) -> Option<Self> {
        match NonZeroU32::new(micros) {
            Some(micros) => Some(Self(micros)),
            None => None,
        }
    }

    /// The step's length in microseconds.
    #[must_use]
    pub const fn micros(self) -> u32 {
        self.0.get()
    }
}

/// The whole steps one [`Tick`] completes at one [`StepLength`], from
/// [`Tick::steps`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Steps {
    first: u64,
    count: u64,
    remainder_micros: u64,
    length: StepLength,
}

impl Steps {
    /// The index of the frame's first whole step. With a count of zero it is
    /// the index of the next step to complete.
    #[must_use]
    pub const fn first(self) -> u64 {
        self.first
    }

    /// How many whole steps complete in the frame.
    #[must_use]
    pub const fn count(self) -> u64 {
        self.count
    }

    /// The indices of the frame's whole steps, in order. Over a run of
    /// frames the ranges are contiguous: every step is handed out once.
    #[must_use]
    pub const fn indices(self) -> Range<u64> {
        self.first..self.first + self.count
    }

    /// The part of the next step already elapsed, in `[0, 1)`, for drawing
    /// between two steps.
    #[must_use]
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    pub const fn fraction(self) -> f32 {
        // A remainder one microsecond short of a very long step rounds to one
        // in `f32`, which would draw a whole step ahead.
        let fraction = (self.remainder_micros as f64 / self.length.micros() as f64) as f32;
        fraction.min(LARGEST_FRACTION)
    }
}

/// The largest `f32` below one.
const LARGEST_FRACTION: f32 = 1.0 - f32::EPSILON / 2.0;

/// Lifecycle stage broadcast — capability init pass (ADR-0082 §5).
/// Fires once at chassis boot, after every capability's actor-framework
/// `claim → init → wire → spawn` completes and before
/// [`InitComponents`] fires. Capabilities that need to send mail to
/// peers during boot subscribe to this stage.
#[repr(C)]
#[aether_data::kind(name = "aether.lifecycle.init_caps", pod, default, eq)]
pub struct InitCaps;

/// Lifecycle stage broadcast — component init pass (ADR-0082 §5).
/// Fires once after [`InitCaps`] settles, before the per-frame loop
/// begins. Component-category actors subscribe here when they need to
/// reach already-wired capabilities during their boot logic.
#[repr(C)]
#[aether_data::kind(name = "aether.lifecycle.init_components", pod, default, eq)]
pub struct InitComponents;

/// Lifecycle stage broadcast — render stage (ADR-0082 §1). Fires every
/// frame after the whole [`Tick`] chain has settled (ADR-0080 §6) on
/// chassis that declare a render state in their lifecycle graph (today:
/// desktop and `substrate_harness`). Render-producing actors compute their
/// per-frame state on [`Tick`] and submit it to `aether.render` here, on
/// `Render` — so a submission integrates the fully-settled cross-actor
/// state of the frame rather than racing other actors' Tick handlers.
/// Headless / hub chassis omit this state from their graph; subscribing
/// on a chassis that doesn't declare it rejects fail-fast at wire time
/// per ADR-0082 §7.
#[repr(C)]
#[aether_data::kind(name = "aether.lifecycle.render", pod, default, eq)]
pub struct Render;

/// Lifecycle stage broadcast — frame-present stage (ADR-0082 §1).
/// Fires every frame after [`Render`] on chassis that drive a display.
/// The default desktop graph routes the quit edge through this stage so
/// the current frame finishes drawing before shutdown.
#[repr(C)]
#[aether_data::kind(name = "aether.lifecycle.present", pod, default, eq)]
pub struct Present;

/// Lifecycle stage broadcast — shutdown stage (ADR-0082 §1). Fires
/// once when the graph reaches a terminal state. Subscribers perform
/// graceful cleanup with the full mail surface still operational
/// (save game state, flush a write, post a metric) before the chassis
/// runs each actor's `unwire` finaliser. Distinct from the actor
/// framework's per-actor `unwire` hook — ADR-0082 §12.
#[repr(C)]
#[aether_data::kind(name = "aether.lifecycle.shutdown", pod, default, eq)]
pub struct Shutdown;

/// Lifecycle escape signal (ADR-0082 §3). The one hardcoded signal the
/// driver recognises. Setting `quit_pending = true` on receipt; the
/// flag is consumed at the next state whose graph declares a `quit`
/// edge. Chassis bridges OS-level termination signals (ctrlc, winit
/// `WindowEvent::CloseRequested`, future hub-shutdown mail) to this
/// kind so three trigger sources converge on one consumption point.
#[repr(C)]
#[aether_data::kind(name = "aether.lifecycle.quit", pod, default, eq)]
pub struct Quit;

/// Driver-internal trigger that advances the lifecycle state machine by one
/// step (ADR-0082 §2). The chassis main loop sends this for every stage in a
/// frame. `delta_micros` is copied into [`Tick`] when the current stage is
/// `Tick`; other stages remain empty signals. The driver then broadcasts,
/// awaits settlement, and advances along the resolved edge (`next` or
/// `quit`). This is the cadence input, not a stage broadcast. Engine-only
/// mail (ADR-0233): the chassis drivers push it from host code through the
/// mailer.
#[repr(C)]
#[aether_data::kind(name = "aether.lifecycle.advance", pod, default, eq, engine_only)]
pub struct LifecycleAdvance {
    pub delta_micros: u32,
}

/// Reply to [`LifecycleAdvance`] signalling that the stage's broadcast
/// root has settled (ADR-0082 §6). The chassis main loop wait-replies
/// on this so cadence couples to actual work completion — back-pressure
/// flows from subscriber drain time back to the chassis. `completed`
/// is the kind id of the state the driver just finished broadcasting;
/// `next` is the kind id of the state the driver will broadcast on the
/// next [`LifecycleAdvance`], or `0` when the lifecycle reached a
/// terminal state.
#[aether_data::kind(name = "aether.lifecycle.advance_complete", copy, default, eq)]
pub struct LifecycleAdvanceComplete {
    pub completed: u64,
    pub next: u64,
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    const LENGTH: StepLength = StepLength::from_micros(20_000).expect("a nonzero length");

    /// The `Tick`s a run of frame deltas produces, each carrying the total
    /// its delta grew.
    fn frames(deltas: &[u32]) -> Vec<Tick> {
        let mut elapsed_micros = 0;
        deltas
            .iter()
            .map(|&delta_micros| {
                elapsed_micros += u64::from(delta_micros);
                Tick { delta_micros, elapsed_micros }
            })
            .collect()
    }

    /// A step boundary exactly at a frame's end belongs to that frame alone.
    /// A boundary counted by `[start, end]` would run the step in both
    /// frames, and one counted by `(start, end)` in neither.
    #[test]
    fn a_boundary_at_the_frame_end_is_counted_in_that_frame_only() {
        let steps: Vec<Steps> = frames(&[10_000, 10_000, 10_000]).into_iter().map(|tick| tick.steps(LENGTH)).collect();

        assert_eq!(steps.iter().map(|steps| steps.first()).collect::<Vec<_>>(), [0, 0, 1]);
        assert_eq!(steps.iter().map(|steps| steps.count()).collect::<Vec<_>>(), [0, 1, 0]);
        assert_eq!(steps.iter().map(|steps| steps.fraction()).collect::<Vec<_>>(), [0.5, 0.0, 0.5]);
    }

    /// Over uneven frames every step is handed out exactly once: the frames'
    /// index ranges concatenate to `0..E / L`. A frame edge that skipped a
    /// step or ran one twice would break the run, and a frame that adds no
    /// time must leave the leftover fraction where it was.
    #[test]
    fn uneven_frames_hand_out_every_step_once() {
        let ticks = frames(&[7_000, 0, 33_000, 95_001, 0, 19_999, 5_000]);
        let total = ticks.last().expect("the run has frames").elapsed_micros;

        let handed_out: Vec<u64> = ticks.iter().flat_map(|tick| tick.steps(LENGTH).indices()).collect();
        assert_eq!(handed_out, (0..total / u64::from(LENGTH.micros())).collect::<Vec<_>>());

        for pair in ticks.windows(2).filter(|pair| pair[1].delta_micros == 0) {
            assert_eq!(pair[1].steps(LENGTH).count(), 0, "a frame that adds no time completes no step");
            assert_eq!(pair[1].steps(LENGTH).fraction().to_bits(), pair[0].steps(LENGTH).fraction().to_bits());
        }
    }

    /// A hand-built `Tick` whose total is below its delta is a frame that
    /// began at zero. A wrapping subtraction would put its start near
    /// `u64::MAX` and underflow the count into billions of steps.
    #[test]
    fn a_total_below_its_delta_counts_from_zero() {
        let steps = Tick { delta_micros: 100_000, elapsed_micros: 60_000 }.steps(LENGTH);

        assert_eq!(steps.indices(), 0..3);
    }

    /// A remainder one microsecond short of the longest step is still short
    /// of it. The quotient rounds to one in `f32`, and a fraction of one
    /// would draw a whole step ahead of the step count.
    #[test]
    fn a_remainder_just_short_of_a_step_is_a_fraction_below_one() {
        let length = StepLength::from_micros(u32::MAX).expect("a nonzero length");
        let elapsed_micros = u64::from(u32::MAX) - 1;
        let steps = Tick { delta_micros: 1, elapsed_micros }.steps(length);

        assert_eq!(steps.count(), 0);
        assert!(steps.fraction() < 1.0, "fraction {} reached a whole step", steps.fraction());
    }
}
