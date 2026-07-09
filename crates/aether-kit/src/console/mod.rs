// `#[handler]` methods take decoded mail by value per the actor ABI.
#![allow(clippy::needless_pass_by_value)]

mod kinds;
mod state;

pub use kinds::*;
pub use state::*;

use alloc::vec::Vec;

use aether_actor::{ActorInitError, Manual, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::input::{InputCapability, InputMailboxExt};
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::render::{DrawSolidQuads, SolidQuad};
use aether_capabilities::text::{
    DrawText, FontMetricsRequest, FontMetricsResult, FontRef, LoadFont, LoadFontResult,
};
use aether_capabilities::{LifecycleCapability, RenderCapability, TextCapability};
use aether_data::MailboxId;
use aether_kinds::keycode::{KEY_BACKSPACE, KEY_DOWN, KEY_ENTER, KEY_LEFT, KEY_RIGHT, KEY_UP};
use aether_kinds::{
    CachedFontMetrics, Key, MouseWheel, QuadSpace, Quit, TextInput, Tick, WindowSize,
};

const HORIZONTAL_PADDING: f32 = 12.0;
const TOP_PADDING: f32 = 10.0;
const BOTTOM_PADDING: f32 = 10.0;
const SEPARATOR_HEIGHT: f32 = 1.0;
const CURSOR_WIDTH: f32 = 8.0;

pub struct ConsoleOverlay {
    config: ConsoleConfig,
    state: ConsoleState,
    window_size: [u32; 2],
    font_id: Option<u32>,
    metrics: Option<CachedFontMetrics>,
}

impl ConsoleOverlay {
    fn visible_rows(&self) -> usize {
        let available =
            (self.config.panel_height - self.input_band_height() - TOP_PADDING).max(0.0);
        (available / self.row_height()).floor().max(0.0) as usize
    }

    fn row_height(&self) -> f32 {
        (self.config.font_size * 1.25).max(1.0)
    }

    fn input_band_height(&self) -> f32 {
        self.row_height() + BOTTOM_PADDING
    }

    fn render(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        if !self.state.open {
            return;
        }

        let width = self.window_size[0] as f32;
        if width <= 0.0 || self.config.panel_height <= 0.0 {
            return;
        }

        let visible_rows = self.visible_rows();
        self.state.clamp_scroll(visible_rows);
        let panel_height = self.config.panel_height.min(self.window_size[1] as f32);
        let mut quads = vec![
            SolidQuad {
                x: 0.0,
                y: 0.0,
                width,
                height: panel_height,
                color: self.config.theme.background_color,
            },
            SolidQuad {
                x: 0.0,
                y: panel_height - SEPARATOR_HEIGHT,
                width,
                height: SEPARATOR_HEIGHT,
                color: self.config.theme.separator_color,
            },
        ];

        let input_y = (panel_height - BOTTOM_PADDING - self.config.font_size).max(TOP_PADDING);
        if self.state.cursor_visible {
            let prompt_width = self.measure(&self.config.prompt);
            let caret_text_width = self.measure_prefix(&self.state.input, self.state.caret);
            quads.push(SolidQuad {
                x: HORIZONTAL_PADDING + prompt_width + caret_text_width,
                y: input_y,
                width: self.cursor_width(),
                height: self.config.font_size,
                color: self.config.theme.cursor_color,
            });
        }

        ctx.actor::<RenderCapability>().send(&DrawSolidQuads {
            space: QuadSpace::Screen,
            quads,
        });

        let Some(font_id) = self.font_id else {
            return;
        };

        let mut y = TOP_PADDING + self.config.font_size;
        for line in self.state.visible_history(visible_rows) {
            ctx.actor::<TextCapability>().send(&DrawText {
                font_id,
                text: line.text,
                size_pixels: self.config.font_size,
                color: ConsoleState::theme_color(&self.config.theme, line.style),
                origin: [HORIZONTAL_PADDING, y],
                space: QuadSpace::Screen,
            });
            y += self.row_height();
        }

        ctx.actor::<TextCapability>().send(&DrawText {
            font_id,
            text: self.config.prompt.clone(),
            size_pixels: self.config.font_size,
            color: self.config.theme.prompt_color,
            origin: [HORIZONTAL_PADDING, input_y],
            space: QuadSpace::Screen,
        });
        ctx.actor::<TextCapability>().send(&DrawText {
            font_id,
            text: self.state.input.clone(),
            size_pixels: self.config.font_size,
            color: self.config.theme.input_color,
            origin: [
                HORIZONTAL_PADDING + self.measure(&self.config.prompt),
                input_y,
            ],
            space: QuadSpace::Screen,
        });
    }

    fn measure(&self, text: &str) -> f32 {
        self.metrics.as_ref().map_or_else(
            || text.chars().count() as f32 * self.cursor_width(),
            |metrics| metrics.measure(text, self.config.font_size),
        )
    }

    fn measure_prefix(&self, text: &str, caret: usize) -> f32 {
        self.metrics.as_ref().map_or_else(
            || caret as f32 * self.cursor_width(),
            |metrics| metrics.caret_x(text, caret, self.config.font_size),
        )
    }

    fn cursor_width(&self) -> f32 {
        self.metrics
            .as_ref()
            .map_or(CURSOR_WIDTH, |metrics| {
                metrics.measure("M", self.config.font_size)
            })
            .max(2.0)
    }

    fn is_activation_text(&self, text: &str) -> bool {
        let mut chars = text.chars();
        let Some(ch) = chars.next() else {
            return false;
        };
        chars.next().is_none() && ch as u32 == self.config.activation_key_code
    }

    fn dispatch_actions(&mut self, ctx: &mut WasmCtx<'_, Manual>, actions: Vec<ConsoleAction>) {
        for action in actions {
            match action {
                ConsoleAction::InvokeExternal { mailbox, payload } => {
                    ctx.send_to(mailbox, &payload);
                }
                ConsoleAction::Quit => {
                    ctx.actor::<LifecycleCapability>().send(&Quit);
                }
            }
        }
    }

    fn register_command(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: RegisterConsoleCommand) {
        let mailbox = if mail.mailbox == MailboxId::NONE {
            ctx.source_mailbox().unwrap_or(MailboxId::NONE)
        } else {
            mail.mailbox
        };
        if mailbox == MailboxId::NONE {
            self.state.push_error(format!(
                "cannot register command without mailbox: {}",
                mail.name
            ));
            return;
        }
        self.state
            .register_external(mail.name, mail.description, mailbox);
    }
}

#[actor(instanced)]
impl WasmActor for ConsoleOverlay {
    type Config = ConsoleConfig;
    const NAMESPACE: &'static str = "aether.kit.console";

    fn init(config: ConsoleConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self {
            state: ConsoleState::new(&config),
            config,
            window_size: [1280, 720],
            font_id: None,
            metrics: None,
        })
    }

    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
        let input = ctx.actor::<InputCapability>();
        input.subscribe::<Key>();
        input.subscribe::<TextInput>();
        input.subscribe::<MouseWheel>();
        input.subscribe::<WindowSize>();

        let font = FontRef::Path {
            namespace: self.config.font_namespace.clone(),
            path: self.config.font_path.clone(),
        };
        ctx.actor::<TextCapability>().send(&LoadFont {
            namespace: self.config.font_namespace.clone(),
            path: self.config.font_path.clone(),
        });
        ctx.actor::<TextCapability>()
            .send(&FontMetricsRequest { font });
    }

    #[handler::manual]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_, Manual>, _tick: Tick) {
        self.state.tick_cursor();
        self.render(ctx);
    }

    #[handler::manual]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_, Manual>, key: Key) {
        if key.code == self.config.activation_key_code {
            self.state.open = !self.state.open;
            self.state.cursor_visible = true;
            return;
        }
        if !self.state.open {
            return;
        }

        match key.code {
            KEY_ENTER => {
                let actions = self.state.submit(&self.config.prompt);
                self.dispatch_actions(ctx, actions);
            }
            KEY_BACKSPACE => self.state.backspace(),
            KEY_LEFT => self.state.move_left(),
            KEY_RIGHT => self.state.move_right(),
            KEY_UP => self.state.history_prev(),
            KEY_DOWN => self.state.history_next(),
            _ => {}
        }
    }

    #[handler::manual]
    fn on_text_input(&mut self, _ctx: &mut WasmCtx<'_, Manual>, input: TextInput) {
        if self.state.open {
            if self.is_activation_text(&input.text) {
                return;
            }
            self.state.insert_text(&input.text);
        }
    }

    #[handler::manual]
    fn on_mouse_wheel(&mut self, _ctx: &mut WasmCtx<'_, Manual>, wheel: MouseWheel) {
        if !self.state.open {
            return;
        }
        let rows = if wheel.delta_y > 0.0 {
            3
        } else if wheel.delta_y < 0.0 {
            -3
        } else {
            0
        };
        self.state.scroll_by(rows, self.visible_rows());
    }

    #[handler::manual]
    fn on_window_size(&mut self, _ctx: &mut WasmCtx<'_, Manual>, size: WindowSize) {
        self.window_size = [size.width, size.height];
    }

    #[handler::manual]
    fn on_load_font_result(&mut self, _ctx: &mut WasmCtx<'_, Manual>, result: LoadFontResult) {
        match result {
            LoadFontResult::Ok { font_id, .. } => self.font_id = Some(font_id),
            LoadFontResult::Err { error, .. } => {
                self.state.push_error(format!("font load failed: {error}"));
            }
        }
    }

    #[handler::manual]
    fn on_font_metrics_result(
        &mut self,
        _ctx: &mut WasmCtx<'_, Manual>,
        result: FontMetricsResult,
    ) {
        match result {
            FontMetricsResult::Ok { metrics } => {
                self.metrics = Some(CachedFontMetrics::new(&metrics));
            }
            FontMetricsResult::Err { error } => {
                self.state
                    .push_error(format!("font metrics failed: {error}"));
            }
        }
    }

    #[handler::manual]
    fn on_register_command(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: RegisterConsoleCommand) {
        self.register_command(ctx, mail);
    }

    #[handler::manual]
    fn on_unregister_command(
        &mut self,
        _ctx: &mut WasmCtx<'_, Manual>,
        mail: UnregisterConsoleCommand,
    ) {
        self.state.unregister_external(&mail.name);
    }

    #[handler::manual]
    fn on_command_output(&mut self, _ctx: &mut WasmCtx<'_, Manual>, output: ConsoleCommandOutput) {
        self.state.append_command_output(output.lines, output.error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_rows_reserve_input_band() {
        let overlay = ConsoleOverlay {
            config: ConsoleConfig {
                panel_height: 100.0,
                font_size: 10.0,
                ..ConsoleConfig::default()
            },
            state: ConsoleState::new(&ConsoleConfig::default()),
            window_size: [100, 100],
            font_id: None,
            metrics: None,
        };

        assert_eq!(overlay.visible_rows(), 5);
    }

    #[test]
    fn default_activation_key_is_backquote_codepoint() {
        assert_eq!(ConsoleConfig::default().activation_key_code, b'`' as u32);
    }

    #[test]
    fn activation_text_matches_single_activation_character_only() {
        let overlay = ConsoleOverlay {
            config: ConsoleConfig::default(),
            state: ConsoleState::new(&ConsoleConfig::default()),
            window_size: [100, 100],
            font_id: None,
            metrics: None,
        };

        assert!(overlay.is_activation_text("`"));
        assert!(!overlay.is_activation_text("``"));
        assert!(!overlay.is_activation_text("a"));
    }
}
