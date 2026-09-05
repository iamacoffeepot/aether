//! The widget-compositing wire vocabulary (ADR-0117).
//!
//! A widget tree is a cluster of inline-child actors (ADR-0114). Each
//! widget draws in its own local coordinates and never touches
//! `aether.render`; instead the cluster's root composites the whole
//! subtree and emits it as one ordered stream from the root, so a dense panel
//! costs one render sender rather than one per widget and draw order is the
//! structural depth-first traversal of the tree. Distinct effective clips may
//! split that stream into contiguous render batches without adding senders.
//!
//! Two kinds carry the protocol:
//!
//! - [`Collect`] flows data-down. A compositing node sends it to each of
//!   its children, in its own layout order, once per frame.
//! - [`WidgetDrawList`] flows events-up. A widget's [`Collect`] handler
//!   **always** replies one — empty when it draws nothing — to its
//!   parent. That always-reply contract is what lets the parent close on
//!   a filled-slot counter rather than a timeout: the intra-cluster send
//!   queue drains breadth-first inside the one host dispatch that
//!   delivered the frame, so counting filled slots against the fanned
//!   count is a structural completion signal, not a temporal guess.
//!
//! Coordinates on a [`WidgetDrawItem`] are local to the widget's own
//! origin; the parent offsets each child's items by the rect it assigned
//! that child from its own layout table (layout is data-down, so no
//! origin handshake beyond attribution). [`WidgetConfig`] is the
//! non-recursive tree description a compositing widget is loaded or
//! spawned with — each child rides as an opaque pre-encoded
//! [`WidgetConfig`] in [`WidgetChildSpec::config`], which breaks the
//! type-level recursion a nested tree would otherwise form.

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::MailboxId;
use aether_math::{Rgba, Vec2};
use serde::{Deserialize, Serialize};

use crate::theme::{TextInk, TextRole, Theme};

/// `aether.kit.widget.collect` — a per-frame poll a compositing node
/// sends to each of its children in layout order. The child answers with
/// its [`WidgetDrawList`]. Fieldless: the poll carries no data, because
/// layout already flowed down at spawn (a child's rect is its parent's to
/// assign) and only geometry flows back up.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.collect")]
pub struct Collect;

/// One added child's identity in a [`ChildrenChanged`] event: its inline
/// `subname` and `type_namespace` — the spawned actor's `NAMESPACE` lineage
/// string, the same address vocabulary lineage addressing speaks. Both are
/// strings, not tags, because the observers this event serves (a debugger, an
/// MCP agent, the behavior host's tree cache) read identity, not an opaque
/// number. Not a kind on its own; only addressable inside
/// [`ChildrenChanged::added`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MembershipEntry {
    pub subname: String,
    pub type_namespace: String,
}

/// `aether.kit.widget.children_changed` — a compositing node's membership
/// delta, emitted up the lane whenever its slot set changes. `added` names
/// each child that appeared (with its type); `removed` names each departed
/// child by subname. The widget runtime buffers deltas at the slot
/// chokepoints and drains them once per activation, so the initial spawn of a
/// stack drains as one batched event carrying all N adds, and a later single
/// despawn as one event with one `removed` entry. It is the discovery signal a
/// lane observer (the behavior host's tree cache, a debugger) reads to know
/// what a node contains and when that changed.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.kit.widget.children_changed")]
pub struct ChildrenChanged {
    pub added: Vec<MembershipEntry>,
    pub removed: Vec<String>,
}

/// A clip rectangle in the current widget composition space.
///
/// On a [`WidgetDrawItem`] the rectangle is local to that item's widget. On a
/// [`WidgetChildSpec`] it is local to the parent that owns the child slot.
/// Composition translates item clips with their geometry and intersects them
/// with parent-local slot clips until the root holds one effective rectangle
/// in screen-pixel coordinates. Only the root converts this kit-owned type to
/// the framebuffer-only `aether_kinds::ClipRect`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct WidgetClipRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WidgetClipRect {
    /// Translate this rectangle into its parent's composition space.
    #[must_use]
    fn offset(self, by: Vec2) -> Self {
        Self { x: self.x + by.x, y: self.y + by.y, ..self }
    }

    /// Whether this rectangle has finite coordinates and a positive finite
    /// extent. Invalid and empty explicit clips omit their item.
    #[must_use]
    fn is_valid(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width > 0.0
            && self.height > 0.0
            && (self.x + self.width).is_finite()
            && (self.y + self.height).is_finite()
    }

    /// Whether the two rectangles share any area. Edge contact is not an
    /// overlap, matching [`Self::subtract`], which leaves `self` whole for it.
    /// An invalid rectangle overlaps nothing.
    #[must_use]
    pub(super) fn overlaps(self, other: Self) -> bool {
        self.is_valid()
            && other.is_valid()
            && self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }

    /// Whether this rectangle is a hairline at `thickness` — thinner than that
    /// on one axis, so it marks what it crosses rather than standing over it.
    /// A caret, an IME underline, a rule, and a focus ring's stroke are all
    /// drawn after the run they touch and are one or two pixels through.
    #[must_use]
    pub(super) fn is_hairline(self, thickness: f32) -> bool {
        self.width < thickness || self.height < thickness
    }

    /// The smallest rectangle containing both. Used only as a conservative
    /// bound over a set of fills, so an invalid operand yields the other.
    #[must_use]
    pub(super) fn union(self, other: Self) -> Self {
        if !self.is_valid() {
            return other;
        }
        if !other.is_valid() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);
        Self { x, y, width: right - x, height: bottom - y }
    }

    /// This rectangle with `hole` cut out of it: the up-to-four strips (left,
    /// right, top, bottom) that together cover everything of `self` outside
    /// `hole`. `self` alone when the two do not overlap; nothing when `hole`
    /// covers it. The root uses it to keep a glyph run out from under every
    /// fill drawn after it, since text reaches the render cap a hop after the
    /// quads and would otherwise print through them.
    #[must_use]
    pub(super) fn subtract(self, hole: Self) -> Vec<Self> {
        let right = self.x + self.width;
        let bottom = self.y + self.height;
        let hole_right = hole.x + hole.width;
        let hole_bottom = hole.y + hole.height;
        let cut_x = self.x.max(hole.x);
        let cut_y = self.y.max(hole.y);
        let cut_right = right.min(hole_right);
        let cut_bottom = bottom.min(hole_bottom);
        if !hole.is_valid() || cut_right <= cut_x || cut_bottom <= cut_y {
            return alloc::vec![self];
        }
        [
            Self { x: self.x, y: self.y, width: cut_x - self.x, height: self.height },
            Self { x: cut_right, y: self.y, width: right - cut_right, height: self.height },
            Self { x: cut_x, y: self.y, width: cut_right - cut_x, height: cut_y - self.y },
            Self { x: cut_x, y: cut_bottom, width: cut_right - cut_x, height: bottom - cut_bottom },
        ]
        .into_iter()
        .filter(|strip| strip.is_valid())
        .collect()
    }
}

/// Result of composing two optional widget clips. Two absent clips are
/// unbounded, while an explicit invalid, empty, disjoint, or edge-touching
/// rectangle is empty and omits the item.
// Crate-internal geometry helper, kept out of the public API. `kinds` is a
// private module glob-re-exported by the crate root (`pub use kinds::*`), so
// `pub` would leak this into the public surface; `pub(crate)` is deliberate and
// `super` is the crate root here, which is why the nursery lint sees it as
// redundant.
#[allow(clippy::redundant_pub_crate)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum WidgetClipIntersection {
    Unbounded,
    Finite { rect: WidgetClipRect },
    Empty,
}

/// Intersect two clips expressed in the same widget composition space.
#[must_use]
fn intersect_widget_clips(item: Option<WidgetClipRect>, slot: Option<WidgetClipRect>) -> WidgetClipIntersection {
    match (item, slot) {
        (None, None) => WidgetClipIntersection::Unbounded,
        (Some(rect), None) | (None, Some(rect)) => {
            if rect.is_valid() {
                WidgetClipIntersection::Finite { rect }
            } else {
                WidgetClipIntersection::Empty
            }
        }
        (Some(item), Some(slot)) => {
            if !item.is_valid() || !slot.is_valid() {
                return WidgetClipIntersection::Empty;
            }
            let x = item.x.max(slot.x);
            let y = item.y.max(slot.y);
            let right = (item.x + item.width).min(slot.x + slot.width);
            let bottom = (item.y + item.height).min(slot.y + slot.height);
            let rect = WidgetClipRect { x, y, width: right - x, height: bottom - y };
            if rect.is_valid() {
                WidgetClipIntersection::Finite { rect }
            } else {
                WidgetClipIntersection::Empty
            }
        }
    }
}

/// One draw in a [`WidgetDrawList`], in the widget's own local
/// coordinates (offset by the parent at composite time). A widget emits a
/// heterogeneous run of these in authored order — the single-list shape
/// preserves per-item solid/textured/text interleave, which a split-vector
/// quads-then-texts split would foreclose. Each variant's optional `clip` is
/// a [`WidgetClipRect`] in the same local space as its geometry. Not a kind on
/// its own; only addressable inside [`WidgetDrawList::items`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum WidgetDrawItem {
    /// A flat-colored rectangle. `(x, y)` is the top-left corner and
    /// `(width, height)` the size, in the widget's local pixels; `color`
    /// is a linear RGBA value.
    Quad { x: f32, y: f32, width: f32, height: f32, color: Rgba, clip: Option<WidgetClipRect> },
    /// A textured rectangle. `(x, y)` is the top-left corner and
    /// `(width, height)` the size in the widget's local pixels;
    /// `(u0, v0)`–`(u1, v1)` selects the texture sub-rectangle;
    /// `texture_id` is a non-owning session id from `CreateTexture`; and
    /// `tint` is a linear RGBA multiplier.
    TexturedQuad {
        texture_id: u32,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        u0: f32,
        v0: f32,
        u1: f32,
        v1: f32,
        tint: Rgba,
        clip: Option<WidgetClipRect>,
    },
    /// A glyph run. `(x, y)` is the baseline origin in local pixels;
    /// `font_id` names a session-scoped font loaded through `aether.text`;
    /// `color` is a linear RGBA multiplier over glyph coverage.
    Text { x: f32, y: f32, font_id: u32, text: String, size_pixels: f32, color: Rgba, clip: Option<WidgetClipRect> },
}

impl WidgetDrawItem {
    /// This item translated by `by` — the offset the parent applies when
    /// it files a child's list into that child's assigned slot. Composing
    /// offsets down the reply chain (each interior node offsets its own
    /// children before replying up) accumulates a node's absolute
    /// position, so the root emits screen-correct geometry.
    #[must_use]
    pub fn offset(&self, by: Vec2) -> Self {
        match self {
            Self::Quad { x, y, width, height, color, clip } => Self::Quad {
                x: x + by.x,
                y: y + by.y,
                width: *width,
                height: *height,
                color: *color,
                clip: clip.map(|rect| rect.offset(by)),
            },
            Self::TexturedQuad { texture_id, x, y, width, height, u0, v0, u1, v1, tint, clip } => Self::TexturedQuad {
                texture_id: *texture_id,
                x: x + by.x,
                y: y + by.y,
                width: *width,
                height: *height,
                u0: *u0,
                v0: *v0,
                u1: *u1,
                v1: *v1,
                tint: *tint,
                clip: clip.map(|rect| rect.offset(by)),
            },
            Self::Text { x, y, font_id, text, size_pixels, color, clip } => Self::Text {
                x: x + by.x,
                y: y + by.y,
                font_id: *font_id,
                text: text.clone(),
                size_pixels: *size_pixels,
                color: *color,
                clip: clip.map(|rect| rect.offset(by)),
            },
        }
    }

    /// Intersect this item's clip with a slot clip in the same coordinate
    /// space. Empty or invalid results omit the item.
    #[must_use]
    pub(super) fn intersect_clip(&self, slot: Option<WidgetClipRect>) -> Option<Self> {
        let own = match self {
            Self::Quad { clip, .. } | Self::TexturedQuad { clip, .. } | Self::Text { clip, .. } => *clip,
        };
        let clip = match intersect_widget_clips(own, slot) {
            WidgetClipIntersection::Unbounded => None,
            WidgetClipIntersection::Finite { rect } => Some(rect),
            WidgetClipIntersection::Empty => return None,
        };
        let mut item = self.clone();
        match &mut item {
            Self::Quad { clip: own, .. } | Self::TexturedQuad { clip: own, .. } | Self::Text { clip: own, .. } => {
                *own = clip;
            }
        }
        Some(item)
    }

    /// The rectangle this item actually paints — its geometry narrowed by its
    /// own clip — or `None` for text, which casts no hole, and for a fill its
    /// clip erases. This is the hole a fill punches in the glyph runs authored
    /// before it: reading the geometry alone would let a row scrolled out of a
    /// viewport cut text the viewport clip already spared it from.
    #[must_use]
    pub(super) fn covered_rect(&self) -> Option<WidgetClipRect> {
        let (rect, clip) = match self {
            Self::Quad { x, y, width, height, clip, .. } | Self::TexturedQuad { x, y, width, height, clip, .. } => {
                (WidgetClipRect { x: *x, y: *y, width: *width, height: *height }, *clip)
            }
            Self::Text { .. } => return None,
        };
        match intersect_widget_clips(Some(rect), clip) {
            WidgetClipIntersection::Finite { rect } => Some(rect),
            WidgetClipIntersection::Unbounded | WidgetClipIntersection::Empty => None,
        }
    }

    /// This item's effective clip, rejecting an invalid explicit rectangle.
    #[must_use]
    pub(super) fn valid_clip(&self) -> WidgetClipIntersection {
        let clip = match self {
            Self::Quad { clip, .. } | Self::TexturedQuad { clip, .. } | Self::Text { clip, .. } => *clip,
        };
        intersect_widget_clips(clip, None)
    }
}

/// `aether.kit.widget.draw_list` — a widget's reply to a [`Collect`], the
/// one channel that flows up the tree. `items` are the widget's draws in
/// authored order, in its local coordinates; `intrinsic` is its measured
/// content size (`[width, height]`) when the parent needs it to position a
/// content-sized slot — a cached event, never a pull — and `None` when the
/// widget's size is externally fixed.
///
/// `overlay` is the widget's draws that must land **over everything else the
/// cluster draws this frame** — an open dropdown's list, a popover — in the
/// same local coordinates as `items`. A compositing parent offsets a child's
/// overlay by the child's slot origin like any draw, but never intersects it
/// with the slot clip (the whole point is to escape the slot), and carries it
/// up as its own `overlay` so the root emits every overlay after every
/// ordinary item. Within that lane it lands at its own slot's position, so a
/// control that must stand over its own siblings is registered after them.
/// Empty for the ordinary widget.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[kind(name = "aether.kit.widget.draw_list")]
pub struct WidgetDrawList {
    /// The measured content size, per axis. A **non-finite** component means
    /// the widget asks for nothing on that axis — a filled tab strip takes
    /// whatever width it is given and reports only its row height — and a
    /// reader of this field takes a component only when it is finite and
    /// non-negative, exactly as the panel does when it sizes a slot.
    pub intrinsic: Option<[f32; 2]>,
    /// The whole of what the widget holds, in logical pixels down, when that
    /// is taller than the viewport it draws in — a virtual list's whole item
    /// vector rather than the window of it on screen. `None` for a widget
    /// whose `intrinsic` height already is everything it holds, which is most
    /// of them.
    ///
    /// It is here because only the widget can answer it: the wrapping is the
    /// widget's, because the font metrics are. A host that draws a container
    /// around a scrolling widget sizes it to this — a four-row table gets a
    /// four-row plate rather than a tall empty box — instead of mirroring the
    /// widget's own row arithmetic and drifting from it (the studio's gap
    /// 41).
    #[serde(default)]
    pub content_height: Option<f32>,
    pub items: Vec<WidgetDrawItem>,
    pub overlay: Vec<WidgetDrawItem>,
}

/// The fixed size of a scroll viewport or its authored content, in logical
/// window pixels. Named fields keep both axes and their units explicit at the
/// schema boundary.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Default)]
pub struct ScrollExtent {
    pub width_pixels: f32,
    pub height_pixels: f32,
}

/// A scroll container's retained content offset, in logical pixels from the
/// content origin. Both axes are clamped independently to the authored
/// [`ScrollExtent`] bounds.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Default)]
pub struct ScrollOffset {
    pub x_pixels: f32,
    pub y_pixels: f32,
}

/// A requested content-space movement in logical pixels. A chassis wheel is
/// converted to this sign convention exactly once: `x_pixels = -delta_x` and
/// `y_pixels = -delta_y`.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Default)]
pub struct ScrollDelta {
    pub x_pixels: f32,
    pub y_pixels: f32,
}

/// `aether.kit.widget.scroll.residual` — the already-converted part of a
/// scroll request that one container could not consume. A parent applies
/// these fields directly; it never reverses their sign a second time.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Default)]
#[kind(name = "aether.kit.widget.scroll.residual")]
pub struct ScrollResidual {
    pub x_pixels: f32,
    pub y_pixels: f32,
}

/// `aether.kit.widget.scroll.outcome` — one container's exact retained offset,
/// consumed movement, and unconsumed residual after a request. `container`
/// identifies the state-owning actor even when an ancestor transparently
/// relays this event to the root.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[kind(name = "aether.kit.widget.scroll.outcome")]
pub struct ScrollOutcome {
    pub container: MailboxId,
    pub offset: ScrollOffset,
    pub consumed: ScrollDelta,
    pub residual: ScrollResidual,
}

/// `aether.kit.widget.scroll.config` — the fixed layout contract for one
/// stateful scroll actor. `viewport_extent` is what the parent places;
/// `content_extent` is the sole clamp authority; `initial_offset` is clamped
/// at init; and `content` is one opaque root spawned through the closed
/// [`WidgetKind`] dispatcher.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[kind(name = "aether.kit.widget.scroll.config")]
pub struct ScrollConfig {
    pub viewport_extent: ScrollExtent,
    pub content_extent: ScrollExtent,
    pub initial_offset: ScrollOffset,
    pub content: WidgetChildSpec,
}

impl Default for ScrollConfig {
    fn default() -> Self {
        Self {
            viewport_extent: ScrollExtent::default(),
            content_extent: ScrollExtent::default(),
            initial_offset: ScrollOffset::default(),
            content: WidgetChildSpec {
                subname: String::from("content"),
                kind: WidgetKind::Composite,
                origin: [0.0, 0.0],
                clip: None,
                config: Vec::new(),
            },
        }
    }
}

/// The kind of actor a [`WidgetChildSpec`] spawns, and the concrete config
/// type its opaque [`WidgetChildSpec::config`] bytes decode as. It is the
/// one tag that lets a single spec type serve both the homogeneous
/// compositing [`WidgetConfig`] tree (every child a `Composite`) and the
/// heterogeneous reference panel (a leaf per widget type). The spawnable set
/// is closed and kit-owned — every variant maps to a compile-time
/// `spawn_inline_child::<P, A>` call — so the dispatch match is exhaustive and
/// an unknown widget is a compile error, not a runtime failure.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WidgetKind {
    /// A nested compositing subtree — the child's `config` decodes as a
    /// [`WidgetConfig`] and spawns another compositing node. The default, so
    /// a spec written for the compositing tree needs no tag.
    #[default]
    Composite,
    /// A static label — `config` decodes as [`LabelConfig`]. Not focusable.
    Label,
    /// A value slider — `config` decodes as [`SliderConfig`].
    Slider,
    /// A radio group — `config` decodes as [`RadioConfig`]; its row count is
    /// its option count.
    Radio,
    /// A single-line text field — `config` decodes as [`TextFieldConfig`].
    TextField,
    /// A push button — `config` decodes as [`ButtonConfig`].
    Button,
    /// A behavior-script host wrapping one stock widget (ADR-0137, issue
    /// 2687) — `config` decodes as [`BehaviorHostSpec`] (the wrapped widget's
    /// kind + config plus the script), and the panel spawns the host by tag
    /// (`aether-behavior`'s `BehaviorHost`) instead of a widget. Only spawnable
    /// under the kit's `behavior` feature; without it the slot is skipped.
    BehaviorHost,
    /// A static image — `config` decodes as [`ImageConfig`]. Not focusable.
    /// Appended to preserve the established wire discriminants above.
    Image,
    /// A multiline text area — `config` decodes as [`TextAreaConfig`]. Appended
    /// after Image so adding it does not renumber that landed discriminant or
    /// any earlier wire discriminant.
    TextArea,
    /// A stateful clipped viewport — `config` decodes as [`ScrollConfig`].
    /// The actor owns its offset and recursively routes wheel residuals.
    /// Appended to preserve every established wire discriminant above.
    Scroll,
    /// A fixed-row virtual list — `config` decodes as [`VirtualListConfig`].
    /// Appended after `Scroll` to preserve every established wire discriminant
    /// above.
    VirtualList,
    /// A boolean switch — `config` decodes as [`ToggleConfig`].
    /// Appended to preserve every established wire discriminant above.
    Toggle,
    /// A horizontal exclusive choice — `config` decodes as
    /// [`SegmentedConfig`]. Appended to preserve established discriminants.
    Segmented,
    /// A typed and steppable bounded number — `config` decodes as
    /// [`NumericConfig`]. Appended to preserve established discriminants.
    Numeric,
    /// One current choice with the alternatives in a list that opens on
    /// demand — `config` decodes as [`DropdownConfig`]. Appended to
    /// preserve established discriminants.
    Dropdown,
    /// A single row of tabs selecting one of several parallel content
    /// sets — `config` decodes as [`TabStripConfig`]. Appended to
    /// preserve established discriminants.
    TabStrip,
    /// A row of application menus — `config` decodes as [`MenuBarConfig`].
    /// Appended to preserve established discriminants.
    MenuBar,
}

impl WidgetKind {
    /// This stock widget's actor type tag — `hash(NAMESPACE)` of the widget
    /// actor `self` spawns, the same value `ActorTypeTag::of::<W>().0` would
    /// produce for the concrete actor type. `None` for container/host variants
    /// (`Composite`, `Scroll`, `BehaviorHost`), which are not stock leaves a
    /// behavior host can wrap. The trunk-reachable producer for
    /// `aether_behavior::host::ChildSpec::type_tag` when composing a
    /// `HostConfig` directly, outside the kit's `WidgetKind::BehaviorHost`
    /// spawn arm — the concrete widget actor types are `runtime`-gated and
    /// not re-exported, so this hashes the actor namespace literals
    /// directly rather than naming the types.
    #[must_use]
    // Id-constant definition for stock widget types whose actors are
    // `runtime`-gated; the runtime tripwire binds it to
    // `ActorTypeTag::of::<W>()`.
    #[allow(clippy::disallowed_methods)]
    pub const fn type_tag(self) -> Option<u64> {
        let tag = match self {
            Self::Label => aether_data::mailbox_id_from_name("aether.kit.widget.label").0,
            Self::Image => aether_data::mailbox_id_from_name("aether.kit.widget.image").0,
            Self::Slider => aether_data::mailbox_id_from_name("aether.kit.widget.slider").0,
            Self::Radio => aether_data::mailbox_id_from_name("aether.kit.widget.radio").0,
            Self::TextField => aether_data::mailbox_id_from_name("aether.kit.widget.text_field").0,
            Self::TextArea => aether_data::mailbox_id_from_name("aether.kit.widget.text_area").0,
            Self::Button => aether_data::mailbox_id_from_name("aether.kit.widget.button").0,
            Self::VirtualList => aether_data::mailbox_id_from_name("aether.kit.widget.virtual_list").0,
            Self::Toggle => aether_data::mailbox_id_from_name("aether.kit.widget.toggle").0,
            Self::Segmented => aether_data::mailbox_id_from_name("aether.kit.widget.segmented").0,
            Self::Numeric => aether_data::mailbox_id_from_name("aether.kit.widget.numeric").0,
            Self::Dropdown => aether_data::mailbox_id_from_name("aether.kit.widget.dropdown").0,
            Self::TabStrip => aether_data::mailbox_id_from_name("aether.kit.widget.tab_strip").0,
            Self::MenuBar => aether_data::mailbox_id_from_name("aether.kit.widget.menu_bar").0,
            Self::Composite | Self::Scroll | Self::BehaviorHost => return None,
        };
        Some(tag)
    }
}

/// Where a wrapped host's script comes from — the kit-local mirror of
/// `aether_behavior`'s `ScriptSource`, carried in a [`BehaviorHostSpec`] so the
/// trunk (always compiled) names no `aether-behavior` type. The `behavior`
/// arm maps it across.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub enum ScriptRef {
    /// No script — the host runs wrapper-transparent until one is loaded.
    #[default]
    None,
    /// The script's wasm bytes inline.
    Inline(Vec<u8>),
    /// Fetch the script from an `aether.fs` namespace at boot.
    FsRef {
        /// The `aether.fs` namespace prefix (`"save"`, `"assets"`, `"config"`).
        namespace: String,
        /// The path within the namespace.
        path: String,
    },
}

/// `aether.kit.behavior_host_spec` — the config bytes a [`WidgetChildSpec`]
/// carries for a [`WidgetKind::BehaviorHost`] slot (issue 2687). It names the
/// wrapped widget's kind + its own pre-encoded config (the same opaque bytes a
/// direct widget slot would carry) plus the script and its fuel knobs. Not
/// recursive — `wrapped` is a plain [`WidgetKind`] discriminant, and
/// `WidgetKind::BehaviorHost` is a unit variant that references nothing back,
/// so the `Schema` derive stays acyclic.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.behavior_host_spec")]
pub struct BehaviorHostSpec {
    /// The stock widget the host interposes on.
    pub wrapped: WidgetKind,
    /// The wrapped widget's own config, pre-encoded (as a direct slot's
    /// `config` would be).
    #[serde(with = "aether_data::bytes")]
    pub wrapped_config: Vec<u8>,
    /// The behavior script.
    pub script: ScriptRef,
    /// Fuel budget per filter call (`0` ⇒ the host default).
    pub fuel_per_call: u64,
    /// Consecutive-trap disable threshold (`0` ⇒ the host default).
    pub disable_after_traps: u32,
}

/// One child's placement in a compositing node's layout table. `subname`
/// is the inline-child address segment the parent spawns and collects it
/// under; `kind` selects which actor the parent spawns and how `config`
/// decodes; `origin` is the local-pixel offset the parent applies to the
/// child's every draw. `config` is the child's own concrete config (a
/// [`WidgetConfig`] for a `Composite`, a [`SliderConfig`] for a `Slider`,
/// …), pre-encoded to bytes — carrying it opaquely (rather than a nested
/// config by value) is what lets a tree nest without forming a
/// self-referential schema, which a recursive `const SCHEMA` cannot
/// express. A layout-owning parent (the reference panel) derives each
/// child's `origin` from its stack order and ignores this field. `clip` is an
/// optional parent-local bound over the child's whole subtree; the reference
/// panel derives its slot clip from the assigned [`WidgetFrame`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WidgetChildSpec {
    pub subname: String,
    pub kind: WidgetKind,
    pub origin: [f32; 2],
    pub clip: Option<WidgetClipRect>,
    #[serde(with = "aether_data::bytes")]
    pub config: Vec<u8>,
}

/// `aether.kit.widget.config` — the tree a compositing widget is
/// instantiated with, at load (the root) or at `spawn_inline_child` (every
/// interior and leaf). `root` marks the node that drives the frame and
/// emits to `aether.render` / `aether.text`; a non-root node instead
/// replies its composite up to its parent. `chrome` is the node's own
/// draws in local coordinates; `intrinsic` is the size it reports up;
/// `children` is its ordered layout table, each carrying its own
/// pre-encoded config.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.config")]
pub struct WidgetConfig {
    pub root: bool,
    pub chrome: Vec<WidgetDrawItem>,
    pub intrinsic: Option<[f32; 2]>,
    pub children: Vec<WidgetChildSpec>,
}

/// Validation feedback attached to a stock widget's external control state.
/// The message is consumer-facing context; the stock widgets render only the
/// named warning/error role so the wire value does not dictate presentation.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub enum WidgetValidation {
    #[default]
    Valid,
    Warning {
        message: String,
    },
    Error {
        message: String,
    },
}

/// Shared external availability and validation state for every stock widget.
/// This is deliberately separate from frame-local hover, press, drag, and
/// focus state, which widgets derive from root-forwarded interaction mail.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WidgetControlState {
    pub visible: bool,
    pub enabled: bool,
    pub read_only: bool,
    pub validation: WidgetValidation,
}

impl Default for WidgetControlState {
    fn default() -> Self {
        Self { visible: true, enabled: true, read_only: false, validation: WidgetValidation::Valid }
    }
}

/// `aether.kit.widget.set_state` — replace a stock widget's external state
/// without resetting its authored value or other configuration.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.set_state")]
pub struct SetWidgetState {
    pub state: WidgetControlState,
}

/// `aether.kit.widget.state_changed` — a source-attributed events-up reply
/// emitted only when a re-sent config or [`SetWidgetState`] changes external
/// state. The panel uses it to keep routing availability synchronized.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.state_changed")]
pub struct WidgetStateChanged {
    pub state: WidgetControlState,
}

/// The widget set's config/style/layout/state/interaction data-down lanes and
/// value/state events-up lanes. Events carry **no widget identity field**: the
/// root attributes replies against the `MailboxId` recorded at spawn
/// (`ctx.source_mailbox`), so identity stays the inline subname. Layout, focus,
/// hover, and external state flow down like compositing `Collect`; the root
/// owns routing and the widget reacts.
///
/// Each per-widget `Config` embeds a [`Theme`] (the theme-first sequencing:
/// there is no separate widget-style kind — a widget's whole look is its
/// theme), and is both the `spawn_inline_child` init config and a re-sendable
/// data-down mail: sending a widget its `Config` kind again reconfigures it in
/// place.
/// `aether.kit.widget.slider.config` — a horizontal value slider over
/// `min..=max`, snapped to `step`, starting at `initial`. The consumer maps
/// the reported `f32` onto its own domain (a `u8` intensity, a preset index).
/// A `step` of `0` (or less) leaves the value continuous.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.slider.config")]
pub struct SliderConfig {
    pub min: f32,
    pub max: f32,
    pub step: f32,
    pub initial: f32,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

impl Default for SliderConfig {
    fn default() -> Self {
        Self {
            min: 0.0,
            max: 1.0,
            step: 0.0,
            initial: 0.0,
            theme: Theme::default(),
            state: WidgetControlState::default(),
        }
    }
}

/// `aether.kit.widget.text_field.config` — a single-line editable string
/// starting at `initial`, capped at `max_chars` characters (`0` = no cap).
/// The field keeps its caret and active selection on UTF-8 character boundaries
/// and places both from resolved font metrics once the font settles.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.text_field.config")]
pub struct TextFieldConfig {
    pub initial: String,
    pub max_chars: u32,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// `aether.kit.widget.text_area.config` — a multiline editable string with a
/// fixed whole-line viewport. `rows` is the number of visible rows (`0` uses
/// one row); `max_chars` counts Unicode scalar values (`0` = no cap).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.text_area.config")]
pub struct TextAreaConfig {
    pub initial: String,
    pub max_chars: u32,
    pub rows: u32,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// `aether.kit.widget.radio.config` — a vertical list of mutually-exclusive
/// `options`, one selected at a time, starting at `initial_index` (clamped
/// into range at init). Each option draws as one theme row.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.radio.config")]
pub struct RadioConfig {
    pub options: Vec<String>,
    pub initial_index: u32,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// One word of a run, and the ink it is written in.
///
/// A [`VirtualListRow`]'s trailing column is a `Vec<InkedSpan>` because the
/// words in it are not one thing said once: `Spell Fire Duration` is three
/// tags, and a tag wears the ink of what it names. Each span draws in its own
/// [`TextInk`]; the spans are laid out on one line with the theme's word gap
/// between them, and the run right-aligns as a whole.
///
/// A span that is only words is written as one (`InkedSpan: From<String> +
/// From<&str>`), so the plain amount stays `vec!["21/20".into()]` and nothing
/// about a one-ink column got harder. Schema-only; nested in
/// [`VirtualListRow`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct InkedSpan {
    pub text: String,
    /// The ink this span is written in. [`TextInk::Inherited`] — the default —
    /// follows the run it sits in, which is the one-ink column every list drew
    /// before spans existed.
    #[serde(default)]
    pub ink: TextInk,
}

impl InkedSpan {
    /// A span of `text` written in `ink`.
    #[must_use]
    pub fn new(text: impl Into<String>, ink: TextInk) -> Self {
        Self { text: text.into(), ink }
    }

    /// The same span written in `ink`.
    #[must_use]
    pub fn with_ink(mut self, ink: TextInk) -> Self {
        self.ink = ink;
        self
    }
}

impl From<String> for InkedSpan {
    fn from(text: String) -> Self {
        Self { text, ink: TextInk::default() }
    }
}

impl From<&str> for InkedSpan {
    fn from(text: &str) -> Self {
        Self::from(String::from(text))
    }
}

/// One verb bound to a single row of a [`VirtualListConfig`] — the `×` that
/// unbinds *this* skill, the `Change gem` that re-picks *this* one.
///
/// The list draws it as a real button at the row's right edge, with the same
/// [`ButtonEmphasis`] / [`ButtonTone`] ladder, the same measured-label-plus-two-pads
/// width, the same elision and the same hover / pressed answer a
/// [`ButtonConfig`] draws with — it is the kit's button face, drawn inside a row
/// the list owns rather than in a slot a layout gave it. A press on one reports
/// [`VirtualListAction`] and leaves the selection alone; a press anywhere else
/// on the row selects as it always did.
///
/// Rank a row verb **down**. A column of rows each carrying a filled accent
/// plate has spent the primary-action token once per row
/// (`designing-a-screen.md` §6), which is the same defect as a screen of five
/// yellow buttons; [`ButtonEmphasis::Text`] or `Outlined` is the row verb's
/// rank, and `Danger` is what says the `×` throws work away. Schema-only;
/// nested in [`VirtualListRow`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct RowAction {
    pub label: String,
    /// How loudly this row's verb asks to be pressed.
    #[serde(default)]
    pub emphasis: ButtonEmphasis,
    /// What it does to the reader's work — `Danger` for the one that unbinds.
    #[serde(default)]
    pub tone: ButtonTone,
}

impl RowAction {
    /// A quiet neutral verb: label only, no plate. The rank a row verb takes
    /// unless it destroys something.
    #[must_use]
    pub fn text(label: impl Into<String>) -> Self {
        Self { label: label.into(), emphasis: ButtonEmphasis::Text, tone: ButtonTone::Neutral }
    }

    /// A verb that throws this row's work away — the `×` on a bound skill.
    #[must_use]
    pub fn danger(label: impl Into<String>) -> Self {
        Self { label: label.into(), emphasis: ButtonEmphasis::Text, tone: ButtonTone::Danger }
    }
}

/// One row of a [`VirtualListConfig`]: what it says, what it says at its right
/// edge, which step of the type scale it is set at, and the verbs bound to it.
///
/// `trailing` is the row's **second column** — a version, a count, a price, a
/// key, a run of tags — set in its own right-aligned column at the row's right
/// edge. The column is as wide as the widest trailing run among the *visible*
/// rows, so the numbers line up and a reader comparing two of them reads down
/// one edge instead of hunting through two sentences. The leading `text` elides
/// into whatever is left; the trailing run gives way only by dropping whole
/// [`InkedSpan`]s off its end, never by cutting one, because a truncated amount
/// is worse than no amount.
///
/// It is a *run of spans* rather than one string because a tag wears the ink of
/// what it names: `Fire` warm, `Cold` cool, `Chaos` violet, all on one line.
/// An empty vector is the row that is only its leading text, and one span is
/// the plain amount (`vec!["21/20".into()]`).
///
/// `role` sets the row's type step, so a list can carry a name at
/// [`TextRole::Body`] and a detail at [`TextRole::Caption`] — which draws in
/// the muted ink — without the host writing its own rows. The default is
/// `Body`, the size every list row was set at before roles reached the list.
///
/// `ink` colours the **leading run only** — the row's name. It is the answer
/// to "this one is rare and that one is not" without a `(unique)` suffix after
/// the name or a plate behind the whole row, and it survives the row being
/// selected or pointed at, because a tier does not stop applying when a reader
/// touches it. Each span of the trailing run carries its own ink the same way —
/// see [`InkedSpan`]. A column of *amounts* still wants one ink down its edge,
/// so leave those spans [`TextInk::Inherited`]; the colours are for a column of
/// tags, where the ink is what the word means rather than decoration on a
/// number.
///
/// `actions` are the verbs bound to this row — see [`RowAction`]. They stand
/// as buttons at the row's right end, in the order written, and the leading
/// text elides against the space they leave exactly as it elides against the
/// scroll bar's gutter. An empty vector is the plain row, laid out as it
/// always was.
///
/// # A row that is a table entry
///
/// `note`, `indent`, `space_before` and `rule_above` are the four fields a
/// *table* wants and a list of choices does not. A statistic and the sentence
/// that qualifies it are one entry, not two rows; a derived figure hangs off
/// the fact above it; a block of entries opens with air and a hairline. Set
/// none of them and the list is laid out exactly as it always was, at one
/// pitch — a row's height starts following its own content only once some row
/// of the vector asks for one of the four.
///
/// `note` is the row's **second line**: set in [`TextRole::Caption`] and the
/// muted ink under the leading run, word-wrapped to the row's own text budget,
/// and capped at three lines with the last elided. A note is prose about the
/// row above it, so it does not read as a row of its own — which a note pushed
/// into the vector as a second [`VirtualListRow`] always did, right down to
/// reading as a statistic whose value failed to draw.
///
/// `indent` moves the leading run and the note right by that many spacing
/// units, and takes the same width off what they are elided and wrapped
/// against. The trailing column and the verbs do **not** move: a value
/// right-aligns on one edge whatever rung its name sits on, which is what lets
/// a reader compare two figures down a column. Indent in the field rather than
/// in the string — padding a name with spaces puts the indent in the text, and
/// a proportional face's space advance is not the spacing unit.
///
/// `space_before` is clear space above the row in spacing units, drawn as
/// **ground** rather than as a taller plate: a group gap, not a fatter row.
/// The first row's space is honoured too. It replaces the blank row a table
/// otherwise pushes in to open a block — a blank band on a raised plate is a
/// hole rather than a gap, and it reads worst of all when the list is scrolled
/// so that it lands at the top.
///
/// `rule_above` draws a hairline in the theme's `outline` across the row's
/// text budget at the **top of that space** — the rule first, then the gap,
/// then the row — so a block boundary reads as a line with air under it. It is
/// per row, where [`VirtualListConfig::ruled`] is per list: `ruled` divides
/// every pair of rows, this opens one block.
///
/// A row is written as a plain string wherever it is only words
/// (`VirtualListRow: From<String> + From<&str>`). Schema-only; nested in
/// [`VirtualListConfig`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct VirtualListRow {
    pub text: String,
    /// The spans set in the row's right-hand column, laid out on one line in
    /// the order written. Empty for a row that is only its leading text.
    #[serde(default)]
    pub trailing: Vec<InkedSpan>,
    /// The type step both runs of this row are set at.
    #[serde(default)]
    pub role: TextRole,
    /// The ink the leading run is written in. [`TextInk::Inherited`] — the
    /// default — is the row ink every list drew before the field existed.
    #[serde(default)]
    pub ink: TextInk,
    /// The verbs bound to this row, drawn as buttons at its right end.
    #[serde(default)]
    pub actions: Vec<RowAction>,
    /// The row's second line — a caption-role, muted, wrapped sentence under
    /// the leading run. `None` is the one-line row.
    #[serde(default)]
    pub note: Option<String>,
    /// How far right the leading run and the note start, in spacing units.
    /// The trailing column and the verbs stay where they are.
    #[serde(default)]
    pub indent: u8,
    /// Clear space above the row in spacing units, drawn as ground.
    #[serde(default)]
    pub space_before: u8,
    /// Whether a hairline stands at the top of that space.
    #[serde(default)]
    pub rule_above: bool,
}

impl VirtualListRow {
    /// The same row carrying `actions` — the builder form, so a row written
    /// from a plain string still reads as one line.
    #[must_use]
    pub fn with_actions(mut self, actions: Vec<RowAction>) -> Self {
        self.actions = actions;
        self
    }

    /// The same row carrying `note` on a second line under its name.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// The same row started `indent` spacing units in — a derived figure
    /// hanging off the fact above it.
    #[must_use]
    pub fn with_indent(mut self, indent: u8) -> Self {
        self.indent = indent;
        self
    }

    /// The same row with `space_before` spacing units of ground above it.
    #[must_use]
    pub fn with_space_before(mut self, space_before: u8) -> Self {
        self.space_before = space_before;
        self
    }

    /// The same row opening a block: a hairline at the top of its space.
    #[must_use]
    pub fn with_rule_above(mut self) -> Self {
        self.rule_above = true;
        self
    }

    /// The same row with `trailing` in its right-hand column. The plain amount
    /// is one span from a string — `row.with_trailing(vec!["21/20".into()])` —
    /// and a run of tags is one span each.
    #[must_use]
    pub fn with_trailing(mut self, trailing: Vec<InkedSpan>) -> Self {
        self.trailing = trailing;
        self
    }

    /// The same row with its name written in `ink`.
    #[must_use]
    pub fn with_ink(mut self, ink: TextInk) -> Self {
        self.ink = ink;
        self
    }
}

impl From<String> for VirtualListRow {
    fn from(text: String) -> Self {
        Self { text, ..Self::default() }
    }
}

impl From<&str> for VirtualListRow {
    fn from(text: &str) -> Self {
        Self::from(String::from(text))
    }
}

/// `aether.kit.widget.virtual_list.config` — a fixed-row viewport over a
/// potentially large item vector. The panel fixes the viewport height from
/// `visible_row_count`; the actor realizes only that bounded row window.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.virtual_list.config")]
pub struct VirtualListConfig {
    pub items: Vec<VirtualListRow>,
    /// The row selected at boot, or `None` for no selection — a list whose
    /// model holds no current item shows none, rather than lighting its
    /// first row as if it did.
    pub initial_selected_index: Option<u32>,
    pub visible_row_count: u32,
    /// The one caption line drawn in place of rows when `items` is empty
    /// (`"No saved builds"`), in the caption role and muted ink. An empty
    /// string draws nothing.
    #[serde(default)]
    pub empty_text: String,
    /// Whether a one-pixel `theme.outline` rule stands between rows. `false`
    /// (the default) is the plain list: rows are told apart by their own fills
    /// and the selection. `true` is for a list whose rows are *entries* rather
    /// than choices — a row of two columns, or a row long enough that the eye
    /// loses which trailing belongs to which name. The rule falls between
    /// rows only: `n` realized rows get `n - 1` rules, so the list is never
    /// underlined at its own bottom edge.
    ///
    /// A table wants [`VirtualListRow::rule_above`] instead — one rule where a
    /// block opens, rather than one between every pair.
    #[serde(default)]
    pub ruled: bool,
    /// How much clear space stands between the rows and the scroll bar's
    /// track, in spacing units — the **gutter** the bar keeps off the values
    /// beside it.
    ///
    /// [`Self::SCROLL_BAR_GAP_UNITS`] by default, which is two: a control
    /// inside a plate sits at least two spacing units from its edge
    /// (`designing-a-screen.md` §6), and from the rows' side the rail is that
    /// edge. One unit was the whole gutter until round 15 and the owner read
    /// it as touching the values twice over — round-14 note 5, *"More left
    /// padding on the scroll bar in build tab"*, and round-17 note 7, *"The
    /// scrollbar is still too close to content to the left side."*
    ///
    /// A host that wants more sets more. The gutter is taken off the rows'
    /// own width, so the leading run elides against what is left of it rather
    /// than running under the bar and being cut by it — and off nothing at
    /// all when the bar stands in the host's own strip
    /// ([`Self::host_scroll_strip`]), where it is the clear space between the
    /// frame's right edge and the track beyond it.
    #[serde(default = "VirtualListConfig::scroll_bar_gap_default")]
    pub scroll_bar_gap_units: u8,
    /// Whether the scroll bar stands in a strip the **host** reserves beside
    /// the list rather than in a gutter cut out of the list's own frame.
    ///
    /// `false` (the default) is the bar every list has drawn: the track down
    /// the frame's right end, the rows laid inside what is left. `true` draws
    /// the track in the strip just past the frame's right edge — the way a
    /// pane's rail is drawn past the body it scrolls — and takes **nothing**
    /// off the rows, so a value's right edge does not move when the vector
    /// starts to overflow. The owner's round-16 note 3: *"I feel like the
    /// scrollbar should EXTEND the panel slightly to exist and be adjacent."*
    ///
    /// A host that sets it owes the widget that column:
    /// [`Self::scroll_strip_width`] is how wide, and the slot's clip has to
    /// reach across it or the track is clipped away with everything else
    /// outside the frame.
    #[serde(default)]
    pub host_scroll_strip: bool,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

impl VirtualListConfig {
    /// The gutter a list keeps between its rows and its scroll bar unless the
    /// host says otherwise: **two** spacing units, the least a control stands
    /// off a plate's edge in the method's own spacing ladder.
    pub const SCROLL_BAR_GAP_UNITS: u8 = 2;

    /// How wide the scroll bar's track is, in spacing units — two, which is
    /// eight pixels on the four-pixel grid: wide enough to grab with a
    /// pointer, narrow enough that it reads as an edge of the list rather
    /// than a column in it.
    pub const SCROLL_BAR_TRACK_UNITS: u8 = 2;

    /// The track's width in `theme`'s own metrics — a metric rather than a
    /// measurement, so it scales with a theme scaled for a dense display, and
    /// never thinner than the one pixel it takes to see it.
    #[must_use]
    pub fn scroll_track_width(theme: &Theme) -> f32 {
        theme.space(Self::SCROLL_BAR_TRACK_UNITS).max(Self::MIN_SCROLL_TRACK_PIXELS)
    }

    /// The strip this list wants beside its frame for a host-owned bar: the
    /// gutter and the track, in `theme`'s metrics. `0.0` while the bar is the
    /// list's own, where the same pair comes out of the frame instead.
    ///
    /// This is the number a host reserves the column with — it lays out
    /// before any draw list arrives, and the track's own width is the kit's,
    /// so a host counting it itself would be copying a constant that can
    /// move.
    #[must_use]
    pub fn scroll_strip_width(&self, theme: &Theme) -> f32 {
        if self.host_scroll_strip {
            Self::host_strip_width(self.scroll_bar_gap_units, theme)
        } else {
            0.0
        }
    }

    /// The same column, measured from the gap units alone. A host that lays a
    /// list out in **its** theme rather than the config's — the reference
    /// panel fans its own theme down to every child — reserves the column with
    /// this, so the strip it clips across is the one the list will draw its
    /// track in, and the formula still lives in one place.
    #[must_use]
    pub fn host_strip_width(scroll_bar_gap_units: u8, theme: &Theme) -> f32 {
        theme.space(scroll_bar_gap_units) + Self::scroll_track_width(theme)
    }

    /// The thinnest a track may be drawn, whatever a theme scales its spacing
    /// down to: a bar nobody can see is a bar nobody can grab.
    const MIN_SCROLL_TRACK_PIXELS: f32 = 1.0;

    fn scroll_bar_gap_default() -> u8 {
        Self::SCROLL_BAR_GAP_UNITS
    }
}

impl Default for VirtualListConfig {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            initial_selected_index: None,
            visible_row_count: 0,
            empty_text: String::new(),
            ruled: false,
            scroll_bar_gap_units: Self::SCROLL_BAR_GAP_UNITS,
            host_scroll_strip: false,
            theme: Theme::default(),
            state: WidgetControlState::default(),
        }
    }
}

/// One choice of a [`DropdownConfig`]: what it reads, and the ink it reads in.
///
/// The ink is here for the same reason it is on [`VirtualListRow`] — a name
/// carries its own tier — and it follows the option onto the **closed row**,
/// because the closed row is that option said again in a smaller space. An
/// option is written as a plain string wherever it is only words
/// (`DropdownOption: From<String> + From<&str>`), so a picker of plain names
/// is `options.map(DropdownOption::from)` and nothing else. Schema-only;
/// nested in [`DropdownConfig`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct DropdownOption {
    pub text: String,
    /// The ink this option is written in, open and closed alike.
    /// [`TextInk::Inherited`] — the default — is the primary ink every option
    /// drew before the field existed.
    #[serde(default)]
    pub ink: TextInk,
}

impl DropdownOption {
    /// The same option written in `ink`.
    #[must_use]
    pub fn with_ink(mut self, ink: TextInk) -> Self {
        self.ink = ink;
        self
    }
}

impl From<String> for DropdownOption {
    fn from(text: String) -> Self {
        Self { text, ink: TextInk::default() }
    }
}

impl From<&str> for DropdownOption {
    fn from(text: &str) -> Self {
        Self::from(String::from(text))
    }
}

/// `aether.kit.widget.dropdown.config` — one current choice among
/// `options`, shown closed as the current option's name (or `placeholder`
/// when nothing is selected) with a chevron, and opened by a press into a
/// list of at most `open_row_count` realized rows drawn in the widget's
/// overlay (see [`WidgetDrawList::overlay`]) below the closed row. A press on
/// a row selects it and closes; Escape or a press elsewhere closes without a
/// change. While open the widget holds the root's pointer grab, reported
/// through [`DropdownOpenChanged`]. Use it for a choice whose current value is
/// what matters and whose alternatives are secondary; three or more options.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.dropdown.config")]
pub struct DropdownConfig {
    pub options: Vec<DropdownOption>,
    pub initial_selected_index: Option<u32>,
    /// What the closed row reads when nothing is selected.
    #[serde(default)]
    pub placeholder: String,
    /// Rows the open list realizes at once; a longer option vector scrolls
    /// inside them.
    pub open_row_count: u32,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// `aether.kit.widget.tab_strip.config` — one horizontal row of `labels`,
/// each sized to its text plus padding, with the selected tab marked by the
/// selection role and an underline. A press or a focused Left/Right selects.
/// Tabs are for parallel content sets viewed one at a time; keep labels to a
/// word or two.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.tab_strip.config")]
pub struct TabStripConfig {
    pub labels: Vec<String>,
    pub initial_index: u32,
    /// Which of the two tab shapes the strip draws.
    /// [`TabStripStyle::Chips`] — the content-sized row every strip drew
    /// before the field existed — unless a host asks for the filled row.
    #[serde(default)]
    pub style: TabStripStyle,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// The two shapes a row of tabs takes.
///
/// The owner's round-8 note 14: "the tab buttons are good but they don't feel
/// like typical tabs … like they aren't small buttons in the section but
/// buttons that take the space and feel more dominant." Both shapes select
/// the same way and report the same [`TabSelected`]; what changes is whether
/// the row is a set of content-sized chips sitting in the section or the
/// section's own top edge.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabStripStyle {
    /// Content-sized tabs on the raised surface, one gap between them, the
    /// current one underlined in the primary ink. What the strip has always
    /// drawn.
    #[default]
    Chips,
    /// Material 3 primary tabs: the tabs divide the strip's whole frame
    /// evenly with no chrome of their own, the current one carries an accent
    /// underline the width of its tab, and a hairline rule in the outline
    /// role runs under the row — so the strip reads as the top edge of the
    /// content it switches rather than as buttons placed on it.
    Filled,
}

/// One entry of a [`Menu`]: its label, the accelerator it advertises at the
/// right edge (`"Cmd+S"`, or empty), whether it can be activated, and whether
/// a divider follows it. Schema-only; nested in [`MenuBarConfig`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct MenuItem {
    pub label: String,
    #[serde(default)]
    pub shortcut: String,
    #[serde(default = "MenuItem::enabled_default")]
    pub enabled: bool,
    #[serde(default)]
    pub separator_after: bool,
}

impl MenuItem {
    const fn enabled_default() -> bool {
        true
    }
}

/// One titled menu of a [`MenuBarConfig`]. Schema-only.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Menu {
    pub title: String,
    pub items: Vec<MenuItem>,
}

/// `aether.kit.widget.menu_bar.config` — one row of menu titles across the
/// top of a screen, the place an application's commands live (File, Edit,
/// View, Help). A press on a title opens that menu's items below it in the
/// widget's overlay ([`WidgetDrawList::overlay`]) under the root's pointer
/// grab, reported through [`MenuOpenChanged`]; while a menu is open, moving
/// the pointer over another title opens that one instead. A press on an
/// enabled item activates it ([`MenuItemActivated`]) and closes; Escape or a
/// press elsewhere closes without activating. The bar is one row high; each
/// title is sized to its text plus padding.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.menu_bar.config")]
pub struct MenuBarConfig {
    pub menus: Vec<Menu>,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// How loudly a button asks to be pressed — the rank Material 3 sets its
/// button styles in, and the reason a region of five verbs does not read as
/// five equal demands.
///
/// The owner's round-8 note 5: "a single yellow button for everything is
/// kinda meh." A screen that fills every verb with the accent has said
/// "primary action" five times, so it has said it nowhere; the accent means
/// the primary action only while one verb per region carries it. The ladder
/// below is the one every published system agrees on, loudest first.
///
/// Nothing else about the button changes with the emphasis: the label is
/// measured, centered, and elided the same way, the reported intrinsic is
/// the same label plus the same pads, and the hit rectangle is the whole
/// frame at every step. A quieter button is a quieter *look*, never a
/// smaller target.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonEmphasis {
    /// A filled plate in the tone's own colour — the accent, or the error
    /// role for a destructive verb. The one primary verb of a region.
    #[default]
    Filled,
    /// A quiet plate ([`Theme::tonal`]) under the tone's colour: a verb that
    /// belongs beside the primary one without competing with it.
    Tonal,
    /// No plate, a hairline stroke, and a hover wash. The secondary verb of a
    /// region, and — in the danger tone — the shape a verb that throws work
    /// away takes.
    Outlined,
    /// Label and hover wash only. The quietest verb on the screen: a dialog's
    /// cancel, a "not now".
    Text,
}

/// What a verb does to the reader's work, which is the one thing a colour
/// role is allowed to say about a button.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonTone {
    /// An ordinary verb. Filled and tonal plates take the accent; outlined
    /// and text buttons read in the primary ink, so the accent keeps meaning
    /// the primary action alone.
    #[default]
    Neutral,
    /// A verb that throws work away — delete, discard, reset. Every emphasis
    /// takes [`Theme::error`], so the colour that means failure everywhere
    /// else on the screen means "this destroys something" here too.
    Danger,
}

/// `aether.kit.widget.button.config` — a momentary push button showing
/// `label`, firing [`ButtonClicked`] on a press-then-release-inside.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.button.config")]
pub struct ButtonConfig {
    pub label: String,
    /// How loudly this verb asks to be pressed. [`ButtonEmphasis::Filled`] —
    /// the accent plate every button drew before the ladder existed — unless
    /// a host ranks it lower.
    #[serde(default)]
    pub emphasis: ButtonEmphasis,
    /// What the verb does to the reader's work. [`ButtonTone::Neutral`]
    /// unless it throws work away.
    #[serde(default)]
    pub tone: ButtonTone,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// `aether.kit.widget.toggle.config` — a boolean switch with a visible
/// `label`, starting at `initial`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.toggle.config")]
pub struct ToggleConfig {
    pub label: String,
    pub initial: bool,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// `aether.kit.widget.segmented.config` — a horizontal list of equal-width,
/// mutually exclusive named options, starting at `initial_index` (clamped
/// into range at init).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.segmented.config")]
pub struct SegmentedConfig {
    pub options: Vec<String>,
    pub initial_index: u32,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// `aether.kit.widget.numeric.config` — a typed, steppable number bounded by
/// `min..=max`, snapped to `step`, and starting at `initial`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.numeric.config")]
pub struct NumericConfig {
    pub min: f32,
    pub max: f32,
    pub step: f32,
    pub initial: f32,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

impl Default for NumericConfig {
    fn default() -> Self {
        Self {
            min: 0.0,
            max: 1.0,
            step: 0.0,
            initial: 0.0,
            theme: Theme::default(),
            state: WidgetControlState::default(),
        }
    }
}

/// `aether.kit.widget.label.config` — static, non-interactive `text`. A label
/// is not focus-eligible (the root's focus register skips it).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.label.config")]
pub struct LabelConfig {
    pub text: String,
    /// Which step of the type scale the text is set at; `Body` unless the
    /// label is a title, a heading, or a caption.
    #[serde(default)]
    pub role: TextRole,
    /// Where the text sits in the assigned frame. `End` is what a column of
    /// numbers wants, so magnitudes line up on their last digit.
    #[serde(default)]
    pub align: TextAlign,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// Horizontal placement of a run of text inside the frame that carries it.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    /// Flush with the frame's left edge (after padding, where a widget pads).
    #[default]
    Start,
    /// Centred on the frame's width.
    Center,
    /// Flush with the frame's right edge.
    End,
}

/// How an [`ImageConfig`]'s natural size maps into its parent-owned frame.
///
/// Schema-only: this value is nested in `ImageConfig`, not addressable as its
/// own mail kind.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageFit {
    /// Distort the full image to fill the assigned frame.
    Fill,
    /// Preserve aspect ratio, show the whole image, and center any letterbox.
    #[default]
    Contain,
    /// Preserve aspect ratio, fill the frame, and center-crop through UVs.
    Cover,
    /// Draw at the configured natural pixel size, centered in the frame.
    Natural,
}

/// `aether.kit.widget.image.config` — a non-interactive image whose texture
/// lifecycle remains owned by the consumer that created `texture_id` through
/// `aether.render`. Natural dimensions drive fit arithmetic and the inherited
/// `WidgetDrawList::intrinsic` channel; they do not resize the parent slot.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.image.config")]
pub struct ImageConfig {
    pub texture_id: u32,
    pub natural_width_pixels: f32,
    pub natural_height_pixels: f32,
    pub fit: ImageFit,
    pub tint: Rgba,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            texture_id: 0,
            natural_width_pixels: 0.0,
            natural_height_pixels: 0.0,
            fit: ImageFit::default(),
            tint: Rgba::WHITE,
            theme: Theme::default(),
            state: WidgetControlState::default(),
        }
    }
}

/// `aether.kit.widget.slider.changed` — a slider's value-up event.
/// `committed` is `false` for the live values a drag streams and `true` for
/// the final value when the drag releases (or an arrow-key nudge lands), so a
/// consumer can throttle expensive work to committed values while still
/// previewing the drag.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.slider.changed")]
pub struct SliderChanged {
    pub value: f32,
    pub committed: bool,
}

/// `aether.kit.widget.text_field.committed` — the shared text-control value-up
/// event. A text field emits it on Enter; a text area emits it on Ctrl+Enter.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.text_field.committed")]
pub struct TextCommitted {
    pub text: String,
}

/// `aether.kit.widget.radio.selected` — a radio group's value-up event,
/// carrying the newly selected option's zero-based `index`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.radio.selected")]
pub struct RadioSelected {
    pub index: u32,
}

/// `aether.kit.widget.virtual_list.selected` — a virtual list's changed
/// selection, attributed by the parent from the sending child's mailbox.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.virtual_list.selected")]
pub struct VirtualListSelected {
    pub selected_index: u32,
}

/// `aether.kit.widget.virtual_list.action` — a verb bound to one row of a
/// virtual list was pressed: `row_index` into the config's `items`,
/// `action_index` into that row's [`RowAction`] vector. Which list it came from
/// is the root's `source_mailbox` attribution, exactly as for
/// [`VirtualListSelected`].
///
/// It is not a selection. A press on a row's verb reports this and leaves the
/// list's selection where it was, so "remove the third skill" costs one press
/// rather than select-then-remove — which is the whole reason a verb sits on
/// the row instead of under the list.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.widget.virtual_list.action")]
pub struct VirtualListAction {
    pub row_index: u32,
    pub action_index: u32,
}

/// `aether.kit.widget.virtual_list.hover` — the row the pointer is resting on
/// changed: `row` is an index into the config's `items`, or `None` once the
/// pointer has left the rows. Which list it came from is the root's
/// `source_mailbox` attribution, exactly as for [`VirtualListSelected`].
///
/// A list keeps its rows out of the host's hit table on purpose — the list owns
/// them, realizes a window of them, and scrolls that window under a pointer
/// that has not moved. So a host that wants to explain the row under the
/// pointer had a choice between doing the list's own geometry a second time and
/// getting it wrong the moment the list scrolled, or explaining nothing (the
/// studio's gap 19). This is the list saying it instead: sent when the answer
/// *changes*, from a pointer move, a wheel, a thumb drag, or the items being
/// replaced under a still pointer.
///
/// It is not a selection and it does not become one. Hovering a row says the
/// reader is looking at it — the tooltip a list of gems owes them — and nothing
/// about what they have chosen.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.widget.virtual_list.hover")]
pub struct VirtualListHover {
    pub row: Option<u32>,
}

/// `aether.kit.widget.button.clicked` — a button's value-up event, fired once
/// per completed press-then-release-inside. Fieldless: the click carries no
/// data, and which button clicked is the root's `source_mailbox` attribution.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.button.clicked")]
pub struct ButtonClicked;

/// `aether.kit.widget.toggle.changed` — a toggle's value-up event.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.toggle.changed")]
pub struct ToggleChanged {
    pub on: bool,
}

/// `aether.kit.widget.segmented.selected` — a segmented control's value-up
/// event carrying the newly selected zero-based `index`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.segmented.selected")]
pub struct SegmentedSelected {
    pub index: u32,
}

/// `aether.kit.widget.numeric.changed` — a numeric editor's value-up event.
/// Preview edits carry `committed: false`; Enter, blur, and step-key changes
/// carry `committed: true`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.numeric.changed")]
pub struct NumericChanged {
    pub value: f32,
    pub committed: bool,
}

/// `aether.kit.widget.dropdown.selected` — the dropdown's current choice
/// changed to `index`. Emitted only on an actual change.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.widget.dropdown.selected")]
pub struct DropdownSelected {
    pub index: u32,
}

/// `aether.kit.widget.dropdown.open_changed` — the dropdown opened or closed
/// its list. The root answers `open: true` by granting the sender the pointer
/// grab ([`crate::focus::Focus::begin_grab`]) so a press anywhere reaches it,
/// and `open: false` by ending the grab.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.widget.dropdown.open_changed")]
pub struct DropdownOpenChanged {
    pub open: bool,
}

/// `aether.kit.widget.dropdown.hover` — the option under the pointer in the
/// **open** list changed: `option` is an index into the config's `options`, or
/// `None` once the pointer has left the list or the list has closed. Which
/// dropdown it came from is the root's `source_mailbox` attribution, exactly as
/// for [`DropdownSelected`].
///
/// The dropdown's twin of [`VirtualListHover`], and it exists for the same
/// reason: the open list is drawn in the widget's overlay out of the host's hit
/// table, so a host that wants to explain the option a reader is resting on had
/// a choice between redoing the list's geometry — which is wrong the moment the
/// list scrolls — and explaining nothing. It is not a choice and does not
/// become one: the reader is looking, not picking, and `DropdownSelected` still
/// reports what they take.
///
/// `x` / `y` / `width` / `height` are that option's **row rectangle** in the
/// open list, in the same window-pixel space the panel gives a widget its
/// frame in, so a host can stand a tooltip on the row without measuring
/// anything. The overlay is offset by its slot's origin and never clipped or
/// moved, so the rectangle is where the row really draws. It is all zeroes when
/// `option` is `None`, which is the event that says to take the tooltip down.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[kind(name = "aether.kit.widget.dropdown.hover")]
pub struct DropdownHover {
    pub option: Option<u32>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// `aether.kit.widget.tab_strip.selected` — the selected tab changed to
/// `index`. Emitted only on an actual change.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.widget.tab_strip.selected")]
pub struct TabSelected {
    pub index: u32,
}

/// `aether.kit.widget.menu_bar.activated` — the item `item` of the menu
/// `menu` (both indices into the config) was activated.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.widget.menu_bar.activated")]
pub struct MenuItemActivated {
    pub menu: u32,
    pub item: u32,
}

/// `aether.kit.widget.menu_bar.open_changed` — a menu opened (`open: true`,
/// the root grants the sender the pointer grab) or every menu closed
/// (`open: false`, the root ends it). Reported once per edge.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.widget.menu_bar.open_changed")]
pub struct MenuOpenChanged {
    pub open: bool,
}

/// `aether.kit.widget.frame` — the layout rect the root assigns a child,
/// data-down. `(x, y)` is the child's top-left in window pixels and
/// `(width, height)` its size. The child caches it to lay out its own local
/// draw and to map a forwarded pointer position into its local space; the
/// root keeps the same rect in its layout table to offset the child's draws
/// and to hit-test pointer input.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.frame")]
pub struct WidgetFrame {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// `aether.kit.widget.focus_gained` — the root tells a child it now holds
/// keyboard focus, so the child takes keys and draws its caret. `keyboard`
/// says how focus arrived: `true` for Tab / Shift+Tab (and any other
/// keyboard traversal), `false` for a pointer press. A child draws its
/// **focus ring only for keyboard focus** — the platform's focus-visible
/// rule: a person who just clicked a tab already knows where focus is, and
/// a box around it reads as a second, unasked-for state.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.widget.focus_gained")]
pub struct FocusGained {
    pub keyboard: bool,
}

/// `aether.kit.widget.focus_lost` — the root tells a child it no longer holds
/// keyboard focus, so the child stops drawing its focus ring and caret.
/// Fieldless.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.focus_lost")]
pub struct FocusLost;

/// `aether.kit.widget.hover_gained` — the root tells a pointer-eligible child
/// that the pointer has entered its live hit rectangle. Fieldless.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.hover_gained")]
pub struct HoverGained;

/// `aether.kit.widget.hover_lost` — the root tells the previously hovered
/// child that the pointer left its live hit rectangle. Fieldless.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.hover_lost")]
pub struct HoverLost;

/// A fixed editor region in window pixel coordinates.
///
/// Named axes and units keep hit geometry explicit at the wire boundary.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq)]
pub struct EditorRegionRect {
    pub x_pixels: f32,
    pub y_pixels: f32,
    pub width_pixels: f32,
    pub height_pixels: f32,
}

/// Raw input lanes an editor region accepts from [`EditorShell`](crate::EditorShell).
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct RegionInputLanes {
    pub pointer_press: bool,
    pub pointer_release: bool,
    pub pointer_motion: bool,
    pub wheel: bool,
    pub key_press: bool,
    pub key_release: bool,
    pub text_input: bool,
    pub ime_preedit: bool,
    pub modifiers: bool,
}

impl RegionInputLanes {
    /// Every raw editor-input lane enabled.
    pub const ALL: Self = Self {
        pointer_press: true,
        pointer_release: true,
        pointer_motion: true,
        wheel: true,
        key_press: true,
        key_release: true,
        text_input: true,
        ime_preedit: true,
        modifiers: true,
    };
}

/// An exact editor-global key chord, matched against the shell's cached
/// modifier snapshot.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
// Named modifier fields are the schema contract, matching `Modifiers` rather
// than hiding the chord behind an opaque mask.
#[allow(clippy::struct_excessive_bools)]
pub struct EditorKeyChord {
    pub key_code: u32,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

/// One independently-rooted editor surface registered with the shell.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RegionSpec {
    pub name: String,
    pub rect: EditorRegionRect,
    pub target: MailboxId,
    pub keyboard_focus_eligible: bool,
    pub input_lanes: RegionInputLanes,
    pub activation_chord: Option<EditorKeyChord>,
}

/// `aether.kit.widget.editor.config` — ordered peer regions routed by an
/// input-only [`EditorShell`](crate::EditorShell).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.editor.config")]
pub struct EditorConfig {
    pub regions: Vec<RegionSpec>,
}

const fn owns_input_by_default() -> bool {
    true
}

/// `aether.kit.widget.panel.config` — the reference panel root's layout
/// config: where the vertical widget stack sits (`x` / `y` top-left, `width`),
/// its base [`Theme`], the font it loads through `aether.text` to fill the
/// theme's `font_id`, and the ordered `children` it stacks. The panel derives
/// each child's row height and focusability from its decoded config and lays
/// them out in the declared order, so the child list — not Rust source — is
/// what a panel contains. An empty `children` list falls back to the built-in
/// reference stack (a label, a slider, a radio group, a text field, an apply
/// button), the copy-paste starting point a real editor panel forks. A
/// [`WidgetKind::Scroll`] row takes its exact width and height from the decoded
/// [`ScrollConfig::viewport_extent`]; other children keep their existing
/// theme/intrinsic row sizing.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.panel.config")]
pub struct PanelConfig {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub font_namespace: String,
    pub font_path: String,
    pub theme: Theme,
    pub children: Vec<WidgetChildSpec>,
    /// Whether this standalone panel subscribes the raw interactive streams.
    /// Set false when an [`EditorShell`](crate::EditorShell) owns them.
    #[serde(default = "owns_input_by_default")]
    pub owns_input: bool,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width: 240.0,
            font_namespace: String::new(),
            font_path: String::new(),
            theme: Theme::default(),
            children: Vec::new(),
            owns_input: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::wire;

    #[test]
    fn widget_kind_preserves_established_wire_discriminants() {
        // Tripwire: WidgetKind is nested in public configs and its encoded
        // variant index is the wire contract. Add variants at the end; never
        // re-bless this golden when inserting a new kind.
        let behavior_host = wire::to_vec(&WidgetKind::BehaviorHost).expect("encode BehaviorHost");
        let image = wire::to_vec(&WidgetKind::Image).expect("encode Image");
        let text_area = wire::to_vec(&WidgetKind::TextArea).expect("encode TextArea");
        let scroll = wire::to_vec(&WidgetKind::Scroll).expect("encode Scroll");
        let virtual_list = wire::to_vec(&WidgetKind::VirtualList).expect("encode VirtualList");
        let toggle = wire::to_vec(&WidgetKind::Toggle).expect("encode Toggle");
        let segmented = wire::to_vec(&WidgetKind::Segmented).expect("encode Segmented");
        let numeric = wire::to_vec(&WidgetKind::Numeric).expect("encode Numeric");
        assert_eq!(behavior_host.as_slice(), 6_u32.to_le_bytes());
        assert_eq!(image.as_slice(), 7_u32.to_le_bytes());
        assert_eq!(text_area.as_slice(), 8_u32.to_le_bytes());
        assert_eq!(scroll.as_slice(), 9_u32.to_le_bytes());
        assert_eq!(virtual_list.as_slice(), 10_u32.to_le_bytes());
        assert_eq!(toggle.as_slice(), 11_u32.to_le_bytes());
        assert_eq!(segmented.as_slice(), 12_u32.to_le_bytes());
        assert_eq!(numeric.as_slice(), 13_u32.to_le_bytes());
    }

    #[test]
    fn quad_offset_translates_position_and_keeps_size() {
        let item = WidgetDrawItem::Quad {
            x: 3.0,
            y: 5.0,
            width: 10.0,
            height: 4.0,
            color: Rgba::new(1.0, 0.0, 0.0, 1.0),
            clip: Some(WidgetClipRect { x: 4.0, y: 6.0, width: 8.0, height: 2.0 }),
        };
        assert_eq!(
            item.offset(Vec2::new(100.0, 20.0)),
            WidgetDrawItem::Quad {
                x: 103.0,
                y: 25.0,
                width: 10.0,
                height: 4.0,
                color: Rgba::new(1.0, 0.0, 0.0, 1.0),
                clip: Some(WidgetClipRect { x: 104.0, y: 26.0, width: 8.0, height: 2.0 }),
            },
            "offset moves the corner by the vector and leaves the extent untouched",
        );
    }

    #[test]
    fn textured_quad_offset_translates_origin_and_clip_only() {
        let item = WidgetDrawItem::TexturedQuad {
            texture_id: 17,
            x: 3.0,
            y: 5.0,
            width: 10.0,
            height: 4.0,
            u0: 0.125,
            v0: 0.25,
            u1: 0.75,
            v1: 0.875,
            tint: Rgba::new(0.5, 0.75, 1.0, 0.8),
            clip: Some(WidgetClipRect { x: 4.0, y: 6.0, width: 8.0, height: 2.0 }),
        };
        assert_eq!(
            item.offset(Vec2::new(100.0, 20.0)),
            WidgetDrawItem::TexturedQuad {
                texture_id: 17,
                x: 103.0,
                y: 25.0,
                width: 10.0,
                height: 4.0,
                u0: 0.125,
                v0: 0.25,
                u1: 0.75,
                v1: 0.875,
                tint: Rgba::new(0.5, 0.75, 1.0, 0.8),
                clip: Some(WidgetClipRect { x: 104.0, y: 26.0, width: 8.0, height: 2.0 }),
            },
            "offset preserves texture identity, extent, UVs, and tint",
        );
    }

    #[test]
    fn text_offset_translates_baseline_and_keeps_content() {
        let item = WidgetDrawItem::Text {
            x: 1.0,
            y: 2.0,
            font_id: 7,
            text: "hp".into(),
            size_pixels: 12.0,
            color: Rgba::WHITE,
            clip: Some(WidgetClipRect { x: 3.0, y: 4.0, width: 8.0, height: 6.0 }),
        };
        assert_eq!(
            item.offset(Vec2::new(10.0, 40.0)),
            WidgetDrawItem::Text {
                x: 11.0,
                y: 42.0,
                font_id: 7,
                text: "hp".into(),
                size_pixels: 12.0,
                color: Rgba::WHITE,
                clip: Some(WidgetClipRect { x: 13.0, y: 44.0, width: 8.0, height: 6.0 }),
            },
            "offset moves the baseline and item-local clip while preserving the glyph run",
        );
    }

    #[test]
    fn clip_intersection_distinguishes_unbounded_finite_and_empty() {
        let outer = WidgetClipRect { x: 10.0, y: 20.0, width: 30.0, height: 40.0 };
        assert_eq!(intersect_widget_clips(None, None), WidgetClipIntersection::Unbounded,);
        assert_eq!(intersect_widget_clips(Some(outer), None), WidgetClipIntersection::Finite { rect: outer },);
        assert_eq!(
            intersect_widget_clips(Some(outer), Some(WidgetClipRect { x: 15.0, y: 25.0, width: 10.0, height: 12.0 }),),
            WidgetClipIntersection::Finite { rect: WidgetClipRect { x: 15.0, y: 25.0, width: 10.0, height: 12.0 } },
        );
        assert_eq!(
            intersect_widget_clips(Some(outer), Some(WidgetClipRect { x: 40.0, y: 20.0, width: 5.0, height: 5.0 }),),
            WidgetClipIntersection::Empty,
            "touching edges have zero area",
        );
        assert_eq!(
            intersect_widget_clips(Some(outer), Some(WidgetClipRect { x: 100.0, y: 100.0, width: 5.0, height: 5.0 }),),
            WidgetClipIntersection::Empty,
            "disjoint rectangles have no effective clip",
        );
    }

    #[test]
    fn clip_intersection_handles_partial_nested_and_invalid_rects() {
        let a = WidgetClipRect { x: -5.0, y: -5.0, width: 20.0, height: 20.0 };
        let b = WidgetClipRect { x: 0.0, y: 3.0, width: 20.0, height: 4.0 };
        let first = match intersect_widget_clips(Some(a), Some(b)) {
            WidgetClipIntersection::Finite { rect } => rect,
            other => panic!("expected finite overlap, got {other:?}"),
        };
        assert_eq!(first, WidgetClipRect { x: 0.0, y: 3.0, width: 15.0, height: 4.0 },);
        assert_eq!(
            intersect_widget_clips(Some(first), Some(WidgetClipRect { x: 2.0, y: 0.0, width: 3.0, height: 20.0 }),),
            WidgetClipIntersection::Finite { rect: WidgetClipRect { x: 2.0, y: 3.0, width: 3.0, height: 4.0 } },
        );
        for invalid in [
            WidgetClipRect { x: 0.0, y: 0.0, width: -1.0, height: 1.0 },
            WidgetClipRect { x: 0.0, y: 0.0, width: 1.0, height: -1.0 },
            WidgetClipRect { x: f32::NAN, y: 0.0, width: 1.0, height: 1.0 },
            WidgetClipRect { x: 0.0, y: 0.0, width: f32::INFINITY, height: 1.0 },
        ] {
            assert_eq!(intersect_widget_clips(Some(invalid), None), WidgetClipIntersection::Empty,);
        }
    }
}
