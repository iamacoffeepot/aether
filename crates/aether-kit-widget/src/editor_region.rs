//! The panel side of an editor region (ADR-0141): the actor that announces a
//! shell-declared region, hosts the [`WidgetPanel`] behind it, and passes the
//! shell's input on to that panel.
//!
//! The shell routes a region's input to the sender of its [`RegionAttach`], so
//! the announcer must also be the recipient. A panel may not mail the shell
//! only when its config says so (ADR-0232 §6), so the announcement lives here,
//! on an actor that always declares the shell, and the panel stays a panel.

use alloc::string::String;

use aether_actor::{ActorInitError, ErasedActorRef, Subname, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_data::ActorMail;
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
};

use crate::{EditorShell, PanelConfig, RegionAttach, WidgetPanel};

/// One editor region backed by a widget panel: it announces itself to the
/// [`EditorShell`] as `config.editor_region`, spawns a [`WidgetPanel`] as its
/// inline child from the same config, and relays each input kind the shell
/// forwards to that child.
///
/// # Agent
/// Load the shell first under its default name (export
/// `aether.kit.widget.editor`), with a region table that names this region.
/// Then load this export (`aether.kit.widget.editor_region`) with the panel's
/// `PanelConfig` and `editor_region` set to that region's name. A region loaded
/// before the shell is refused: it declares the shell. The panel it hosts is
/// its child `panel`, so the panel's own children sit one level further down.
pub struct EditorRegion {
    config: PanelConfig,
    panel: Option<ErasedActorRef>,
}

impl EditorRegion {
    /// Hand one relayed input event to the hosted panel. A region whose panel
    /// failed to spawn has nowhere to send it, and says so once, in `wire`.
    fn relay<A, K: ActorMail>(&self, ctx: &mut WasmCtx<'_, A>, payload: &K) {
        if let Some(panel) = self.panel {
            ctx.send_to(panel, payload);
        }
    }
}

#[actor(instanced, depends(EditorShell), spawns(WidgetPanel))]
impl WasmActor for EditorRegion {
    type Config = PanelConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.editor_region";

    fn init(config: PanelConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        if config.editor_region.is_empty() {
            return Err(ActorInitError::from("an editor region needs a non-empty editor_region to announce"));
        }
        Ok(Self { config, panel: None })
    }

    /// Announce the region, then spawn the panel behind it. The panel owns no
    /// input and names no region: the shell's input reaches it through this
    /// actor's relay handlers.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_, Self>) {
        ctx.send::<EditorShell>(&RegionAttach { region: self.config.editor_region.clone() });

        let panel_config = PanelConfig { owns_input: false, editor_region: String::new(), ..self.config.clone() };
        match ctx.spawn_inline_child::<EditorRegion, WidgetPanel>(Subname::Named("panel"), &panel_config) {
            Ok(panel) => self.panel = Some(panel.erase()),
            Err(error) => tracing::warn!(
                target: "aether_kit_widget_editor",
                region = self.config.editor_region.as_str(),
                ?error,
                "editor region panel spawn failed; the region relays nothing",
            ),
        }
    }

    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        self.relay(ctx, &press);
    }

    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        self.relay(ctx, &release);
    }

    #[handler::single]
    fn on_mouse_move(&mut self, ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        self.relay(ctx, &moved);
    }

    #[handler::single]
    fn on_mouse_wheel(&mut self, ctx: &mut WasmCtx<'_>, wheel: MouseWheel) {
        self.relay(ctx, &wheel);
    }

    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        self.relay(ctx, &key);
    }

    #[handler::single]
    fn on_key_release(&mut self, ctx: &mut WasmCtx<'_>, release: KeyRelease) {
        self.relay(ctx, &release);
    }

    #[handler::single]
    fn on_text_input(&mut self, ctx: &mut WasmCtx<'_>, input: TextInput) {
        self.relay(ctx, &input);
    }

    #[handler::single]
    fn on_ime_preedit(&mut self, ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        self.relay(ctx, &preedit);
    }

    /// Modifier state is relayed like any other event. The shell primes a
    /// newly focused region with the cached modifiers before the event that
    /// focused it, and one relay hop keeps that order.
    #[handler::single]
    fn on_modifiers(&mut self, ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        self.relay(ctx, &modifiers);
    }
}
