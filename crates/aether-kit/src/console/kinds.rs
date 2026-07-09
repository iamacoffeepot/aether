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
}

impl Default for ConsoleTheme {
    fn default() -> Self {
        Self {
            background_color: [0.02, 0.025, 0.03, 0.88],
            separator_color: [0.36, 0.40, 0.46, 0.90],
            prompt_color: [0.65, 0.92, 0.72, 1.0],
            input_color: [0.92, 0.95, 0.98, 1.0],
            output_color: [0.78, 0.82, 0.88, 1.0],
            error_color: [1.0, 0.45, 0.42, 1.0],
            cursor_color: [1.0, 1.0, 1.0, 1.0],
        }
    }
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
            activation_key_code: b'`' as u32,
            font_namespace: String::from("assets"),
            font_path: String::from("fonts/RobotoMono.ttf"),
            font_size: 16.0,
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
