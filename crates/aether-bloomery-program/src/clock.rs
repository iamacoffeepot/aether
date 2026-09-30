//! Waiting from a rule: the driver-native `clock.until` program and the
//! helpers that compute its due time from recorded time (ADR-0245).
//!
//! A rule requests a wait by naming the reserved [`CLOCK`] head with
//! [`ClockUntil::NAME`] and an [`Until`] input built by [`wait`] or
//! [`wait_spread`], and fires on the recorded run with a `Ran<ClockUntil>`
//! trigger. No bundle hosts the program: the driver runs it itself.
//!
//! [`CLOCK`]: aether_bloomery_kinds::CLOCK

use aether_bloomery_kinds::{Fired, Mode, Until};

use crate::{At, Program};

/// `clock.until`: the driver-native timer. Its run finishes once journal time
/// reaches the input's `due_millis`, recording a [`Fired`] result.
///
/// It is declared by hand, with no `run`: the driver never invokes it, so
/// this impl only types its runs, `Ran<ClockUntil>`, for the rules that fire
/// on them.
pub struct ClockUntil;

impl Program for ClockUntil {
    const NAME: &'static str = "clock.until";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Wait until journal time reaches a due time.";
    const DOC: &'static str = "Finishes once journal time reaches `due_millis`, recording the due time it fired for. \
                               A due time more than seven days after the request is refused.";
    type Input = Until;
    type Result = Fired;
}

/// Wait `millis` after the entry at `at` was recorded.
///
/// The due time comes from recorded time alone, so a rule that replays the
/// same entry computes the same due time.
#[must_use]
pub const fn wait(at: At, millis: u64) -> Until {
    Until { due_millis: at.recorded_at_millis.saturating_add(millis) }
}

/// Wait `millis` after the entry at `at` was recorded, plus a deterministic
/// offset in `0..spread` drawn from `at.seq`.
///
/// Rules triggered by neighbouring entries get different offsets, so a burst
/// of waits set together does not fire together. A `spread` of `0` adds
/// nothing.
#[must_use]
pub const fn wait_spread(at: At, millis: u64, spread: u64) -> Until {
    let offset = if spread == 0 {
        0
    } else {
        splitmix64(at.seq.0) % spread
    };
    Until { due_millis: at.recorded_at_millis.saturating_add(millis).saturating_add(offset) }
}

/// One splitmix64 step over `value`: a cheap, well-mixed hash of a seq.
const fn splitmix64(value: u64) -> u64 {
    let mut mixed = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use aether_bloomery_kinds::Seq;

    use super::{At, wait_spread};

    fn at(seq: u64) -> At {
        At { seq: Seq(seq), cause: None, recorded_at_millis: 1_000 }
    }

    #[test]
    fn spread_offsets_stay_in_bounds_and_differ_across_neighbouring_seqs() {
        // Catches an offset that escapes `0..spread`, one that collapses to a
        // constant so a burst still fires together, and a zero spread that
        // divides by zero or adds anything.
        let spread = 250;
        let offsets: Vec<u64> = (1..=16).map(|seq| wait_spread(at(seq), 5_000, spread).due_millis - 6_000).collect();
        assert!(offsets.iter().all(|offset| *offset < spread), "{offsets:?}");
        assert!(offsets.windows(2).any(|pair| pair[0] != pair[1]), "neighbouring seqs spread: {offsets:?}");

        assert_eq!(wait_spread(at(7), 5_000, 0).due_millis, 6_000);
    }
}
