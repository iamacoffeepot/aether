// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload and hands
// it off, so a by-value parameter is the contract, not a copy the body
// could borrow.
#![allow(clippy::needless_pass_by_value)]

//! The widget-compositing actor (ADR-0117).
//!
//! One `#[actor(instanced)]` type realizes every role in a widget tree —
//! root, interior, and leaf — selected by its [`WidgetConfig`]. It is
//! loaded (the root) or spawned as an inline child (everything below), and
//! the same protocol handlers drive it whichever it is:
//!
//! - The **root** (`config.root`) subscribes the frame stage once and, on
//!   each `Tick`, resets its [`Composite`], appends its own chrome, and
//!   sends [`Collect`] to each child in layout order. When its slots close
//!   it emits the whole subtree as contiguous compatible solid/textured
//!   runs plus one `DrawTextBatch` to `aether.render` /
//!   `aether.text`, remaining the cluster's only render sender.
//! - An **interior** node does the same on each inbound [`Collect`], but
//!   instead of emitting it replies its flattened composite up to its
//!   parent — withholding that reply until its own slots close, which is
//!   what carries a nested subtree's internal order up intact.
//! - A **leaf** (no children) answers a [`Collect`] with its own chrome
//!   immediately, because a slotless [`Composite`] is complete on sight.
//!
//! The whole collect cascade settles inside the single host dispatch that
//! delivered the frame: an intra-cluster send is queued and drained
//! breadth-first before control returns to the host (ADR-0114), so the
//! filled-slot counter — not a re-poke or a deadline — is the sound flush
//! trigger. A self-addressed flush would run *before* the children's
//! replies on that same FIFO, so counting is the only correct signal.
//!
//! Children are spawned lazily on the node's first activation. Inline children
//! now run `wire`, but `init` still cannot spawn; keeping root and interior
//! spawning on the shared `Tick` / `Collect` activation path gives both roles
//! one guarded layout setup.

extern crate alloc;

mod kinds;
pub use kinds::*;
pub mod composite;
mod editor;
pub mod focus;
mod panel;
pub mod routing;
mod scroll;
pub mod set;
mod state;
pub mod text_edit;
pub mod theme;

pub use editor::EditorShell;
pub use panel::WidgetPanel;
pub use scroll::ScrollWidget;
pub use theme::{SetTheme, Theme, ThemeState};

// A cdylib carries one `export!` (the shared init/receive FFI entry); the macro
// emits the wasm32 FFI shims and the `aether.kinds` custom section for every
// listed actor. This is a grab-bag widget module (ADR-0138), so the bare list
// designates NO default: every actor is selector-only by `module@actor`
// selector (`aether_kit_widget@aether.kit.widget.*` /
// `aether_kit_widget@aether.kit.widget.editor`), never by list position. The
// `behavior` feature (ADR-0137, issue 2687) appends `aether-behavior`'s
// `BehaviorHost` so the panel's `WidgetKind::BehaviorHost` arm can spawn it by
// tag; the two invocations are cfg-exclusive, keeping the ordinary build's
// exported set (and its `aether.kinds` section) unchanged.
#[cfg(not(feature = "behavior"))]
aether_actor::export!(
    Widget,
    ScrollWidget,
    set::SliderWidget,
    set::TextFieldWidget,
    set::TextAreaWidget,
    set::RadioGroupWidget,
    set::ButtonWidget,
    set::LabelWidget,
    set::ImageWidget,
    set::VirtualListWidget,
    set::ToggleWidget,
    set::SegmentedWidget,
    set::NumericWidget,
    EditorShell,
    WidgetPanel
);

#[cfg(feature = "behavior")]
aether_actor::export!(
    Widget,
    ScrollWidget,
    set::SliderWidget,
    set::TextFieldWidget,
    set::TextAreaWidget,
    set::RadioGroupWidget,
    set::ButtonWidget,
    set::LabelWidget,
    set::ImageWidget,
    set::VirtualListWidget,
    set::ToggleWidget,
    set::SegmentedWidget,
    set::NumericWidget,
    EditorShell,
    WidgetPanel,
    aether_behavior::BehaviorHost
);

use aether_actor::{ActorInitError, Addressable, Manual, Subname, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_data::Kind;
use aether_kinds::{ClipRect, QuadSpace, Tick};
use aether_lifecycle::LifecycleCapability;
use aether_lifecycle::LifecycleMailboxExt;
use aether_math::Vec2;
use aether_render::{
    DrawSolidQuads, DrawTexturedQuads, RenderCapability, SolidQuad, TexturedQuad as RenderTexturedQuad,
};
use aether_text::{DrawText, DrawTextBatch, TextCapability};

use crate::composite::Composite;
use crate::kinds::WidgetClipIntersection;

/// A compositing widget node. `config` fixes its role and layout;
/// `composite` accumulates its subtree each frame; `spawned` guards the
/// one-time lazy spawn of its children.
pub struct Widget {
    config: WidgetConfig,
    composite: Composite,
    frame_discharge: FrameDischarge,
    spawned: bool,
}

/// One-shot completion state shared by every composite owner. A frame starts
/// open, then closes after its draw list has been emitted or replied upward.
/// Late or duplicate child replies cannot discharge the same frame twice.
#[derive(Debug)]
struct FrameDischarge {
    closed: bool,
}

impl Default for FrameDischarge {
    fn default() -> Self {
        Self { closed: true }
    }
}

impl FrameDischarge {
    fn begin_frame(&mut self) {
        self.closed = false;
    }

    fn is_closed(&self) -> bool {
        self.closed
    }

    fn close_frame(&mut self) -> bool {
        if self.closed {
            return false;
        }
        self.closed = true;
        true
    }
}

/// Decode a compositing child config and enforce the tree's single-root
/// invariant. Nested widgets are always driven by their parent's `Collect`;
/// allowing one to retain `root = true` would also subscribe it to `Tick` and
/// create a second renderer for the same subtree.
fn decode_nested_widget_config(spec: &WidgetChildSpec) -> Option<WidgetConfig> {
    let Some(config) = WidgetConfig::decode_from_bytes(&spec.config) else {
        tracing::warn!(
            target: "aether_kit_widget",
            subname = %spec.subname,
            "widget child config failed to decode; slot skipped",
        );
        return None;
    };
    if config.root {
        tracing::warn!(
            target: "aether_kit_widget",
            subname = %spec.subname,
            "nested widget child cannot be a root; slot skipped",
        );
        return None;
    }
    Some(config)
}

impl Widget {
    /// Spawn this node's children once and register a slot per child. An
    /// `init` cannot spawn, so this runs from the node's first activation
    /// handler and is shared by root and interior roles.
    /// A child whose subname fails validation or whose config fails to
    /// decode is skipped with a warn — its slot is never registered, so
    /// the completion counter stays honest.
    fn ensure_spawned(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        if self.spawned {
            return;
        }
        self.spawned = true;
        for spec in &self.config.children {
            let Some(child_config) = decode_nested_widget_config(spec) else {
                continue;
            };
            match ctx.spawn_inline_child::<Self>(Subname::Named(&spec.subname), &child_config) {
                Ok(alias) => self.composite.register_slot(
                    alias,
                    Vec2::new(spec.origin[0], spec.origin[1]),
                    spec.clip,
                    &spec.subname,
                    <Self as Addressable>::NAMESPACE,
                ),
                Err(error) => tracing::warn!(
                    target: "aether_kit_widget",
                    subname = %spec.subname,
                    ?error,
                    "widget child spawn failed; slot skipped",
                ),
            }
        }
    }

    /// Open a frame and fan `Collect` to every child. Resets the
    /// composite, lays down own chrome, then polls each child in layout
    /// order. A leaf (no children) is already complete, so it finishes on
    /// the spot; a node with children finishes later, from `on_draw_list`.
    fn drive_frame(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        self.ensure_spawned(ctx);
        flush_membership(&mut self.composite, ctx);
        self.composite.begin_frame();
        self.frame_discharge.begin_frame();
        self.composite.extend_chrome(self.config.chrome.iter().cloned());
        for spec in &self.config.children {
            if let Some(child) = ctx.child(&spec.subname) {
                child.send(&Collect);
            }
        }
        if self.composite.is_complete() {
            self.finish(ctx);
        }
    }

    /// Discharge the closed composite: the root emits it to the render /
    /// text caps; an interior or leaf node replies it up to its parent.
    fn finish(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        if self.frame_discharge.is_closed() {
            return;
        }
        let list = self.composite.flatten(self.config.intrinsic);
        if self.config.root {
            emit(ctx, &list);
        } else if let Some(parent) = ctx.parent() {
            parent.send(&list);
        } else {
            tracing::warn!(target: "aether_kit_widget", "non-root widget finished without a parent; draw list dropped");
        }
        let closed = self.frame_discharge.close_frame();
        debug_assert!(closed, "an open widget frame closes exactly once");
    }
}

/// Drain any buffered membership change and emit it up the parent lane. The
/// first-spawn burst of the whole stack drains as one batched
/// `ChildrenChanged`; a later single add/remove as one event. A loaded root
/// has no up-lane consumer, so the drain is a harmless no-send there (kept
/// mechanical for uniformity and future re-parenting). Shared by the
/// compositing node and the reference panel, which both own a `Composite`.
pub(crate) fn flush_membership(composite: &mut Composite, ctx: &mut WasmCtx<'_, Manual>) {
    if let Some(changed) = composite.take_membership_changes()
        && let Some(parent) = ctx.parent()
    {
        parent.send(&changed);
    }
}

/// Attribute an inbound child draw list to its slot by the reply source and
/// report whether the frame's slots have now all filled. The caller
/// discharges the completed composite its own way (the node replies up or
/// emits; the panel emits), so only the fill + completeness check is shared.
pub(crate) fn accept_child_list(
    composite: &mut Composite,
    ctx: &mut WasmCtx<'_, Manual>,
    list: WidgetDrawList,
) -> bool {
    if let Some(source) = ctx.source_mailbox() {
        composite.fill(source, list);
    }
    composite.is_complete()
}

fn accept_open_child_list(
    discharge: &FrameDischarge,
    composite: &mut Composite,
    ctx: &mut WasmCtx<'_, Manual>,
    list: WidgetDrawList,
) -> bool {
    !discharge.is_closed() && accept_child_list(composite, ctx, list)
}

/// One contiguous run of non-text widget draws with one render capability
/// shape. Text is filtered before planning and remains on its later lane.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PreparedClip {
    Unbounded,
    Finite { rect: WidgetClipRect },
}

impl PreparedClip {
    fn for_item(item: &WidgetDrawItem) -> Option<Self> {
        match item.valid_clip() {
            WidgetClipIntersection::Unbounded => Some(Self::Unbounded),
            WidgetClipIntersection::Finite { rect } => Some(Self::Finite { rect }),
            WidgetClipIntersection::Empty => None,
        }
    }

    fn framebuffer(self) -> Option<ClipRect> {
        match self {
            Self::Unbounded => None,
            Self::Finite { rect } => Some(framebuffer_clip(rect)),
        }
    }
}

#[derive(Debug, PartialEq)]
enum DirectRun {
    Solid { clip: PreparedClip, quads: Vec<SolidQuad> },
    Textured { texture_id: u32, clip: PreparedClip, quads: Vec<RenderTexturedQuad> },
}

/// Plan the filtered non-text subsequence in one pass. Adjacent solids
/// coalesce by effective clip; adjacent textured items coalesce by texture id
/// and effective clip. A kind, texture, or clip transition flushes without
/// globally regrouping repeated keys, preserving painter order. Invalid
/// explicit clips omit their items.
fn direct_runs(list: &WidgetDrawList) -> Vec<DirectRun> {
    let mut runs: Vec<DirectRun> = Vec::new();
    for item in &list.items {
        match item {
            WidgetDrawItem::Quad { x, y, width, height, color, .. } => {
                let Some(clip) = PreparedClip::for_item(item) else {
                    continue;
                };
                let quad = SolidQuad { x: *x, y: *y, width: *width, height: *height, color: *color };
                if let Some(DirectRun::Solid { clip: run_clip, quads }) = runs.last_mut()
                    && *run_clip == clip
                {
                    quads.push(quad);
                } else {
                    runs.push(DirectRun::Solid { clip, quads: vec![quad] });
                }
            }
            WidgetDrawItem::TexturedQuad { texture_id, x, y, width, height, u0, v0, u1, v1, tint, .. } => {
                let Some(clip) = PreparedClip::for_item(item) else {
                    continue;
                };
                let quad = RenderTexturedQuad {
                    x: *x,
                    y: *y,
                    width: *width,
                    height: *height,
                    u0: *u0,
                    v0: *v0,
                    u1: *u1,
                    v1: *v1,
                    tint: *tint,
                };
                if let Some(DirectRun::Textured { texture_id: run_texture_id, clip: run_clip, quads }) = runs.last_mut()
                    && *run_texture_id == *texture_id
                    && *run_clip == clip
                {
                    quads.push(quad);
                } else {
                    runs.push(DirectRun::Textured { texture_id: *texture_id, clip, quads: vec![quad] });
                }
            }
            WidgetDrawItem::Text { .. } => {}
        }
    }
    runs
}

fn framebuffer_clip(rect: WidgetClipRect) -> ClipRect {
    ClipRect { x: rect.x, y: rect.y, width: rect.width, height: rect.height }
}

/// Collect the filtered text subsequence into authored-order text items.
/// Invalid clips omit their items before the root converts the remaining clips
/// into framebuffer coordinates.
fn text_items(list: &WidgetDrawList) -> Vec<DrawText> {
    let mut items: Vec<DrawText> = Vec::new();
    for item in &list.items {
        if let WidgetDrawItem::Text { x, y, font_id, text, size_pixels, color, .. } = item {
            let Some(clip) = PreparedClip::for_item(item) else {
                continue;
            };
            items.push(DrawText {
                font_id: *font_id,
                text: text.clone(),
                size_pixels: *size_pixels,
                color: *color,
                origin: [*x, *y],
                space: QuadSpace::Screen,
                clip: clip.framebuffer(),
            });
        }
    }
    items
}

/// Emit a flattened subtree as the cluster's single render + text sender.
/// Compatible solid/textured runs preserve authored non-text order through
/// same-recipient FIFO, then one authored-order `DrawTextBatch` reaches the
/// text cap. Text's extra hop keeps the established later lane. Public so a
/// peer compositor in another crate (the terrain workbench panel) reuses the
/// same single-sender flush for its own composite.
pub fn emit(ctx: &mut WasmCtx<'_, Manual>, list: &WidgetDrawList) {
    for run in direct_runs(list) {
        match run {
            DirectRun::Solid { clip, quads } => {
                ctx.actor::<RenderCapability>().send(&DrawSolidQuads {
                    space: QuadSpace::Screen,
                    clip: clip.framebuffer(),
                    quads,
                });
            }
            DirectRun::Textured { texture_id, clip, quads } => {
                ctx.actor::<RenderCapability>().send(&DrawTexturedQuads {
                    texture_id,
                    space: QuadSpace::Screen,
                    clip: clip.framebuffer(),
                    quads,
                });
            }
        }
    }
    let items = text_items(list);
    if !items.is_empty() {
        ctx.actor::<TextCapability>().send(&DrawTextBatch { items });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_math::Rgba;

    fn quad(x: f32, clip: Option<WidgetClipRect>) -> WidgetDrawItem {
        WidgetDrawItem::Quad { x, y: 0.0, width: 1.0, height: 1.0, color: Rgba::WHITE, clip }
    }

    fn text(x: f32, label: &str, clip: Option<WidgetClipRect>) -> WidgetDrawItem {
        WidgetDrawItem::Text { x, y: 0.0, font_id: 1, text: label.into(), size_pixels: 12.0, color: Rgba::WHITE, clip }
    }

    fn textured(texture_id: u32, x: f32, clip: Option<WidgetClipRect>) -> WidgetDrawItem {
        WidgetDrawItem::TexturedQuad {
            texture_id,
            x,
            y: 0.0,
            width: 1.0,
            height: 1.0,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            tint: Rgba::WHITE,
            clip,
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one cohesive direct-run order/key matrix
    fn direct_planner_coalesces_only_adjacent_compatible_non_text_items() {
        let a = WidgetClipRect { x: 1.0, y: 2.0, width: 3.0, height: 4.0 };
        let b = WidgetClipRect { x: 5.0, y: 6.0, width: 7.0, height: 8.0 };
        let list = WidgetDrawList {
            intrinsic: None,
            items: vec![
                quad(0.0, Some(a)),
                textured(7, 1.0, Some(a)),
                text(0.0, "text", Some(b)),
                textured(7, 2.0, Some(a)),
                textured(8, 3.0, Some(a)),
                textured(7, 4.0, Some(a)),
                textured(7, 5.0, Some(b)),
                quad(6.0, Some(b)),
                quad(7.0, Some(b)),
            ],
        };
        let runs = direct_runs(&list);
        assert_eq!(runs.len(), 6);
        assert!(matches!(
            &runs[0],
            DirectRun::Solid { clip, quads }
                if *clip == PreparedClip::Finite { rect: a }
                    && quads.iter().map(|quad| quad.x).eq([0.0])
        ));
        assert!(
            matches!(
                &runs[1],
                DirectRun::Textured {
                    texture_id: 7,
                    clip,
                    quads,
                } if *clip == PreparedClip::Finite { rect: a }
                    && quads.iter().map(|quad| quad.x).eq([1.0, 2.0])
            ),
            "text does not split a compatible textured run"
        );
        assert!(matches!(
            &runs[2],
            DirectRun::Textured {
                texture_id: 8,
                clip,
                quads,
            } if *clip == PreparedClip::Finite { rect: a }
                && quads.iter().map(|quad| quad.x).eq([3.0])
        ));
        assert!(
            matches!(
                &runs[3],
                DirectRun::Textured {
                    texture_id: 7,
                    clip,
                    quads,
                } if *clip == PreparedClip::Finite { rect: a }
                    && quads.iter().map(|quad| quad.x).eq([4.0])
            ),
            "a repeated texture key after a different texture must not reorder"
        );
        assert!(matches!(
            &runs[4],
            DirectRun::Textured {
                texture_id: 7,
                clip,
                quads,
            } if *clip == PreparedClip::Finite { rect: b }
                && quads.iter().map(|quad| quad.x).eq([5.0])
        ));
        assert!(matches!(
            &runs[5],
            DirectRun::Solid { clip, quads }
                if *clip == PreparedClip::Finite { rect: b }
                    && quads.iter().map(|quad| quad.x).eq([6.0, 7.0])
        ));
    }

    #[test]
    fn direct_planner_keeps_unclipped_solids_one_batch_and_omits_invalid_clips() {
        let invalid = WidgetClipRect { x: 0.0, y: 0.0, width: -1.0, height: 2.0 };
        let list =
            WidgetDrawList { intrinsic: None, items: vec![quad(1.0, None), quad(2.0, Some(invalid)), quad(3.0, None)] };
        let runs = direct_runs(&list);
        assert_eq!(runs.len(), 1);
        assert!(matches!(
            &runs[0],
            DirectRun::Solid { clip: PreparedClip::Unbounded, quads }
                if quads.iter().map(|quad| quad.x).eq([1.0, 3.0])
        ));
    }

    #[test]
    fn text_items_preserve_authored_order_and_omit_clipped_out_items() {
        let invalid = WidgetClipRect { x: 0.0, y: 0.0, width: -1.0, height: 2.0 };
        let list = WidgetDrawList {
            intrinsic: None,
            items: vec![
                text(10.0, "first", None),
                quad(0.0, None),
                text(20.0, "second", None),
                text(30.0, "skipped", Some(invalid)),
                text(40.0, "third", None),
            ],
        };

        let items = text_items(&list);
        assert_eq!(items.len(), 3, "three valid text items survive filtering");
        assert!(items.iter().map(|item| item.text.as_str()).eq(["first", "second", "third"]));
        assert!(items.iter().map(|item| item.origin[0]).eq([10.0, 20.0, 40.0]));
    }

    #[test]
    fn nested_widget_config_rejects_a_second_root() {
        let mut config = WidgetConfig { root: false, ..WidgetConfig::default() };
        let spec = |config: &WidgetConfig| WidgetChildSpec {
            subname: "nested".into(),
            kind: WidgetKind::Composite,
            origin: [0.0, 0.0],
            clip: None,
            config: config.encode_into_bytes(),
        };

        assert!(decode_nested_widget_config(&spec(&config)).is_some());
        config.root = true;
        assert!(decode_nested_widget_config(&spec(&config)).is_none());
    }

    #[test]
    fn frame_discharge_is_idempotent_for_late_or_duplicate_replies() {
        let mut discharge = FrameDischarge::default();
        assert!(discharge.is_closed());

        discharge.begin_frame();
        assert!(!discharge.is_closed());
        assert!(discharge.close_frame());
        assert!(discharge.is_closed());
        assert!(!discharge.close_frame(), "a duplicate or late reply cannot close the frame twice");
        assert!(discharge.is_closed());

        discharge.begin_frame();
        assert!(!discharge.is_closed(), "the next frame reopens the one-shot discharge state");
    }
}

/// Compositing widget. Loaded as the root of a widget cluster or spawned
/// as an inline child within one; its [`WidgetConfig`] selects the role.
///
/// # Agent
/// Load the root with a `WidgetConfig { root: true, chrome, children }`;
/// each `children` entry carries a pre-encoded child `WidgetConfig` in its
/// `config` bytes, so the whole tree ships in one load. The cluster is one
/// render sender: the root emits every widget's solid/textured draws in
/// structural depth-first order, grouping only adjacent compatible items, so
/// a background drawn as root chrome sits under the children by construction.
#[actor(instanced)]
impl WasmActor for Widget {
    type Config = WidgetConfig;
    const NAMESPACE: &'static str = "aether.kit.widget";

    fn init(config: WidgetConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Widget { config, composite: Composite::new(), frame_discharge: FrameDischarge::default(), spawned: false })
    }

    /// The root subscribes the frame stage once (the root-subscribes-once
    /// pattern). `Tick` is a frame-lifecycle stage, so it rides
    /// `aether.lifecycle` (ADR-0082), not the input cap. A non-root node
    /// is driven by its parent's `Collect`, so it subscribes nothing.
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        if self.config.root {
            ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
        }
    }

    /// Root frame driver: open a frame and fan `Collect`. Non-root nodes
    /// are not tick-subscribed, so this is the root's path only.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler::manual]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_, Manual>, _tick: Tick) {
        self.drive_frame(ctx);
    }

    /// A collect poll from this node's parent. A leaf answers with its own
    /// chrome at once; an interior node fans `Collect` to its own children
    /// and withholds its reply until its slots close.
    ///
    /// # Agent
    /// Sent by a compositing parent each frame; not useful to send
    /// manually.
    #[handler::manual]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_, Manual>, _collect: Collect) {
        self.drive_frame(ctx);
    }

    /// A child's draw list. Attribute it to the child's slot by the
    /// inbound source, and when every slot has replied this frame,
    /// discharge the composite (emit at the root, reply up at an interior
    /// node).
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    //noinspection DuplicatedCode -- actor macros require one draw-list handler per composite owner type.
    #[handler::manual]
    fn on_draw_list(&mut self, ctx: &mut WasmCtx<'_, Manual>, list: WidgetDrawList) {
        if accept_open_child_list(&self.frame_discharge, &mut self.composite, ctx, list) {
            self.finish(ctx);
        }
    }
}
