use alloc::string::String;
use alloc::vec::Vec;

use aether_data::MailboxId;
use serde::{Deserialize, Serialize};

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct ConsoleTheme {
    pub background_color: [f32; 4],
    pub separator_color: [f32; 4],
    pub prompt_color: [f32; 4],
    pub input_color: [f32; 4],
    pub output_color: [f32; 4],
    pub error_color: [f32; 4],
    pub cursor_color: [f32; 4],
    #[serde(default)]
    pub markdown: ConsoleMarkdownTheme,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct ConsoleMarkdownTheme {
    pub heading_color: [f32; 4],
    pub emphasis_color: [f32; 4],
    pub strong_color: [f32; 4],
    pub inline_code_color: [f32; 4],
    pub inline_code_background_color: [f32; 4],
    pub fenced_code_color: [f32; 4],
    pub fenced_code_background_color: [f32; 4],
    pub link_color: [f32; 4],
    pub image_color: [f32; 4],
    pub quote_marker_color: [f32; 4],
    pub quote_text_color: [f32; 4],
    pub list_marker_color: [f32; 4],
    pub task_marker_color: [f32; 4],
    pub table_border_color: [f32; 4],
    pub table_header_color: [f32; 4],
    pub table_text_color: [f32; 4],
    pub thematic_break_color: [f32; 4],
    pub muted_marker_color: [f32; 4],
    pub escaped_marker_color: [f32; 4],
    pub code_padding_pixels: f32,
    pub strong_offset_pixels: f32,
}

impl ConsoleMarkdownTheme {
    #[must_use]
    pub fn from_palette(
        separator_color: [f32; 4],
        prompt_color: [f32; 4],
        input_color: [f32; 4],
        output_color: [f32; 4],
    ) -> Self {
        Self {
            heading_color: prompt_color,
            emphasis_color: input_color,
            strong_color: prompt_color,
            inline_code_color: input_color,
            inline_code_background_color: scaled_color(separator_color, 0.35, 0.45),
            fenced_code_color: input_color,
            fenced_code_background_color: scaled_color(separator_color, 0.25, 0.38),
            link_color: [0.45, 0.74, 1.0, 1.0],
            image_color: [0.68, 0.82, 1.0, 1.0],
            quote_marker_color: separator_color,
            quote_text_color: output_color,
            list_marker_color: separator_color,
            task_marker_color: prompt_color,
            table_border_color: separator_color,
            table_header_color: prompt_color,
            table_text_color: output_color,
            thematic_break_color: separator_color,
            muted_marker_color: separator_color,
            escaped_marker_color: input_color,
            code_padding_pixels: 4.0,
            strong_offset_pixels: 1.0,
        }
    }
}

impl Default for ConsoleMarkdownTheme {
    fn default() -> Self {
        Self::from_palette(
            [0.36, 0.40, 0.46, 0.90],
            [0.65, 0.92, 0.72, 1.0],
            [0.92, 0.95, 0.98, 1.0],
            [0.78, 0.82, 0.88, 1.0],
        )
    }
}

impl Default for ConsoleTheme {
    fn default() -> Self {
        let separator_color = [0.36, 0.40, 0.46, 0.90];
        let prompt_color = [0.65, 0.92, 0.72, 1.0];
        let input_color = [0.92, 0.95, 0.98, 1.0];
        let output_color = [0.78, 0.82, 0.88, 1.0];
        Self {
            background_color: [0.02, 0.025, 0.03, 0.88],
            separator_color,
            prompt_color,
            input_color,
            output_color,
            error_color: [1.0, 0.45, 0.42, 1.0],
            cursor_color: [1.0, 1.0, 1.0, 1.0],
            markdown: ConsoleMarkdownTheme::from_palette(
                separator_color,
                prompt_color,
                input_color,
                output_color,
            ),
        }
    }
}

fn scaled_color(color: [f32; 4], rgb_scale: f32, alpha: f32) -> [f32; 4] {
    [
        color[0] * rgb_scale,
        color[1] * rgb_scale,
        color[2] * rgb_scale,
        alpha,
    ]
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.console.config")]
pub struct ConsoleConfig {
    pub panel_height: f32,
    pub activation_key_code: u32,
    pub font_namespace: String,
    pub font_path: String,
    pub font_size: f32,
    pub scrollback_limit: u32,
    pub prompt: String,
    pub theme: ConsoleTheme,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            panel_height: 280.0,
            activation_key_code: u32::from(b'`'),
            font_namespace: String::from("assets"),
            font_path: String::from("fonts/RobotoMono.ttf"),
            font_size: 18.0,
            scrollback_limit: 256,
            prompt: String::from("> "),
            theme: ConsoleTheme::default(),
        }
    }
}

#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.kit.console.register_command")]
pub struct RegisterConsoleCommand {
    pub name: String,
    pub description: String,
    pub mailbox: MailboxId,
}

#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.kit.console.unregister_command")]
pub struct UnregisterConsoleCommand {
    pub name: String,
}

#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.kit.console.command_invoked")]
pub struct ConsoleCommandInvoked {
    pub name: String,
    pub args: Vec<String>,
    pub input: String,
}

#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq,
)]
#[kind(name = "aether.kit.console.command_output")]
pub struct ConsoleCommandOutput {
    pub command: String,
    pub lines: Vec<String>,
    pub error: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::Kind;

    #[test]
    fn command_output_round_trips() {
        let output = ConsoleCommandOutput {
            command: String::from("diagnostics"),
            lines: Vec::from([String::from("ok")]),
            error: false,
        };

        let decoded = ConsoleCommandOutput::decode_from_bytes(&output.encode_into_bytes())
            .expect("output decodes");

        assert_eq!(decoded, output);
    }

    #[test]
    fn default_theme_carries_explicit_markdown_styles() {
        let theme = ConsoleTheme::default();

        assert_eq!(theme.markdown.heading_color, theme.prompt_color);
        assert_eq!(theme.markdown.emphasis_color, theme.input_color);
        assert_eq!(theme.markdown.table_border_color, theme.separator_color);
        assert!(theme.markdown.code_padding_pixels > 0.0);
        assert!(theme.markdown.strong_offset_pixels > 0.0);
    }

    #[test]
    fn registration_schema_has_stable_kind_name() {
        assert_eq!(
            <RegisterConsoleCommand as Kind>::NAME,
            "aether.kit.console.register_command"
        );
        let registration = RegisterConsoleCommand {
            name: String::from("profile"),
            description: String::from("show profiling data"),
            mailbox: MailboxId(0x4000_0000_0000_0001),
        };

        let decoded = RegisterConsoleCommand::decode_from_bytes(&registration.encode_into_bytes())
            .expect("registration decodes");

        assert_eq!(decoded, registration);
    }
}
