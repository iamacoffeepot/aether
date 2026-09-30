//! Typed sink used to assert editor-shell routing without giving peer regions
//! their own input subscriptions.
//!
//! The probe announces itself to the shell in `wire` under its configured
//! region name, so the shell routes to the sender of that announcement rather
//! than to an id the scenario computed (ADR-0230). That makes assembly
//! shell-first: a probe loaded before the shell exists announces into nothing
//! and is never routed to.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
};
use aether_test_fixtures_kinds::{
    DrainEditorInputs, DrainEditorInputsResult, EditorRegionProbeConfig, ObservedEditorInput,
};
use aether_widget::{EditorShell, RegionAttach};
use core::mem;

pub struct EditorRegionProbe {
    region_name: String,
    inputs: Vec<ObservedEditorInput>,
}

#[actor(instanced, root, depends(EditorShell))]
impl WasmActor for EditorRegionProbe {
    type Config = EditorRegionProbeConfig;
    const NAMESPACE: &'static str = "test.editor_region_probe";

    fn init(config: EditorRegionProbeConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { region_name: config.name, inputs: Vec::new() })
    }

    /// Tell the shell which declared region this probe stands behind. The
    /// shell keeps this send's sender as the region's address.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        ctx.send::<EditorShell>(&RegionAttach { region: self.region_name.clone() });
    }

    #[handler::event]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, MouseButton { button, x, y, .. }: MouseButton) {
        self.inputs.push(ObservedEditorInput::PointerPress { button, x_pixels: x, y_pixels: y });
    }

    #[handler::event]
    fn on_mouse_button_release(
        &mut self,
        _ctx: &mut WasmCtx<'_>,
        MouseButtonRelease { button, x, y, .. }: MouseButtonRelease,
    ) {
        self.inputs.push(ObservedEditorInput::PointerRelease { button, x_pixels: x, y_pixels: y });
    }

    #[handler::event]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, MouseMove { x, y, .. }: MouseMove) {
        self.inputs.push(ObservedEditorInput::PointerMotion { x_pixels: x, y_pixels: y });
    }

    #[handler::event]
    fn on_mouse_wheel(&mut self, _ctx: &mut WasmCtx<'_>, MouseWheel { delta_x, delta_y, x, y, .. }: MouseWheel) {
        self.inputs.push(ObservedEditorInput::Wheel {
            delta_x_pixels: delta_x,
            delta_y_pixels: delta_y,
            x_pixels: x,
            y_pixels: y,
        });
    }

    #[handler::event]
    fn on_key(&mut self, _ctx: &mut WasmCtx<'_>, Key { code, .. }: Key) {
        self.inputs.push(ObservedEditorInput::KeyPress { code });
    }

    #[handler::event]
    fn on_key_release(&mut self, _ctx: &mut WasmCtx<'_>, KeyRelease { code, .. }: KeyRelease) {
        self.inputs.push(ObservedEditorInput::KeyRelease { code });
    }

    #[handler::event]
    fn on_text_input(&mut self, _ctx: &mut WasmCtx<'_>, input: TextInput) {
        self.inputs.push(ObservedEditorInput::TextInput { text: input.text });
    }

    #[handler::event]
    fn on_ime_preedit(&mut self, _ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        self.inputs.push(ObservedEditorInput::ImePreedit {
            text: preedit.text,
            cursor_begin: preedit.cursor_begin,
            cursor_end: preedit.cursor_end,
        });
    }

    #[handler::event]
    fn on_modifiers(&mut self, _ctx: &mut WasmCtx<'_>, Modifiers { shift, ctrl, alt, meta, .. }: Modifiers) {
        self.inputs.push(ObservedEditorInput::Modifiers { shift, ctrl, alt, meta });
    }

    #[handler::request]
    fn on_drain_editor_inputs(&mut self, _ctx: &mut WasmCtx<'_>, _query: DrainEditorInputs) -> DrainEditorInputsResult {
        DrainEditorInputsResult { region_name: self.region_name.clone(), inputs: mem::take(&mut self.inputs) }
    }
}
