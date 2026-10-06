//! Input-only editor shell over independently-rooted peer regions (ADR-0141).

use aether_actor::{ActorInitError, ProtocolRef, Target, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_data::ActorMail;
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
};
use aether_window::WindowCapability;

use super::routing::{RegionFocusTransition, RegionInputLane, Routing};
use super::{EditorConfig, EditorInput, RegionAttach};

/// The sole interactive-input subscriber for a configured set of editor peers.
///
/// It holds no address of its own: [`Routing`] stores the proof each region
/// handed over when it announced itself (ADR-0230), cast once to
/// [`EditorInput`], and gives that same value back as a route's target, so the
/// shell has nothing to resolve and no way to address a region that never
/// announced.
pub struct EditorShell {
    routing: Routing,
}

impl EditorShell {
    /// The shell's only send: prime a newly focused region with the cached
    /// modifiers, once any have arrived, then hand `payload` to `target`.
    ///
    /// The reference [`Routing`] returned is handed to the send whole: no
    /// position is opened anywhere in the shell.
    fn forward<A, K: ActorMail, I>(
        &self,
        ctx: &mut WasmCtx<'_, A>,
        focus: Option<RegionFocusTransition>,
        target: Option<ProtocolRef<EditorInput>>,
        payload: &K,
    ) where
        ProtocolRef<EditorInput>: Target<K, I>,
    {
        self.prime(ctx, focus);

        if let Some(reference) = target {
            ctx.send_to(reference, payload);
        }
    }

    /// Send the cached modifiers to the region `focus` newly focused, when it
    /// takes the modifiers lane and any have arrived.
    fn prime<A>(&self, ctx: &mut WasmCtx<'_, A>, focus: Option<RegionFocusTransition>) {
        if let Some(next) = focus.and_then(|transition| transition.next)
            && self.routing.target_accepts(next, RegionInputLane::Modifiers)
            && let Some(modifiers) = self.routing.cached_modifiers()
        {
            ctx.send_to(next, modifiers);
        }
    }
}

// A root singleton guest (ADR-0241 §5), so a region can name the shell by bare type
// from its own `wire` and announce itself. Its cardinality is not a choice:
// the shell subscribes *every* window's nine raw input kinds (a window
// subscribe covers every window), so a second shell in one engine is a
// double-delivery bug rather than a configuration. It is therefore loaded
// under its default name, and cannot be composed beneath a wasm parent.
#[actor(root, depends(WindowCapability))]
impl WasmActor for EditorShell {
    type Config = EditorConfig;
    const NAMESPACE: &'static str = "aether.widget.editor";

    fn init(config: EditorConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { routing: Routing::new(&config.regions) })
    }

    /// Subscribe to raw interactive input from every window. The shell has no
    /// lifecycle, render, or window-size role.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.subscribe::<WindowCapability, MouseButton>();
        ctx.subscribe::<WindowCapability, MouseButtonRelease>();
        ctx.subscribe::<WindowCapability, MouseMove>();
        ctx.subscribe::<WindowCapability, MouseWheel>();
        ctx.subscribe::<WindowCapability, Key>();
        ctx.subscribe::<WindowCapability, KeyRelease>();
        ctx.subscribe::<WindowCapability, TextInput>();
        ctx.subscribe::<WindowCapability, ImePreedit>();
        ctx.subscribe::<WindowCapability, Modifiers>();
        Ok(())
    }

    /// A region announcing that it is the actor behind one of the declared
    /// region names. The address is the envelope sender, never a field of the
    /// mail: the host stamped it, so it is a proof rather than a position the
    /// sender chose. It is cast to [`EditorInput`] here, once, so every later
    /// forward sends through a typed proof. An unknown name, a second
    /// announcement for a name already attached, a sourceless dispatch, and a
    /// sender that does not cover [`EditorInput`] are each reported and
    /// ignored — none of them may re-point a live route.
    #[handler::tell]
    fn on_region_attach(&mut self, ctx: &mut WasmCtx<'_>, attach: RegionAttach) {
        let Some(reference) = ctx.sender() else {
            tracing::warn!(
                target: "aether_widget_editor",
                region = attach.region.as_str(),
                "region attach arrived with no sender; ignoring",
            );
            return;
        };
        let Some(reference) = ctx.cast::<EditorInput>(reference) else {
            tracing::warn!(
                target: "aether_widget_editor",
                region = attach.region.as_str(),
                "region attach sender does not cover the editor input protocol; ignoring",
            );
            return;
        };

        if !self.routing.attach(&attach.region, reference) {
            tracing::warn!(
                target: "aether_widget_editor",
                region = attach.region.as_str(),
                "region attach names no unattached declared region; ignoring",
            );
        }
    }

    #[handler::event]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        let route = self.routing.pointer_press(&press);
        self.forward(ctx, route.focus, route.target, &press);
    }

    #[handler::event]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        let target = self.routing.pointer_release(&release);
        self.forward(ctx, None, target, &release);
    }

    /// Motion goes to the region under the pointer — and, first, to the region
    /// it just left. That region hit-tests the same position against its own
    /// table, finds nothing, and hands the child it had lit its `HoverLost`;
    /// without it the abandoned pane keeps drawing a hover wash under a pointer
    /// that is in another pane entirely.
    #[handler::event]
    fn on_mouse_move(&mut self, ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        let route = self.routing.pointer_motion(&moved);
        self.forward(ctx, None, route.exited, &moved);
        self.forward(ctx, None, route.target, &moved);
    }

    #[handler::event]
    fn on_mouse_wheel(&mut self, ctx: &mut WasmCtx<'_>, wheel: MouseWheel) {
        self.forward(ctx, None, self.routing.wheel(&wheel), &wheel);
    }

    #[handler::event]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        let route = self.routing.key_press(&key);
        self.forward(ctx, route.focus, route.target, &key);
    }

    #[handler::event]
    fn on_key_release(&mut self, ctx: &mut WasmCtx<'_>, release: KeyRelease) {
        let route = self.routing.key_release(&release);
        self.forward(ctx, route.focus, route.target, &release);
    }

    #[handler::event]
    fn on_text_input(&mut self, ctx: &mut WasmCtx<'_>, input: TextInput) {
        self.forward(ctx, None, self.routing.text_input_target(), &input);
    }

    #[handler::event]
    fn on_ime_preedit(&mut self, ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        self.forward(ctx, None, self.routing.ime_preedit_target(), &preedit);
    }

    #[handler::event]
    fn on_modifiers(&mut self, ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        let target = self.routing.modifiers(&modifiers);
        self.forward(ctx, None, target, &modifiers);
    }
}
