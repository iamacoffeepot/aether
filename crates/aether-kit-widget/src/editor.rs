//! Input-only editor shell over independently-rooted peer regions (ADR-0141).

use alloc::vec::Vec;

use aether_actor::{ActorInitError, AnyActorRef, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_data::{Kind, MailboxId};
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
};
use aether_window::{WindowCapability, WindowManagerMailboxExt, WindowSelector};

use super::routing::{RegionFocusTransition, RegionInputLane, Routing};
use super::{EditorConfig, RegionAttach};

/// The sole interactive-input subscriber for a configured set of editor peers.
///
/// `routing` keys its table by position, because a position is what a hit test
/// and a focus cycle compare; `regions` holds the proof behind each of those
/// positions, taken from the envelope sender of the region's own
/// [`RegionAttach`] (ADR-0230). A routed position the shell holds no proof for
/// is dropped rather than sent to.
pub struct EditorShell {
    routing: Routing,
    regions: Vec<AnyActorRef>,
}

impl EditorShell {
    /// Send `payload` to a routed position through the proof the shell was
    /// handed for it, warning and dropping when the table names a position no
    /// region ever announced.
    ///
    /// Every send the shell makes goes through here, because this is the one
    /// place a proof is unwrapped back to a position, and one place is the
    /// most that may exist: the inherit-by-id send still takes a `MailboxId`,
    /// so the reference has to be spent to call it. Issue #6304 narrows that
    /// signature to `AnyActorRef`, and this `reference.id()` goes with it —
    /// the reference is then handed over whole and nothing else here moves.
    fn send_proven<K: Kind>(&self, ctx: &mut WasmCtx<'_>, target: MailboxId, payload: &K) {
        let Some(reference) = self.regions.iter().copied().find(|reference| reference.id() == target) else {
            tracing::warn!(
                target: "aether_kit_widget_editor",
                position = target.0,
                "routed to a position no region announced; dropping the input",
            );
            return;
        };

        ctx.send_to(reference.id(), payload);
    }

    fn prime_focus(&self, ctx: &mut WasmCtx<'_>, transition: Option<RegionFocusTransition>) {
        let Some(target) = transition.and_then(|transition| transition.next) else {
            return;
        };
        if self.routing.target_accepts(target, RegionInputLane::Modifiers) {
            self.send_proven(ctx, target, &self.routing.cached_modifiers());
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
            self.send_proven(ctx, target, payload);
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
        Ok(Self { routing: Routing::new(&config.regions), regions: Vec::new() })
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

        if self.routing.attach(&attach.region, reference.id()) {
            self.regions.push(reference);
        } else {
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
        if let Some(target) = self.routing.pointer_release(release) {
            self.send_proven(ctx, target, &release);
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
            self.send_proven(ctx, exited, &moved);
        }
        if let Some(target) = route.target {
            self.send_proven(ctx, target, &moved);
        }
    }

    #[handler::single]
    fn on_mouse_wheel(&mut self, ctx: &mut WasmCtx<'_>, wheel: MouseWheel) {
        if let Some(target) = self.routing.wheel(wheel) {
            self.send_proven(ctx, target, &wheel);
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
            self.send_proven(ctx, target, &input);
        }
    }

    #[handler::single]
    fn on_ime_preedit(&mut self, ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        if let Some(target) = self.routing.ime_preedit_target() {
            self.send_proven(ctx, target, &preedit);
        }
    }

    #[handler::single]
    fn on_modifiers(&mut self, ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        if let Some(target) = self.routing.modifiers(modifiers) {
            self.send_proven(ctx, target, &modifiers);
        }
    }
}
