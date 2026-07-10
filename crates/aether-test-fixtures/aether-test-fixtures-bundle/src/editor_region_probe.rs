//! Typed sink used to assert editor-shell routing without giving peer regions
//! their own input subscriptions.

use aether_actor::{ActorInitError, Manual, OutboundReply, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
};
use aether_test_fixtures_kinds::{
    DrainEditorInputs, DrainEditorInputsResult, EditorRegionProbeConfig, ObservedEditorInput,
};
use core::mem;

pub struct EditorRegionProbe {
    region_name: String,
    inputs: Vec<ObservedEditorInput>,
}

#[actor(instanced)]
impl WasmActor for EditorRegionProbe {
    type Config = EditorRegionProbeConfig;
    const NAMESPACE: &'static str = "test.editor_region_probe";

    fn init(config: EditorRegionProbeConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { region_name: config.name, inputs: Vec::new() })
    }

    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, press: MouseButton) {
        self.inputs.push(ObservedEditorInput::PointerPress {
            button: press.button,
            x_pixels: press.x,
            y_pixels: press.y,
        });
    }

    #[handler::single]
    fn on_mouse_button_release(&mut self, _ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        self.inputs.push(ObservedEditorInput::PointerRelease {
            button: release.button,
            x_pixels: release.x,
            y_pixels: release.y,
        });
    }

    #[handler::single]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        self.inputs.push(ObservedEditorInput::PointerMotion { x_pixels: moved.x, y_pixels: moved.y });
    }

    #[handler::single]
    fn on_mouse_wheel(&mut self, _ctx: &mut WasmCtx<'_>, wheel: MouseWheel) {
        self.inputs.push(ObservedEditorInput::Wheel {
            delta_x_pixels: wheel.delta_x,
            delta_y_pixels: wheel.delta_y,
            x_pixels: wheel.x,
            y_pixels: wheel.y,
        });
    }

    #[handler::single]
    fn on_key(&mut self, _ctx: &mut WasmCtx<'_>, key: Key) {
        self.inputs.push(ObservedEditorInput::KeyPress { code: key.code });
    }

    #[handler::single]
    fn on_key_release(&mut self, _ctx: &mut WasmCtx<'_>, release: KeyRelease) {
        self.inputs.push(ObservedEditorInput::KeyRelease { code: release.code });
    }

    #[handler::single]
    fn on_text_input(&mut self, _ctx: &mut WasmCtx<'_>, input: TextInput) {
        self.inputs.push(ObservedEditorInput::TextInput { text: input.text });
    }

    #[handler::single]
    fn on_ime_preedit(&mut self, _ctx: &mut WasmCtx<'_>, preedit: ImePreedit) {
        self.inputs.push(ObservedEditorInput::ImePreedit {
            text: preedit.text,
            cursor_begin: preedit.cursor_begin,
            cursor_end: preedit.cursor_end,
        });
    }

    #[handler::single]
    fn on_modifiers(&mut self, _ctx: &mut WasmCtx<'_>, modifiers: Modifiers) {
        self.inputs.push(ObservedEditorInput::Modifiers {
            shift: modifiers.shift,
            ctrl: modifiers.ctrl,
            alt: modifiers.alt,
            meta: modifiers.meta,
        });
    }

    #[handler::manual]
    fn on_drain_editor_inputs(&mut self, ctx: &mut WasmCtx<'_, Manual>, _query: DrainEditorInputs) {
        if ctx.reply_target().is_some() {
            ctx.reply(&DrainEditorInputsResult {
                region_name: self.region_name.clone(),
                inputs: mem::take(&mut self.inputs),
            });
        }
    }
}
