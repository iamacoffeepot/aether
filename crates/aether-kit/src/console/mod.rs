// `#[handler]` methods take decoded mail by value per the actor ABI.
#![allow(clippy::needless_pass_by_value)]

mod kinds;
mod markdown;
mod state;

pub use kinds::*;
pub use state::*;

use alloc::vec::Vec;

use aether_actor::{ActorInitError, Manual, ReplyMode, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::input::{InputCapability, InputMailboxExt};
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::text::{
    DrawText, FontMetricsRequest, FontMetricsResult, FontRef, LoadFont, LoadFontBytes, LoadFontResult,
    MEMORY_FONT_NAMESPACE,
};
use aether_capabilities::{LifecycleCapability, TextCapability};
use aether_data::MailboxId;
use aether_kinds::keycode::{KEY_BACKSPACE, KEY_DOWN, KEY_ENTER, KEY_LEFT, KEY_RIGHT, KEY_UP};
use aether_kinds::{CachedFontMetrics, Key, KeyRelease, MouseWheel, QuadSpace, Quit, TextInput, Tick, WindowSize};
use aether_math::Rgba;
use aether_render::{DrawSolidQuads, RenderCapability, SolidQuad};
use serde::{Deserialize, Serialize};

use self::markdown::{MarkdownLine, MarkdownTone};

const HORIZONTAL_PADDING: f32 = 12.0;
const TOP_PADDING: f32 = 10.0;
const BOTTOM_PADDING: f32 = 10.0;
const HISTORY_INPUT_GAP: f32 = 8.0;
const SEPARATOR_HEIGHT: f32 = 1.0;
const CURSOR_WIDTH: f32 = 8.0;
const BACKSPACE_INITIAL_DELAY_TICKS: u32 = 18;
const BACKSPACE_REPEAT_INTERVAL_TICKS: u32 = 3;
const EMBEDDED_FONT_NAME: &str = "SourceCodePro-Regular.ttf";
const EMBEDDED_FONT_BYTES: &[u8] = include_bytes!("../../assets/fonts/SourceCodePro-Regular.ttf");

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.console.font_load_context")]
struct ConsoleFontLoadContext {
    embedded: bool,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.console.font_metrics_context")]
struct ConsoleFontMetricsContext {
    font_id: u32,
}

pub struct ConsoleOverlay {
    config: ConsoleConfig,
    state: ConsoleState,
    window_size: [u32; 2],
    font_id: Option<u32>,
    metrics: Option<CachedFontMetrics>,
    embedded_font_requested: bool,
    override_font_failed: bool,
    backspace_held: bool,
    backspace_ticks: u32,
}

impl ConsoleOverlay {
    fn visible_rows(&self) -> usize {
        let available = (self.input_y() - HISTORY_INPUT_GAP - self.history_top_y() - self.config.font_size).max(0.0);
        f32_floor_to_usize(available / self.row_height()) + usize::from(available > 0.0)
    }

    fn row_height(&self) -> f32 {
        (self.config.font_size * 1.25).max(1.0)
    }

    fn history_top_y(&self) -> f32 {
        TOP_PADDING + self.config.font_size
    }

    fn input_y(&self) -> f32 {
        let panel_height = self.config.panel_height.min(bounded_u32_to_f32(self.window_size[1]));
        (panel_height - BOTTOM_PADDING - self.config.font_size).max(TOP_PADDING)
    }

    fn render(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        if !self.state.open {
            return;
        }

        let width = bounded_u32_to_f32(self.window_size[0]);
        if width <= 0.0 || self.config.panel_height <= 0.0 {
            return;
        }

        let visible_rows = self.visible_rows();
        self.state.clamp_scroll(visible_rows);
        let panel_height = self.config.panel_height.min(bounded_u32_to_f32(self.window_size[1]));
        let history = self.state.visible_markdown_history(visible_rows);
        let mut quads = vec![
            SolidQuad { x: 0.0, y: 0.0, width, height: panel_height, color: self.config.theme.background_color },
            SolidQuad {
                x: 0.0,
                y: panel_height - SEPARATOR_HEIGHT,
                width,
                height: SEPARATOR_HEIGHT,
                color: self.config.theme.separator_color,
            },
        ];

        let input_y = self.input_y();
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

        let mut y = self.history_top_y();
        for line in &history {
            if y + self.config.font_size > input_y - HISTORY_INPUT_GAP {
                break;
            }
            self.push_markdown_quads(&mut quads, line, y, width);
            y += self.row_height();
        }

        ctx.actor::<RenderCapability>().send(&DrawSolidQuads { space: QuadSpace::Screen, clip: None, quads });

        let Some(font_id) = self.font_id else {
            return;
        };

        let mut y = self.history_top_y();
        for line in &history {
            if y + self.config.font_size > input_y - HISTORY_INPUT_GAP {
                break;
            }
            self.draw_markdown_line(ctx, font_id, line, y);
            y += self.row_height();
        }

        ctx.actor::<TextCapability>().send(&DrawText {
            font_id,
            text: self.config.prompt.clone(),
            size_pixels: self.config.font_size,
            color: self.config.theme.prompt_color,
            origin: [HORIZONTAL_PADDING, input_y],
            space: QuadSpace::Screen,
            clip: None,
        });
        ctx.actor::<TextCapability>().send(&DrawText {
            font_id,
            text: self.state.input.clone(),
            size_pixels: self.config.font_size,
            color: self.config.theme.input_color,
            origin: [HORIZONTAL_PADDING + self.measure(&self.config.prompt), input_y],
            space: QuadSpace::Screen,
            clip: None,
        });
    }

    fn push_markdown_quads(&self, quads: &mut Vec<SolidQuad>, line: &MarkdownLine, y: f32, width: f32) {
        let padding = self.config.theme.markdown.code_padding_pixels.max(0.0);
        if line.thematic_break {
            quads.push(SolidQuad {
                x: HORIZONTAL_PADDING,
                y: self.config.font_size.mul_add(0.55, y),
                width: HORIZONTAL_PADDING.mul_add(-2.0, width).max(0.0),
                height: 1.0,
                color: self.config.theme.markdown.thematic_break_color,
            });
            return;
        }
        if line.code_block {
            quads.push(SolidQuad {
                x: HORIZONTAL_PADDING - padding,
                y: y - padding,
                width: padding.mul_add(2.0, HORIZONTAL_PADDING.mul_add(-2.0, width)),
                height: self.config.font_size + (padding * 2.0),
                color: self.config.theme.markdown.fenced_code_background_color,
            });
            return;
        }

        let mut x = HORIZONTAL_PADDING;
        for run in &line.runs {
            let run_width = self.measure(&run.text);
            if run.tone == MarkdownTone::InlineCode {
                quads.push(SolidQuad {
                    x: x - padding,
                    y: y - padding,
                    width: run_width + (padding * 2.0),
                    height: self.config.font_size + (padding * 2.0),
                    color: self.config.theme.markdown.inline_code_background_color,
                });
            }
            x += run_width;
        }
    }

    fn draw_markdown_line(&self, ctx: &mut WasmCtx<'_, Manual>, font_id: u32, line: &MarkdownLine, y: f32) {
        let mut x = HORIZONTAL_PADDING;
        for run in &line.runs {
            if run.text.is_empty() || run.tone == MarkdownTone::ThematicBreak {
                continue;
            }
            let color = self.markdown_color(line.style, run.tone);
            ctx.actor::<TextCapability>().send(&DrawText {
                font_id,
                text: run.text.clone(),
                size_pixels: self.config.font_size,
                color,
                origin: [x, y],
                space: QuadSpace::Screen,
                clip: None,
            });
            if run.tone == MarkdownTone::Strong {
                ctx.actor::<TextCapability>().send(&DrawText {
                    font_id,
                    text: run.text.clone(),
                    size_pixels: self.config.font_size,
                    color,
                    origin: [x + self.config.theme.markdown.strong_offset_pixels.max(0.0), y],
                    space: QuadSpace::Screen,
                    clip: None,
                });
            }
            x += self.measure(&run.text);
        }
    }

    fn markdown_color(&self, style: LineStyle, tone: MarkdownTone) -> Rgba {
        if style == LineStyle::Error
            && matches!(
                tone,
                MarkdownTone::Text
                    | MarkdownTone::Heading
                    | MarkdownTone::Emphasis
                    | MarkdownTone::Strong
                    | MarkdownTone::InlineCode
                    | MarkdownTone::FencedCode
                    | MarkdownTone::Link
                    | MarkdownTone::Image
                    | MarkdownTone::QuoteText
                    | MarkdownTone::TableHeader
                    | MarkdownTone::TableText
            )
        {
            return self.config.theme.error_color;
        }

        let markdown = &self.config.theme.markdown;
        match tone {
            MarkdownTone::Text => ConsoleState::theme_color(&self.config.theme, style),
            MarkdownTone::Heading => markdown.heading_color,
            MarkdownTone::Emphasis => markdown.emphasis_color,
            MarkdownTone::Strong => markdown.strong_color,
            MarkdownTone::InlineCode => markdown.inline_code_color,
            MarkdownTone::FencedCode => markdown.fenced_code_color,
            MarkdownTone::Link => markdown.link_color,
            MarkdownTone::Image => markdown.image_color,
            MarkdownTone::QuoteMarker => markdown.quote_marker_color,
            MarkdownTone::QuoteText => markdown.quote_text_color,
            MarkdownTone::ListMarker => markdown.list_marker_color,
            MarkdownTone::TaskMarker => markdown.task_marker_color,
            MarkdownTone::TableBorder => markdown.table_border_color,
            MarkdownTone::TableHeader => markdown.table_header_color,
            MarkdownTone::TableText => markdown.table_text_color,
            MarkdownTone::ThematicBreak => markdown.thematic_break_color,
            MarkdownTone::MutedMarker => markdown.muted_marker_color,
            MarkdownTone::EscapedMarker => markdown.escaped_marker_color,
        }
    }

    fn measure(&self, text: &str) -> f32 {
        self.metrics.as_ref().map_or_else(
            || bounded_usize_to_f32(text.chars().count()) * self.cursor_width(),
            |metrics| metrics.measure(text, self.config.font_size),
        )
    }

    fn measure_prefix(&self, text: &str, caret: usize) -> f32 {
        self.metrics.as_ref().map_or_else(
            || bounded_usize_to_f32(caret) * self.cursor_width(),
            |metrics| metrics.caret_x(text, caret, self.config.font_size),
        )
    }

    fn cursor_width(&self) -> f32 {
        self.metrics.as_ref().map_or(CURSOR_WIDTH, |metrics| metrics.measure("M", self.config.font_size)).max(2.0)
    }

    fn is_activation_text(text: &str, activation_key_code: u32) -> bool {
        let mut chars = text.chars();
        let Some(ch) = chars.next() else {
            return false;
        };
        chars.next().is_none() && u32::from(ch) == activation_key_code
    }

    fn dispatch_actions(ctx: &mut WasmCtx<'_, Manual>, actions: Vec<ConsoleAction>) {
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

    fn has_font_override(config: &ConsoleConfig) -> bool {
        !config.font_path.is_empty()
    }

    fn request_initial_font(&mut self, ctx: &mut WasmCtx<'_>) {
        if Self::has_font_override(&self.config) {
            self.request_configured_font(ctx);
        } else {
            self.request_embedded_font(ctx);
        }
    }

    fn request_configured_font<M: ReplyMode>(&self, ctx: &mut WasmCtx<'_, M>) {
        Self::request_font(ctx, self.config.font_namespace.as_str(), self.config.font_path.as_str(), false);
    }

    fn request_embedded_font<M: ReplyMode>(&mut self, ctx: &mut WasmCtx<'_, M>) {
        if self.embedded_font_requested {
            return;
        }
        self.embedded_font_requested = true;
        let load = LoadFontBytes { name: EMBEDDED_FONT_NAME.into(), bytes: EMBEDDED_FONT_BYTES.to_vec() };
        let _ = ctx.actor::<TextCapability>().send_with_context(&load, &ConsoleFontLoadContext { embedded: true });
    }

    fn request_font<M: ReplyMode>(ctx: &mut WasmCtx<'_, M>, namespace: &str, path: &str, embedded: bool) {
        let load = LoadFont { namespace: namespace.into(), path: path.into() };
        let _ = ctx.actor::<TextCapability>().send_with_context(&load, &ConsoleFontLoadContext { embedded });
    }

    fn request_font_metrics(ctx: &mut WasmCtx<'_, Manual>, font_id: u32) {
        let request = FontMetricsRequest { font: FontRef::Id(font_id) };
        let _ = ctx.actor::<TextCapability>().send_with_context(&request, &ConsoleFontMetricsContext { font_id });
    }

    fn is_embedded_font_ref(namespace: &str, path: &str) -> bool {
        namespace == MEMORY_FONT_NAMESPACE && path == EMBEDDED_FONT_NAME
    }

    fn handle_override_font_failure(&mut self, ctx: &mut WasmCtx<'_, Manual>, error: String) {
        if !self.override_font_failed {
            self.override_font_failed = true;
            self.state.push_error(format!("font override failed: {error}"));
        }
        self.request_embedded_font(ctx);
    }

    fn register_command(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: RegisterConsoleCommand) {
        let mailbox = if mail.mailbox == MailboxId::NONE {
            ctx.source_mailbox().unwrap_or(MailboxId::NONE)
        } else {
            mail.mailbox
        };
        if mailbox == MailboxId::NONE {
            self.state.push_error(format!("cannot register command without mailbox: {}", mail.name));
            return;
        }
        self.state.register_external(mail.name, mail.description, mailbox);
    }

    fn press_backspace(&mut self) {
        self.state.backspace();
        self.backspace_held = true;
        self.backspace_ticks = 0;
    }

    fn release_backspace(&mut self) {
        self.backspace_held = false;
        self.backspace_ticks = 0;
    }

    fn tick_backspace_repeat(&mut self) {
        if !self.state.open || !self.backspace_held {
            return;
        }

        self.backspace_ticks = self.backspace_ticks.saturating_add(1);
        if self.backspace_ticks < BACKSPACE_INITIAL_DELAY_TICKS {
            return;
        }
        if (self.backspace_ticks - BACKSPACE_INITIAL_DELAY_TICKS).is_multiple_of(BACKSPACE_REPEAT_INTERVAL_TICKS) {
            self.state.backspace();
        }
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
            embedded_font_requested: false,
            override_font_failed: false,
            backspace_held: false,
            backspace_ticks: 0,
        })
    }

    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
        let input = ctx.actor::<InputCapability>();
        if self.config.owns_input {
            input.subscribe::<Key>();
            input.subscribe::<KeyRelease>();
            input.subscribe::<TextInput>();
            input.subscribe::<MouseWheel>();
        }
        input.subscribe::<WindowSize>();

        self.request_initial_font(ctx);
    }

    #[handler::manual]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_, Manual>, _tick: Tick) {
        self.state.tick_cursor();
        self.tick_backspace_repeat();
        self.render(ctx);
    }

    #[handler::manual]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_, Manual>, key: Key) {
        if key.code == self.config.activation_key_code {
            self.state.open = !self.state.open;
            self.state.cursor_visible = true;
            if !self.state.open {
                self.release_backspace();
            }
            return;
        }
        if !self.state.open {
            return;
        }

        match key.code {
            KEY_ENTER => {
                let actions = self.state.submit(&self.config.prompt);
                Self::dispatch_actions(ctx, actions);
            }
            KEY_BACKSPACE => self.press_backspace(),
            KEY_LEFT => self.state.move_left(),
            KEY_RIGHT => self.state.move_right(),
            KEY_UP => self.state.history_prev(),
            KEY_DOWN => self.state.history_next(),
            _ => {}
        }
    }

    #[handler::manual]
    fn on_key_release(&mut self, _ctx: &mut WasmCtx<'_, Manual>, key: KeyRelease) {
        if key.code == KEY_BACKSPACE {
            self.release_backspace();
        }
    }

    #[handler::manual]
    fn on_text_input(&mut self, _ctx: &mut WasmCtx<'_, Manual>, input: TextInput) {
        if self.state.open {
            if Self::is_activation_text(&input.text, self.config.activation_key_code) {
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
    fn on_load_font_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: LoadFontResult) {
        let Some(context) = ctx.take_context::<ConsoleFontLoadContext>() else {
            return;
        };
        match result {
            LoadFontResult::Ok { font_id, .. } => {
                self.font_id = Some(font_id);
                Self::request_font_metrics(ctx, font_id);
            }
            LoadFontResult::Err { namespace, path, error } => {
                if context.embedded || Self::is_embedded_font_ref(&namespace, &path) {
                    self.state.push_error(format!("embedded font load failed: {error}"));
                } else {
                    self.handle_override_font_failure(ctx, error);
                }
            }
        }
    }

    #[handler::manual]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, result: FontMetricsResult) {
        let Some(_context) = ctx.take_context::<ConsoleFontMetricsContext>() else {
            return;
        };
        match result {
            FontMetricsResult::Ok { metrics } => {
                self.metrics = Some(CachedFontMetrics::new(&metrics));
            }
            FontMetricsResult::Err { error } => {
                self.state.push_error(format!("font metrics failed: {error}"));
            }
        }
    }

    #[handler::manual]
    fn on_register_command(&mut self, ctx: &mut WasmCtx<'_, Manual>, mail: RegisterConsoleCommand) {
        self.register_command(ctx, mail);
    }

    #[handler::manual]
    fn on_unregister_command(&mut self, _ctx: &mut WasmCtx<'_, Manual>, mail: UnregisterConsoleCommand) {
        self.state.unregister_external(&mail.name);
    }

    #[handler::manual]
    fn on_command_output(&mut self, _ctx: &mut WasmCtx<'_, Manual>, output: ConsoleCommandOutput) {
        self.state.append_command_output(output.lines, output.error);
    }
}

fn bounded_u32_to_f32(value: u32) -> f32 {
    let bounded = u16::try_from(value).unwrap_or(u16::MAX);
    f32::from(bounded)
}

fn bounded_usize_to_f32(value: usize) -> f32 {
    let bounded = u16::try_from(value).unwrap_or(u16::MAX);
    f32::from(bounded)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn f32_floor_to_usize(value: f32) -> usize {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }

    // Row counts come from bounded panel pixel geometry and are small in practice.
    value.floor() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay(config: ConsoleConfig) -> ConsoleOverlay {
        ConsoleOverlay {
            state: ConsoleState::new(&config),
            config,
            window_size: [100, 100],
            font_id: None,
            metrics: None,
            embedded_font_requested: false,
            override_font_failed: false,
            backspace_held: false,
            backspace_ticks: 0,
        }
    }

    #[test]
    fn visible_rows_reserve_input_band() {
        let overlay = overlay(ConsoleConfig { panel_height: 100.0, font_size: 10.0, ..ConsoleConfig::default() });

        assert_eq!(overlay.visible_rows(), 4);
    }

    #[test]
    fn default_font_size_is_larger_than_initial_sketch() {
        assert_eq!(ConsoleConfig::default().font_size, 18.0);
    }

    #[test]
    fn default_activation_key_is_backquote_codepoint() {
        assert_eq!(ConsoleConfig::default().activation_key_code, u32::from(b'`'));
    }

    #[test]
    fn activation_text_matches_single_activation_character_only() {
        let overlay = overlay(ConsoleConfig::default());

        assert!(ConsoleOverlay::is_activation_text("`", overlay.config.activation_key_code));
        assert!(!ConsoleOverlay::is_activation_text("``", overlay.config.activation_key_code));
        assert!(!ConsoleOverlay::is_activation_text("a", overlay.config.activation_key_code));
    }

    #[test]
    fn held_backspace_repeats_after_initial_delay() {
        let mut overlay = overlay(ConsoleConfig::default());
        overlay.state.open = true;
        overlay.state.insert_text("abcd");

        overlay.press_backspace();
        for _ in 0..BACKSPACE_INITIAL_DELAY_TICKS {
            overlay.tick_backspace_repeat();
        }

        assert_eq!(overlay.state.input, "ab");
        overlay.release_backspace();
        for _ in 0..BACKSPACE_REPEAT_INTERVAL_TICKS {
            overlay.tick_backspace_repeat();
        }
        assert_eq!(overlay.state.input, "ab");
    }

    #[test]
    fn markdown_tones_map_to_explicit_theme_fields() {
        let overlay = overlay(ConsoleConfig::default());
        let markdown = overlay.config.theme.markdown;

        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::Heading), markdown.heading_color);
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::Emphasis), markdown.emphasis_color);
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::Strong), markdown.strong_color);
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::InlineCode), markdown.inline_code_color);
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::FencedCode), markdown.fenced_code_color);
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::Link), markdown.link_color);
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::Image), markdown.image_color);
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::QuoteMarker), markdown.quote_marker_color);
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::QuoteText), markdown.quote_text_color);
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::ListMarker), markdown.list_marker_color);
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::TaskMarker), markdown.task_marker_color);
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::TableBorder), markdown.table_border_color);
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::TableHeader), markdown.table_header_color);
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::TableText), markdown.table_text_color);
        assert_eq!(
            overlay.markdown_color(LineStyle::Output, MarkdownTone::ThematicBreak),
            markdown.thematic_break_color
        );
        assert_eq!(overlay.markdown_color(LineStyle::Output, MarkdownTone::MutedMarker), markdown.muted_marker_color);
        assert_eq!(
            overlay.markdown_color(LineStyle::Output, MarkdownTone::EscapedMarker),
            markdown.escaped_marker_color
        );
    }
}
