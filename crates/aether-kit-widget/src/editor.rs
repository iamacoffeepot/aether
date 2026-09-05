//! Input-only editor shell over independently-rooted peer regions (ADR-0141).

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_data::{Kind, MailboxId};
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
};
use aether_window::{WindowCapability, WindowManagerMailboxExt, WindowSelector};

use super::EditorConfig;
use super::routing::{RegionFocusTransition, RegionInputLane, Routing};

/// The sole interactive-input subscriber for a configured set of editor peers.
pub struct EditorShell {
    routing: Routing,
}

impl EditorShell {
    fn prime_focus(&self, ctx: &mut WasmCtx<'_>, transition: Option<RegionFocusTransition>) {
        let Some(target) = transition.and_then(|transition| transition.next) else {
            return;
        };
        if self.routing.target_accepts(target, RegionInputLane::Modifiers) {
            ctx.send_to(target, &self.routing.cached_modifiers());
        }
    }

    fn forward<K: Kind>(
        &self,
        ctx: &mut WasmCtx<'_>,
        focus: Option<RegionFocusTransition>,
        target: Option<MailboxId>,
        payload: &K,
    ) {
        self.prime_focus(ctx, focus);
        if let Some(target) = target {
            ctx.send_to(target, payload);
        }
    }
}

#[actor(instanced, composable)]
impl WasmActor for EditorShell {
    type Config = EditorConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.editor";

    fn init(config: EditorConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { routing: Routing::new(&config.regions) })
    }

    /// Subscribe to raw interactive input from every window. The shell has no
    /// lifecycle, render, or window-size role.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
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

    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        let route = self.routing.pointer_press(press);
        self.forward(ctx, route.focus, route.target, &press);
    }

    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if let Some(target) = self.routing.pointer_release(release) {
            ctx.send_to(target, &release);
        }
    }

    /// Motion goes to the region under the pointer — and, first, to the region
    /// it just left. That region hit-tests the same position against its own
    /// table, finds nothing, and hands the child it had lit its `HoverLost`;
    /// without it the abandoned pane keeps drawing a hover wash under a pointer
    /// that is in another pane entirely.
    #[handler::single]
    fn on_mouse_move(&mut self, ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        let route = self.routing.pointer_motion(moved);
        if let Some(exited) = route.exited {
            ctx.send_to(exited, &moved);
        }
        if let Some(target) = route.target {
            ctx.send_to(target, &moved);
        }
    }

    #[handler::single]
    fn on_mouse_wheel(&mut self, ctx: &mut WasmCtx<'_>, wheel: MouseWheel) {
        if let Some(target) = self.routing.wheel(wheel) {
            ctx.send_to(target, &wheel);
        }
    }

    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        let route = self.routing.key_press(key);
        self.forward(ctx, route.focus, route.target, &key);
    }

    #[handler::single]
    fn on_key_release(&mut self, ctx: &mut WasmCtx<'_>, release: KeyRelease) {
        let route = self.routing.key_release(release);
        self.forward(ctx, route.focus, route.target, &release);
    }

    #[handler::single]
    fn on_text_input(&mut self, ctx: &mut WasmCtx<'_>, input: TextInput) {
        if let Some(target) = self.routing.text_input_target() {
            ctx.send_to(target, &input);
        }
    }

    #[handler::single]
    fn on_ime_preedit(&mut self, ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        if let Some(target) = self.routing.ime_preedit_target() {
            ctx.send_to(target, &preedit);
        }
    }

    #[handler::single]
    fn on_modifiers(&mut self, ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        if let Some(target) = self.routing.modifiers(modifiers) {
            ctx.send_to(target, &modifiers);
        }
    }
}
