//! The focus-and-input routing a panel root embeds (issue 2660).
//!
//! [`Focus`] is the plain state the root-owned focus model carries: an
//! ordered set of child entries (each a hit rect plus whether it can take
//! keyboard focus), the currently focused child, and the drag-captured child.
//! It does no mail and holds no capability handle — the root drives it,
//! mirroring how [`Composite`](crate::runtime::composite::Composite) is the
//! bookkeeping half of the draw protocol while the actor owns the sends.
//!
//! Widgets never subscribe to input themselves. The root subscribes the
//! pointer and keyboard streams once, then asks [`Focus`] where each event
//! goes: a pointer event to the drag-captured child if one holds capture, else
//! to the topmost child under the cursor; a keyboard event to the focused
//! child. Focus moves on a focusable hit or on Tab, and each move yields the
//! `(previous, next)` pair the root turns into a `FocusLost` to the old holder
//! and a `FocusGained` to the new one.
//!
//! Rects are window-pixel 2D, carried as a zero-depth [`Aabb`] (`z = 0`) so
//! the hit test reuses `aether_math::Aabb::contains_point` rather than a
//! hand-rolled bounds check.

use alloc::vec::Vec;

use aether_data::MailboxId;
use aether_math::{Aabb, Vec3};

/// The `(previous, next)` focus holders across a focus move: `previous` is the
/// child that just lost focus (`None` if nothing was focused), `next` the one
/// that just gained it (`None` if focus cleared). The root sends `FocusLost`
/// to `previous` and `FocusGained` to `next`.
pub type FocusTransition = (Option<MailboxId>, Option<MailboxId>);

/// One child's hit rect and focus eligibility. `rect` is the child's
/// window-pixel bounds (zero-depth), the same rect the root assigned it as a
/// `WidgetFrame`; `focusable` is `false` for a widget that takes no keyboard
/// input (a label), which the Tab cycle and focus-on-hit both skip.
struct Entry {
    child: MailboxId,
    rect: Aabb,
    focusable: bool,
}

/// The root's focus-and-input routing table: the child entries in registration
/// (layout / Tab) order, plus the focused and drag-captured children. Rebuilt
/// each spawn pass with [`Self::clear`] and [`Self::register`]; queried per
/// input event.
#[derive(Default)]
pub struct Focus {
    entries: Vec<Entry>,
    focused: Option<MailboxId>,
    capture: Option<MailboxId>,
}

impl Focus {
    /// An empty routing table — no children, nothing focused or captured.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every entry and clear the focused / captured children — the reset
    /// a root runs before it re-registers its layout (e.g. a respawn).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.focused = None;
        self.capture = None;
    }

    /// Register a child's hit rect in layout order. `(x, y)` is the top-left
    /// and `(width, height)` the size, in window pixels; `focusable` marks
    /// whether Tab and focus-on-hit consider it. Registration order is Tab
    /// order; draw / registration order is also the hit-test stack (a later
    /// child sits on top of an earlier one where they overlap).
    pub fn register(
        &mut self,
        child: MailboxId,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        focusable: bool,
    ) {
        let rect = Aabb::from_min_max(Vec3::new(x, y, 0.0), Vec3::new(x + width, y + height, 0.0));
        self.entries.push(Entry {
            child,
            rect,
            focusable,
        });
    }

    /// The topmost child whose rect contains `(x, y)`, or `None` if the point
    /// is over no child. Topmost is the last-registered match, since a later
    /// child draws over an earlier one.
    #[must_use]
    pub fn hit_test(&self, x: f32, y: f32) -> Option<MailboxId> {
        let point = Vec3::new(x, y, 0.0);
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.rect.contains_point(point))
            .map(|entry| entry.child)
    }

    /// Where a pointer event routes: the drag-captured child if one holds
    /// capture (so a drag that leaves the widget's rect still reaches it),
    /// else the topmost child under the cursor.
    #[must_use]
    pub fn pointer_target(&self, x: f32, y: f32) -> Option<MailboxId> {
        self.capture.or_else(|| self.hit_test(x, y))
    }

    /// Where a keyboard event routes: the focused child, or `None` if nothing
    /// is focused.
    #[must_use]
    pub fn keyboard_target(&self) -> Option<MailboxId> {
        self.focused
    }

    /// Begin drag capture on `child` — set by the root on a left press that
    /// hits a widget, so subsequent moves route to it until release.
    pub fn begin_capture(&mut self, child: MailboxId) {
        self.capture = Some(child);
    }

    /// Clear drag capture — the root's response to the matching release.
    pub fn clear_capture(&mut self) {
        self.capture = None;
    }

    /// The drag-captured child, if any.
    #[must_use]
    pub fn captured(&self) -> Option<MailboxId> {
        self.capture
    }

    /// Set focus to `next`, returning the `(previous, next)` transition when
    /// it actually changed, or `None` when `next` already held focus (so the
    /// root sends no redundant `FocusLost` / `FocusGained`).
    pub fn set_focus(&mut self, next: Option<MailboxId>) -> Option<FocusTransition> {
        if self.focused == next {
            return None;
        }
        let previous = self.focused;
        self.focused = next;
        Some((previous, next))
    }

    /// Focus the topmost focusable child under `(x, y)`, returning the
    /// transition when focus moved. A press over empty space or over a
    /// non-focusable child (a label) leaves the current focus untouched.
    pub fn focus_hit(&mut self, x: f32, y: f32) -> Option<FocusTransition> {
        let point = Vec3::new(x, y, 0.0);
        let hit = self
            .entries
            .iter()
            .rev()
            .find(|entry| entry.focusable && entry.rect.contains_point(point))
            .map(|entry| entry.child)?;
        self.set_focus(Some(hit))
    }

    /// Advance focus to the next focusable child in registration order,
    /// wrapping — the Tab cycle. From no focus (or a focused child no longer
    /// registered) it lands on the first focusable child; returns the
    /// transition, or `None` when there is no focusable child to move to.
    pub fn advance_focus(&mut self) -> Option<FocusTransition> {
        let count = self.entries.len();
        if count == 0 {
            return None;
        }
        // Start scanning just past the current holder (or before the first
        // entry when nothing is focused), wrapping once around the ring.
        let start = self
            .focused
            .and_then(|current| self.entries.iter().position(|entry| entry.child == current))
            .map_or(0, |index| index + 1);
        for offset in 0..count {
            let entry = &self.entries[(start + offset) % count];
            if entry.focusable {
                return self.set_focus(Some(entry.child));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn focus_with_three() -> Focus {
        let mut focus = Focus::new();
        // Two focusable widgets and one non-focusable label, in Tab order.
        focus.register(MailboxId(1), 0.0, 0.0, 10.0, 10.0, true);
        focus.register(MailboxId(2), 0.0, 20.0, 10.0, 10.0, false); // a label
        focus.register(MailboxId(3), 0.0, 40.0, 10.0, 10.0, true);
        focus
    }

    #[test]
    fn hit_test_picks_topmost_on_overlap() {
        let mut focus = Focus::new();
        // Two overlapping rects; the later registration draws on top, so it
        // wins the hit where they overlap.
        focus.register(MailboxId(1), 0.0, 0.0, 20.0, 20.0, true);
        focus.register(MailboxId(2), 10.0, 10.0, 20.0, 20.0, true);
        assert_eq!(
            focus.hit_test(15.0, 15.0),
            Some(MailboxId(2)),
            "the later-registered (topmost) rect wins the overlap",
        );
        assert_eq!(
            focus.hit_test(2.0, 2.0),
            Some(MailboxId(1)),
            "outside the top rect, the lower one is hit",
        );
        assert_eq!(focus.hit_test(50.0, 50.0), None, "empty space hits nothing");
    }

    #[test]
    fn tab_wraps_in_registration_order_skipping_non_focusable() {
        let mut focus = focus_with_three();
        // From nothing, Tab lands on the first focusable.
        assert_eq!(focus.advance_focus(), Some((None, Some(MailboxId(1)))));
        // Next Tab skips the non-focusable label (id 2) and lands on id 3.
        assert_eq!(
            focus.advance_focus(),
            Some((Some(MailboxId(1)), Some(MailboxId(3)))),
        );
        // Past the last focusable it wraps back to the first.
        assert_eq!(
            focus.advance_focus(),
            Some((Some(MailboxId(3)), Some(MailboxId(1)))),
        );
    }

    #[test]
    fn capture_overrides_hit_for_pointer_routing() {
        let mut focus = focus_with_three();
        // With no capture, a pointer routes by hit test — a point over no
        // child routes nowhere.
        assert_eq!(focus.pointer_target(100.0, 100.0), None);
        focus.begin_capture(MailboxId(1));
        // Captured: every pointer routes to the captor regardless of position.
        assert_eq!(focus.pointer_target(100.0, 100.0), Some(MailboxId(1)));
        assert_eq!(focus.pointer_target(5.0, 45.0), Some(MailboxId(1)));
        focus.clear_capture();
        // Uncaptured: the point (5, 45) falls in id 3's rect (y 40..50).
        assert_eq!(focus.pointer_target(5.0, 45.0), Some(MailboxId(3)));
    }

    #[test]
    fn focus_hit_ignores_non_focusable_and_reports_the_pair() {
        let mut focus = focus_with_three();
        // A hit on the focusable id 1 focuses it.
        assert_eq!(focus.focus_hit(5.0, 5.0), Some((None, Some(MailboxId(1)))));
        // A hit on the label (id 2) does not steal focus — no transition.
        assert_eq!(focus.focus_hit(5.0, 25.0), None);
        assert_eq!(focus.keyboard_target(), Some(MailboxId(1)));
        // A hit on the other focusable moves focus, reporting prev + next.
        assert_eq!(
            focus.focus_hit(5.0, 45.0),
            Some((Some(MailboxId(1)), Some(MailboxId(3)))),
        );
    }

    #[test]
    fn set_focus_to_current_holder_is_a_no_op() {
        let mut focus = focus_with_three();
        focus.set_focus(Some(MailboxId(1)));
        assert_eq!(
            focus.set_focus(Some(MailboxId(1))),
            None,
            "re-focusing the current holder yields no transition",
        );
    }
}
