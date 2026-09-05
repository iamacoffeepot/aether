//! The compositing bookkeeping a widget node embeds (ADR-0117).
//!
//! A [`Composite`] is the plain state a compositing widget carries: an
//! ordered set of child slots plus an own-chrome buffer. It does no mail
//! and holds no capability handle — the actor drives it. Its job is the
//! three pieces of protocol logic worth isolating: keyed attribution of a
//! child's reply into the right slot, the filled-slot completion counter,
//! and the depth-first flatten that offsets each child's draws by the rect
//! its slot was assigned.
//!
//! Draw order emerges by construction. `flatten` lays down the node's own
//! chrome first, then each slot in registration order (which the node set
//! from its own layout, never from mail-arrival order), and an interior
//! node's flattened list becomes its parent's slot payload — so a subtree
//! carries its own internal order wherever it is placed.
//!
//! There are two lanes, not two layers. Everything above happens once in the
//! ordinary lane and once in the overlay: a slot the node marked overlay
//! ([`Composite::set_slot_overlay`]) flattens its draws into the overlay
//! beside the node's own overlay chrome ([`Composite::extend_overlay`]), in
//! the same chrome-then-slots order. That is how a plate and the children it
//! hosts stay one group — which is what the root's clip subtraction reads to
//! decide whose text a fill may cut.

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::MailboxId;
use aether_math::Vec2;

use crate::{ChildrenChanged, MembershipEntry, WidgetClipRect, WidgetDrawItem, WidgetDrawList};

/// One child's place in a compositing node's layout. `child` is the
/// inline-child alias the reply is attributed to; `subname` is the child's
/// inline address segment (recorded so a despawn can name what it removed
/// without the caller re-supplying it); `origin` is the offset applied to
/// every draw the child reports; `list` is that child's draws for the current
/// frame, `None` until it replies. `clip` is the optional parent-local bound
/// enforced over every draw returned by the child subtree. `overlay` puts the
/// child's ordinary draws in the overlay lane instead of the ordinary one —
/// see [`Composite::set_slot_overlay`].
struct Slot {
    child: MailboxId,
    subname: String,
    origin: Vec2,
    clip: Option<WidgetClipRect>,
    overlay: bool,
    list: Option<WidgetDrawList>,
}

/// One membership change buffered at a slot chokepoint, folded into a
/// [`ChildrenChanged`] event when the owning actor drains it. An add carries
/// the full identity (subname + the spawned actor's type namespace); a remove
/// names just the subname, which the dropped [`Slot`] already stored.
enum MembershipDelta {
    Added { subname: String, type_namespace: String },
    Removed { subname: String },
}

/// A compositing node's per-frame accumulator: registered child slots plus
/// the node's own chrome. Reset each frame with [`Self::begin_frame`],
/// filled as children reply, flattened once [`Self::is_complete`]. It also
/// buffers membership deltas at the slot chokepoints ([`Self::register_slot`]
/// / [`Self::forget_slot`]) for the owning actor to drain and emit up the
/// lane — orthogonal to the per-frame fill cycle.
#[derive(Default)]
pub struct Composite {
    slots: Vec<Slot>,
    chrome: Vec<WidgetDrawItem>,
    overlay_chrome: Vec<WidgetDrawItem>,
    pending_membership: Vec<MembershipDelta>,
}

impl Composite {
    /// An empty composite — no slots, no chrome.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a child slot at `origin` and optional parent-local `clip`,
    /// once, when the child is spawned, naming it `subname` and its actor type
    /// `type_namespace` (the spawned actor's `NAMESPACE`). The slot persists
    /// across frames (the child's alias and assigned layout are stable); only
    /// its per-frame `list` resets. A duplicate registration of the same
    /// `child` is ignored so a
    /// re-`wire` cannot inflate the completion count — and records no
    /// membership delta, so a re-register does not re-announce the child.
    pub fn register_slot(
        &mut self,
        child: MailboxId,
        origin: Vec2,
        clip: Option<WidgetClipRect>,
        subname: &str,
        type_namespace: &str,
    ) {
        if self.slots.iter().any(|slot| slot.child == child) {
            return;
        }
        self.slots.push(Slot { child, subname: String::from(subname), origin, clip, overlay: false, list: None });
        self.pending_membership.push(MembershipDelta::Added {
            subname: String::from(subname),
            type_namespace: String::from(type_namespace),
        });
    }

    /// Update an existing slot's parent-local origin and clip without changing
    /// membership or its current-frame reply. Stateful containers use this to
    /// move one retained content root as their offset changes; re-registering
    /// would either be ignored as a duplicate or manufacture false membership
    /// churn.
    pub fn update_slot_layout(&mut self, child: MailboxId, origin: Vec2, clip: Option<WidgetClipRect>) -> bool {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.child == child) else {
            return false;
        };
        slot.origin = origin;
        slot.clip = clip;
        true
    }

    /// Put a registered slot's ordinary draws in the **overlay lane** rather
    /// than the ordinary one, without touching its membership, layout, or
    /// current-frame reply. Returns whether a slot was found.
    ///
    /// This is the lane a plate that *hosts* the root's own children needs
    /// (the studio's gap 15). A popover's plate stands over the primary
    /// content, so it goes in the overlay — but a popover's controls are
    /// ordinary widgets of the root, and ordinary draws are what an overlay
    /// fill cuts text out from under. Marking the popover's children overlay
    /// too moves the whole group into one lane: the plate goes down first
    /// ([`Self::extend_overlay`]), the group's children follow in slot order,
    /// the fill still cuts the primary content's glyphs under it, and it
    /// cannot cut its own children's — the subtraction is positional, and
    /// their labels are authored after the plate that hosts them.
    ///
    /// There is no layer number and no z-index here: the group is a set of
    /// slots the root already ordered, and the lane is the two-step order the
    /// root already emits in.
    pub fn set_slot_overlay(&mut self, child: MailboxId, overlay: bool) -> bool {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.child == child) else {
            return false;
        };
        slot.overlay = overlay;
        true
    }

    /// Drop the slot for `child` — the despawn counterpart of
    /// [`Self::register_slot`], so a torn-down child stops being counted
    /// toward completion. Records a membership delta naming the dropped slot's
    /// `subname` (self-derived, so the caller need only key by the stable
    /// `child` alias). Returns whether a slot was removed.
    pub fn forget_slot(&mut self, child: MailboxId) -> bool {
        let Some(index) = self.slots.iter().position(|slot| slot.child == child) else {
            return false;
        };
        let removed = self.slots.remove(index);
        self.pending_membership.push(MembershipDelta::Removed { subname: removed.subname });
        true
    }

    /// Drain the buffered membership deltas into one [`ChildrenChanged`]
    /// (`added` folds every buffered add, `removed` every buffered remove, in
    /// buffer order), clearing the buffer. `None` when nothing changed since
    /// the last drain — so a quiet frame emits no event, and the first-spawn
    /// burst of N adds drains as one batched event.
    pub fn take_membership_changes(&mut self) -> Option<ChildrenChanged> {
        if self.pending_membership.is_empty() {
            return None;
        }
        let mut added = Vec::new();
        let mut removed = Vec::new();
        for delta in self.pending_membership.drain(..) {
            match delta {
                MembershipDelta::Added { subname, type_namespace } => {
                    added.push(MembershipEntry { subname, type_namespace });
                }
                MembershipDelta::Removed { subname } => removed.push(subname),
            }
        }
        Some(ChildrenChanged { added, removed })
    }

    /// Begin a frame: clear both chrome buffers and reset every slot to
    /// unfilled, so the completion counter re-counts this frame's replies.
    /// The slot set (children + origins + lanes) is untouched.
    pub fn begin_frame(&mut self) {
        self.chrome.clear();
        self.overlay_chrome.clear();
        for slot in &mut self.slots {
            slot.list = None;
        }
    }

    /// Append one of the node's own draws to its chrome (local
    /// coordinates). Chrome flattens before any child, so it draws under
    /// the children — the fills-under-labels layering.
    pub fn extend_chrome(&mut self, items: impl IntoIterator<Item = WidgetDrawItem>) {
        self.chrome.extend(items);
    }

    /// Append one of the node's own draws to its **overlay** chrome (local
    /// coordinates): the same fills-under-children rule, one lane up. The
    /// node's overlay chrome flattens before any slot's overlay, so a plate
    /// laid down here stands under the group of children raised onto it with
    /// [`Self::set_slot_overlay`] and over everything in the ordinary lane.
    pub fn extend_overlay(&mut self, items: impl IntoIterator<Item = WidgetDrawItem>) {
        self.overlay_chrome.extend(items);
    }

    /// File a child's reply into its slot, attributed by the child's
    /// alias. A reply from a `child` with no registered slot is dropped
    /// (it cannot belong to this node's layout); a second reply from the
    /// same child overwrites. Returns whether the reply landed in a slot.
    pub fn fill(&mut self, child: MailboxId, list: WidgetDrawList) -> bool {
        if let Some(slot) = self.slots.iter_mut().find(|slot| slot.child == child) {
            slot.list = Some(list);
            true
        } else {
            false
        }
    }

    /// Whether every registered slot has replied this frame. Vacuously
    /// true for a leaf (no slots), which is the completion signal that
    /// makes a childless node finish immediately after its own chrome.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.slots.iter().all(|slot| slot.list.is_some())
    }

    /// Flatten own chrome then every slot's draws — each offset by the slot's
    /// origin and intersected with its parent-local clip — into one
    /// [`WidgetDrawList`] in depth-first order, tagged with `intrinsic`. Own
    /// chrome stays in this node's local space for its eventual parent to
    /// constrain. A slot still `None` (called before completion) contributes
    /// nothing; call once [`Self::is_complete`].
    ///
    /// A child's `overlay` is offset by its slot origin like any draw but never
    /// intersected with the slot clip, and lands in the flattened list's own
    /// `overlay` — so an open dropdown's list escapes its row and the root
    /// emits it after every ordinary item of the whole cluster.
    ///
    /// A slot marked with [`Self::set_slot_overlay`] puts its ordinary `items`
    /// in that same `overlay`, still offset by its origin and still cut to its
    /// slot clip: the clip is where the root framed the child, which the lane
    /// does not change. The lane order is the node's overlay chrome first,
    /// then each overlay slot in registration order, so a plate and the
    /// children standing on it arrive in the order the root laid them out.
    #[must_use]
    pub fn flatten(&self, intrinsic: Option<[f32; 2]>) -> WidgetDrawList {
        let mut items = self.chrome.clone();
        let mut overlay = self.overlay_chrome.clone();
        for slot in &self.slots {
            let Some(list) = &slot.list else {
                continue;
            };
            let placed = list.items.iter().filter_map(|item| item.offset(slot.origin).intersect_clip(slot.clip));
            if slot.overlay {
                overlay.extend(placed);
            } else {
                items.extend(placed);
            }
            overlay.extend(list.overlay.iter().map(|item| item.offset(slot.origin)));
        }
        WidgetDrawList { content_height: None, intrinsic, items, overlay }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_math::Rgba;

    fn quad(x: f32, tag: f32) -> WidgetDrawItem {
        WidgetDrawItem::Quad { x, y: 0.0, width: 1.0, height: 1.0, color: Rgba::new(tag, 0.0, 0.0, 1.0), clip: None }
    }

    fn clipped_quad(x: f32, tag: f32, clip: WidgetClipRect) -> WidgetDrawItem {
        let mut item = quad(x, tag);
        let WidgetDrawItem::Quad { clip: own, .. } = &mut item else {
            unreachable!("quad helper always returns a quad")
        };
        *own = Some(clip);
        item
    }

    fn textured(x: f32, y: f32, clip: WidgetClipRect) -> WidgetDrawItem {
        WidgetDrawItem::TexturedQuad {
            texture_id: 23,
            x,
            y,
            width: 12.0,
            height: 9.0,
            u0: 0.125,
            v0: 0.25,
            u1: 0.75,
            v1: 0.875,
            tint: Rgba::new(0.4, 0.6, 0.8, 0.9),
            clip: Some(clip),
        }
    }

    fn list(items: Vec<WidgetDrawItem>) -> WidgetDrawList {
        WidgetDrawList { content_height: None, intrinsic: None, items, overlay: Vec::new() }
    }

    #[test]
    fn a_leaf_with_no_slots_is_immediately_complete() {
        let composite = Composite::new();
        assert!(composite.is_complete(), "a node with no registered slots completes vacuously — the childless case");
    }

    #[test]
    fn completion_gates_on_every_registered_slot() {
        let mut composite = Composite::new();
        let a = MailboxId(1);
        let b = MailboxId(2);
        composite.register_slot(a, Vec2::ZERO, None, "a", "aether.kit.widget");
        composite.register_slot(b, Vec2::ZERO, None, "b", "aether.kit.widget");
        composite.begin_frame();
        assert!(!composite.is_complete(), "two slots, none filled");
        assert!(composite.fill(a, list(vec![quad(0.0, 0.1)])));
        assert!(!composite.is_complete(), "one of two slots filled");
        assert!(composite.fill(b, list(vec![quad(0.0, 0.2)])));
        assert!(composite.is_complete(), "both slots filled closes the counter");
    }

    #[test]
    fn a_reply_from_an_unregistered_child_is_dropped() {
        let mut composite = Composite::new();
        composite.register_slot(MailboxId(1), Vec2::ZERO, None, "a", "aether.kit.widget");
        composite.begin_frame();
        assert!(
            !composite.fill(MailboxId(99), list(vec![quad(0.0, 0.5)])),
            "a stray reply from a non-slot child does not land",
        );
        assert!(!composite.is_complete(), "and does not close the real slot's counter");
    }

    #[test]
    fn begin_frame_resets_fills_but_keeps_slots() {
        let mut composite = Composite::new();
        let a = MailboxId(1);
        composite.register_slot(a, Vec2::ZERO, None, "a", "aether.kit.widget");
        composite.begin_frame();
        composite.fill(a, list(vec![quad(0.0, 0.1)]));
        assert!(composite.is_complete());
        composite.begin_frame();
        assert!(!composite.is_complete(), "a new frame re-opens the slot so its reply must arrive again");
    }

    #[test]
    fn updating_slot_layout_moves_and_clips_without_membership_churn() {
        let mut composite = Composite::new();
        let child = MailboxId(1);
        composite.register_slot(child, Vec2::ZERO, None, "content", "aether.kit.widget");
        let initial_membership = composite.take_membership_changes().expect("registration emits membership");
        assert_eq!(initial_membership.added.len(), 1);

        let clip = WidgetClipRect { x: 0.0, y: 0.0, width: 8.0, height: 6.0 };
        assert!(composite.update_slot_layout(child, Vec2::new(-3.0, -2.0), Some(clip)));
        assert!(composite.take_membership_changes().is_none(), "layout motion is not a membership change");
        composite.begin_frame();
        assert!(composite.fill(child, list(vec![quad(4.0, 0.5)])));
        assert_eq!(
            composite.flatten(None).items,
            vec![WidgetDrawItem::Quad {
                x: 1.0,
                y: -2.0,
                width: 1.0,
                height: 1.0,
                color: Rgba::new(0.5, 0.0, 0.0, 1.0),
                clip: Some(clip),
            }],
        );
    }

    #[test]
    fn forget_slot_removes_a_child_from_the_count() {
        let mut composite = Composite::new();
        let a = MailboxId(1);
        let b = MailboxId(2);
        composite.register_slot(a, Vec2::ZERO, None, "a", "aether.kit.widget");
        composite.register_slot(b, Vec2::ZERO, None, "b", "aether.kit.widget");
        composite.begin_frame();
        composite.fill(a, list(vec![quad(0.0, 0.1)]));
        assert!(!composite.is_complete(), "b still owed");
        assert!(composite.forget_slot(b), "b removed");
        assert!(composite.is_complete(), "with b despawned the counter closes on a alone");
    }

    #[test]
    fn flatten_lays_chrome_first_then_slots_offset_in_order() {
        let mut composite = Composite::new();
        let a = MailboxId(1);
        let b = MailboxId(2);
        composite.register_slot(a, Vec2::new(100.0, 0.0), None, "a", "aether.kit.widget");
        composite.register_slot(b, Vec2::new(200.0, 0.0), None, "b", "aether.kit.widget");
        composite.begin_frame();
        composite.extend_chrome([quad(0.0, 0.9)]); // chrome at local origin
        composite.fill(a, list(vec![quad(1.0, 0.1)]));
        composite.fill(b, list(vec![quad(2.0, 0.2)]));

        let flat = composite.flatten(Some([300.0, 10.0]));
        assert_eq!(flat.intrinsic, Some([300.0, 10.0]));
        // Chrome first (x=0, untranslated), then slot a (x = 1 + 100),
        // then slot b (x = 2 + 200) — depth-first, offset by slot origin.
        let xs: Vec<f32> = flat
            .items
            .iter()
            .map(|item| match item {
                WidgetDrawItem::Quad { x, .. }
                | WidgetDrawItem::TexturedQuad { x, .. }
                | WidgetDrawItem::Text { x, .. } => *x,
            })
            .collect();
        assert_eq!(
            xs,
            vec![0.0, 101.0, 202.0],
            "chrome draws under the children, then slots in registration order, \
             each offset by its assigned origin",
        );
    }

    #[test]
    fn nested_flatten_moves_textured_geometry_and_clip_but_not_payload() {
        let mut interior = Composite::new();
        interior.register_slot(
            MailboxId(11),
            Vec2::new(4.0, 3.0),
            Some(WidgetClipRect { x: 6.0, y: 5.0, width: 10.0, height: 8.0 }),
            "leaf",
            "aether.kit.widget",
        );
        interior.begin_frame();
        assert!(interior.fill(
            MailboxId(11),
            list(vec![textured(2.0, 1.0, WidgetClipRect { x: 3.0, y: 2.0, width: 20.0, height: 20.0 },)]),
        ));

        let mut root = Composite::new();
        root.register_slot(
            MailboxId(22),
            Vec2::new(10.0, 8.0),
            Some(WidgetClipRect { x: 12.0, y: 10.0, width: 20.0, height: 16.0 }),
            "interior",
            "aether.kit.widget",
        );
        root.begin_frame();
        assert!(root.fill(MailboxId(22), interior.flatten(None)));

        assert_eq!(
            root.flatten(None).items,
            vec![WidgetDrawItem::TexturedQuad {
                texture_id: 23,
                x: 16.0,
                y: 12.0,
                width: 12.0,
                height: 9.0,
                u0: 0.125,
                v0: 0.25,
                u1: 0.75,
                v1: 0.875,
                tint: Rgba::new(0.4, 0.6, 0.8, 0.9),
                clip: Some(WidgetClipRect { x: 17.0, y: 13.0, width: 9.0, height: 8.0 }),
            }],
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one cohesive nested clip/order invariant
    fn nested_flatten_translates_intersects_and_omits_without_reordering() {
        let mut interior = Composite::new();
        interior.register_slot(
            MailboxId(11),
            Vec2::new(4.0, 3.0),
            Some(WidgetClipRect { x: 6.0, y: 5.0, width: 10.0, height: 8.0 }),
            "leaf",
            "aether.kit.widget",
        );
        interior.begin_frame();
        interior.extend_chrome([clipped_quad(0.0, 0.2, WidgetClipRect { x: 0.0, y: 0.0, width: 30.0, height: 20.0 })]);
        assert!(interior.fill(
            MailboxId(11),
            list(vec![
                clipped_quad(0.0, 0.3, WidgetClipRect { x: 2.0, y: 1.0, width: 20.0, height: 20.0 },),
                clipped_quad(1.0, 0.4, WidgetClipRect { x: 30.0, y: 30.0, width: 4.0, height: 4.0 },),
                clipped_quad(2.0, 0.5, WidgetClipRect { x: 3.0, y: 2.0, width: 12.0, height: 12.0 },),
            ]),
        ));
        let interior_list = interior.flatten(None);
        assert_eq!(interior_list.items.len(), 3, "the disjoint middle item is omitted");

        let mut root = Composite::new();
        root.register_slot(
            MailboxId(22),
            Vec2::new(10.0, 8.0),
            Some(WidgetClipRect { x: 12.0, y: 10.0, width: 20.0, height: 16.0 }),
            "interior",
            "aether.kit.widget",
        );
        root.begin_frame();
        root.extend_chrome([quad(0.0, 0.1)]);
        assert!(root.fill(MailboxId(22), interior_list));

        let flat = root.flatten(None);
        let mut xs = Vec::new();
        let mut tags = Vec::new();
        let mut clips = Vec::new();
        for item in &flat.items {
            let (x, tag, clip) = match item {
                WidgetDrawItem::Quad { x, color, clip, .. } => (*x, color.r, *clip),
                WidgetDrawItem::TexturedQuad { .. } | WidgetDrawItem::Text { .. } => {
                    unreachable!("test builds only solid quads")
                }
            };
            xs.push(x);
            tags.push(tag);
            clips.push(clip);
        }
        assert_eq!(xs, vec![0.0, 10.0, 14.0, 16.0]);
        assert_eq!(tags, vec![0.1, 0.2, 0.3, 0.5]);
        assert_eq!(
            clips,
            vec![
                None,
                Some(WidgetClipRect { x: 12.0, y: 10.0, width: 20.0, height: 16.0 }),
                Some(WidgetClipRect { x: 16.0, y: 13.0, width: 10.0, height: 8.0 }),
                Some(WidgetClipRect { x: 17.0, y: 13.0, width: 9.0, height: 8.0 }),
            ],
        );
    }

    #[test]
    fn an_overlay_group_flattens_plate_then_children_into_the_overlay_lane() {
        // Tripwire: a popover's plate stands over the primary content, and its
        // controls are ordinary widgets of the root. Leaving those controls in
        // the ordinary lane puts them under the plate *and* under the clip
        // subtraction, which deletes their labels; the group has to arrive in
        // one lane, plate first, children after, each still cut to the slot the
        // root framed it in.
        let mut root = Composite::new();
        let plate = MailboxId(1);
        let outside = MailboxId(2);
        root.register_slot(
            plate,
            Vec2::new(100.0, 0.0),
            Some(WidgetClipRect { x: 0.0, y: 0.0, width: 40.0, height: 10.0 }),
            "on_plate",
            "aether.kit.widget",
        );
        root.register_slot(outside, Vec2::new(200.0, 0.0), None, "under_plate", "aether.kit.widget");
        assert!(root.set_slot_overlay(plate, true));
        assert!(!root.set_slot_overlay(MailboxId(99), true), "an unregistered child has no lane to set");

        root.begin_frame();
        root.extend_chrome([quad(0.0, 0.1)]);
        root.extend_overlay([quad(1.0, 0.2)]);
        assert!(root.fill(
            plate,
            list(vec![
                quad(2.0, 0.3),
                clipped_quad(50.0, 0.4, WidgetClipRect { x: 50.0, y: 0.0, width: 4.0, height: 4.0 })
            ])
        ));
        assert!(root.fill(outside, list(vec![quad(3.0, 0.5)])));

        let flat = root.flatten(None);
        let tags = |items: &[WidgetDrawItem]| {
            items
                .iter()
                .map(|item| match item {
                    WidgetDrawItem::Quad { color, .. } => color.r,
                    WidgetDrawItem::TexturedQuad { .. } | WidgetDrawItem::Text { .. } => {
                        unreachable!("test builds only solid quads")
                    }
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(tags(&flat.items), vec![0.1, 0.5], "only the ordinary chrome and the ordinary slot");
        assert_eq!(
            tags(&flat.overlay),
            vec![0.2, 0.3],
            "the node's own overlay chrome first, then the overlay slot — and the child draw \
             that fell outside its slot clip is dropped there exactly as it would be here",
        );
    }

    #[test]
    fn registers_buffer_one_batched_add_per_child_in_order() {
        let mut composite = Composite::new();
        composite.register_slot(MailboxId(1), Vec2::ZERO, None, "alpha", "aether.kit.widget.slider");
        composite.register_slot(MailboxId(2), Vec2::ZERO, None, "beta", "aether.kit.widget.button");
        let changed = composite.take_membership_changes().expect("two adds are buffered and drain together");
        assert!(changed.removed.is_empty(), "no removals in an add-only batch");
        assert_eq!(
            changed.added,
            vec![
                MembershipEntry { subname: "alpha".into(), type_namespace: "aether.kit.widget.slider".into() },
                MembershipEntry { subname: "beta".into(), type_namespace: "aether.kit.widget.button".into() },
            ],
            "both adds drain as one batch, in registration order, carrying subname + type",
        );
    }

    #[test]
    fn forget_buffers_one_remove_naming_the_dropped_subname() {
        let mut composite = Composite::new();
        composite.register_slot(MailboxId(1), Vec2::ZERO, None, "alpha", "aether.kit.widget");
        composite.take_membership_changes().expect("drain the add so the remove stands alone");
        assert!(composite.forget_slot(MailboxId(1)), "the slot is removed");
        let changed = composite.take_membership_changes().expect("the remove is buffered");
        assert!(changed.added.is_empty(), "no adds in a remove-only batch");
        assert_eq!(
            changed.removed,
            vec![String::from("alpha")],
            "the remove names the dropped slot's subname, self-derived from the slot",
        );
    }

    #[test]
    fn take_membership_changes_clears_the_buffer_and_is_none_when_quiet() {
        let mut composite = Composite::new();
        assert!(composite.take_membership_changes().is_none(), "nothing has changed yet, so there is no event");
        composite.register_slot(MailboxId(1), Vec2::ZERO, None, "alpha", "aether.kit.widget");
        assert!(composite.take_membership_changes().is_some(), "the buffered add drains as an event");
        assert!(
            composite.take_membership_changes().is_none(),
            "the drain cleared the buffer, so a second drain finds nothing",
        );
    }

    #[test]
    fn a_dedup_suppressed_reregister_records_no_delta() {
        let mut composite = Composite::new();
        composite.register_slot(MailboxId(1), Vec2::ZERO, None, "alpha", "aether.kit.widget");
        composite.take_membership_changes().expect("drain the first, genuine add");
        composite.register_slot(MailboxId(1), Vec2::ZERO, None, "alpha", "aether.kit.widget");
        assert!(
            composite.take_membership_changes().is_none(),
            "a re-register of an existing child is dedup-suppressed and announces nothing",
        );
    }
}
