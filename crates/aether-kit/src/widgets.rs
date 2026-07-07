//! The widget-compositing wire vocabulary (ADR-0117).
//!
//! A widget tree is a cluster of inline-child actors (ADR-0114). Each
//! widget draws in its own local coordinates and never touches
//! `aether.render`; instead the cluster's root composites the whole
//! subtree and emits it as **one** ordered batch, so a dense panel costs
//! one render sender rather than one per widget and draw order is the
//! structural depth-first traversal of the tree.
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

use aether_math::Vec2;
use serde::{Deserialize, Serialize};

use crate::theme::Theme;

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
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.kit.widget.children_changed")]
pub struct ChildrenChanged {
    pub added: Vec<MembershipEntry>,
    pub removed: Vec<String>,
}

/// One draw in a [`WidgetDrawList`], in the widget's own local
/// coordinates (offset by the parent at composite time). A widget emits a
/// heterogeneous run of these in authored order — the single-list shape
/// preserves per-item quad/text interleave, which a two-vector
/// quads-then-texts split would foreclose. Not a kind on its own; only
/// addressable inside [`WidgetDrawList::items`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum WidgetDrawItem {
    /// A flat-colored rectangle. `(x, y)` is the top-left corner and
    /// `(width, height)` the size, in the widget's local pixels; `color`
    /// is a linear RGBA value.
    Quad {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: [f32; 4],
    },
    /// A glyph run. `(x, y)` is the baseline origin in local pixels;
    /// `font_id` names a session-scoped font loaded through `aether.text`;
    /// `color` is a linear RGBA multiplier over glyph coverage.
    Text {
        x: f32,
        y: f32,
        font_id: u32,
        text: String,
        size_pixels: f32,
        color: [f32; 4],
    },
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
            Self::Quad {
                x,
                y,
                width,
                height,
                color,
            } => Self::Quad {
                x: x + by.x,
                y: y + by.y,
                width: *width,
                height: *height,
                color: *color,
            },
            Self::Text {
                x,
                y,
                font_id,
                text,
                size_pixels,
                color,
            } => Self::Text {
                x: x + by.x,
                y: y + by.y,
                font_id: *font_id,
                text: text.clone(),
                size_pixels: *size_pixels,
                color: *color,
            },
        }
    }
}

/// `aether.kit.widget.draw_list` — a widget's reply to a [`Collect`], the
/// one channel that flows up the tree. `items` are the widget's draws in
/// authored order, in its local coordinates; `intrinsic` is its measured
/// content size (`[width, height]`) when the parent needs it to position a
/// content-sized slot — a cached event, never a pull — and `None` when the
/// widget's size is externally fixed.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq,
)]
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
#[derive(
    aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default,
)]
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
            Self::Slider => aether_data::mailbox_id_from_name("aether.kit.widget.slider").0,
            Self::Radio => aether_data::mailbox_id_from_name("aether.kit.widget.radio").0,
            Self::TextField => aether_data::mailbox_id_from_name("aether.kit.widget.text_field").0,
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
/// child's `origin` from its stack order and ignores this field.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WidgetChildSpec {
    pub subname: String,
    pub kind: WidgetKind,
    pub origin: [f32; 2],
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

/// The four data-down lanes of the widget set (config / style / layout
/// frame) and the one events-up lane (value) that the reference panel root
/// drives its inline widget children through. The kinds carry **no widget
/// identity field**: a value-up reply is attributed by the root against the
/// `MailboxId` it recorded when it spawned each child (`ctx.source_mailbox`),
/// so a widget's identity stays its inline subname rather than a field the
/// widget could get wrong. Layout and focus flow down the same way the
/// compositing `Collect` does — the root owns every child's rect and focus,
/// and the widget only reacts.
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
}

/// `aether.kit.widget.text_field.config` — a single-line editable string
/// starting at `initial`, capped at `max_chars` characters (`0` = no cap).
/// The field holds a `String` and a byte-offset caret; there is no selection
/// in v1.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.text_field.config")]
pub struct TextFieldConfig {
    pub initial: String,
    pub max_chars: u32,
    pub theme: Theme,
}

/// `aether.kit.widget.radio.config` — a vertical list of mutually-exclusive
/// `options`, one selected at a time, starting at `initial_index` (clamped
/// into range at init). Each option draws as one theme row.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.radio.config")]
pub struct RadioConfig {
    pub options: Vec<String>,
    pub initial_index: u32,
    pub theme: Theme,
}

/// `aether.kit.widget.button.config` — a momentary push button showing
/// `label`, firing [`ButtonClicked`] on a press-then-release-inside.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.button.config")]
pub struct ButtonConfig {
    pub label: String,
    pub theme: Theme,
}

/// `aether.kit.widget.label.config` — static, non-interactive `text`. A label
/// is not focus-eligible (the root's focus register skips it).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.label.config")]
pub struct LabelConfig {
    pub text: String,
    pub theme: Theme,
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

/// `aether.kit.widget.text_field.committed` — a text field's value-up event,
/// emitted when the field's Enter key commits its current contents.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_offset_translates_position_and_keeps_size() {
        let item = WidgetDrawItem::Quad {
            x: 3.0,
            y: 5.0,
            width: 10.0,
            height: 4.0,
            color: [1.0, 0.0, 0.0, 1.0],
        };
        assert_eq!(
            item.offset(Vec2::new(100.0, 20.0)),
            WidgetDrawItem::Quad {
                x: 103.0,
                y: 25.0,
                width: 10.0,
                height: 4.0,
                color: [1.0, 0.0, 0.0, 1.0],
            },
            "offset moves the corner by the vector and leaves the extent untouched",
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
            color: [1.0, 1.0, 1.0, 1.0],
        };
        assert_eq!(
            item.offset(Vec2::new(10.0, 40.0)),
            WidgetDrawItem::Text {
                x: 11.0,
                y: 42.0,
                font_id: 7,
                text: "hp".into(),
                size_pixels: 12.0,
                color: [1.0, 1.0, 1.0, 1.0],
            },
            "offset moves the baseline and preserves the glyph run",
        );
    }
}
