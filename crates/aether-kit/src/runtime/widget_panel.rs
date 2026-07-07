// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `runtime/widget.rs`).
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
//! - **Input.** It subscribes the pointer / keyboard streams once (the input
//!   cap) and the frame stage once (the lifecycle cap), then routes each event
//!   through [`Focus`]: keyboard to the focused child, pointer to the hit or
//!   drag-captured child, Tab to cycle focus, a left press to set focus + drag
//!   capture. Focus transitions fan `FocusGained` / `FocusLost` down.
//! - **Draw.** Each frame it drives [`Composite`] — `Collect` down, draw lists
//!   up — and emits the whole panel as one `DrawSolidQuads` (plus text).
//! - **Value.** Each value-up event (`SliderChanged` / `TextCommitted` /
//!   `RadioSelected` / `ButtonClicked`), attributed by `ctx.source_mailbox()`,
//!   is the seam a real editor translates into world-knob driver mail; the
//!   reference logs it.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{
    ActorInitError, Manual, OutboundReply, Subname, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_capabilities::input::InputMailboxExt;
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::text::{LoadFont, LoadFontResult};
use aether_capabilities::{InputCapability, LifecycleCapability, TextCapability};
use aether_data::{Kind, MailboxId};
use aether_kinds::keycode::KEY_TAB;
use aether_kinds::mouse_button;
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel,
    TextInput, Tick,
};
use aether_math::Vec2;

use crate::runtime::composite::Composite;
use crate::runtime::focus::{Focus, FocusTransition};
use crate::runtime::widget::emit;
use crate::runtime::widgets::{
    ButtonWidget, LabelWidget, RadioGroupWidget, SliderWidget, TextFieldWidget, quad,
};
use crate::theme::{SetTheme, Theme};
use crate::widgets::{
    ButtonClicked, ButtonConfig, Collect, FocusGained, FocusLost, LabelConfig, PanelConfig,
    RadioConfig, RadioSelected, SliderChanged, SliderConfig, TextCommitted, TextFieldConfig,
    WidgetChildSpec, WidgetDrawList, WidgetFrame, WidgetKind,
};

/// One spawned child's alias plus the logical name the panel attributes its
/// value-up events under (for the map-editor translation / logging) — the
/// child's spec subname.
struct ChildRef {
    id: MailboxId,
    name: String,
}

/// The reference panel root. Loaded as a component with a [`PanelConfig`]; its
/// export name is `aether.kit.widget.panel`.
pub struct WidgetPanel {
    config: PanelConfig,
    /// The live theme — `config.theme` with the real `font_id` stamped once
    /// the font loads. Fanned down to every child on change.
    theme: Theme,
    composite: Composite,
    focus: Focus,
    children: Vec<ChildRef>,
    spawned: bool,
    /// The total stack height, for the background chrome; set at spawn.
    panel_height: f32,
}

impl WidgetPanel {
    /// Spawn the declared widget stack once (from the first frame — an inline
    /// child gets no `wire` and `init` cannot spawn, so the root spawns from
    /// its first send-capable handler). Each spec spawns its kind's actor from
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

        let row_rect = |y: f32, height: f32| WidgetFrame {
            x,
            y,
            width,
            height,
        };

        let specs = if self.config.children.is_empty() {
            reference_stack(&self.theme)
        } else {
            self.config.children.clone()
        };

        let mut first = true;
        for spec in &specs {
            // Decode the concrete config, spawn the kind's actor, and derive
            // the row height + focusability from that config. `None` from any
            // arm (an undecodable config, a spawn failure, or a rejected
            // container) skips the slot entirely so the stack stays honest.
            let placed = match spec.kind {
                WidgetKind::Label => decode_child::<LabelConfig>(spec)
                    .and_then(|config| spawn::<LabelWidget>(ctx, &spec.subname, &config))
                    .map(|id| (id, row, false)),
                WidgetKind::Slider => decode_child::<SliderConfig>(spec)
                    .and_then(|config| spawn::<SliderWidget>(ctx, &spec.subname, &config))
                    .map(|id| (id, row, true)),
                WidgetKind::Radio => decode_child::<RadioConfig>(spec).and_then(|config| {
                    let height = row * config.options.len() as f32;
                    spawn::<RadioGroupWidget>(ctx, &spec.subname, &config)
                        .map(|id| (id, height, true))
                }),
                WidgetKind::TextField => decode_child::<TextFieldConfig>(spec)
                    .and_then(|config| spawn::<TextFieldWidget>(ctx, &spec.subname, &config))
                    .map(|id| (id, row, true)),
                WidgetKind::Button => decode_child::<ButtonConfig>(spec)
                    .and_then(|config| spawn::<ButtonWidget>(ctx, &spec.subname, &config))
                    .map(|id| (id, row, true)),
                WidgetKind::Composite => {
                    tracing::warn!(
                        target: "aether_kit",
                        subname = %spec.subname,
                        "a Composite child in a panel is not supported (nested \
                         containers are out of scope in v1); slot skipped",
                    );
                    None
                }
            };
            let Some((id, height, focusable)) = placed else {
                continue;
            };
            if !first {
                y += gap;
            }
            first = false;
            self.place(
                ctx,
                id,
                row_rect(y, height),
                focusable,
                spec.subname.clone(),
            );
            y += height;
        }

        self.panel_height = y - self.config.y;
    }

    /// Record one spawned child's rect into the composite (as its draw offset)
    /// and the focus table (as its hit rect), send it its `WidgetFrame`, and
    /// remember it for value-up attribution.
    fn place(
        &mut self,
        ctx: &mut WasmCtx<'_, Manual>,
        id: MailboxId,
        frame: WidgetFrame,
        focusable: bool,
        name: String,
    ) {
        self.composite
            .register_slot(id, Vec2::new(frame.x, frame.y));
        self.focus
            .register(id, frame.x, frame.y, frame.width, frame.height, focusable);
        ctx.send_to(id, &frame);
        self.children.push(ChildRef { id, name });
    }

    /// Discharge a closed frame: flatten the composite and emit it as the
    /// panel's single render + text output.
    fn finish(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        let list = self.composite.flatten(None);
        emit(ctx, &list);
    }

    /// Re-fan the live theme to every child (after a font stamp or a restyle).
    fn fan_theme(&self, ctx: &mut WasmCtx<'_>) {
        for child in &self.children {
            ctx.send_to(
                child.id,
                &SetTheme {
                    theme: self.theme.clone(),
                },
            );
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

/// Decode one child spec's opaque config bytes as the concrete config type
/// `C` its [`WidgetKind`] selects, warning and yielding `None` on a decode
/// failure so the caller skips the slot (mirroring `widget.rs`).
fn decode_child<C: Kind>(spec: &WidgetChildSpec) -> Option<C> {
    let config = C::decode_from_bytes(&spec.config);
    if config.is_none() {
        tracing::warn!(
            target: "aether_kit",
            subname = %spec.subname,
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
        config,
    };
    alloc::vec![
        spec(
            "label",
            WidgetKind::Label,
            LabelConfig {
                text: String::from("Controls"),
                theme: theme.clone(),
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
            }
            .encode_into_bytes(),
        ),
        spec(
            "radio",
            WidgetKind::Radio,
            RadioConfig {
                options: alloc::vec![
                    String::from("Low"),
                    String::from("Medium"),
                    String::from("High"),
                ],
                initial_index: 0,
                theme: theme.clone(),
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
            }
            .encode_into_bytes(),
        ),
        spec(
            "button",
            WidgetKind::Button,
            ButtonConfig {
                label: String::from("Apply"),
                theme: theme.clone(),
            }
            .encode_into_bytes(),
        ),
    ]
}

/// Send a focus transition down: `FocusLost` to the child that lost focus,
/// `FocusGained` to the one that gained it.
fn apply_focus(ctx: &mut WasmCtx<'_>, (previous, next): FocusTransition) {
    if let Some(prev) = previous {
        ctx.send_to(prev, &FocusLost);
    }
    if let Some(gained) = next {
        ctx.send_to(gained, &FocusGained);
    }
}

/// Spawn one inline widget, logging and dropping the slot on failure. Keeps
/// the per-widget spawn sites in [`WidgetPanel::ensure_spawned`] to one line.
fn spawn<A>(ctx: &mut WasmCtx<'_, Manual>, subname: &str, config: &A::Config) -> Option<MailboxId>
where
    A: aether_actor::Instanced + WasmActor + aether_actor::ErasedWasmActor,
    <A as WasmActor>::State: aether_actor::ErasedWasmActor,
{
    match ctx.spawn_inline_child::<A>(Subname::Named(subname), config) {
        Ok(id) => Some(id),
        Err(error) => {
            tracing::warn!(
                target: "aether_kit",
                subname,
                ?error,
                "widget spawn failed; slot skipped",
            );
            None
        }
    }
}

/// The reference panel root. Load it as a component (export
/// `aether.kit.widget.panel`) with a [`PanelConfig`].
///
/// # Agent
/// Load `aether_kit.wasm` with `export: "aether.kit.widget.panel"` and a
/// `PanelConfig` (top-left, width, theme, a font to load, and the `children`
/// it stacks). With an empty `children` list it spawns a demonstration stack
/// of every widget; otherwise it stacks exactly the declared specs. It routes
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
            focus: Focus::new(),
            children: Vec::new(),
            spawned: false,
            panel_height: 0.0,
        })
    }

    /// Subscribe the pointer / keyboard streams (input cap) and the frame
    /// stage (lifecycle cap) once, and kick off the font load. Widgets never
    /// subscribe — the root forwards everything.
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        let input = ctx.actor::<InputCapability>();
        input.subscribe::<MouseButton>();
        input.subscribe::<MouseButtonRelease>();
        input.subscribe::<MouseMove>();
        input.subscribe::<MouseWheel>();
        input.subscribe::<Key>();
        input.subscribe::<KeyRelease>();
        input.subscribe::<TextInput>();
        input.subscribe::<ImePreedit>();
        input.subscribe::<Modifiers>();
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
        if !self.config.font_path.is_empty() {
            ctx.actor::<TextCapability>().send(&LoadFont {
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
    #[handler::manual]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_, Manual>, _tick: Tick) {
        self.ensure_spawned(ctx);
        self.composite.begin_frame();
        let background = quad(
            self.config.x,
            self.config.y,
            self.config.width,
            self.panel_height,
            self.theme.surface,
        );
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
    #[handler::manual]
    fn on_draw_list(&mut self, ctx: &mut WasmCtx<'_, Manual>, list: WidgetDrawList) {
        if let Some(source) = ctx.source_mailbox() {
            self.composite.fill(source, list);
        }
        if self.composite.is_complete() {
            self.finish(ctx);
        }
    }

    /// A left press sets focus + drag capture on the hit child and forwards
    /// the press; any press forwards to the hit child.
    #[handler]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
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
    #[handler]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if let Some(child) = self.focus.pointer_target(release.x, release.y) {
            ctx.send_to(child, &release);
        }
        if release.button == mouse_button::LEFT {
            self.focus.clear_capture();
        }
    }

    /// A move forwards to the captured (dragged) or hit child.
    #[handler]
    fn on_mouse_move(&mut self, ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if let Some(child) = self.focus.pointer_target(moved.x, moved.y) {
            ctx.send_to(child, &moved);
        }
    }

    /// Reserved for scroll-aware widgets; no widget consumes the wheel in v1,
    /// so the panel absorbs it here (subscribed, but dropped) rather than
    /// forwarding an unhandled kind to a child.
    // Subscribing the stream needs a handler to sink it; there is no per-panel
    // wheel state yet, so the handler is deliberately empty.
    #[allow(clippy::unused_self)]
    #[handler]
    fn on_mouse_wheel(&mut self, _ctx: &mut WasmCtx<'_>, _wheel: MouseWheel) {}

    /// Tab cycles focus; every other key forwards to the focused child.
    #[handler]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if key.code == KEY_TAB {
            if let Some(transition) = self.focus.advance_focus() {
                apply_focus(ctx, transition);
            }
            return;
        }
        if let Some(child) = self.focus.keyboard_target() {
            ctx.send_to(child, &key);
        }
    }

    /// Reserved: no widget consumes key releases in v1, so the panel absorbs
    /// them here (subscribed, but dropped) rather than forwarding an unhandled
    /// kind to a child.
    // Subscribing the stream needs a handler to sink it; there is no per-panel
    // key-release state yet, so the handler is deliberately empty.
    #[allow(clippy::unused_self)]
    #[handler]
    fn on_key_release(&mut self, _ctx: &mut WasmCtx<'_>, _release: KeyRelease) {}

    /// Committed text forwards to the focused child.
    #[handler]
    fn on_text_input(&mut self, ctx: &mut WasmCtx<'_>, input: TextInput) {
        if let Some(child) = self.focus.keyboard_target() {
            ctx.send_to(child, &input);
        }
    }

    /// An IME composition forwards to the focused child.
    #[handler]
    fn on_ime_preedit(&mut self, ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        if let Some(child) = self.focus.keyboard_target() {
            ctx.send_to(child, &preedit);
        }
    }

    /// Modifier state forwards to the focused child (the text field caches
    /// it).
    #[handler]
    fn on_modifiers(&mut self, ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        if let Some(child) = self.focus.keyboard_target() {
            ctx.send_to(child, &modifiers);
        }
    }

    /// A slider value-up. The map-editor seam: translate to world-knob driver
    /// mail here. The reference logs the attributed value.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::manual]
    fn on_slider_changed(&mut self, ctx: &mut WasmCtx<'_, Manual>, changed: SliderChanged) {
        tracing::info!(
            target: "aether_kit",
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
            target: "aether_kit",
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
            target: "aether_kit",
            widget = self.child_name(ctx.source_mailbox()),
            index = selected.index,
            "widget radio selected",
        );
    }

    /// A button click. The map-editor seam; the reference logs it.
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::manual]
    fn on_button_clicked(&mut self, ctx: &mut WasmCtx<'_, Manual>, _clicked: ButtonClicked) {
        tracing::info!(
            target: "aether_kit",
            widget = self.child_name(ctx.source_mailbox()),
            "widget button clicked",
        );
    }

    /// The font finished loading: stamp the real `font_id` into the theme and
    /// re-fan it so every child draws text with it.
    #[handler]
    fn on_load_font_result(&mut self, ctx: &mut WasmCtx<'_>, result: LoadFontResult) {
        match result {
            LoadFontResult::Ok { font_id, .. } => {
                self.theme.font_id = font_id;
                self.fan_theme(ctx);
            }
            LoadFontResult::Err { error, .. } => {
                tracing::warn!(target: "aether_kit", %error, "panel font load failed");
            }
        }
    }

    /// A live restyle: adopt the new theme and re-fan it to every child.
    #[handler]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        self.theme = set.theme;
        self.fan_theme(ctx);
    }
}
