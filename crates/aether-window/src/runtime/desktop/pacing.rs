//! Frame pacing for the desktop window loop: which visible windows are due a
//! frame at a given instant, and when the loop next has to wake for one.
//!
//! A [`WindowPresentation::Display`] or [`WindowPresentation::Uncapped`]
//! window is due on every turn: the first is held to the display by its
//! blocking present, the second by nothing. A
//! [`WindowPresentation::Capped`] window is due once the instant kept for it
//! here has passed. Nothing in this module reads a clock or sleeps: every
//! question takes `now`, and the loop waits with the event loop's own
//! `WaitUntil`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use aether_data::ErasedActorPath;

use crate::WindowPresentation;

/// One reading of the visible windows against an instant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct FrameSchedule {
    /// The visible windows whose next frame is due, which the loop asks to
    /// redraw and a frame may draw.
    pub due: BTreeSet<ErasedActorPath>,
    /// The earliest instant a capped window that is not yet due comes due.
    /// `None` when every visible window is already due or none is capped.
    pub wake: Option<Instant>,
}

/// The instant each capped window's next frame is due.
///
/// A capped window with no instant here has drawn no capped frame yet and is
/// due at once.
#[derive(Default)]
pub(super) struct FramePacing {
    due: BTreeMap<ErasedActorPath, Instant>,
}

impl FramePacing {
    /// Which of the `visible` windows are due at `now`, and the earliest
    /// instant one of the rest comes due.
    pub fn schedule(&self, visible: &[(ErasedActorPath, WindowPresentation)], now: Instant) -> FrameSchedule {
        let mut schedule = FrameSchedule::default();
        for (path, presentation) in visible {
            let pending = match presentation {
                WindowPresentation::Display | WindowPresentation::Uncapped => None,
                WindowPresentation::Capped { .. } => self.due.get(path).copied().filter(|due| *due > now),
            };
            match pending {
                None => {
                    schedule.due.insert(path.clone());
                }
                Some(due) => schedule.wake = Some(schedule.wake.map_or(due, |wake| wake.min(due))),
            }
        }
        schedule
    }

    /// Record a frame that began at `now` and drew `drawn`: each capped
    /// window among them gets its next due instant, and a window in neither
    /// `live` nor capped any more is forgotten.
    pub fn frame_drawn(
        &mut self,
        live: &[(ErasedActorPath, WindowPresentation)],
        drawn: &[ErasedActorPath],
        now: Instant,
    ) {
        let mut capped = BTreeMap::new();
        for (path, presentation) in live {
            if let WindowPresentation::Capped { frames_per_second } = presentation {
                capped.insert(path, frames_per_second.period());
            }
        }

        self.due.retain(|path, _| capped.contains_key(path));
        for path in drawn {
            if let Some(period) = capped.get(path) {
                let previous = self.due.get(path).copied().unwrap_or(now);
                self.due.insert(path.clone(), next_due(previous, *period, now));
            }
        }
    }
}

/// The instant a capped window's next frame is due, after the frame that was
/// due at `previous` began at `now`.
///
/// It is one `period` after `previous`, which keeps the cadence when a frame
/// begins a little late. When that instant has itself already passed, the
/// loop stalled for more than a period, and the next frame is due one
/// `period` after `now`: the frames the stall swallowed are not drawn in a
/// burst afterwards.
fn next_due(previous: Instant, period: Duration, now: Instant) -> Instant {
    let scheduled = previous + period;
    if scheduled > now {
        scheduled
    } else {
        now + period
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FrameRate;

    fn window(name: &str) -> ErasedActorPath {
        crate::window_path(&aether_data::LoadName::new(name).expect("fixture window name"))
    }

    fn capped(frames_per_second: u32) -> WindowPresentation {
        WindowPresentation::Capped {
            frames_per_second: FrameRate::new(frames_per_second).expect("a rate inside the range"),
        }
    }

    const PERIOD: Duration = Duration::from_millis(100);

    /// A capped window that has drawn a frame is not due again until one
    /// period later, and the loop is told that instant. Fails if a capped
    /// window is due on every turn, which is a cap that polls at full speed.
    #[test]
    fn a_capped_window_is_not_due_before_its_instant() {
        let start = Instant::now();
        let visible = [(window("game"), capped(10))];
        let mut pacing = FramePacing::default();
        assert_eq!(pacing.schedule(&visible, start).due, BTreeSet::from([window("game")]), "the first frame is due");

        pacing.frame_drawn(&visible, &[window("game")], start);

        let early = pacing.schedule(&visible, start + Duration::from_millis(99));
        assert!(early.due.is_empty(), "the window is not due inside its period");
        assert_eq!(early.wake, Some(start + PERIOD), "the loop wakes at the due instant");
        let on_time = pacing.schedule(&visible, start + PERIOD);
        assert_eq!(on_time.due, BTreeSet::from([window("game")]));
        assert_eq!(on_time.wake, None);
    }

    /// A frame that begins a little late keeps the cadence, and one that
    /// begins more than a period late is followed by a full period. Fails if
    /// the next instant is back-dated across a stall, which would leave the
    /// window due on every turn until it caught up: a burst of frames.
    #[test]
    fn a_stall_is_followed_by_one_frame_and_a_full_period() {
        let start = Instant::now();
        let visible = [(window("game"), capped(10))];
        let mut pacing = FramePacing::default();
        pacing.frame_drawn(&visible, &[window("game")], start);

        let late = start + PERIOD + Duration::from_millis(5);
        pacing.frame_drawn(&visible, &[window("game")], late);
        assert_eq!(pacing.schedule(&visible, late).wake, Some(start + PERIOD * 2), "a late frame keeps the cadence");

        let stalled = start + PERIOD * 7 + Duration::from_millis(30);
        assert_eq!(pacing.schedule(&visible, stalled).due, BTreeSet::from([window("game")]), "due once, at now");
        pacing.frame_drawn(&visible, &[window("game")], stalled);
        let after = pacing.schedule(&visible, stalled + Duration::from_millis(1));
        assert!(after.due.is_empty(), "the stall's missed frames are not drawn afterwards");
        assert_eq!(after.wake, Some(stalled + PERIOD));
    }

    /// Windows are paced apart: a display-paced or uncapped window is due on
    /// every turn beside a capped one that is waiting, and the earliest
    /// waiting instant is the one reported. Fails if one capped window's
    /// wait holds back a window that is not capped, or if a later instant
    /// hides an earlier one.
    #[test]
    fn each_window_is_due_by_its_own_presentation() {
        let start = Instant::now();
        let visible = [
            (window("fast"), capped(20)),
            (window("free"), WindowPresentation::Uncapped),
            (window("slow"), capped(10)),
            (window("vsync"), WindowPresentation::Display),
        ];
        let mut pacing = FramePacing::default();
        pacing.frame_drawn(&visible, &[window("fast"), window("slow")], start);

        let schedule = pacing.schedule(&visible, start + Duration::from_millis(1));

        assert_eq!(schedule.due, BTreeSet::from([window("free"), window("vsync")]));
        assert_eq!(schedule.wake, Some(start + PERIOD / 2));
    }

    /// A window that stops being capped loses its instant, so capping it
    /// again later starts due. Fails if a stale instant from the earlier cap
    /// survives, which would pace the new cap from a frame drawn before it.
    #[test]
    fn a_window_no_longer_capped_is_forgotten() {
        let start = Instant::now();
        let mut pacing = FramePacing::default();
        pacing.frame_drawn(&[(window("game"), capped(1))], &[window("game")], start);

        pacing.frame_drawn(&[(window("game"), WindowPresentation::Uncapped)], &[window("game")], start);

        let again = pacing.schedule(&[(window("game"), capped(1))], start + Duration::from_millis(1));
        assert_eq!(again.due, BTreeSet::from([window("game")]));
    }
}
