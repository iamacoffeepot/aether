//! Input-only editor shell over independently-rooted peer regions (ADR-0141).

use aether_actor::{ActorInitError, ErasedActorRef, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_data::Kind;
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
};
use aether_window::{WindowCapability, WindowManagerMailboxExt, WindowSelector};

use super::routing::{RegionFocusTransition, RegionInputLane, Routing};
use super::{EditorConfig, RegionAttach};

/// The sole interactive-input subscriber for a configured set of editor peers.
///
/// It holds no address of its own: [`Routing`] stores the proof each region
/// handed over when it announced itself (ADR-0230) and gives that same value
/// back as a route's target, so the shell has nothing to resolve and no way to
/// address a region that never announced.
pub struct EditorShell {
    routing: Routing,
}

impl EditorShell {
    /// The shell's only send: prime a newly focused region with the cached
    /// modifiers, then hand `payload` to `target`.
    ///
    /// The reference [`Routing`] returned is handed to the send whole: no
    /// position is opened anywhere in the shell.
    ///
    /// Priming recurses exactly once: the nested call carries no focus edge of
    /// its own, so it sends the modifiers and returns.
    fn forward<K: Kind>(
        &self,
        ctx: &mut WasmCtx<'_>,
        focus: Option<RegionFocusTransition>,
        target: Option<ErasedActorRef>,
        payload: &K,
    ) {
        if let Some(next) = focus.and_then(|transition| transition.next)
            && self.routing.target_accepts(next, RegionInputLane::Modifiers)
        {
            self.forward(ctx, None, Some(next), &self.routing.cached_modifiers());
        }

        if let Some(reference) = target {
            ctx.send_to(reference, payload);
        }
    }
}

// Keyless `Embedded` singleton, so a region can name the shell by bare type
// from its own `wire` and announce itself. Its cardinality is not a choice:
// the shell subscribes *every* window's nine raw input kinds with
// `WindowSelector::All`, so a second shell in one engine is a double-delivery
// bug rather than a configuration. It is therefore loaded under its default
// name, and cannot be composed beneath a wasm parent.
#[actor]
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

    /// A region announcing that it is the actor behind one of the declared
    /// region names. The address is the envelope sender, never a field of the
    /// mail: the host stamped it, so it is a proof rather than a position the
    /// sender chose. An unknown name, a second announcement for a name already
    /// attached, and a sourceless dispatch are each reported and ignored —
    /// none of them may re-point a live route.
    #[handler::single]
    fn on_region_attach(&mut self, ctx: &mut WasmCtx<'_>, attach: RegionAttach) {
        let Some(reference) = ctx.sender() else {
            tracing::warn!(
                target: "aether_kit_widget_editor",
                region = attach.region.as_str(),
                "region attach arrived with no sender; ignoring",
            );
            return;
        };

        if !self.routing.attach(&attach.region, reference) {
            tracing::warn!(
                target: "aether_kit_widget_editor",
                region = attach.region.as_str(),
                "region attach names no unattached declared region; ignoring",
            );
        }
    }

    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        let route = self.routing.pointer_press(press);
        self.forward(ctx, route.focus, route.target, &press);
    }

    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        let target = self.routing.pointer_release(release);
        self.forward(ctx, None, target, &release);
    }

    /// Motion goes to the region under the pointer — and, first, to the region
    /// it just left. That region hit-tests the same position against its own
    /// table, finds nothing, and hands the child it had lit its `HoverLost`;
    /// without it the abandoned pane keeps drawing a hover wash under a pointer
    /// that is in another pane entirely.
    #[handler::single]
    fn on_mouse_move(&mut self, ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        let route = self.routing.pointer_motion(moved);
        self.forward(ctx, None, route.exited, &moved);
        self.forward(ctx, None, route.target, &moved);
    }

    #[handler::single]
    fn on_mouse_wheel(&mut self, ctx: &mut WasmCtx<'_>, wheel: MouseWheel) {
        self.forward(ctx, None, self.routing.wheel(wheel), &wheel);
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
        self.forward(ctx, None, self.routing.text_input_target(), &input);
    }

    #[handler::single]
    fn on_ime_preedit(&mut self, ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        self.forward(ctx, None, self.routing.ime_preedit_target(), &preedit);
    }

    #[handler::single]
    fn on_modifiers(&mut self, ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        let target = self.routing.modifiers(modifiers);
        self.forward(ctx, None, target, &modifiers);
    }
}
