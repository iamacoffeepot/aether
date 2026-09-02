// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]
// A radio group's row count is its (small) option count; the `usize as f32`
// for its stacked pixel height cannot lose precision at any real option count.
#![allow(clippy::cast_precision_loss)]

//! The reference panel root (issue 2660): the test vehicle, the copy-paste
//! template a consumer forks, and the map-editor seam.
//!
//! It embeds the two helper structs the widget tier is built from —
//! [`Composite`] (the ADR-0117 draw protocol's bookkeeping) and [`Focus`] (the
//! root-owned focus-and-input model) — and ties them together:
//!
//! - **Spawn.** On its first frame it spawns its declared vertical stack of
//!   inline widgets (each [`WidgetChildSpec`] naming a [`WidgetKind`] and
//!   carrying that widget's pre-encoded config), assigns each a
//!   [`WidgetFrame`] rect derived from stack order, and records that rect into
//!   both [`Composite`] (to offset the child's draws) and [`Focus`] (to
//!   hit-test and Tab-cycle it). An empty child list falls back to the
//!   built-in reference stack (a label, a slider, a radio group, a text
//!   field, an apply button).
//! - **Font.** In `wire` it loads a font through `aether.text` and, when the
//!   `load_font_result` arrives, stamps the session-scoped `font_id` into its
//!   [`Theme`] and re-fans it — the inline responsibility the theme module
//!   hands the panel root.
//! - **Input.** It subscribes to pointer / keyboard streams from every window
//!   and the frame stage once (the lifecycle cap), then routes each event
//!   through [`Focus`]: keyboard to the focused child, pointer to the hit or
//!   drag-captured child, Tab to cycle focus, a left press to set focus + drag
//!   capture. Focus transitions fan `FocusGained` / `FocusLost` down.
//! - **Draw.** Each frame it drives [`Composite`] — `Collect` down, draw lists
//!   up — and emits the whole panel as contiguous equal-clip solid batches
//!   (plus text) from one root render sender.
//! - **Value.** Each value-up event (`SliderChanged` / `TextCommitted` /
//!   `RadioSelected` / `VirtualListSelected` / `ButtonClicked` / `ToggleChanged` /
//!   `SegmentedSelected` / `NumericChanged` / `DropdownSelected` /
//!   `TabSelected`), attributed by
//!   `ctx.source_mailbox()`, is the seam a real editor translates into
//!   world-knob driver mail; the reference logs it.
//! - **Grab.** `DropdownOpenChanged` is the one events-up kind the root
//!   answers itself: an open list takes the modal pointer grab
//!   (`Focus::begin_grab`) so every press reaches it wherever it lands, and
//!   the close gives it back.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, Addressable, Manual, Subname, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_data::{Kind, MailboxId};
use aether_kinds::keycode::KEY_TAB;
use aether_kinds::mouse_button;
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput, Tick,
};
use aether_lifecycle::LifecycleCapability;
use aether_lifecycle::LifecycleMailboxExt;
use aether_math::Vec2;
use aether_text::{LoadFont, LoadFontResult, TextCapability};
use aether_window::{WindowCapability, WindowManagerMailboxExt, WindowSelector};

use crate::composite::Composite;
use crate::focus::{
    AvailabilityEffects, Focus, FocusDirection, FocusEligibility, FocusRect, FocusTransition, HoverTransition,
};
use crate::set::{
    ButtonWidget, DropdownWidget, ImageWidget, LabelWidget, NumericWidget, RadioGroupWidget, SegmentedWidget,
    SliderWidget, TabStripWidget, TextAreaWidget, TextFieldWidget, ToggleWidget, VirtualListWidget, quad,
};
use crate::theme::{SetTheme, TextRole, Theme};
use crate::{
    ButtonClicked, ButtonConfig, Collect, DropdownConfig, DropdownOpenChanged, DropdownSelected, FocusGained,
    FocusLost, HoverGained, HoverLost, ImageConfig, LabelConfig, NumericChanged, NumericConfig, PanelConfig,
    RadioConfig, RadioSelected, ScrollConfig, ScrollExtent, ScrollOutcome, ScrollResidual, ScrollWidget,
    SegmentedConfig, SegmentedSelected, SliderChanged, SliderConfig, TabSelected, TabStripConfig, TextAlign,
    TextAreaConfig, TextCommitted, TextFieldConfig, ToggleChanged, ToggleConfig, VirtualListConfig,
    VirtualListSelected, Widget, WidgetChildSpec, WidgetClipRect, WidgetControlState, WidgetDrawList, WidgetFrame,
    WidgetKind, WidgetStateChanged,
};
use crate::{FrameDischarge, decode_nested_widget_config};
use crate::{accept_open_child_list, emit, flush_membership};

/// One spawned child's alias plus the logical name the panel attributes its
/// value-up events under (for the map-editor translation / logging) — the
/// child's spec subname.
struct ChildRef {
    id: MailboxId,
    name: String,
}

#[derive(Clone, Copy)]
pub enum ChildLayout {
    Panel { row_height_pixels: f32 },
    Content { assigned_extent: ScrollExtent },
}

impl ChildLayout {
    fn row_height_pixels(self) -> f32 {
        match self {
            Self::Panel { row_height_pixels } => row_height_pixels,
            Self::Content { assigned_extent } => assigned_extent.height_pixels,
        }
    }

    fn scroll_viewport_mismatch(self, viewport: ScrollExtent) -> Option<ScrollExtent> {
        match self {
            Self::Panel { .. } => None,
            Self::Content { assigned_extent } => (viewport != assigned_extent).then_some(assigned_extent),
        }
    }
}

pub struct SpawnedChild {
    pub id: MailboxId,
    pub width_pixels: Option<f32>,
    pub height_pixels: f32,
    pub pointer_eligible: bool,
    pub focusable: bool,
    pub state: WidgetControlState,
    pub type_namespace: &'static str,
    pub scroll_viewport: Option<ScrollExtent>,
}

struct VirtualListProfile {
    height: f32,
    eligible: bool,
}

#[cfg(feature = "behavior")]
struct ChildProfile {
    height: f32,
    pointer_eligible: bool,
    focusable: bool,
    state: WidgetControlState,
}

/// The reference panel root. Loaded as a component with a [`PanelConfig`]; its
/// export name is `aether.kit.widget.panel`.
pub struct WidgetPanel {
    config: PanelConfig,
    /// The live theme — `config.theme` with the real `font_id` stamped once
    /// the font loads. Fanned down to every child on change.
    theme: Theme,
    composite: Composite,
    frame_discharge: FrameDischarge,
    focus: Focus,
    /// Wheel-only hit table. It intentionally excludes ordinary controls so
    /// drag capture in `focus` cannot steal a separate wheel gesture.
    scroll_focus: Focus,
    children: Vec<ChildRef>,
    spawned: bool,
    /// The total stack height, for the background chrome; set at spawn.
    panel_height: f32,
    /// Latest modifier state, used by panel-owned forward/reverse Tab routing.
    modifiers: Modifiers,
}

impl WidgetPanel {
    /// Spawn the declared widget stack once (from the first frame — an inline
    /// `init` cannot spawn, so the root spawns from its first activation
    /// handler). Each spec spawns its kind's actor from
    /// its decoded config; the row height and focusability derive from that
    /// config, and the vertical position derives from stack order (`origin` on
    /// the spec is ignored — the panel owns layout). Each widget gets its rect
    /// in both the composite layout table and the focus table, and its
    /// `WidgetFrame`. An empty child list falls back to [`reference_stack`].
    fn ensure_spawned(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        if self.spawned {
            return;
        }
        self.spawned = true;

        let x = self.config.x;
        let width = self.config.width;
        let row = self.theme.row_height;
        let gap = self.theme.gap;
        let mut y = self.config.y;

        let row_rect = |y: f32, width: f32, height: f32| WidgetFrame { x, y, width, height };

        let specs = if self.config.children.is_empty() {
            reference_stack(&self.theme)
        } else {
            self.config.children.clone()
        };

        let mut first = true;
        for spec in &specs {
            // Decode the concrete config, spawn the kind's actor, and derive
            // the row height + focusability from that config — plus the
            // spawned type's `NAMESPACE` for the membership record, carried
            // as data because the type is erased past this match. `None`
            // from any arm (an undecodable config, a spawn failure, or a
            // rejected container) skips the slot entirely so the stack
            // stays honest.
            let placed = spawn_widget_child::<Self>(ctx, spec, ChildLayout::Panel { row_height_pixels: row });
            let Some(placed) = placed else {
                continue;
            };
            if !first {
                y += gap;
            }
            first = false;
            self.place(
                ctx,
                &placed,
                row_rect(y, placed.width_pixels.unwrap_or(width), placed.height_pixels),
                spec.subname.clone(),
            );
            y += placed.height_pixels;
        }

        self.panel_height = y - self.config.y;
    }

    /// Record one spawned child's rect into the composite (as its draw offset,
    /// under its `name` subname and the spawned actor type `A`'s namespace) and
    /// the focus table (as its hit rect), send it its `WidgetFrame`, and
    /// remember it for value-up attribution.
    fn place(&mut self, ctx: &mut WasmCtx<'_, Manual>, child: &SpawnedChild, frame: WidgetFrame, name: String) {
        let focus_rect = FocusRect { x: frame.x, y: frame.y, width: frame.width, height: frame.height };
        self.composite.register_slot(
            child.id,
            Vec2::new(frame.x, frame.y),
            Some(WidgetClipRect { x: frame.x, y: frame.y, width: frame.width, height: frame.height }),
            &name,
            child.type_namespace,
        );
        self.focus.register(
            child.id,
            focus_rect,
            FocusEligibility { pointer: child.pointer_eligible, keyboard: child.focusable },
            &child.state,
        );
        if child.scroll_viewport.is_some() {
            self.scroll_focus.register(
                child.id,
                focus_rect,
                FocusEligibility { pointer: true, keyboard: false },
                &WidgetControlState::default(),
            );
        }
        ctx.send_to(child.id, &frame);
        self.children.push(ChildRef { id: child.id, name });
    }

    /// Discharge a closed frame: flatten the composite and emit it from the
    /// panel's single render + text sender.
    fn finish(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        if self.frame_discharge.is_closed() {
            return;
        }
        let list = self.composite.flatten(None);
        emit(ctx, &list);
        let closed = self.frame_discharge.close_frame();
        debug_assert!(closed, "an open panel frame closes exactly once");
    }

    /// Re-fan the live theme to every child (after a font stamp or a restyle).
    fn fan_theme(&self, ctx: &mut WasmCtx<'_>) {
        for child in &self.children {
            ctx.send_to(child.id, &SetTheme { theme: self.theme.clone() });
        }
    }

    /// The logical name of the child a value-up event came from, for
    /// attribution.
    fn child_name(&self, source: Option<MailboxId>) -> &str {
        source
            .and_then(|id| self.children.iter().find(|child| child.id == id))
            .map_or("unknown", |child| child.name.as_str())
    }
}

/// Decode, spawn, and derive one panel child's static/dynamic routing profile.
/// Keeping this dispatch out of `ensure_spawned` leaves the layout loop focused
/// on ordering and placement.
pub fn spawn_widget_child<P>(
    ctx: &mut WasmCtx<'_, Manual>,
    spec: &WidgetChildSpec,
    layout: ChildLayout,
) -> Option<SpawnedChild>
where
    P: WasmActor,
{
    let row = layout.row_height_pixels();
    match spec.kind {
        WidgetKind::Label => decode_child::<LabelConfig>(spec).and_then(|config| {
            let id = spawn::<P, LabelWidget>(ctx, &spec.subname, &config)?;
            Some(SpawnedChild {
                id,
                width_pixels: None,
                height_pixels: row,
                pointer_eligible: false,
                focusable: false,
                state: config.state,
                type_namespace: <LabelWidget as Addressable>::NAMESPACE,
                scroll_viewport: None,
            })
        }),
        WidgetKind::Image => decode_child::<ImageConfig>(spec).and_then(|config| {
            let id = spawn::<P, ImageWidget>(ctx, &spec.subname, &config)?;
            Some(SpawnedChild {
                id,
                width_pixels: None,
                height_pixels: row,
                pointer_eligible: false,
                focusable: false,
                state: config.state,
                type_namespace: <ImageWidget as Addressable>::NAMESPACE,
                scroll_viewport: None,
            })
        }),
        WidgetKind::Slider => decode_child::<SliderConfig>(spec).and_then(|config| {
            let id = spawn::<P, SliderWidget>(ctx, &spec.subname, &config)?;
            Some(SpawnedChild {
                id,
                width_pixels: None,
                height_pixels: row,
                pointer_eligible: true,
                focusable: true,
                state: config.state,
                type_namespace: <SliderWidget as Addressable>::NAMESPACE,
                scroll_viewport: None,
            })
        }),
        WidgetKind::Radio => decode_child::<RadioConfig>(spec).and_then(|config| {
            let height = row * config.options.len() as f32;
            let id = spawn::<P, RadioGroupWidget>(ctx, &spec.subname, &config)?;
            Some(SpawnedChild {
                id,
                width_pixels: None,
                height_pixels: height,
                pointer_eligible: true,
                focusable: true,
                state: config.state,
                type_namespace: <RadioGroupWidget as Addressable>::NAMESPACE,
                scroll_viewport: None,
            })
        }),
        WidgetKind::TextField => decode_child::<TextFieldConfig>(spec).and_then(|config| {
            let id = spawn::<P, TextFieldWidget>(ctx, &spec.subname, &config)?;
            Some(SpawnedChild {
                id,
                width_pixels: None,
                height_pixels: row,
                pointer_eligible: true,
                focusable: true,
                state: config.state,
                type_namespace: <TextFieldWidget as Addressable>::NAMESPACE,
                scroll_viewport: None,
            })
        }),
        WidgetKind::TextArea => decode_child::<TextAreaConfig>(spec).and_then(|config| {
            let height = row * config.rows.max(1) as f32;
            let id = spawn::<P, TextAreaWidget>(ctx, &spec.subname, &config)?;
            Some(SpawnedChild {
                id,
                width_pixels: None,
                height_pixels: height,
                pointer_eligible: true,
                focusable: true,
                state: config.state,
                type_namespace: <TextAreaWidget as Addressable>::NAMESPACE,
                scroll_viewport: None,
            })
        }),
        WidgetKind::Button => spawn_button_child::<P>(ctx, spec, row),
        WidgetKind::VirtualList => spawn_virtual_list_child::<P>(ctx, spec, row),
        WidgetKind::Toggle
        | WidgetKind::Segmented
        | WidgetKind::Numeric
        | WidgetKind::Dropdown
        | WidgetKind::TabStrip => spawn_row_control_child::<P>(ctx, spec, row),
        WidgetKind::BehaviorHost => spawn_behavior_host(ctx, spec, row),
        WidgetKind::Composite => spawn_composite_child::<P>(ctx, spec, layout, row),
        WidgetKind::Scroll => spawn_scroll_child::<P>(ctx, spec, layout),
    }
}

fn spawn_button_child<P: WasmActor>(
    ctx: &mut WasmCtx<'_, Manual>,
    spec: &WidgetChildSpec,
    row: f32,
) -> Option<SpawnedChild> {
    let config = decode_child::<ButtonConfig>(spec)?;
    let id = spawn::<P, ButtonWidget>(ctx, &spec.subname, &config)?;
    Some(SpawnedChild {
        id,
        width_pixels: None,
        height_pixels: row,
        pointer_eligible: true,
        focusable: true,
        state: config.state,
        type_namespace: <ButtonWidget as Addressable>::NAMESPACE,
        scroll_viewport: None,
    })
}

fn spawn_virtual_list_child<P: WasmActor>(
    ctx: &mut WasmCtx<'_, Manual>,
    spec: &WidgetChildSpec,
    row: f32,
) -> Option<SpawnedChild> {
    let config = decode_child::<VirtualListConfig>(spec)?;
    let profile = virtual_list_profile(&spec.subname, row, &config)?;
    let state = config.state.clone();
    spawn::<P, VirtualListWidget>(ctx, &spec.subname, &config).map(|id| SpawnedChild {
        id,
        width_pixels: None,
        height_pixels: profile.height,
        pointer_eligible: profile.eligible,
        focusable: profile.eligible,
        state,
        type_namespace: <VirtualListWidget as Addressable>::NAMESPACE,
        scroll_viewport: None,
    })
}

fn virtual_list_profile(subname: &str, row_height: f32, config: &VirtualListConfig) -> Option<VirtualListProfile> {
    let Some(height) = virtual_list_height(row_height, config.visible_row_count) else {
        tracing::warn!(
            target: "aether_kit_widget",
            subname,
            row_height,
            visible_row_count = config.visible_row_count,
            "virtual-list viewport height is invalid; slot skipped",
        );
        return None;
    };
    Some(VirtualListProfile { height, eligible: !config.items.is_empty() && config.visible_row_count > 0 })
}

fn virtual_list_height(row_height: f32, visible_row_count: u32) -> Option<f32> {
    if !row_height.is_finite() || row_height <= 0.0 {
        return None;
    }
    let height = row_height * visible_row_count as f32;
    (height.is_finite() && height >= 0.0).then_some(height)
}

fn spawn_composite_child<P: WasmActor>(
    ctx: &mut WasmCtx<'_, Manual>,
    spec: &WidgetChildSpec,
    layout: ChildLayout,
    row_height_pixels: f32,
) -> Option<SpawnedChild> {
    if matches!(layout, ChildLayout::Panel { .. }) {
        tracing::warn!(
            target: "aether_kit_widget",
            subname = %spec.subname,
            "a bare Composite child is supported only as scroll content; panel slot skipped",
        );
        return None;
    }
    decode_nested_widget_config(spec).and_then(|config| {
        let intrinsic = config.intrinsic;
        spawn::<P, Widget>(ctx, &spec.subname, &config).map(|id| SpawnedChild {
            id,
            width_pixels: intrinsic.and_then(|extent| (extent[0].is_finite() && extent[0] >= 0.0).then_some(extent[0])),
            height_pixels: intrinsic
                .and_then(|extent| (extent[1].is_finite() && extent[1] >= 0.0).then_some(extent[1]))
                .unwrap_or(row_height_pixels),
            pointer_eligible: false,
            focusable: false,
            state: WidgetControlState::default(),
            type_namespace: <Widget as Addressable>::NAMESPACE,
            scroll_viewport: None,
        })
    })
}

fn spawn_scroll_child<P: WasmActor>(
    ctx: &mut WasmCtx<'_, Manual>,
    spec: &WidgetChildSpec,
    layout: ChildLayout,
) -> Option<SpawnedChild> {
    decode_child::<ScrollConfig>(spec).and_then(|config| {
        if let Some(assigned_extent) = layout.scroll_viewport_mismatch(config.viewport_extent) {
            tracing::warn!(
                target: "aether_kit_widget",
                subname = %spec.subname,
                assigned_width_pixels = assigned_extent.width_pixels,
                assigned_height_pixels = assigned_extent.height_pixels,
                viewport_width_pixels = config.viewport_extent.width_pixels,
                viewport_height_pixels = config.viewport_extent.height_pixels,
                "nested scroll viewport does not match its assigned content extent; slot skipped",
            );
            return None;
        }
        let viewport = config.viewport_extent;
        spawn::<P, ScrollWidget>(ctx, &spec.subname, &config).map(|id| SpawnedChild {
            id,
            width_pixels: Some(viewport.width_pixels),
            height_pixels: viewport.height_pixels,
            pointer_eligible: false,
            focusable: false,
            state: WidgetControlState::default(),
            type_namespace: <ScrollWidget as Addressable>::NAMESPACE,
            scroll_viewport: Some(viewport),
        })
    })
}

/// Spawn the three issue-2926 one-row control children. Keeping their
/// mechanical decode/spawn profiles together prevents the main exhaustive
/// dispatcher from becoming a second long-form implementation surface.
fn spawn_row_control_child<P: WasmActor>(
    ctx: &mut WasmCtx<'_, Manual>,
    spec: &WidgetChildSpec,
    row: f32,
) -> Option<SpawnedChild> {
    match spec.kind {
        WidgetKind::Toggle => decode_child::<ToggleConfig>(spec).and_then(|config| {
            spawn::<P, ToggleWidget>(ctx, &spec.subname, &config).map(|id| SpawnedChild {
                id,
                width_pixels: None,
                height_pixels: row,
                pointer_eligible: true,
                focusable: true,
                state: config.state,
                type_namespace: <ToggleWidget as Addressable>::NAMESPACE,
                scroll_viewport: None,
            })
        }),
        WidgetKind::Segmented => decode_child::<SegmentedConfig>(spec).and_then(|config| {
            spawn::<P, SegmentedWidget>(ctx, &spec.subname, &config).map(|id| SpawnedChild {
                id,
                width_pixels: None,
                height_pixels: row,
                pointer_eligible: true,
                focusable: true,
                state: config.state,
                type_namespace: <SegmentedWidget as Addressable>::NAMESPACE,
                scroll_viewport: None,
            })
        }),
        WidgetKind::Numeric => decode_child::<NumericConfig>(spec).and_then(|config| {
            spawn::<P, NumericWidget>(ctx, &spec.subname, &config).map(|id| SpawnedChild {
                id,
                width_pixels: None,
                height_pixels: row,
                pointer_eligible: true,
                focusable: true,
                state: config.state,
                type_namespace: <NumericWidget as Addressable>::NAMESPACE,
                scroll_viewport: None,
            })
        }),
        WidgetKind::Dropdown => decode_child::<DropdownConfig>(spec).and_then(|config| {
            spawn::<P, DropdownWidget>(ctx, &spec.subname, &config).map(|id| SpawnedChild {
                id,
                width_pixels: None,
                height_pixels: row,
                pointer_eligible: true,
                focusable: true,
                state: config.state,
                type_namespace: <DropdownWidget as Addressable>::NAMESPACE,
                scroll_viewport: None,
            })
        }),
        WidgetKind::TabStrip => decode_child::<TabStripConfig>(spec).and_then(|config| {
            spawn::<P, TabStripWidget>(ctx, &spec.subname, &config).map(|id| SpawnedChild {
                id,
                width_pixels: None,
                height_pixels: row,
                pointer_eligible: true,
                focusable: true,
                state: config.state,
                type_namespace: <TabStripWidget as Addressable>::NAMESPACE,
                scroll_viewport: None,
            })
        }),
        _ => None,
    }
}

/// Decode one child spec's opaque config bytes as the concrete config type
/// `C` its [`WidgetKind`] selects, warning and yielding `None` on a decode
/// failure so the caller skips the slot (mirroring `widget.rs`).
fn decode_child<C: Kind>(spec: &WidgetChildSpec) -> Option<C> {
    decode_named(&spec.subname, &spec.config)
}

fn decode_named<C: Kind>(subname: &str, bytes: &[u8]) -> Option<C> {
    let config = C::decode_from_bytes(bytes);
    if config.is_none() {
        tracing::warn!(
            target: "aether_kit_widget",
            subname,
            "widget child config failed to decode; slot skipped",
        );
    }
    config
}

/// The built-in reference stack the panel falls back to when its config
/// declares no `children`: a label, a slider over `0..=255`, a three-option
/// radio group, a text field, and an apply button — the former hardcode,
/// expressed as the child-spec data the panel could equally have been handed.
/// Each spec's `origin` is unused (the panel derives layout from stack order);
/// the concrete configs carry the panel's live `theme`.
fn reference_stack(theme: &Theme) -> Vec<WidgetChildSpec> {
    let spec = |subname: &str, kind: WidgetKind, config: Vec<u8>| WidgetChildSpec {
        subname: String::from(subname),
        kind,
        origin: [0.0, 0.0],
        clip: None,
        config,
    };
    vec![
        spec(
            "label",
            WidgetKind::Label,
            LabelConfig {
                text: String::from("Controls"),
                role: TextRole::Body,
                align: TextAlign::Start,
                theme: theme.clone(),
                state: WidgetControlState::default(),
            }
            .encode_into_bytes(),
        ),
        spec(
            "slider",
            WidgetKind::Slider,
            SliderConfig {
                min: 0.0,
                max: 255.0,
                step: 1.0,
                initial: 40.0,
                theme: theme.clone(),
                state: WidgetControlState::default(),
            }
            .encode_into_bytes(),
        ),
        spec(
            "radio",
            WidgetKind::Radio,
            RadioConfig {
                options: vec![String::from("Low"), String::from("Medium"), String::from("High")],
                initial_index: 0,
                theme: theme.clone(),
                state: WidgetControlState::default(),
            }
            .encode_into_bytes(),
        ),
        spec(
            "text_field",
            WidgetKind::TextField,
            TextFieldConfig {
                initial: String::new(),
                max_chars: 32,
                theme: theme.clone(),
                state: WidgetControlState::default(),
            }
            .encode_into_bytes(),
        ),
        spec(
            "button",
            WidgetKind::Button,
            ButtonConfig { label: String::from("Apply"), theme: theme.clone(), state: WidgetControlState::default() }
                .encode_into_bytes(),
        ),
    ]
}

/// Send a focus transition down: `FocusLost` to the child that lost focus,
/// `FocusGained` to the one that gained it.
fn apply_focus<M: aether_actor::ReplyMode>(ctx: &mut WasmCtx<'_, M>, transition: FocusTransition) {
    let FocusTransition { previous, next } = transition;
    if let Some(prev) = previous {
        ctx.send_to(prev, &FocusLost);
    }
    if let Some(gained) = next {
        ctx.send_to(gained, &FocusGained);
    }
}

/// Send hover edges lost-before-gained so sibling crossings cannot leave two
/// controls hovered during the breadth-first drain.
fn apply_hover<M: aether_actor::ReplyMode>(ctx: &mut WasmCtx<'_, M>, transition: HoverTransition) {
    let HoverTransition { previous, next } = transition;
    if let Some(previous) = previous {
        ctx.send_to(previous, &HoverLost);
    }
    if let Some(next) = next {
        ctx.send_to(next, &HoverGained);
    }
}

fn apply_availability<M: aether_actor::ReplyMode>(ctx: &mut WasmCtx<'_, M>, effects: AvailabilityEffects) {
    if let Some(hover) = effects.hover {
        apply_hover(ctx, hover);
    }
    if let Some(focus) = effects.focus {
        apply_focus(ctx, focus);
    }
}

/// Spawn one inline widget under the caller's actual logical actor type,
/// logging and dropping the slot on failure.
fn spawn<P, A>(ctx: &mut WasmCtx<'_, Manual>, subname: &str, config: &A::Config) -> Option<MailboxId>
where
    P: WasmActor,
    A: aether_actor::ChildOf<P> + aether_actor::Instanced + WasmActor + aether_actor::ErasedWasmActor,
    <A as WasmActor>::State: aether_actor::ErasedWasmActor,
{
    match ctx.spawn_inline_child::<P, A>(Subname::Named(subname), config) {
        Ok(id) => Some(id),
        Err(error) => {
            tracing::warn!(
                target: "aether_kit_widget",
                subname,
                ?error,
                "widget spawn failed; slot skipped",
            );
            None
        }
    }
}

#[cfg(feature = "behavior")]
fn behavior_mirror_kinds() -> Vec<u64> {
    vec![
        LabelConfig::ID.0,
        ImageConfig::ID.0,
        SliderConfig::ID.0,
        TextFieldConfig::ID.0,
        TextAreaConfig::ID.0,
        ButtonConfig::ID.0,
        RadioConfig::ID.0,
        VirtualListConfig::ID.0,
        ToggleConfig::ID.0,
        SegmentedConfig::ID.0,
        NumericConfig::ID.0,
        DropdownConfig::ID.0,
        TabStripConfig::ID.0,
        SliderChanged::ID.0,
        TextCommitted::ID.0,
        ButtonClicked::ID.0,
        RadioSelected::ID.0,
        VirtualListSelected::ID.0,
        ToggleChanged::ID.0,
        SegmentedSelected::ID.0,
        NumericChanged::ID.0,
        DropdownSelected::ID.0,
        DropdownOpenChanged::ID.0,
        TabSelected::ID.0,
        FocusGained::ID.0,
        FocusLost::ID.0,
        HoverGained::ID.0,
        HoverLost::ID.0,
        crate::SetWidgetState::ID.0,
        WidgetStateChanged::ID.0,
        crate::ChildrenChanged::ID.0,
        ScrollOutcome::ID.0,
        ScrollResidual::ID.0,
    ]
}

/// Spawn a [`WidgetKind::BehaviorHost`] slot (issue 2687): decode the
/// [`BehaviorHostSpec`](crate::BehaviorHostSpec), map the wrapped widget kind
/// to its type tag, build the `aether-behavior` `HostConfig`, and spawn the
/// host by tag (#2692) — the host then spawns the wrapped widget as its own
/// inline child and interposes on the slot's mail. Returns the named
/// [`SpawnedChild`] profile the other arms produce; `None` (slot skipped) on
/// an unsupported wrapped kind, a decode failure, or a spawn error. The
/// panel's per-frame `Collect` is handed to the host as its FRAME trigger.
#[cfg(feature = "behavior")]
fn spawn_behavior_host(ctx: &mut WasmCtx<'_, Manual>, spec: &WidgetChildSpec, row: f32) -> Option<SpawnedChild> {
    use crate::{BehaviorHostSpec, ScriptRef};
    use aether_actor::ActorTypeTag;
    use aether_behavior::HostConfig;
    use aether_behavior::host::{ChildSpec, ScriptSource};

    let host_spec = decode_child::<BehaviorHostSpec>(spec)?;
    let Some(type_tag) = host_spec.wrapped.type_tag() else {
        tracing::warn!(
            target: "aether_kit_widget",
            subname = %spec.subname,
            wrapped = ?host_spec.wrapped,
            "BehaviorHost cannot wrap this widget kind (container or host); slot skipped",
        );
        return None;
    };
    let profile = match host_spec.wrapped {
        WidgetKind::Label => {
            let config = decode_named::<LabelConfig>(&spec.subname, &host_spec.wrapped_config)?;
            ChildProfile { height: row, pointer_eligible: false, focusable: false, state: config.state }
        }
        WidgetKind::Image => {
            let config = decode_named::<ImageConfig>(&spec.subname, &host_spec.wrapped_config)?;
            ChildProfile { height: row, pointer_eligible: false, focusable: false, state: config.state }
        }
        WidgetKind::Slider => {
            let config = decode_named::<SliderConfig>(&spec.subname, &host_spec.wrapped_config)?;
            ChildProfile { height: row, pointer_eligible: true, focusable: true, state: config.state }
        }
        WidgetKind::Radio => {
            let config = decode_named::<RadioConfig>(&spec.subname, &host_spec.wrapped_config)?;
            ChildProfile {
                height: row * config.options.len() as f32,
                pointer_eligible: true,
                focusable: true,
                state: config.state,
            }
        }
        WidgetKind::TextField => {
            let config = decode_named::<TextFieldConfig>(&spec.subname, &host_spec.wrapped_config)?;
            ChildProfile { height: row, pointer_eligible: true, focusable: true, state: config.state }
        }
        WidgetKind::TextArea => {
            let config = decode_named::<TextAreaConfig>(&spec.subname, &host_spec.wrapped_config)?;
            ChildProfile {
                height: row * config.rows.max(1) as f32,
                pointer_eligible: true,
                focusable: true,
                state: config.state,
            }
        }
        WidgetKind::Button => {
            let config = decode_named::<ButtonConfig>(&spec.subname, &host_spec.wrapped_config)?;
            ChildProfile { height: row, pointer_eligible: true, focusable: true, state: config.state }
        }
        WidgetKind::VirtualList => {
            let config = decode_named::<VirtualListConfig>(&spec.subname, &host_spec.wrapped_config)?;
            let profile = virtual_list_profile(&spec.subname, row, &config)?;
            ChildProfile {
                height: profile.height,
                pointer_eligible: profile.eligible,
                focusable: profile.eligible,
                state: config.state,
            }
        }
        WidgetKind::Toggle => {
            let config = decode_named::<ToggleConfig>(&spec.subname, &host_spec.wrapped_config)?;
            ChildProfile { height: row, pointer_eligible: true, focusable: true, state: config.state }
        }
        WidgetKind::Segmented => {
            let config = decode_named::<SegmentedConfig>(&spec.subname, &host_spec.wrapped_config)?;
            ChildProfile { height: row, pointer_eligible: true, focusable: true, state: config.state }
        }
        WidgetKind::Numeric => {
            let config = decode_named::<NumericConfig>(&spec.subname, &host_spec.wrapped_config)?;
            ChildProfile { height: row, pointer_eligible: true, focusable: true, state: config.state }
        }
        WidgetKind::Dropdown => {
            let config = decode_named::<DropdownConfig>(&spec.subname, &host_spec.wrapped_config)?;
            ChildProfile { height: row, pointer_eligible: true, focusable: true, state: config.state }
        }
        WidgetKind::TabStrip => {
            let config = decode_named::<TabStripConfig>(&spec.subname, &host_spec.wrapped_config)?;
            ChildProfile { height: row, pointer_eligible: true, focusable: true, state: config.state }
        }
        WidgetKind::Composite | WidgetKind::Scroll | WidgetKind::BehaviorHost => return None,
    };
    let script = match host_spec.script {
        ScriptRef::None => ScriptSource::None,
        ScriptRef::Inline(bytes) => ScriptSource::Inline(bytes),
        ScriptRef::FsRef { namespace, path } => ScriptSource::FsRef { namespace, path },
    };
    let fuel_per_call = if host_spec.fuel_per_call != 0 {
        host_spec.fuel_per_call
    } else {
        HostConfig::DEFAULT_FUEL_PER_CALL
    };
    let disable_after_traps = if host_spec.disable_after_traps != 0 {
        host_spec.disable_after_traps
    } else {
        HostConfig::DEFAULT_DISABLE_AFTER_TRAPS
    };
    let config = HostConfig {
        child: ChildSpec {
            // The wrapped widget nests under this host. Scoped inline folds
            // let every behavior host own the same local slot shape without
            // colliding elsewhere in the component cluster.
            type_tag,
            subname: alloc::format!("{}_wrapped", spec.subname),
            config: host_spec.wrapped_config,
        },
        script,
        fuel_per_call,
        disable_after_traps,
        // The panel drives the wrapped slot with `Collect` each frame; hand the
        // host that kind as its FRAME sentinel trigger.
        frame_trigger: Collect::ID.0,
        mirror_kinds: behavior_mirror_kinds(),
    };
    let bytes = config.encode_into_bytes();
    match ctx.spawn_inline_child_by_tag(
        ActorTypeTag::of::<aether_behavior::BehaviorHost>(),
        Subname::Named(&spec.subname),
        &bytes,
    ) {
        Ok(id) => Some(SpawnedChild {
            id,
            width_pixels: None,
            height_pixels: profile.height,
            pointer_eligible: profile.pointer_eligible,
            focusable: profile.focusable,
            state: profile.state,
            type_namespace: <aether_behavior::BehaviorHost as Addressable>::NAMESPACE,
            scroll_viewport: None,
        }),
        Err(error) => {
            tracing::warn!(
                target: "aether_kit_widget",
                subname = %spec.subname,
                ?error,
                "behavior host spawn failed; slot skipped",
            );
            None
        }
    }
}

/// The `behavior`-feature-off stub: a `WidgetKind::BehaviorHost` slot needs the
/// host actor, which is only linked under the kit's `behavior` feature.
#[cfg(not(feature = "behavior"))]
fn spawn_behavior_host(_ctx: &mut WasmCtx<'_, Manual>, spec: &WidgetChildSpec, _row: f32) -> Option<SpawnedChild> {
    tracing::warn!(
        target: "aether_kit_widget",
        subname = %spec.subname,
        "WidgetKind::BehaviorHost needs the kit `behavior` feature; slot skipped",
    );
    None
}

/// The reference panel root. Load it as a component (export
/// `aether.kit.widget.panel`) with a [`PanelConfig`].
///
/// # Agent
/// Load `aether_kit_widget.wasm` with `export: "aether.kit.widget.panel"` and a
/// `PanelConfig` (top-left, width, theme, a font to load, and the `children`
/// it stacks). With an empty `children` list it spawns a demonstration stack
/// of the original interactive/reference widgets; otherwise it stacks exactly
/// the declared specs (including `Image`, `Toggle`, `Segmented`, and `Numeric`
/// children). It routes
/// real input through the focus model and logs each value-up event. Fork it
/// into a real editor panel by handing it your own `children` and translating
/// the value-up handlers into your own world-knob driver mail.
#[actor(instanced)]
impl WasmActor for WidgetPanel {
    type Config = PanelConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.panel";

    fn init(config: PanelConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(WidgetPanel {
            theme: config.theme.clone(),
            config,
            composite: Composite::new(),
            frame_discharge: FrameDischarge::default(),
            focus: Focus::new(),
            scroll_focus: Focus::new(),
            children: Vec::new(),
            spawned: false,
            panel_height: 0.0,
            modifiers: Modifiers::default(),
        })
    }

    /// Subscribe to pointer / keyboard streams from every window and the frame
    /// stage once, then kick off the font load. Widgets never subscribe — the
    /// root forwards everything.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        if self.config.owns_input {
            let window = ctx.actor::<WindowCapability>();
            window.subscribe::<MouseButton>(WindowSelector::All);
            window.subscribe::<MouseButtonRelease>(WindowSelector::All);
            window.subscribe::<MouseMove>(WindowSelector::All);
            window.subscribe::<MouseWheel>(WindowSelector::All);
            window.subscribe::<Key>(WindowSelector::All);
            window.subscribe::<KeyRelease>(WindowSelector::All);
            window.subscribe::<TextInput>(WindowSelector::All);
            window.subscribe::<ImePreedit>(WindowSelector::All);
            window.subscribe::<Modifiers>(WindowSelector::All);
        }
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
        if !self.config.font_path.is_empty() {
            ctx.actor::<TextCapability>()
                .send(&LoadFont { namespace: self.config.font_namespace.clone(), path: self.config.font_path.clone() });
        }
    }

    /// Frame driver: spawn on the first tick, then open a composite frame, lay
    /// the panel background, and fan `Collect` to every child. A leaf-free
    /// panel finishes from `on_draw_list` once the slots close.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler::manual]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_, Manual>, _tick: Tick) {
        self.ensure_spawned(ctx);
        flush_membership(&mut self.composite, ctx);
        self.composite.begin_frame();
        self.frame_discharge.begin_frame();
        let background = quad(self.config.x, self.config.y, self.config.width, self.panel_height, self.theme.surface);
        self.composite.extend_chrome([background]);
        for child in &self.children {
            ctx.send_to(child.id, &Collect);
        }
        if self.composite.is_complete() {
            self.finish(ctx);
        }
    }

    /// A child's draw list: attribute it by source and, when every child has
    /// replied this frame, emit the whole panel.
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

    /// A left press sets focus + drag capture on the hit child and forwards
    /// the press; any press forwards to the hit child. A modal grab (an open
    /// dropdown) takes every press before any of that: the grab holder must
    /// see the press that lands outside it, which is how it learns to close.
    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if let Some(grabbed) = self.focus.grabbed() {
            ctx.send_to(grabbed, &press);
            return;
        }
        let target = if press.button == mouse_button::LEFT {
            let hit = self.focus.hit_test(press.x, press.y);
            if let Some(child) = hit {
                self.focus.begin_capture(child);
            }
            if let Some(transition) = self.focus.focus_hit(press.x, press.y) {
                apply_focus(ctx, transition);
            }
            hit
        } else {
            self.focus.pointer_target(press.x, press.y)
        };
        if let Some(child) = target {
            ctx.send_to(child, &press);
        }
    }

    /// A release forwards to the captured / hit child and clears capture.
    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if let Some(child) = self.focus.pointer_target(release.x, release.y) {
            ctx.send_to(child, &release);
        }
        if release.button == mouse_button::LEFT
            && let Some(transition) = self.focus.release_capture(release.x, release.y)
        {
            apply_hover(ctx, transition);
        }
    }

    /// A move forwards to the grabbed, captured (dragged), or hit child —
    /// `pointer_target`'s own precedence, so an open dropdown tracks the
    /// pointer over rows drawn outside its slot. Hover edges are suppressed
    /// while a grab holds: nothing under a modal overlay should light up, and
    /// the next motion after the grab ends re-derives hover anyway.
    #[handler::single]
    fn on_mouse_move(&mut self, ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if self.focus.grabbed().is_none()
            && let Some(transition) = self.focus.update_hover(moved.x, moved.y)
        {
            apply_hover(ctx, transition);
        }
        if let Some(child) = self.focus.pointer_target(moved.x, moved.y) {
            ctx.send_to(child, &moved);
        }
    }

    /// Route a wheel to the topmost scroll viewport under the cursor. This is
    /// deliberately a fresh `hit_test`, not `pointer_target`: a button's drag
    /// capture owns move/release, not a separate wheel gesture.
    #[handler::single]
    fn on_mouse_wheel(&mut self, ctx: &mut WasmCtx<'_>, wheel: MouseWheel) {
        if let Some(child) = self.scroll_focus.hit_test(wheel.x, wheel.y) {
            ctx.send_to(child, &wheel);
        }
    }

    /// Tab cycles focus; every other key forwards to the focused child.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if key.code == KEY_TAB {
            let direction = if self.modifiers.shift {
                FocusDirection::Backward
            } else {
                FocusDirection::Forward
            };
            if let Some(transition) = self.focus.move_focus(direction) {
                apply_focus(ctx, transition);
            }
            return;
        }
        if let Some(child) = self.focus.keyboard_target() {
            ctx.send_to(child, &key);
        }
    }

    /// Key releases forward to the focused child (Button uses matching Space
    /// release for exactly-once activation).
    #[handler::single]
    fn on_key_release(&mut self, ctx: &mut WasmCtx<'_>, release: KeyRelease) {
        if let Some(child) = self.focus.keyboard_target() {
            ctx.send_to(child, &release);
        }
    }

    /// Committed text forwards to the focused child.
    #[handler::single]
    fn on_text_input(&mut self, ctx: &mut WasmCtx<'_>, input: TextInput) {
        if let Some(child) = self.focus.keyboard_target() {
            ctx.send_to(child, &input);
        }
    }

    /// An IME composition forwards to the focused child.
    #[handler::single]
    fn on_ime_preedit(&mut self, ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        if let Some(child) = self.focus.keyboard_target() {
            ctx.send_to(child, &preedit);
        }
    }

    /// Modifier state forwards to the focused child (the text field caches
    /// it).
    #[handler::single]
    fn on_modifiers(&mut self, ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        self.modifiers = modifiers;
        if let Some(child) = self.focus.keyboard_target() {
            ctx.send_to(child, &modifiers);
        }
    }

    /// Keep dynamic routing availability synchronized with the external state
    /// a child actually adopted. Source attribution identifies the panel slot.
    #[handler::manual]
    fn on_widget_state_changed(&mut self, ctx: &mut WasmCtx<'_, Manual>, changed: WidgetStateChanged) {
        let Some(source) = ctx.source_mailbox() else {
            return;
        };
        let effects = self.focus.update_availability(source, &changed.state);
        apply_availability(ctx, effects);
    }

    /// Observe one descendant scroll container's exact typed outcome. The
    /// `container` field remains authoritative after intermediate scroll
    /// actors relay the event unchanged.
    #[allow(clippy::unused_self)] // actor handler ABI always receives state
    #[handler::manual]
    fn on_scroll_outcome(&mut self, ctx: &mut WasmCtx<'_, Manual>, outcome: ScrollOutcome) {
        tracing::info!(
            target: "aether_kit_widget",
            source = ctx.source_mailbox().unwrap_or(MailboxId::NONE).0,
            container = outcome.container.0,
            offset_x_pixels = outcome.offset.x_pixels,
            offset_y_pixels = outcome.offset.y_pixels,
            consumed_x_pixels = outcome.consumed.x_pixels,
            consumed_y_pixels = outcome.consumed.y_pixels,
            residual_x_pixels = outcome.residual.x_pixels,
            residual_y_pixels = outcome.residual.y_pixels,
            "widget scroll outcome",
        );
    }

    /// The root is the terminal residual sink. Log every named axis field and
    /// drop the remainder; no second wheel-sign conversion occurs here.
    #[allow(clippy::unused_self)] // actor handler ABI always receives state
    #[handler::manual]
    fn on_scroll_residual(&mut self, ctx: &mut WasmCtx<'_, Manual>, residual: ScrollResidual) {
        tracing::info!(
            target: "aether_kit_widget",
            source = ctx.source_mailbox().unwrap_or(MailboxId::NONE).0,
            residual_x_pixels = residual.x_pixels,
            residual_y_pixels = residual.y_pixels,
            "widget terminal scroll residual",
        );
    }

    /// A slider value-up. The map-editor seam: translate to world-knob driver
    /// mail here. The reference logs the attributed value.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::manual]
    fn on_slider_changed(&mut self, ctx: &mut WasmCtx<'_, Manual>, changed: SliderChanged) {
        tracing::info!(
            target: "aether_kit_widget",
            widget = self.child_name(ctx.source_mailbox()),
            value = changed.value,
            committed = changed.committed,
            "widget slider changed",
        );
    }

    /// A text-field commit. The map-editor seam; the reference logs it.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::manual]
    fn on_text_committed(&mut self, ctx: &mut WasmCtx<'_, Manual>, committed: TextCommitted) {
        tracing::info!(
            target: "aether_kit_widget",
            widget = self.child_name(ctx.source_mailbox()),
            text = %committed.text,
            "widget text committed",
        );
    }

    /// A radio selection. The map-editor seam; the reference logs it.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::manual]
    fn on_radio_selected(&mut self, ctx: &mut WasmCtx<'_, Manual>, selected: RadioSelected) {
        tracing::info!(
            target: "aether_kit_widget",
            widget = self.child_name(ctx.source_mailbox()),
            index = selected.index,
            "widget radio selected",
        );
    }

    /// A virtual-list selection. The map-editor seam; the reference logs it.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::manual]
    fn on_virtual_list_selected(&mut self, ctx: &mut WasmCtx<'_, Manual>, selected: VirtualListSelected) {
        tracing::info!(
            target: "aether_kit_widget",
            widget = self.child_name(ctx.source_mailbox()),
            selected_index = selected.selected_index,
            "widget virtual list selected",
        );
    }

    /// A button click. The map-editor seam; the reference logs it.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::manual]
    fn on_button_clicked(&mut self, ctx: &mut WasmCtx<'_, Manual>, _clicked: ButtonClicked) {
        tracing::info!(
            target: "aether_kit_widget",
            widget = self.child_name(ctx.source_mailbox()),
            "widget button clicked",
        );
    }

    /// A toggle value-up. The map-editor seam; the reference logs it.
    #[handler::manual]
    fn on_toggle_changed(&mut self, ctx: &mut WasmCtx<'_, Manual>, changed: ToggleChanged) {
        tracing::info!(
            target: "aether_kit_widget",
            widget = self.child_name(ctx.source_mailbox()),
            on = changed.on,
            "widget toggle changed",
        );
    }

    /// A segmented selection. The map-editor seam; the reference logs it.
    #[handler::manual]
    fn on_segmented_selected(&mut self, ctx: &mut WasmCtx<'_, Manual>, selected: SegmentedSelected) {
        tracing::info!(
            target: "aether_kit_widget",
            widget = self.child_name(ctx.source_mailbox()),
            index = selected.index,
            "widget segmented selected",
        );
    }

    /// A dropdown's choice. The map-editor seam; the reference logs it.
    #[handler::manual]
    fn on_dropdown_selected(&mut self, ctx: &mut WasmCtx<'_, Manual>, selected: DropdownSelected) {
        tracing::info!(
            target: "aether_kit_widget",
            widget = self.child_name(ctx.source_mailbox()),
            index = selected.index,
            "widget dropdown selected",
        );
    }

    /// A dropdown's list opened or closed. Not a value event: the root answers
    /// it by granting or ending the modal pointer grab, so a press anywhere on
    /// the window reaches the open list — the one input fact a widget cannot
    /// arrange for itself.
    #[handler::manual]
    fn on_dropdown_open_changed(&mut self, ctx: &mut WasmCtx<'_, Manual>, changed: DropdownOpenChanged) {
        let Some(source) = ctx.source_mailbox() else {
            return;
        };
        if changed.open {
            self.focus.begin_grab(source);
        } else if self.focus.grabbed() == Some(source) {
            self.focus.end_grab();
        }
    }

    /// A tab strip's selection. The map-editor seam; the reference logs it.
    #[handler::manual]
    fn on_tab_selected(&mut self, ctx: &mut WasmCtx<'_, Manual>, selected: TabSelected) {
        tracing::info!(
            target: "aether_kit_widget",
            widget = self.child_name(ctx.source_mailbox()),
            index = selected.index,
            "widget tab selected",
        );
    }

    /// A numeric preview or commit. The map-editor seam; the reference logs it.
    #[handler::manual]
    fn on_numeric_changed(&mut self, ctx: &mut WasmCtx<'_, Manual>, changed: NumericChanged) {
        tracing::info!(
            target: "aether_kit_widget",
            widget = self.child_name(ctx.source_mailbox()),
            value = changed.value,
            committed = changed.committed,
            "widget numeric changed",
        );
    }

    /// The font finished loading: stamp the real `font_id` into the theme and
    /// re-fan it so every child draws text with it.
    #[handler::single]
    fn on_load_font_result(&mut self, ctx: &mut WasmCtx<'_>, result: LoadFontResult) {
        match result {
            LoadFontResult::Ok { font_id, .. } => {
                self.theme.font_id = font_id;
                self.fan_theme(ctx);
            }
            LoadFontResult::Err { error, .. } => {
                tracing::warn!(target: "aether_kit_widget", %error, "panel font load failed");
            }
        }
    }

    /// A live restyle: adopt the new theme and re-fan it to every child.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        self.theme = set.theme;
        self.fan_theme(ctx);
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;

    #[test]
    fn nested_scroll_requires_the_exact_named_assigned_extent() {
        let assigned_extent = ScrollExtent { width_pixels: 80.0, height_pixels: 50.0 };
        let content = ChildLayout::Content { assigned_extent };
        assert_eq!(content.scroll_viewport_mismatch(assigned_extent), None);
        assert_eq!(
            content.scroll_viewport_mismatch(ScrollExtent { width_pixels: 80.0, height_pixels: 49.0 }),
            Some(assigned_extent),
        );
        assert_eq!(
            ChildLayout::Panel { row_height_pixels: 24.0 }
                .scroll_viewport_mismatch(ScrollExtent { width_pixels: 12.0, height_pixels: 8.0 }),
            None,
        );
    }

    #[test]
    fn scroll_config_decodes_from_the_closed_child_spec_and_rejects_bad_bytes() {
        let config = ScrollConfig {
            viewport_extent: ScrollExtent { width_pixels: 40.0, height_pixels: 30.0 },
            content_extent: ScrollExtent { width_pixels: 60.0, height_pixels: 90.0 },
            ..ScrollConfig::default()
        };
        let valid = WidgetChildSpec {
            subname: String::from("scroll"),
            kind: WidgetKind::Scroll,
            origin: [0.0, 0.0],
            clip: None,
            config: config.encode_into_bytes(),
        };
        let decoded = decode_child::<ScrollConfig>(&valid).expect("scroll config decodes");
        assert_eq!(decoded.viewport_extent, config.viewport_extent);
        assert_eq!(decoded.content_extent, config.content_extent);

        let malformed = WidgetChildSpec { config: vec![0xff], ..valid };
        assert!(decode_child::<ScrollConfig>(&malformed).is_none());
    }

    #[cfg(not(feature = "behavior"))]
    #[test]
    fn feature_off_keeps_behavior_host_and_scroll_unwrappable() {
        assert_eq!(WidgetKind::BehaviorHost.type_tag(), None);
        assert_eq!(WidgetKind::Scroll.type_tag(), None);
    }

    #[test]
    fn virtual_list_height_is_finite_and_preserves_zero_viewports() {
        assert_eq!(virtual_list_height(24.0, 5), Some(120.0));
        assert_eq!(virtual_list_height(24.0, 0), Some(0.0));
        assert_eq!(virtual_list_height(0.0, 5), None);
        assert_eq!(virtual_list_height(-1.0, 5), None);
        assert_eq!(virtual_list_height(f32::NAN, 5), None);
        assert_eq!(virtual_list_height(f32::MAX, 2), None);
    }
}

#[cfg(all(test, feature = "behavior"))]
mod behavior_tests {
    use super::*;
    use crate::{BehaviorHostSpec, ScriptRef};
    use aether_actor::ActorTypeTag;
    use aether_data::Kind;

    // Tripwire: `WidgetKind::type_tag` (the trunk accessor the `behavior`
    // spawn arm now calls directly) points each stock kind at its own
    // concrete widget type (not a transposed neighbour) and refuses to wrap
    // a container or a host. A mis-wired arm would spawn the wrong widget
    // under a host — silent until the pixels are wrong.
    #[test]
    fn wrapped_tag_maps_each_stock_widget_and_rejects_unwrappable() {
        assert_eq!(WidgetKind::Slider.type_tag(), Some(ActorTypeTag::of::<SliderWidget>().0));
        assert_eq!(WidgetKind::Button.type_tag(), Some(ActorTypeTag::of::<ButtonWidget>().0));
        assert_eq!(WidgetKind::Label.type_tag(), Some(ActorTypeTag::of::<LabelWidget>().0));
        assert_eq!(WidgetKind::Image.type_tag(), Some(ActorTypeTag::of::<ImageWidget>().0));
        assert_eq!(WidgetKind::Radio.type_tag(), Some(ActorTypeTag::of::<RadioGroupWidget>().0));
        assert_eq!(WidgetKind::TextField.type_tag(), Some(ActorTypeTag::of::<TextFieldWidget>().0));
        assert_eq!(WidgetKind::TextArea.type_tag(), Some(ActorTypeTag::of::<TextAreaWidget>().0));
        assert_eq!(WidgetKind::VirtualList.type_tag(), Some(ActorTypeTag::of::<VirtualListWidget>().0));
        assert_eq!(WidgetKind::Toggle.type_tag(), Some(ActorTypeTag::of::<ToggleWidget>().0));
        assert_eq!(WidgetKind::Segmented.type_tag(), Some(ActorTypeTag::of::<SegmentedWidget>().0));
        assert_eq!(WidgetKind::Numeric.type_tag(), Some(ActorTypeTag::of::<NumericWidget>().0));
        assert_eq!(WidgetKind::Composite.type_tag(), None);
        assert_eq!(WidgetKind::Scroll.type_tag(), None);
        assert_eq!(WidgetKind::BehaviorHost.type_tag(), None);
    }

    #[test]
    fn behavior_host_mirrors_scroll_observability_kinds() {
        let mirrored = behavior_mirror_kinds();
        assert!(mirrored.contains(&ScrollOutcome::ID.0));
        assert!(mirrored.contains(&ScrollResidual::ID.0));
    }

    // Tripwire: a `BehaviorHostSpec` carrying a stock wrapped widget encodes as
    // its `WidgetChildSpec.config` and decodes back through `decode_child`, so
    // the panel arm can recover the wrapped kind + script it was handed.
    #[test]
    fn host_spec_round_trips_through_child_config() {
        let spec = BehaviorHostSpec {
            wrapped: WidgetKind::Slider,
            wrapped_config: vec![1, 2, 3],
            script: ScriptRef::FsRef { namespace: String::from("assets"), path: String::from("scripts/knob.wasm") },
            fuel_per_call: 0,
            disable_after_traps: 0,
        };
        let child = WidgetChildSpec {
            subname: String::from("knob"),
            kind: WidgetKind::BehaviorHost,
            origin: [0.0, 0.0],
            clip: None,
            config: spec.encode_into_bytes(),
        };
        let decoded = decode_child::<BehaviorHostSpec>(&child).expect("host spec decodes");
        assert_eq!(decoded.wrapped, WidgetKind::Slider);
        assert!(matches!(decoded.script, ScriptRef::FsRef { .. }));
    }
}
