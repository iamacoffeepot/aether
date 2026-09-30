// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (the full rationale is on the same allow in `lib.rs`).
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
//!   `RadioSelected` / `VirtualListSelected` / `VirtualListActivated` /
//!   `VirtualListHover` / `DropdownHover` /
//!   `ButtonActivated` / `ToggleChanged` /
//!   `SegmentedSelected` / `NumericChanged` / `DropdownSelected` /
//!   `TabStripSelected`), attributed by
//!   `ctx.sender()`, is the seam a real editor translates into
//!   world-knob driver mail; the reference logs it.
//! - **Grab.** `WidgetOpenChanged` is the one events-up kind the root
//!   answers itself: an open list or menu takes the modal pointer grab
//!   (`Focus::begin_grab`) so every press reaches it wherever it lands, and
//!   the close gives it back.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use aether_actor::{
    ActorInitError, Addressable, DependsOn, Erased, ErasedActorRef, ErasedWasmActor, ModuleChild, ReplyMode, Sends,
    Spawns, Subname, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_data::Kind;
use aether_kinds::keycode::KEY_TAB;
use aether_kinds::mouse_button;
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput, Tick,
};
use aether_lifecycle::LifecycleCapability;
use aether_math::Vec2;
use aether_render::RenderCapability;
use aether_text::{LoadFont, LoadFontResult, TextCapability};
use aether_window::WindowCapability;

use crate::composite::Composite;
use crate::focus::{
    AvailabilityEffects, Focus, FocusDirection, FocusEligibility, FocusRect, FocusTransition, HoverTransition,
};
use crate::lanes::{WidgetLaneSet, WidgetLanes};
use crate::layout::{Cell, Column, Row};
use crate::set::{
    ButtonWidget, DropdownWidget, ImageWidget, LabelWidget, MenuBarWidget, NumericWidget, RadioGroupWidget,
    SegmentedWidget, SliderWidget, TabStripWidget, TextAreaWidget, TextFieldWidget, ToggleWidget, VirtualListWidget,
    quad,
};
use crate::theme::{SetTheme, TextRole, Theme};
use crate::{
    ButtonActivated, ButtonConfig, Collect, DropdownConfig, DropdownHover, DropdownSelected, EditorRegion, FocusGained,
    FocusLost, HoverGained, HoverLost, ImageConfig, LabelConfig, MenuBarActivated, MenuBarConfig, NumericChanged,
    NumericConfig, PanelConfig, RadioConfig, RadioSelected, ScrollConfig, ScrollExtent, ScrollOutcome, ScrollResidual,
    ScrollWidget, SegmentedConfig, SegmentedSelected, SliderChanged, SliderConfig, TabStripConfig, TabStripSelected,
    TextAlign, TextAreaConfig, TextCommitted, TextFieldConfig, ToggleChanged, ToggleConfig, VirtualListActivated,
    VirtualListConfig, VirtualListHover, VirtualListSelected, Widget, WidgetChildSpec, WidgetClipRect,
    WidgetControlState, WidgetDrawList, WidgetEligibilityChanged, WidgetFrame, WidgetKind, WidgetOpenChanged,
    WidgetStateChanged,
};
use crate::{FrameDischarge, decode_nested_widget_config};
use crate::{accept_open_child_list, emit, flush_membership};

/// One spawned child's lanes plus the logical name the panel attributes its
/// value-up events under (for the map-editor translation / logging) — the
/// child's spec subname.
struct ChildRef {
    lanes: WidgetLanes,
    name: String,
}

#[derive(Clone, Copy)]
pub enum ChildLayout {
    Panel { row_height_pixels: f32 },
    Content { assigned_extent: ScrollExtent },
}

impl ChildLayout {
    pub(crate) fn row_height_pixels(self) -> f32 {
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

/// One spawned child as its host holds it: the lanes it is sent through, and
/// the profile its host places and routes it by.
pub struct SpawnedChild {
    pub lanes: WidgetLanes,
    pub profile: ChildProfile,
}

/// What a host needs to place and route one spawned child, apart from the
/// lanes it sends through: its size, its eligibility for pointer, keyboard,
/// and wheel input, and its type's namespace for the membership record.
pub struct ChildProfile {
    pub width_pixels: Option<f32>,
    pub height_pixels: f32,
    pub pointer_eligible: bool,
    pub focusable: bool,
    pub state: WidgetControlState,
    pub type_namespace: &'static str,
    pub scroll_viewport: Option<ScrollExtent>,
    /// The gap units of the clear column this child draws in **beside** its
    /// frame, `None` when everything it draws is inside it. A virtual list
    /// configured with `VirtualListConfig::host_scroll_strip` draws its track
    /// past its own right edge, so the host owes it that column as clip *and*
    /// as hit area: a slot clipped to the frame erases the bar, and a press
    /// where the bar is reaches nothing.
    pub host_scroll_strip_units: Option<u8>,
    /// Whether this child scrolls **itself** on the wheel: a virtual list owns
    /// its realized window, so the wheel over it belongs to it rather than to
    /// the nearest scroll container. It joins the same wheel-only hit table a
    /// scroll viewport does, which is what keeps a drag capture from stealing
    /// a wheel gesture.
    pub wheel_eligible: bool,
}

impl ChildProfile {
    /// The profile of a `C` laid out as one full-width row of `height_pixels`
    /// that draws only inside its frame and does not scroll itself: every stock
    /// control but the virtual list, the composite, and the scroll container.
    fn row<C: Addressable>(height_pixels: f32, eligibility: FocusEligibility, state: WidgetControlState) -> Self {
        Self {
            width_pixels: None,
            height_pixels,
            pointer_eligible: eligibility.pointer,
            focusable: eligibility.keyboard,
            state,
            type_namespace: C::NAMESPACE,
            scroll_viewport: None,
            host_scroll_strip_units: None,
            wheel_eligible: false,
        }
    }
}

struct VirtualListProfile {
    height: f32,
    eligible: bool,
}

/// The reference panel root. Loaded as a component with a [`PanelConfig`]; its
/// export name is `aether.widget.panel`.
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
    /// A live `SetTheme` or resolved `LoadFontResult` arrived before children existed.
    pending_style: bool,
    /// The total stack height, for the background chrome; set at spawn.
    panel_height: f32,
    /// Latest modifier state: Tab direction, and the chord fanned to a child
    /// that just gained focus. `None` until the first `Modifiers` arrives,
    /// which reads as no modifier held.
    modifiers: Option<Modifiers>,
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
    fn ensure_spawned<A: SpawnsWidgets, M: ReplyMode>(&mut self, ctx: &mut WasmCtx<'_, A, M>) {
        if self.spawned {
            return;
        }
        self.spawned = true;

        let row = self.theme.row_height;

        let specs = if self.config.children.is_empty() {
            reference_stack(&self.theme)
        } else {
            self.config.children.clone()
        };

        // Decode the concrete config, spawn the kind's actor, and derive the
        // row height + focusability from that config — plus the spawned
        // type's `NAMESPACE` for the membership record, carried as data
        // because the type is erased past that match. `None` from any arm
        // (an undecodable config, a spawn failure, or a rejected container)
        // skips the slot entirely so the stack stays honest.
        let spawned: Vec<(SpawnedChild, String)> = specs
            .iter()
            .filter_map(|spec| {
                spawn_widget_child(ctx, spec, ChildLayout::Panel { row_height_pixels: row })
                    .map(|child| (child, spec.subname.clone()))
            })
            .collect();

        let placed =
            stack_column(&self.config, self.theme.gap).place(&stack_rows(spawned.iter().map(|(child, _)| child)));
        for ((child, name), (_, frame)) in spawned.iter().zip(placed.frames) {
            self.place(ctx, child, frame, name.clone());
        }

        self.panel_height = placed.height;
        // Replay only a live update that beat the first Tick. Spawning with no such
        // update must keep each child's own config theme.
        if self.pending_style {
            self.fan_theme(ctx);
            self.pending_style = false;
        }
    }

    /// Record one spawned child's rect into the composite (as its draw offset,
    /// under its `name` subname and the spawned child type's namespace) and
    /// the focus table (as its hit rect), send it its `WidgetFrame`, and
    /// remember it for value-up attribution.
    ///
    /// `assigned` is the whole rectangle the stack gave this child. The slot
    /// clip and both hit rects are that rectangle; what the child is *handed*
    /// is [`content_frame`], the same rectangle less the clear column a
    /// host-owned scroll bar stands in. Reserving the column that way is what
    /// the flag asks of a host: the bar ends up beside the rows and still
    /// inside the panel, and the clip — which was already the assigned
    /// rectangle — reaches across it, where a track drawn past the full width
    /// would have been clipped away with a press over it reaching nothing.
    fn place<A, M: ReplyMode>(
        &mut self,
        ctx: &mut WasmCtx<'_, A, M>,
        child: &SpawnedChild,
        assigned: WidgetFrame,
        name: String,
    ) {
        let SpawnedChild { lanes, profile } = child;
        let key = lanes.key();
        let frame = content_frame(&assigned, self.scroll_strip_pixels(profile));
        let focus_rect = FocusRect { x: assigned.x, y: assigned.y, width: assigned.width, height: assigned.height };
        self.composite.register_slot(
            key,
            Vec2::new(assigned.x, assigned.y),
            Some(WidgetClipRect { x: assigned.x, y: assigned.y, width: assigned.width, height: assigned.height }),
            &name,
            profile.type_namespace,
        );
        self.focus.register(
            key,
            focus_rect,
            FocusEligibility { pointer: profile.pointer_eligible, keyboard: profile.focusable },
            &profile.state,
        );
        if profile.scroll_viewport.is_some() || profile.wheel_eligible {
            self.scroll_focus.register(
                key,
                focus_rect,
                FocusEligibility { pointer: true, keyboard: false },
                &WidgetControlState::default(),
            );
        }
        if let Some(styled) = lanes.styled {
            ctx.send_to(styled, &frame);
        }
        self.children.push(ChildRef { lanes: *lanes, name });
    }

    /// How wide a column this child's scroll bar stands in beside its rows,
    /// in the panel's own live theme — the theme the panel fans down to every
    /// child, so the column it reserves is the one the child draws in.
    fn scroll_strip_pixels(&self, child: &ChildProfile) -> f32 {
        child.host_scroll_strip_units.map_or(0.0, |units| VirtualListConfig::host_strip_width(units, &self.theme))
    }

    /// Discharge a closed frame: flatten the composite and emit it from the
    /// panel's single render + text sender.
    fn finish<A: DependsOn<RenderCapability> + DependsOn<TextCapability>, M: ReplyMode>(
        &mut self,
        ctx: &mut WasmCtx<'_, A, M>,
    ) {
        if self.frame_discharge.is_closed() {
            return;
        }
        let list = self.composite.flatten(None);
        emit(ctx, &list);
        let closed = self.frame_discharge.close_frame();
        debug_assert!(closed, "an open panel frame closes exactly once");
    }

    /// Re-fan the live theme to every child (after a font stamp or a restyle).
    fn fan_theme<A, M: ReplyMode>(&self, ctx: &mut WasmCtx<'_, A, M>) {
        for styled in self.children.iter().filter_map(|child| child.lanes.styled) {
            ctx.send_to(styled, &SetTheme { theme: self.theme.clone() });
        }
    }

    /// Adopt a live style change now, and either fan it immediately or keep it
    /// until the first successful spawn so the FIFO drain applies it before Collect.
    fn retain_or_fan_theme<A, M: ReplyMode>(&mut self, ctx: &mut WasmCtx<'_, A, M>) {
        if self.spawned {
            self.fan_theme(ctx);
            self.pending_style = false;
        } else {
            self.pending_style = true;
        }
    }

    /// The logical name of the child a value-up event came from, for
    /// attribution.
    fn child_name(&self, source: Option<ErasedActorRef>) -> &str {
        source.and_then(|source| self.child(source)).map_or("unknown", |child| child.name.as_str())
    }

    /// The child whose key is `key`: the lookup from a routing table's answer
    /// to the child it names.
    fn child(&self, key: ErasedActorRef) -> Option<&ChildRef> {
        self.children.iter().find(|child| child.lanes.key() == key)
    }

    /// The lanes of the child whose key is `key`, to send it what a routing
    /// decision chose it for.
    fn lanes(&self, key: ErasedActorRef) -> Option<WidgetLanes> {
        self.child(key).map(|child| child.lanes)
    }

    /// The lanes of the child keyboard input goes to, if one holds focus.
    fn focused_lanes(&self) -> Option<WidgetLanes> {
        self.lanes(self.focus.keyboard_target()?)
    }

    /// Send a focus transition down: `FocusLost` to the child that lost focus,
    /// then `FocusGained` and the panel's latest [`Modifiers`] to the one that
    /// gained it. Lost still goes first. `keyboard` rides on the gain so the
    /// child knows whether to draw its ring (see [`FocusGained`]). Before the
    /// first `Modifiers` arrives there is none to send, and a child that has
    /// none reads the same no-modifier state. Only a child with a text-entry
    /// lane caches the chord, so only it is primed.
    fn apply_focus<A>(&self, sends: &mut Sends<'_, A>, transition: FocusTransition, keyboard: bool) {
        let FocusTransition { previous, next } = transition;
        if let Some(control) = previous.and_then(|key| self.lanes(key)?.control) {
            sends.send_to(control, &FocusLost);
        }
        if let Some(gained) = next.and_then(|key| self.lanes(key)) {
            if let Some(control) = gained.control {
                sends.send_to(control, &FocusGained { keyboard });
            }
            if let (Some(text_entry), Some(modifiers)) = (gained.text_entry, &self.modifiers) {
                sends.send_to(text_entry, modifiers);
            }
        }
    }

    /// Send hover edges lost-before-gained so sibling crossings cannot leave
    /// two controls hovered during the breadth-first drain.
    fn apply_hover<A>(&self, sends: &mut Sends<'_, A>, transition: HoverTransition) {
        let HoverTransition { previous, next } = transition;
        if let Some(hover) = previous.and_then(|key| self.lanes(key)?.hover) {
            sends.send_to(hover, &HoverLost);
        }
        if let Some(hover) = next.and_then(|key| self.lanes(key)?.hover) {
            sends.send_to(hover, &HoverGained);
        }
    }

    fn apply_availability<A>(&self, sends: &mut Sends<'_, A>, effects: AvailabilityEffects) {
        if let Some(hover) = effects.hover {
            self.apply_hover(sends, hover);
        }
        if let Some(focus) = effects.focus {
            self.apply_focus(sends, focus, false);
        }
    }
}

/// The [`Column`] the panel stacks its children down: the top-left and width
/// its config names, with the live theme's `gap` between rows.
///
/// The panel owns layout, so this is the one place the stack's geometry is
/// stated. `gap` is passed rather than read off the config because the theme
/// the panel actually fans down is the one whose spacing the children draw to.
fn stack_column(config: &PanelConfig, gap: f32) -> Column {
    Column { origin: Vec2::new(config.x, config.y), width: config.width, gap }
}

/// One [`Row`] per spawned child, in spec order, keyed by that child's proof.
///
/// A spec that failed to spawn never reaches here, so it contributes neither a
/// row nor the gap that would have preceded it.
fn stack_rows<'a>(children: impl IntoIterator<Item = &'a SpawnedChild>) -> Vec<Row<ErasedActorRef>> {
    children
        .into_iter()
        .map(|child| stack_row(child.lanes.key(), child.profile.width_pixels, child.profile.height_pixels))
        .collect()
}

/// One child's [`Row`], keyed by `key`.
///
/// Each row is the height the child's own config asked for. A child that named
/// its own width ([`ChildProfile::width_pixels`] — a button sized to its label)
/// gets exactly that width; one that did not takes the whole column.
fn stack_row<K: Copy>(key: K, width_pixels: Option<f32>, height_pixels: f32) -> Row<K> {
    let cell = width_pixels.map_or_else(|| Cell::share(key, 1.0), |pixels| Cell::fixed(key, pixels));
    Row::cells(height_pixels, vec![cell])
}

/// The sixteen stock widget children [`spawn_widget_child`] can spawn inline,
/// as one bound. [`WidgetPanel`] and [`ScrollWidget`] declare all sixteen in
/// their `#[actor(spawns(..))]`, so a generic helper that reaches
/// [`spawn_widget_child`] names this bound instead of sixteen [`Spawns`]
/// bounds. The blanket impl is the only one: an actor has it exactly when it
/// declares every one of the sixteen.
pub trait SpawnsWidgets:
    Spawns<LabelWidget>
    + Spawns<ImageWidget>
    + Spawns<SliderWidget>
    + Spawns<RadioGroupWidget>
    + Spawns<TextFieldWidget>
    + Spawns<TextAreaWidget>
    + Spawns<ButtonWidget>
    + Spawns<VirtualListWidget>
    + Spawns<Widget>
    + Spawns<ScrollWidget>
    + Spawns<ToggleWidget>
    + Spawns<SegmentedWidget>
    + Spawns<NumericWidget>
    + Spawns<DropdownWidget>
    + Spawns<TabStripWidget>
    + Spawns<MenuBarWidget>
{
}

impl<A> SpawnsWidgets for A where
    A: Spawns<LabelWidget>
        + Spawns<ImageWidget>
        + Spawns<SliderWidget>
        + Spawns<RadioGroupWidget>
        + Spawns<TextFieldWidget>
        + Spawns<TextAreaWidget>
        + Spawns<ButtonWidget>
        + Spawns<VirtualListWidget>
        + Spawns<Widget>
        + Spawns<ScrollWidget>
        + Spawns<ToggleWidget>
        + Spawns<SegmentedWidget>
        + Spawns<NumericWidget>
        + Spawns<DropdownWidget>
        + Spawns<TabStripWidget>
        + Spawns<MenuBarWidget>
{
}

/// Decode, spawn, and derive one panel child's static/dynamic routing profile.
/// Keeping this dispatch out of `ensure_spawned` leaves the layout loop focused
/// on ordering and placement.
pub fn spawn_widget_child<A: SpawnsWidgets, M: ReplyMode>(
    ctx: &mut WasmCtx<'_, A, M>,
    spec: &WidgetChildSpec,
    layout: ChildLayout,
) -> Option<SpawnedChild> {
    let row = layout.row_height_pixels();
    match spec.kind {
        WidgetKind::Label
        | WidgetKind::Image
        | WidgetKind::Slider
        | WidgetKind::Radio
        | WidgetKind::TextField
        | WidgetKind::TextArea => spawn_content_child(ctx, spec, row),
        WidgetKind::Button => spawn_button_child(ctx, spec, row),
        WidgetKind::VirtualList => spawn_virtual_list_child(ctx, spec, row),
        WidgetKind::Toggle
        | WidgetKind::Segmented
        | WidgetKind::Numeric
        | WidgetKind::Dropdown
        | WidgetKind::TabStrip
        | WidgetKind::MenuBar => spawn_row_control_child(ctx, spec, row),
        WidgetKind::Composite => spawn_composite_child(ctx, spec, layout, row),
        WidgetKind::Scroll => spawn_scroll_child(ctx, spec, layout),
    }
}

/// Spawn the display and value children whose profile is a fixed height and a
/// fixed eligibility pair. Their decode/spawn bodies are mechanically alike,
/// so they sit together here for the same reason
/// [`spawn_row_control_child`] does: the exhaustive dispatcher above stays a
/// dispatcher, and a reader looking for one kind's profile finds every
/// sibling profile beside it.
fn spawn_content_child<A: SpawnsWidgets, M: ReplyMode>(
    ctx: &mut WasmCtx<'_, A, M>,
    spec: &WidgetChildSpec,
    row: f32,
) -> Option<SpawnedChild> {
    match spec.kind {
        WidgetKind::Label => decode_child::<LabelConfig>(spec).and_then(|config| {
            let lanes = spawn::<LabelWidget, A, M>(ctx, &spec.subname, &config)?;
            Some(SpawnedChild {
                lanes,
                profile: ChildProfile::row::<LabelWidget>(
                    row,
                    // Pointer-eligible for hover only: a label whose text is wider
                    // than its slot reveals the rest on a raised plate while the
                    // pointer is over it, and hover edges follow the pointer hit
                    // table. Still never focusable, so a press on a label clears
                    // focus like a press on the background.
                    FocusEligibility { pointer: true, keyboard: false },
                    config.state,
                ),
            })
        }),
        WidgetKind::Image => decode_child::<ImageConfig>(spec).and_then(|config| {
            let lanes = spawn::<ImageWidget, A, M>(ctx, &spec.subname, &config)?;
            Some(SpawnedChild {
                lanes,
                profile: ChildProfile::row::<ImageWidget>(
                    row,
                    FocusEligibility { pointer: false, keyboard: false },
                    config.state,
                ),
            })
        }),
        WidgetKind::Slider => decode_child::<SliderConfig>(spec).and_then(|config| {
            let lanes = spawn::<SliderWidget, A, M>(ctx, &spec.subname, &config)?;
            Some(SpawnedChild {
                lanes,
                profile: ChildProfile::row::<SliderWidget>(
                    row,
                    FocusEligibility { pointer: true, keyboard: true },
                    config.state,
                ),
            })
        }),
        WidgetKind::Radio => decode_child::<RadioConfig>(spec).and_then(|config| {
            let height = row * config.options.len() as f32;
            let lanes = spawn::<RadioGroupWidget, A, M>(ctx, &spec.subname, &config)?;
            Some(SpawnedChild {
                lanes,
                profile: ChildProfile::row::<RadioGroupWidget>(
                    height,
                    FocusEligibility { pointer: true, keyboard: true },
                    config.state,
                ),
            })
        }),
        WidgetKind::TextField => decode_child::<TextFieldConfig>(spec).and_then(|config| {
            let lanes = spawn::<TextFieldWidget, A, M>(ctx, &spec.subname, &config)?;
            Some(SpawnedChild {
                lanes,
                profile: ChildProfile::row::<TextFieldWidget>(
                    row,
                    FocusEligibility { pointer: true, keyboard: true },
                    config.state,
                ),
            })
        }),
        WidgetKind::TextArea => decode_child::<TextAreaConfig>(spec).and_then(|config| {
            let height = row * config.rows.max(1) as f32;
            let lanes = spawn::<TextAreaWidget, A, M>(ctx, &spec.subname, &config)?;
            Some(SpawnedChild {
                lanes,
                profile: ChildProfile::row::<TextAreaWidget>(
                    height,
                    FocusEligibility { pointer: true, keyboard: true },
                    config.state,
                ),
            })
        }),
        _ => None,
    }
}

fn spawn_button_child<A: Spawns<ButtonWidget>, M: ReplyMode>(
    ctx: &mut WasmCtx<'_, A, M>,
    spec: &WidgetChildSpec,
    row: f32,
) -> Option<SpawnedChild> {
    let config = decode_child::<ButtonConfig>(spec)?;
    let lanes = spawn::<ButtonWidget, A, M>(ctx, &spec.subname, &config)?;
    Some(SpawnedChild {
        lanes,
        profile: ChildProfile::row::<ButtonWidget>(
            row,
            FocusEligibility { pointer: true, keyboard: true },
            config.state,
        ),
    })
}

fn spawn_virtual_list_child<A: Spawns<VirtualListWidget>, M: ReplyMode>(
    ctx: &mut WasmCtx<'_, A, M>,
    spec: &WidgetChildSpec,
    row: f32,
) -> Option<SpawnedChild> {
    let config = decode_child::<VirtualListConfig>(spec)?;
    let profile = virtual_list_profile(&spec.subname, row, &config)?;
    let state = config.state.clone();
    let host_scroll_strip_units = host_scroll_strip_units(&config);
    spawn::<VirtualListWidget, A, M>(ctx, &spec.subname, &config).map(|lanes| SpawnedChild {
        lanes,
        profile: ChildProfile {
            width_pixels: None,
            height_pixels: profile.height,
            pointer_eligible: profile.eligible,
            focusable: profile.eligible,
            state,
            type_namespace: <VirtualListWidget as Addressable>::NAMESPACE,
            host_scroll_strip_units,
            wheel_eligible: true,
            scroll_viewport: None,
        },
    })
}

/// The gap units a list's scroll strip is measured from, or `None` when its
/// bar comes out of its own frame. `VirtualListConfig::host_scroll_strip` is a
/// request to the host: the list draws its track past its right edge and takes
/// nothing off its rows, so the column has to come from whoever placed it.
fn host_scroll_strip_units(config: &VirtualListConfig) -> Option<u8> {
    config.host_scroll_strip.then_some(config.scroll_bar_gap_units)
}

/// The frame a child lays its content in, out of the rectangle the stack
/// assigned it: that rectangle less the clear column a host-owned scroll bar
/// stands in (`VirtualListConfig::host_scroll_strip`). The panel keeps
/// clipping and hit-testing the slot by the whole assigned rectangle, so the
/// column stays inside the panel and the track drawn in it is neither clipped
/// away nor unreachable by a press. A strip that is not a positive, finite
/// number, or one wider than the assignment, reserves nothing.
#[must_use]
pub fn content_frame(assigned: &WidgetFrame, strip_pixels: f32) -> WidgetFrame {
    let strip = if strip_pixels.is_finite() && (0.0..=assigned.width).contains(&strip_pixels) {
        strip_pixels
    } else {
        0.0
    };
    WidgetFrame { x: assigned.x, y: assigned.y, width: assigned.width - strip, height: assigned.height }
}

fn virtual_list_profile(subname: &str, row_height: f32, config: &VirtualListConfig) -> Option<VirtualListProfile> {
    let Some(height) = virtual_list_height(row_height, config.visible_row_count) else {
        tracing::warn!(
            target: "aether_widget",
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

fn spawn_composite_child<A: Spawns<Widget>, M: ReplyMode>(
    ctx: &mut WasmCtx<'_, A, M>,
    spec: &WidgetChildSpec,
    layout: ChildLayout,
    row_height_pixels: f32,
) -> Option<SpawnedChild> {
    if matches!(layout, ChildLayout::Panel { .. }) {
        tracing::warn!(
            target: "aether_widget",
            subname = %spec.subname,
            "a bare Composite child is supported only as scroll content; panel slot skipped",
        );
        return None;
    }
    decode_nested_widget_config(spec).and_then(|config| {
        let intrinsic = config.intrinsic;
        spawn::<Widget, A, M>(ctx, &spec.subname, &config).map(|lanes| SpawnedChild {
            lanes,
            profile: ChildProfile {
                width_pixels: intrinsic
                    .and_then(|extent| (extent[0].is_finite() && extent[0] >= 0.0).then_some(extent[0])),
                height_pixels: intrinsic
                    .and_then(|extent| (extent[1].is_finite() && extent[1] >= 0.0).then_some(extent[1]))
                    .unwrap_or(row_height_pixels),
                pointer_eligible: false,
                focusable: false,
                state: WidgetControlState::default(),
                type_namespace: <Widget as Addressable>::NAMESPACE,
                host_scroll_strip_units: None,
                wheel_eligible: false,
                scroll_viewport: None,
            },
        })
    })
}

fn spawn_scroll_child<A: Spawns<ScrollWidget>, M: ReplyMode>(
    ctx: &mut WasmCtx<'_, A, M>,
    spec: &WidgetChildSpec,
    layout: ChildLayout,
) -> Option<SpawnedChild> {
    decode_child::<ScrollConfig>(spec).and_then(|config| {
        if let Some(assigned_extent) = layout.scroll_viewport_mismatch(config.viewport_extent) {
            tracing::warn!(
                target: "aether_widget",
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
        spawn::<ScrollWidget, A, M>(ctx, &spec.subname, &config).map(|lanes| SpawnedChild {
            lanes,
            profile: ChildProfile {
                width_pixels: Some(viewport.width_pixels),
                height_pixels: viewport.height_pixels,
                pointer_eligible: false,
                focusable: false,
                state: WidgetControlState::default(),
                type_namespace: <ScrollWidget as Addressable>::NAMESPACE,
                host_scroll_strip_units: None,
                wheel_eligible: false,
                scroll_viewport: Some(viewport),
            },
        })
    })
}

/// Spawn the one-row control children. Keeping their mechanical decode/spawn
/// profiles together prevents the main exhaustive dispatcher from becoming a
/// second long-form implementation surface.
fn spawn_row_control_child<A: SpawnsWidgets, M: ReplyMode>(
    ctx: &mut WasmCtx<'_, A, M>,
    spec: &WidgetChildSpec,
    row: f32,
) -> Option<SpawnedChild> {
    match spec.kind {
        WidgetKind::Toggle => decode_child::<ToggleConfig>(spec).and_then(|config| {
            spawn::<ToggleWidget, A, M>(ctx, &spec.subname, &config).map(|lanes| SpawnedChild {
                lanes,
                profile: ChildProfile::row::<ToggleWidget>(
                    row,
                    FocusEligibility { pointer: true, keyboard: true },
                    config.state,
                ),
            })
        }),
        WidgetKind::Segmented => decode_child::<SegmentedConfig>(spec).and_then(|config| {
            spawn::<SegmentedWidget, A, M>(ctx, &spec.subname, &config).map(|lanes| SpawnedChild {
                lanes,
                profile: ChildProfile::row::<SegmentedWidget>(
                    row,
                    FocusEligibility { pointer: true, keyboard: true },
                    config.state,
                ),
            })
        }),
        WidgetKind::Numeric => decode_child::<NumericConfig>(spec).and_then(|config| {
            spawn::<NumericWidget, A, M>(ctx, &spec.subname, &config).map(|lanes| SpawnedChild {
                lanes,
                profile: ChildProfile::row::<NumericWidget>(
                    row,
                    FocusEligibility { pointer: true, keyboard: true },
                    config.state,
                ),
            })
        }),
        WidgetKind::Dropdown => decode_child::<DropdownConfig>(spec).and_then(|config| {
            spawn::<DropdownWidget, A, M>(ctx, &spec.subname, &config).map(|lanes| SpawnedChild {
                lanes,
                profile: ChildProfile::row::<DropdownWidget>(
                    row,
                    FocusEligibility { pointer: true, keyboard: true },
                    config.state,
                ),
            })
        }),
        WidgetKind::TabStrip => decode_child::<TabStripConfig>(spec).and_then(|config| {
            spawn::<TabStripWidget, A, M>(ctx, &spec.subname, &config).map(|lanes| SpawnedChild {
                lanes,
                profile: ChildProfile::row::<TabStripWidget>(
                    row,
                    FocusEligibility { pointer: true, keyboard: true },
                    config.state,
                ),
            })
        }),
        WidgetKind::MenuBar => decode_child::<MenuBarConfig>(spec).and_then(|config| {
            spawn::<MenuBarWidget, A, M>(ctx, &spec.subname, &config).map(|lanes| SpawnedChild {
                lanes,
                profile: ChildProfile::row::<MenuBarWidget>(
                    row,
                    FocusEligibility { pointer: true, keyboard: true },
                    config.state,
                ),
            })
        }),
        _ => None,
    }
}

/// Decode one child spec's opaque config bytes as the concrete config type
/// `C` its [`WidgetKind`] selects, warning and yielding `None` on a decode
/// failure so the caller skips the slot (mirroring `decode_nested_widget_config`
/// in `lib.rs`).
fn decode_child<C: Kind>(spec: &WidgetChildSpec) -> Option<C> {
    decode_named(&spec.subname, &spec.config)
}

fn decode_named<C: Kind>(subname: &str, bytes: &[u8]) -> Option<C> {
    let config = C::decode_from_bytes(bytes);
    if config.is_none() {
        tracing::warn!(
            target: "aether_widget",
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
                initial: 0,
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
            ButtonConfig { label: String::from("Apply"), theme: theme.clone(), ..ButtonConfig::default() }
                .encode_into_bytes(),
        ),
    ]
}

/// Spawn one inline widget under the caller's actual logical actor type and
/// narrow it to the lanes its type covers, logging and dropping the slot on
/// failure.
fn spawn<C, A, M: ReplyMode>(ctx: &mut WasmCtx<'_, A, M>, subname: &str, config: &C::Config) -> Option<WidgetLanes>
where
    C: ModuleChild + ErasedWasmActor + WidgetLaneSet,
    <C as WasmActor>::State: ErasedWasmActor,
    A: Spawns<C>,
{
    match ctx.spawn_inline::<C>(Subname::Named(subname), config) {
        Ok(child) => Some(C::lanes(child)),
        Err(error) => {
            tracing::warn!(
                target: "aether_widget",
                subname,
                ?error,
                "widget spawn failed; slot skipped",
            );
            None
        }
    }
}

/// The reference panel root. Load it as a component (export
/// `aether.widget.panel`) with a [`PanelConfig`].
///
/// # Agent
/// Load `aether_widget.wasm` with `export: "aether.widget.panel"` and a
/// `PanelConfig` (top-left, width, theme, a font to load, and the `children`
/// it stacks). With an empty `children` list it spawns a demonstration stack
/// of the original interactive/reference widgets; otherwise it stacks exactly
/// the declared specs (including `Image`, `Toggle`, `Segmented`, and `Numeric`
/// children). It routes
/// real input through the focus model and logs each value-up event. Fork it
/// into a real editor panel by handing it your own `children` and translating
/// the value-up handlers into your own world-knob driver mail.
///
/// A panel behind an editor-shell region (ADR-0141) is not loaded directly:
/// load [`EditorRegion`] (export `aether.widget.editor_region`) with this
/// config, and it hosts the panel as its child. A panel refuses a non-empty
/// `editor_region`.
#[actor(
    instanced,
    root,
    child_of(EditorRegion),
    depends(WindowCapability, LifecycleCapability, RenderCapability, TextCapability),
    spawns(
        LabelWidget,
        ImageWidget,
        SliderWidget,
        RadioGroupWidget,
        TextFieldWidget,
        TextAreaWidget,
        ButtonWidget,
        VirtualListWidget,
        Widget,
        ScrollWidget,
        ToggleWidget,
        SegmentedWidget,
        NumericWidget,
        DropdownWidget,
        TabStripWidget,
        MenuBarWidget
    )
)]
impl WasmActor for WidgetPanel {
    type Config = PanelConfig;
    const NAMESPACE: &'static str = "aether.widget.panel";

    fn init(config: PanelConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        if !config.editor_region.is_empty() {
            return Err(ActorInitError::from(
                "editor_region is announced by aether.widget.editor_region; load that export instead",
            ));
        }
        Ok(WidgetPanel {
            theme: config.theme.clone(),
            config,
            composite: Composite::new(),
            frame_discharge: FrameDischarge::default(),
            focus: Focus::new(),
            scroll_focus: Focus::new(),
            children: Vec::new(),
            spawned: false,
            pending_style: false,
            panel_height: 0.0,
            modifiers: None,
        })
    }

    /// Subscribe to pointer / keyboard streams from every window and the frame
    /// stage once, then kick off the font load. Widgets never subscribe — the
    /// root forwards everything.
    ///
    /// A panel that owns no input subscribes none: it is the child of an
    /// [`EditorRegion`], which relays the editor shell's input to it.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        if self.config.owns_input {
            ctx.subscribe::<WindowCapability, MouseButton>();
            ctx.subscribe::<WindowCapability, MouseButtonRelease>();
            ctx.subscribe::<WindowCapability, MouseMove>();
            ctx.subscribe::<WindowCapability, MouseWheel>();
            ctx.subscribe::<WindowCapability, Key>();
            ctx.subscribe::<WindowCapability, KeyRelease>();
            ctx.subscribe::<WindowCapability, TextInput>();
            ctx.subscribe::<WindowCapability, ImePreedit>();
            ctx.subscribe::<WindowCapability, Modifiers>();
        }
        ctx.subscribe::<LifecycleCapability, Tick>();
        if !self.config.font_path.is_empty() {
            ctx.send::<TextCapability>(&LoadFont {
                namespace: self.config.font_namespace.clone(),
                path: self.config.font_path.clone(),
            });
        }
    }

    /// Frame driver: spawn on the first tick, then open a composite frame, lay
    /// the panel background, and fan `Collect` to every child. A leaf-free
    /// panel finishes from `on_draw_list` once the slots close.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        self.ensure_spawned(ctx);
        flush_membership(&mut self.composite, ctx);
        self.composite.begin_frame();
        self.frame_discharge.begin_frame();
        let background = quad(self.config.x, self.config.y, self.config.width, self.panel_height, self.theme.surface);
        self.composite.extend_chrome([background]);
        for child in &self.children {
            ctx.send_to(child.lanes.slot, &Collect);
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
    #[handler::single]
    fn on_draw_list(&mut self, ctx: &mut WasmCtx<'_>, list: WidgetDrawList) {
        if accept_open_child_list(&self.frame_discharge, &mut self.composite, ctx, list) {
            self.finish(ctx);
        }
    }

    /// A left press sets focus + drag capture on the hit child and forwards
    /// the press; any press forwards to the hit child. A modal grab (an open
    /// dropdown) takes every press before any of that: the grab holder must
    /// see the press that lands outside it, which is how it learns to close.
    ///
    /// A left press that lands on no focusable child **clears** focus — the
    /// panel background, a label, the gap between two rows. Clicking away from
    /// a control is how a person says "I am done with that one", so the field
    /// they were typing in must stop being active and take its `FocusLost`.
    /// Nothing else is cancelled: a drag capture, a modal grab, and every
    /// child's own value are untouched. A root that forks this panel copies
    /// the rule — leaving it out is what keeps a pressed input lit forever.
    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if let Some(grabbed) = self.focus.grabbed() {
            if let Some(control) = self.lanes(grabbed).and_then(|lanes| lanes.control) {
                ctx.send_to(control, &press);
            }
            return;
        }
        let target = if press.button == mouse_button::LEFT {
            let hit = self.focus.hit_test(press.x, press.y);
            if let Some(child) = hit {
                self.focus.begin_capture(child);
            }
            let focusable = self.focus.focus_hit_test(press.x, press.y);
            if let Some(transition) = self.focus.set_focus(focusable) {
                self.apply_focus(&mut ctx.sends(), transition, false);
            }
            hit
        } else {
            self.focus.pointer_target(press.x, press.y)
        };
        if let Some(control) = target.and_then(|key| self.lanes(key)?.control) {
            ctx.send_to(control, &press);
        }
    }

    /// A release forwards to the captured / hit child and clears capture. The
    /// hover half of that is suppressed while a modal grab holds, the same as
    /// [`Self::on_mouse_move`]'s — `Focus::release_capture` owns the rule, so
    /// a release inside an open menu cannot light the control its plate
    /// stands over.
    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if let Some(control) = self.focus.pointer_target(release.x, release.y).and_then(|key| self.lanes(key)?.control)
        {
            ctx.send_to(control, &release);
        }
        if release.button == mouse_button::LEFT
            && let Some(transition) = self.focus.release_capture(release.x, release.y)
        {
            self.apply_hover(&mut ctx.sends(), transition);
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
            self.apply_hover(&mut ctx.sends(), transition);
        }
        if let Some(motion) = self.focus.pointer_target(moved.x, moved.y).and_then(|key| self.lanes(key)?.motion) {
            ctx.send_to(motion, &moved);
        }
    }

    /// Route a wheel to the topmost self-scrolling child under the cursor — a
    /// scroll viewport, or a virtual list, which owns the window it realizes.
    /// This is deliberately a fresh `hit_test`, not `pointer_target`: a
    /// button's drag capture owns move/release, not a separate wheel gesture.
    #[handler::single]
    fn on_mouse_wheel(&mut self, ctx: &mut WasmCtx<'_>, wheel: MouseWheel) {
        if let Some(lane) = self.scroll_focus.hit_test(wheel.x, wheel.y).and_then(|key| self.lanes(key)?.wheel) {
            ctx.send_to(lane, &wheel);
        }
    }

    /// Tab cycles focus; every other key forwards to the focused child.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if key.code == KEY_TAB {
            let direction = if self.modifiers.as_ref().is_some_and(|held| held.shift) {
                FocusDirection::Backward
            } else {
                FocusDirection::Forward
            };
            if let Some(transition) = self.focus.move_focus(direction) {
                self.apply_focus(&mut ctx.sends(), transition, true);
            }
            return;
        }
        if let Some(control) = self.focused_lanes().and_then(|lanes| lanes.control) {
            ctx.send_to(control, &key);
        }
    }

    /// Key releases forward to the focused child (Button uses matching Space
    /// release for exactly-once activation).
    #[handler::single]
    fn on_key_release(&mut self, ctx: &mut WasmCtx<'_>, release: KeyRelease) {
        if let Some(key_release) = self.focused_lanes().and_then(|lanes| lanes.key_release) {
            ctx.send_to(key_release, &release);
        }
    }

    /// Committed text forwards to the focused child.
    #[handler::single]
    fn on_text_input(&mut self, ctx: &mut WasmCtx<'_>, input: TextInput) {
        if let Some(text_entry) = self.focused_lanes().and_then(|lanes| lanes.text_entry) {
            ctx.send_to(text_entry, &input);
        }
    }

    /// An IME composition forwards to the focused child.
    #[handler::single]
    fn on_ime_preedit(&mut self, ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        if let Some(text_entry) = self.focused_lanes().and_then(|lanes| lanes.text_entry) {
            ctx.send_to(text_entry, &preedit);
        }
    }

    /// Modifier state forwards to the focused child (the text field caches
    /// it).
    #[handler::single]
    fn on_modifiers(&mut self, ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        if let Some(text_entry) = self.focused_lanes().and_then(|lanes| lanes.text_entry) {
            ctx.send_to(text_entry, &modifiers);
        }
        self.modifiers = Some(modifiers);
    }

    /// Keep dynamic routing availability synchronized with the external state
    /// a child actually adopted. Source attribution identifies the panel slot.
    #[handler::single]
    fn on_widget_state_changed(&mut self, ctx: &mut WasmCtx<'_, Erased>, changed: WidgetStateChanged) {
        let Some(source) = ctx.sender() else {
            return;
        };
        let effects = self.focus.update_availability(source, &changed.state);
        self.apply_availability(&mut ctx.sends(), effects);
    }

    /// Keep content-derived pointer/keyboard eligibility synchronized. Source
    /// attribution identifies the panel slot the event came from.
    #[handler::single]
    fn on_widget_eligibility_changed(&mut self, ctx: &mut WasmCtx<'_, Erased>, changed: WidgetEligibilityChanged) {
        let Some(source) = ctx.sender() else {
            return;
        };
        let effects = self
            .focus
            .update_eligibility(source, FocusEligibility { pointer: changed.pointer, keyboard: changed.keyboard });
        self.apply_availability(&mut ctx.sends(), effects);
    }

    /// Observe one descendant scroll container's exact typed outcome. The
    /// owner is the direct child `widget` names, `relays` scroll hops inward:
    /// each intermediate scroll that relays the event adds one.
    #[handler::single]
    fn on_scroll_outcome(&mut self, ctx: &mut WasmCtx<'_, Erased>, outcome: ScrollOutcome) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            relays = outcome.relays,
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
    #[handler::single]
    fn on_scroll_residual(&mut self, ctx: &mut WasmCtx<'_, Erased>, residual: ScrollResidual) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
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
    #[handler::single]
    fn on_slider_changed(&mut self, ctx: &mut WasmCtx<'_, Erased>, changed: SliderChanged) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            value = changed.value,
            committed = changed.committed,
            "widget slider changed",
        );
    }

    /// A text-field commit. The map-editor seam; the reference logs it.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::single]
    fn on_text_committed(&mut self, ctx: &mut WasmCtx<'_, Erased>, committed: TextCommitted) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            text = %committed.text,
            "widget text committed",
        );
    }

    /// A radio selection. The map-editor seam; the reference logs it.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::single]
    fn on_radio_selected(&mut self, ctx: &mut WasmCtx<'_, Erased>, selected: RadioSelected) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            index = selected.index,
            "widget radio selected",
        );
    }

    /// A virtual-list selection. The map-editor seam; the reference logs it.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::single]
    fn on_virtual_list_selected(&mut self, ctx: &mut WasmCtx<'_, Erased>, selected: VirtualListSelected) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            index = selected.index,
            "widget virtual list selected",
        );
    }

    /// A verb bound to one virtual-list row was pressed. Not a selection — the
    /// list reports this instead of one, so a host acts on `row_index` without
    /// the row having become current. The map-editor seam; the reference logs
    /// it.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::single]
    fn on_virtual_list_activated(&mut self, ctx: &mut WasmCtx<'_, Erased>, action: VirtualListActivated) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            index = action.index,
            action = action.action,
            "widget virtual list activated",
        );
    }

    /// The row of a virtual list under the pointer changed. Not a selection —
    /// the reader is looking, not choosing — so a host stands this row's
    /// explanation and leaves the current item alone. `None` is the pointer
    /// having left the rows. The map-editor seam; the reference logs it.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::single]
    fn on_virtual_list_hover(&mut self, ctx: &mut WasmCtx<'_, Erased>, hover: VirtualListHover) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            index = hover.index,
            "widget virtual list hover",
        );
    }

    /// The option under the pointer in a dropdown's **open** list changed —
    /// the dropdown's twin of `on_virtual_list_hover`. Not a choice: the reader
    /// is looking, so a host stands this option's explanation on the row
    /// rectangle the event carries and leaves the current choice alone. `None`
    /// is the pointer having left the list, or the list having closed. The
    /// map-editor seam; the reference logs it.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::single]
    fn on_dropdown_hover(&mut self, ctx: &mut WasmCtx<'_, Erased>, hover: DropdownHover) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            index = hover.index,
            "widget dropdown hover",
        );
    }

    /// A button click. The map-editor seam; the reference logs it.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::single]
    fn on_button_activated(&mut self, ctx: &mut WasmCtx<'_, Erased>, _clicked: ButtonActivated) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            "widget button activated",
        );
    }

    /// A toggle value-up. The map-editor seam; the reference logs it.
    #[handler::single]
    fn on_toggle_changed(&mut self, ctx: &mut WasmCtx<'_, Erased>, changed: ToggleChanged) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            on = changed.on,
            "widget toggle changed",
        );
    }

    /// A segmented selection. The map-editor seam; the reference logs it.
    #[handler::single]
    fn on_segmented_selected(&mut self, ctx: &mut WasmCtx<'_, Erased>, selected: SegmentedSelected) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            index = selected.index,
            "widget segmented selected",
        );
    }

    /// A dropdown's choice. The map-editor seam; the reference logs it.
    #[handler::single]
    fn on_dropdown_selected(&mut self, ctx: &mut WasmCtx<'_, Erased>, selected: DropdownSelected) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            index = selected.index,
            "widget dropdown selected",
        );
    }

    /// A child raised or put away an overlay — a dropdown's list, a menu
    /// bar's menu. Not a value event: the root answers it by granting or
    /// ending the modal pointer grab, so a press anywhere on the window
    /// reaches the open thing — the one input fact a widget cannot arrange for
    /// itself. One handler for every overlay-bearing widget, because the
    /// handshake does not vary by widget and a root that implemented it for
    /// one kind and not the next left that one open with no grab.
    #[handler::single]
    fn on_widget_open_changed(&mut self, ctx: &mut WasmCtx<'_, Erased>, changed: WidgetOpenChanged) {
        let Some(source) = ctx.sender() else {
            return;
        };
        if changed.open {
            self.focus.begin_grab(source);
        } else if self.focus.grabbed() == Some(source) {
            self.focus.end_grab();
        }
    }

    /// A menu item's activation. The map-editor seam; the reference logs it.
    #[handler::single]
    fn on_menu_bar_activated(&mut self, ctx: &mut WasmCtx<'_, Erased>, activated: MenuBarActivated) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            menu = activated.menu,
            item = activated.item,
            "widget menu bar activated",
        );
    }

    /// A tab strip's selection. The map-editor seam; the reference logs it.
    #[handler::single]
    fn on_tab_strip_selected(&mut self, ctx: &mut WasmCtx<'_, Erased>, selected: TabStripSelected) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
            index = selected.index,
            "widget tab strip selected",
        );
    }

    /// A numeric preview or commit. The map-editor seam; the reference logs it.
    #[handler::single]
    fn on_numeric_changed(&mut self, ctx: &mut WasmCtx<'_, Erased>, changed: NumericChanged) {
        tracing::info!(
            target: "aether_widget",
            widget = self.child_name(ctx.sender()),
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
                self.retain_or_fan_theme(ctx);
            }
            LoadFontResult::Err { error, .. } => {
                tracing::warn!(target: "aether_widget", %error, "panel font load failed");
            }
        }
    }

    /// A live restyle: adopt the new theme and re-fan it to every child.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        self.theme = set.theme;
        self.retain_or_fan_theme(ctx);
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use aether_data::MailboxId;

    // Tripwire: the panel's vertical stack, pinned rect by rect against the
    // hand-rolled loop it replaced. The three things that loop got right and
    // a layout rewrite can silently lose: one gap *between* rows and none
    // after the last (an off-by-one there slides every child down by a gap
    // and mis-reports the background's height), a child that named its own
    // width keeping exactly that width instead of being stretched to the
    // pane, and the reported `panel_height` being the occupied extent rather
    // than the extent plus a trailing gap.
    #[test]
    fn the_stack_gaps_between_rows_only_and_keeps_a_self_sized_child_at_its_own_width() {
        let config = PanelConfig { x: 10.0, y: 20.0, width: 300.0, ..PanelConfig::default() };
        let rows = vec![
            stack_row(MailboxId(1), None, 24.0),
            stack_row(MailboxId(2), Some(80.0), 24.0),
            stack_row(MailboxId(3), None, 48.0),
        ];

        let placed = stack_column(&config, 8.0).place(&rows);

        let rect = |id: u64| {
            let frame = placed.frame(&MailboxId(id)).expect("every spawned child is placed");
            [frame.x, frame.y, frame.width, frame.height]
        };
        assert_eq!(rect(1), [10.0, 20.0, 300.0, 24.0]);
        assert_eq!(rect(2), [10.0, 52.0, 80.0, 24.0], "a self-sized child is not stretched to the pane");
        assert_eq!(rect(3), [10.0, 84.0, 300.0, 48.0]);
        // 24 + 8 + 24 + 8 + 48 — the background chrome's height, no trailing gap.
        assert_eq!(placed.height, 112.0);

        let empty: Vec<SpawnedChild> = Vec::new();
        assert_eq!(stack_column(&config, 8.0).place(&stack_rows(&empty)).height, 0.0, "an empty stack takes no height");
    }

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

    #[test]
    fn a_host_strip_list_owns_the_column_its_bar_stands_in() {
        // Tripwire: `host_scroll_strip` is a request the *host* has to honour.
        // The list draws its track at `frame.width + gap` and takes nothing
        // off its rows, so a slot clipped to the frame drops the track and the
        // thumb and a press where the bar is reaches nothing — a list that
        // overflows with no visible, grabbable bar at all, which is worse than
        // the inside-the-frame bar the flag replaced.
        let theme = Theme::default();
        let assigned = WidgetFrame { x: 4.0, y: 8.0, width: 200.0, height: 96.0 };

        let config = VirtualListConfig { host_scroll_strip: true, ..VirtualListConfig::default() };
        let units = host_scroll_strip_units(&config).expect("the flag asks the host for a column");
        let strip = VirtualListConfig::host_strip_width(units, &theme);
        assert_eq!(strip, config.scroll_strip_width(&theme), "the host reserves exactly what the list asks for");

        let frame = content_frame(&assigned, strip);
        assert_eq!(
            (frame.x, frame.width),
            (4.0, 200.0 - strip),
            "the strip comes out of the rows' own width, so the bar stands beside them and inside the panel",
        );
        assert!(
            frame.width + theme.space(units) + VirtualListConfig::scroll_track_width(&theme) <= assigned.width,
            "and the track's far edge is inside the rectangle the panel clips and hit-tests the slot by",
        );

        let inside_the_frame = VirtualListConfig::default();
        assert_eq!(host_scroll_strip_units(&inside_the_frame), None, "a list whose bar is its own asks for nothing");
        assert_eq!(content_frame(&assigned, 0.0).width, assigned.width, "and keeps the whole assignment");
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
