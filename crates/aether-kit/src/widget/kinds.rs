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

use aether_math::{Rgba, Vec2};
use serde::{Deserialize, Serialize};

use crate::widget::theme::Theme;

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
}

/// Result of composing two optional widget clips. Two absent clips are
/// unbounded, while an explicit invalid, empty, disjoint, or edge-touching
/// rectangle is empty and omits the item.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum WidgetClipIntersection {
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
            if rect.is_valid() { WidgetClipIntersection::Finite { rect } } else { WidgetClipIntersection::Empty }
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
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[kind(name = "aether.kit.widget.draw_list")]
pub struct WidgetDrawList {
    pub intrinsic: Option<[f32; 2]>,
    pub items: Vec<WidgetDrawItem>,
}

/// The kind of actor a [`WidgetChildSpec`] spawns, and the concrete config
/// type its opaque [`WidgetChildSpec::config`] bytes decode as. It is the
/// one tag that lets a single spec type serve both the homogeneous
/// compositing [`WidgetConfig`] tree (every child a `Composite`) and the
/// heterogeneous reference panel (a leaf per widget type). The spawnable set
/// is closed and kit-owned — every variant maps to a compile-time
/// `spawn_inline_child::<A>` call — so the dispatch match is exhaustive and
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
    /// A multiline text area — `config` decodes as [`TextAreaConfig`]. Kept at
    /// the end so adding it does not renumber the landed image discriminant or
    /// any earlier wire discriminant.
    TextArea,
}

impl WidgetKind {
    /// This stock widget's actor type tag — `hash(NAMESPACE)` of the widget
    /// actor `self` spawns, the same value `ActorTypeTag::of::<W>().0` would
    /// produce for the concrete actor type. `None` for the two unwrappable
    /// variants (`Composite`, `BehaviorHost`), which have no single wrapped
    /// actor. The trunk-reachable producer for
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
            Self::Composite | Self::BehaviorHost => return None,
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

/// `aether.kit.widget.button.config` — a momentary push button showing
/// `label`, firing [`ButtonClicked`] on a press-then-release-inside.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.button.config")]
pub struct ButtonConfig {
    pub label: String,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
}

/// `aether.kit.widget.label.config` — static, non-interactive `text`. A label
/// is not focus-eligible (the root's focus register skips it).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.label.config")]
pub struct LabelConfig {
    pub text: String,
    pub theme: Theme,
    #[serde(default)]
    pub state: WidgetControlState,
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

/// `aether.kit.widget.button.clicked` — a button's value-up event, fired once
/// per completed press-then-release-inside. Fieldless: the click carries no
/// data, and which button clicked is the root's `source_mailbox` attribution.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.button.clicked")]
pub struct ButtonClicked;

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
/// keyboard focus, so the child draws its focus ring and caret. Fieldless.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.focus_gained")]
pub struct FocusGained;

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

/// `aether.kit.widget.panel.config` — the reference panel root's layout
/// config: where the vertical widget stack sits (`x` / `y` top-left, `width`),
/// its base [`Theme`], the font it loads through `aether.text` to fill the
/// theme's `font_id`, and the ordered `children` it stacks. The panel derives
/// each child's row height and focusability from its decoded config and lays
/// them out in the declared order, so the child list — not Rust source — is
/// what a panel contains. An empty `children` list falls back to the built-in
/// reference stack (a label, a slider, a radio group, a text field, an apply
/// button), the copy-paste starting point a real editor panel forks. A
/// [`WidgetKind::Composite`] child (a nested container) is out of scope in v1
/// and is rejected with a warn.
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::wire::to_vec;
    use alloc::vec;

    #[test]
    fn text_area_appends_after_the_landed_image_wire_discriminant() {
        assert_eq!(to_vec(&WidgetKind::BehaviorHost).expect("encode BehaviorHost"), vec![6, 0, 0, 0]);
        assert_eq!(to_vec(&WidgetKind::Image).expect("encode Image"), vec![7, 0, 0, 0]);
        assert_eq!(to_vec(&WidgetKind::TextArea).expect("encode TextArea"), vec![8, 0, 0, 0]);
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
