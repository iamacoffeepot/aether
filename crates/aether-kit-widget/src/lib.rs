// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload and hands
// it off, so a by-value parameter is the contract, not a copy the body
// could borrow.
#![allow(clippy::needless_pass_by_value)]

//! The widget-compositing actor (ADR-0117).
//!
//! One `#[actor(instanced, composable)]` type realizes every role in a widget tree —
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
pub mod layout;
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
pub use theme::{SetTheme, TextInk, TextRole, Theme, ThemeState};

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
//
// The `export!` macro itself gates its emitted entry surface behind the invoking
// crate's `library` feature, so a consuming cdylib (aether-kit's workbench) links
// the widget `WasmActor` impls for inline-spawn — enabling `library` — without
// inheriting a second copy of the `receive_p32` / `init` FFI shims that would
// collide with its own `export!`. The call sites stay bare.
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
use aether_render::QuadBlend;
use aether_render::{
    DrawSolidQuads, DrawTexturedQuads, RenderCapability, SolidQuad, TexturedQuad as RenderTexturedQuad,
};
use aether_text::{DrawText, DrawTextBatch, TextCapability};

use crate::composite::Composite;
use crate::kinds::WidgetClipIntersection;
use crate::set::FONT_LINE_BOX_RATIO;

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
            match ctx.spawn_inline_child::<Self, Self>(Subname::Named(&spec.subname), &child_config) {
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

/// The fills a glyph run has to stay out from under: every fill that lands
/// after it. Built by walking a lane backwards, so at each text item the set
/// holds exactly the lane's later fills — seeded, for the ordinary lane, with
/// the whole overlay, which is after all of it.
///
/// `bounds` is the union of the set. Almost no glyph run in a frame has a
/// later fill anywhere near it, and one rejection against the union answers
/// that without touching the list.
#[derive(Default)]
struct LaterFills {
    rects: Vec<WidgetClipRect>,
    bounds: Option<WidgetClipRect>,
}

impl LaterFills {
    fn push(&mut self, rect: WidgetClipRect) {
        self.bounds = Some(self.bounds.map_or(rect, |bounds| bounds.union(rect)));
        self.rects.push(rect);
    }

    /// `clip` with every later fill that stands over the run cut out of it:
    /// `clip` alone when none of them does, nothing when together they cover
    /// the run.
    ///
    /// `run` is the box the glyphs occupy and `hairline` the thickness under
    /// which a fill is a mark rather than an occluder. Both tests guard the
    /// same failure — cutting a run's scissor where no glyph of it was
    /// meaningfully hidden, which costs batches without changing what a reader
    /// sees:
    ///
    /// - A fill that misses the run is skipped. A text item's clip is
    ///   routinely far larger than its glyphs — every row of a list carries
    ///   the whole viewport, with the scroll bar's column inside it — so
    ///   without this each row's scissor would come back as strips around the
    ///   rows below it and the bar beside it.
    /// - A hairline is skipped ([`WidgetClipRect::is_hairline`]): a caret, an
    ///   IME underline, a rule, and a focus ring's stroke are all drawn after
    ///   the run they touch, and reading them as holes would split a focused
    ///   field's run into strips around a one-pixel bar.
    fn cut(&self, clip: WidgetClipRect, run: WidgetClipRect, hairline: f32) -> Vec<WidgetClipRect> {
        if !self.bounds.is_some_and(|bounds| run.overlaps(bounds)) {
            return vec![clip];
        }
        let mut remaining = vec![clip];
        for hole in &self.rects {
            if remaining.is_empty() {
                break;
            }
            if !hole.overlaps(run) || hole.is_hairline(hairline) || !remaining.iter().any(|part| part.overlaps(*hole)) {
                continue;
            }
            remaining = remaining.into_iter().flat_map(|part| part.subtract(*hole)).collect();
        }
        remaining
    }
}

/// A generous ceiling on one character's advance, as a fraction of the draw
/// size — an em, which no Latin face in normal use exceeds. It bounds the box
/// a run occupies from its origin, and errs wide on purpose: too wide costs a
/// batch when a fill happens to sit beside a run, while too narrow would stop
/// a fill covering the run's tail from clipping it. The kit's other estimate,
/// `set::APPROX_ADVANCE_RATIO`, aims at the middle instead, because a caret
/// placed too far right is as wrong as one placed too far left.
const GLYPH_ADVANCE_CEILING: f32 = 1.0;

/// How thin a fill has to be to read as a mark on the text it crosses rather
/// than something standing over it, as a fraction of the draw size. Half a
/// draw size is well above every stroke the kit draws over a run — a caret and
/// an IME underline are one pixel, a rule and a focus ring two — and well
/// below the shortest thing that stands over a line, which is a row.
const HAIRLINE_RATIO: f32 = 0.5;

/// The box one text item occupies: from its pen origin, one line tall
/// (`set::FONT_LINE_BOX_RATIO`) and at most one em per character wide.
/// `aether.text` lays every run out on a single line — it has no line break —
/// so one box covers it.
fn glyph_box(x: f32, y: f32, text: &str, size_pixels: f32) -> WidgetClipRect {
    #[allow(clippy::cast_precision_loss)] // a run long enough to lose precision here is already far off-screen
    let chars = text.chars().count() as f32;
    WidgetClipRect {
        x,
        y,
        width: chars * size_pixels * GLYPH_ADVANCE_CEILING,
        height: size_pixels * FONT_LINE_BOX_RATIO,
    }
}

/// Collect the filtered text subsequence into authored-order text items.
/// Invalid clips omit their items before the root converts the remaining clips
/// into framebuffer coordinates.
///
/// Text reaches the render cap one hop after the quads a cluster sends
/// directly, so no fill can cover the glyphs authored before it by draw order
/// alone. The hierarchy answers that the way a plate always has — by not
/// drawing what it covers: a text item's clip is re-clipped to the part of its
/// rect the fills **after it** leave uncovered
/// ([`WidgetClipRect::subtract`]), and omitted when nothing is left. Only the
/// fills that reach the run's own line take part ([`LaterFills::cut`]); a clip
/// is a scissor bound and is routinely much larger than the glyphs inside it.
/// An unclipped text item (root chrome with no clip) cannot be cut and is
/// drawn as authored.
///
/// The subtraction is **positional**, which is what makes a plate able to hold
/// children *and* a control on that plate able to open over its own siblings.
/// A fill only cuts the glyphs authored before it, so a plate's own fill
/// leaves the labels its children draw after it whole, while an open dropdown
/// list — registered after the controls it stands over — cuts every one of
/// them. The rule reads the same in both lanes: [`emit`] runs it over the
/// ordinary items with the overlay's fills already in the set (the overlay is
/// entirely after the ordinary lane) and again over the overlay's own items
/// from empty.
fn text_items(list: &WidgetDrawList) -> Vec<DrawText> {
    let mut later = LaterFills::default();
    for rect in list.overlay.iter().filter_map(WidgetDrawItem::covered_rect) {
        later.push(rect);
    }
    let mut items: Vec<DrawText> = Vec::new();
    for item in list.items.iter().rev() {
        let WidgetDrawItem::Text { x, y, font_id, text, size_pixels, color, .. } = item else {
            if let Some(rect) = item.covered_rect() {
                later.push(rect);
            }
            continue;
        };
        let Some(clip) = PreparedClip::for_item(item) else {
            continue;
        };
        let draw = |clip: Option<ClipRect>| DrawText {
            font_id: *font_id,
            text: text.clone(),
            size_pixels: *size_pixels,
            color: *color,
            origin: [*x, *y],
            space: QuadSpace::Screen,
            clip,
        };
        match clip {
            PreparedClip::Finite { rect } => {
                let run = glyph_box(*x, *y, text, *size_pixels);
                let hairline = size_pixels * HAIRLINE_RATIO;
                items.extend(later.cut(rect, run, hairline).into_iter().map(|part| draw(Some(framebuffer_clip(part)))));
            }
            PreparedClip::Unbounded => items.push(draw(None)),
        }
    }
    items.reverse();
    items
}

/// Emit a flattened subtree as the cluster's single render + text sender.
/// Compatible solid/textured runs preserve authored non-text order through
/// same-recipient FIFO, then one authored-order `DrawTextBatch` reaches the
/// text cap. Text's extra hop keeps the established later lane. Public so a
/// peer compositor in another crate (the terrain workbench panel) reuses the
/// same single-sender flush for its own composite.
pub fn emit(ctx: &mut WasmCtx<'_, Manual>, list: &WidgetDrawList) {
    emit_layer(ctx, list);
    if !list.overlay.is_empty() {
        emit_layer(
            ctx,
            &WidgetDrawList { content_height: None, intrinsic: None, items: list.overlay.clone(), overlay: Vec::new() },
        );
    }
}

/// One layer of a flattened list — its `items` — as solid / textured runs
/// then one text batch. Called for the ordinary items and again for the
/// overlay, so an overlay's quads and glyphs are submitted after every
/// ordinary quad and glyph respectively.
fn emit_layer(ctx: &mut WasmCtx<'_, Manual>, list: &WidgetDrawList) {
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
                    blend: QuadBlend::Straight,
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
    use crate::set::text_origin_y;
    use aether_data::MailboxId;
    use aether_math::Rgba;

    fn quad(x: f32, clip: Option<WidgetClipRect>) -> WidgetDrawItem {
        WidgetDrawItem::Quad { x, y: 0.0, width: 1.0, height: 1.0, color: Rgba::WHITE, clip }
    }

    fn fill(rect: WidgetClipRect) -> WidgetDrawItem {
        WidgetDrawItem::Quad {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            color: Rgba::WHITE,
            clip: None,
        }
    }

    /// A one-line run at `x`, its pen origin at the top of whatever bounds it —
    /// which is where a widget puts a run inside the row it clipped it to, and
    /// what the flatten reads to decide which fills reach the line.
    fn text(x: f32, label: &str, clip: Option<WidgetClipRect>) -> WidgetDrawItem {
        WidgetDrawItem::Text {
            x,
            y: clip.map_or(0.0, |rect| rect.y),
            font_id: 1,
            text: label.into(),
            size_pixels: 12.0,
            color: Rgba::WHITE,
            clip,
        }
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
            content_height: None,
            intrinsic: None,
            overlay: Vec::new(),
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
        let list = WidgetDrawList {
            content_height: None,
            intrinsic: None,
            items: vec![quad(1.0, None), quad(2.0, Some(invalid)), quad(3.0, None)],
            overlay: Vec::new(),
        };
        let runs = direct_runs(&list);
        assert_eq!(runs.len(), 1);
        assert!(matches!(
            &runs[0],
            DirectRun::Solid { clip: PreparedClip::Unbounded, quads }
                if quads.iter().map(|quad| quad.x).eq([1.0, 3.0])
        ));
    }

    #[test]
    fn ordinary_text_under_an_overlay_fill_is_cut_to_what_the_fill_leaves() {
        // Tripwire: a glyph run reaches the render cap a hop after the
        // overlay's fill, so the root must not send the part of it the fill
        // covers — a row wholly under an open dropdown's list sends nothing,
        // a row the list half covers sends only its uncovered strip.
        let row = |y: f32| WidgetClipRect { x: 10.0, y, width: 200.0, height: 24.0 };
        let list = WidgetDrawList {
            content_height: None,
            intrinsic: None,
            items: vec![text(12.0, "covered", Some(row(40.0))), text(12.0, "half", Some(row(64.0)))],
            overlay: vec![WidgetDrawItem::Quad {
                x: 10.0,
                y: 40.0,
                width: 200.0,
                height: 36.0,
                color: Rgba::WHITE,
                clip: None,
            }],
        };
        let items = text_items(&list);
        assert_eq!(items.len(), 1, "the wholly covered row sends no glyphs");
        assert_eq!(items[0].text, "half");
        let clip = items[0].clip.clone().expect("the half-covered row keeps a finite clip");
        assert_eq!((clip.y, clip.height), (76.0, 12.0), "only the strip below the fill survives");
    }

    #[test]
    fn a_background_widgets_own_overlay_stays_under_a_plate_raised_after_it() {
        // Tripwire: the studio's gem question. A numeric in the sheet behind
        // it is still latched hovered when the question opens, and a hovered
        // single-line edit whose value overruns its box reports an overflow
        // reveal plate in its own overlay — an overlay that has not escaped a
        // group, because its slot is not in one. That plate belongs where its
        // slot sits, under the plate the question raised after it. Holding
        // every child's overlay back to the end of the lane put it over the
        // question instead and blanked the title.
        let title_row = WidgetClipRect { x: 300.0, y: 350.0, width: 420.0, height: 20.0 };
        let background = MailboxId(1);
        let title = MailboxId(2);
        let ok = MailboxId(3);

        let mut composite = Composite::new();
        composite.register_slot(background, Vec2::ZERO, None, "sheet_numeric", "aether.kit.widget");
        composite.register_slot(title, Vec2::ZERO, None, "picker_title", "aether.kit.widget");
        composite.register_slot(ok, Vec2::ZERO, None, "picker_ok", "aether.kit.widget");
        composite.set_slot_overlay(title, true);
        composite.set_slot_overlay(ok, true);
        composite.begin_frame();
        // The question's plate, raised before the controls standing on it.
        composite.extend_overlay([fill(WidgetClipRect { x: 290.0, y: 340.0, width: 440.0, height: 300.0 })]);
        composite.fill(
            background,
            WidgetDrawList {
                content_height: None,
                intrinsic: None,
                items: vec![text(8.0, "24.0", Some(WidgetClipRect { x: 0.0, y: 350.0, width: 100.0, height: 20.0 }))],
                // The reveal plate, over the numeric and whatever sits to its
                // right — which is where the question is standing.
                overlay: vec![fill(WidgetClipRect { x: 0.0, y: 350.0, width: 800.0, height: 20.0 })],
            },
        );
        composite.fill(
            title,
            WidgetDrawList {
                content_height: None,
                intrinsic: None,
                items: vec![text(302.0, "Pick a gem", Some(title_row))],
                overlay: Vec::new(),
            },
        );
        composite.fill(
            ok,
            WidgetDrawList {
                content_height: None,
                intrinsic: None,
                items: vec![fill(WidgetClipRect { x: 300.0, y: 600.0, width: 80.0, height: 24.0 })],
                overlay: Vec::new(),
            },
        );

        let flat = composite.flatten(None);
        let overlay_lane =
            WidgetDrawList { content_height: None, intrinsic: None, items: flat.overlay, overlay: Vec::new() };
        assert!(
            text_items(&overlay_lane).iter().any(|item| item.text == "Pick a gem"),
            "the question's own title keeps its run: what a widget behind the plate raises went down \
             where its slot sits, under the plate, not over the group",
        );
    }

    #[test]
    fn an_overlay_plate_cuts_the_content_under_it_and_only_the_labels_its_list_covers() {
        // Tripwire: the subtraction inside a lane is positional, which is the
        // whole of gap 30. A plate's fill is authored before the labels its
        // children draw, so it must leave them whole — cutting a lane's text
        // against all of its fills would delete every label on the plate, the
        // defect that kept the studio's plates in chrome. A list one of those
        // controls opens is authored *after* them, so it must cut the ones it
        // covers — not cutting them at all was the studio's dialog printing
        // its own row text through its open dropdown.
        let plate = WidgetClipRect { x: 100.0, y: 100.0, width: 200.0, height: 80.0 };
        let row = |y: f32| WidgetClipRect { x: 110.0, y, width: 180.0, height: 20.0 };
        let opened_list = WidgetClipRect { x: 110.0, y: 130.0, width: 180.0, height: 45.0 };
        let list = WidgetDrawList {
            content_height: None,
            intrinsic: None,
            items: vec![text(110.0, "primary content", Some(plate))],
            overlay: vec![
                fill(plate),
                text(110.0, "the picker's own row", Some(row(110.0))),
                text(110.0, "a button under the list", Some(row(140.0))),
                fill(opened_list),
                text(115.0, "an option in the list", Some(row(135.0))),
            ],
        };

        assert!(text_items(&list).is_empty(), "the content the plate stands over sends no glyphs");
        let overlay_lane =
            WidgetDrawList { content_height: None, intrinsic: None, items: list.overlay, overlay: Vec::new() };
        assert!(
            text_items(&overlay_lane)
                .iter()
                .map(|item| item.text.as_str())
                .eq(["the picker's own row", "an option in the list"]),
            "the plate leaves the labels drawn on it whole, its open list deletes the one it covers, \
             and the list's own option is authored after the list's fill",
        );
    }

    #[test]
    fn one_run_crossed_by_two_later_fills_loses_both_bands() {
        // Tripwire: the holes accumulate across the lane rather than the last
        // one walked winning. Two controls opening over the same row — a menu
        // and a tooltip — each owe their bite out of it.
        let run = WidgetClipRect { x: 0.0, y: 0.0, width: 100.0, height: 10.0 };
        let column = |x: f32| WidgetClipRect { x, y: -2.0, width: 10.0, height: 24.0 };
        let list = WidgetDrawList {
            content_height: None,
            intrinsic: None,
            overlay: Vec::new(),
            items: vec![text(0.0, "a run that fills its whole row", Some(run)), fill(column(20.0)), fill(column(60.0))],
        };

        let mut spans: Vec<(f32, f32)> = text_items(&list)
            .iter()
            .map(|item| {
                let clip = item.clip.clone().expect("a cut run keeps a finite clip");
                (clip.x, clip.width)
            })
            .collect();
        spans.sort_by(|left, right| left.0.total_cmp(&right.0));
        assert_eq!(spans, vec![(0.0, 20.0), (30.0, 30.0), (70.0, 30.0)]);
    }

    #[test]
    fn a_caret_inside_the_line_marks_the_run_rather_than_standing_over_it() {
        // Tripwire: a field draws its caret after the value, inside the row's
        // padding. Reading it as a hole cuts the run's one scissor into strips
        // around a one-pixel bar — same pixels, three batches per focused
        // field, every frame — where a fill that genuinely stands over the
        // line reaches past it.
        // The real placement rule on both sides: the run sits at the pen
        // origin its row centers it on, the caret inside the row's padding.
        let row = WidgetClipRect { x: 0.0, y: 0.0, width: 100.0, height: 24.0 };
        let value = WidgetDrawItem::Text {
            x: 4.0,
            y: text_origin_y(row.y, row.height, 14.0),
            font_id: 1,
            text: "value".into(),
            size_pixels: 14.0,
            color: Rgba::WHITE,
            clip: Some(row),
        };
        let caret = WidgetClipRect { x: 40.0, y: 4.0, width: 1.0, height: 16.0 };
        let list = WidgetDrawList {
            content_height: None,
            intrinsic: None,
            overlay: Vec::new(),
            items: vec![value, fill(caret)],
        };

        let items = text_items(&list);
        assert_eq!(items.len(), 1, "the run stays one item; got {items:?}");
        assert_eq!(
            items[0].clip.clone().map(|clip| (clip.x, clip.width)),
            Some((row.x, row.width)),
            "and keeps its whole scissor",
        );
    }

    #[test]
    fn a_fill_the_viewport_clipped_away_punches_no_hole() {
        // Tripwire: the hole is what a fill *paints*, not the rectangle it was
        // authored at. A virtual list's row scrolled above its viewport keeps
        // its full geometry and is clipped to nothing visible; reading the
        // geometry would erase the header text it was scrolled behind.
        let header = WidgetClipRect { x: 0.0, y: 0.0, width: 200.0, height: 20.0 };
        let viewport = WidgetClipRect { x: 0.0, y: 20.0, width: 200.0, height: 100.0 };
        let list = WidgetDrawList {
            content_height: None,
            intrinsic: None,
            overlay: Vec::new(),
            items: vec![
                text(0.0, "header", Some(header)),
                WidgetDrawItem::Quad {
                    x: 0.0,
                    y: 0.0,
                    width: 200.0,
                    height: 24.0,
                    color: Rgba::WHITE,
                    clip: Some(viewport),
                },
            ],
        };

        let items = text_items(&list);
        assert!(items.iter().map(|item| item.text.as_str()).eq(["header"]));
        assert_eq!(
            items[0].clip.clone().map(|clip| (clip.y, clip.height)),
            Some((header.y, header.height)),
            "the scrolled row paints nothing over the header, so it takes nothing out of it",
        );
    }

    #[test]
    fn text_items_preserve_authored_order_and_omit_clipped_out_items() {
        let invalid = WidgetClipRect { x: 0.0, y: 0.0, width: -1.0, height: 2.0 };
        let list = WidgetDrawList {
            content_height: None,
            intrinsic: None,
            overlay: Vec::new(),
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
#[actor(instanced, composable)]
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
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
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
